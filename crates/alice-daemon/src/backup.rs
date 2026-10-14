//! Backups (spec §5.6): SQLite's online backup API on a dedicated read-only connection (one
//! step: a consistent snapshot, while WAL readers and the writer go on), checked with
//! `quick_check`, then renamed into place — a failed backup leaves no file. Rotation keeps the
//! newest backup of each of the last `daily` days and of each of the last `weekly` ISO weeks,
//! pre-migration and before-restore copies for 30 days, and never deletes the newest backup.
//!
//! Disk: a backup needs room for a second copy, so the job checks the free space first
//! (`space_needed`, `free_space`). Leftover temporary files of a killed copy are swept.
//!
//! A backup is a copy of `alice.db` and nothing else: never `secrets.toml` (or its lock), the
//! token or the vector files. The copy holds every memory, so it is private to its owner: the
//! file is created 0600 and the directory 0700 (Unix).

use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use alice_core::settings::BackupSettings;
use chrono::{DateTime, Datelike, NaiveDateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    Scheduled,
    Manual,
    PreMigration,
    BeforeRestore,
}

impl BackupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackupKind::Scheduled => "scheduled",
            BackupKind::Manual => "manual",
            BackupKind::PreMigration => "pre_migration",
            BackupKind::BeforeRestore => "before_restore",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BackupInfo {
    pub name: String,
    #[schemars(with = "String")]
    pub path: PathBuf,
    pub kind: BackupKind,
    #[schemars(with = "String")]
    pub created_at: DateTime<Utc>,
    pub bytes: u64,
}

const KEEP_SPECIAL_DAYS: i64 = 30;

/// Where backups go: `backup.dir`, or `<home>/backups`.
pub fn backup_dir(home: &AliceHome, s: &BackupSettings) -> PathBuf {
    s.dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.backups_dir())
}

fn stamp(now: DateTime<Utc>) -> String {
    now.format("%Y%m%dT%H%M%SZ").to_string()
}

fn name_for(kind: BackupKind, now: DateTime<Utc>, version: i64) -> String {
    match kind {
        // Manual backups share the scheduled ones' name and rotation.
        BackupKind::Scheduled | BackupKind::Manual => format!("alice-{}.db", stamp(now)),
        BackupKind::PreMigration => format!("alice-pre-migration-v{version}-{}.db", stamp(now)),
        BackupKind::BeforeRestore => format!("alice-before-restore-{}.db", stamp(now)),
    }
}

/// (kind, time) of a backup file name; `None` for anything else.
fn parse_name(name: &str) -> Option<(BackupKind, DateTime<Utc>)> {
    let rest = name.strip_prefix("alice-")?.strip_suffix(".db")?;
    let (kind, ts) = if let Some(r) = rest.strip_prefix("pre-migration-") {
        (BackupKind::PreMigration, r.split_once('-')?.1)
    } else if let Some(r) = rest.strip_prefix("before-restore-") {
        (BackupKind::BeforeRestore, r)
    } else {
        (BackupKind::Scheduled, rest)
    };
    let t = NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%SZ")
        .ok()?
        .and_utc();
    Some((kind, t))
}

/// Pages copied per step of a backup, so the stop check runs about every 4 MB.
const STEP_PAGES: i32 = 1024;
/// Restarts (the writer committed during the copy and SQLite began again) after which the
/// rest is copied in one step, which cannot be restarted.
const MAX_RESTARTS: u32 = 3;
/// A temporary file of a killed copy is swept when it is this old.
const STALE_TEMP: std::time::Duration = std::time::Duration::from_secs(3_600);

/// Copy `db` into `dir` as a checked, private backup file. On any failure nothing is left
/// behind: the copy is made under a temporary name and renamed into place only when whole.
pub fn create(db: &Path, dir: &Path, kind: BackupKind, now: DateTime<Utc>) -> Result<BackupInfo> {
    create_stoppable(db, dir, kind, now, &|| false)?
        .ok_or_else(|| AliceError::Other("the backup was stopped".into()))
}

/// [`create`] that looks at `stop` between steps of about 4 MB and gives up (`Ok(None)`, no file
/// left) when it says so. The copy is made in steps, each a consistent view of its pages; when
/// the writer commits meanwhile SQLite starts the copy again, and after `MAX_RESTARTS` of those
/// the rest is taken in one step (a snapshot nothing can restart, but not stoppable).
pub fn create_stoppable(
    db: &Path,
    dir: &Path,
    kind: BackupKind,
    now: DateTime<Utc>,
    stop: &dyn Fn() -> bool,
) -> Result<Option<BackupInfo>> {
    alice_core::store::create_private_dir(dir)?;
    if !db.exists() {
        return Err(AliceError::NotFound(format!("database {}", db.display())));
    }
    sweep_temp(dir);
    let name = name_for(kind, now, alice_core::store::migrations::CURRENT_VERSION);
    let path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let journal = dir.join(format!(".{name}.tmp-journal"));
    // A temporary file left by a run that was killed.
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&journal);
    let made = (|| -> Result<bool> {
        let src = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut dst = Connection::open(&tmp)?;
        alice_core::store::make_private(&tmp)?;
        {
            use rusqlite::backup::StepResult;
            let b = rusqlite::backup::Backup::new(&src, &mut dst)?;
            let (mut restarts, mut last_remaining) = (0u32, None::<i32>);
            loop {
                if stop() {
                    return Ok(false);
                }
                let pages = if restarts >= MAX_RESTARTS {
                    -1
                } else {
                    STEP_PAGES
                };
                match b.step(pages)? {
                    StepResult::Done => break,
                    StepResult::Busy | StepResult::Locked => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    _ => {}
                }
                let remaining = b.progress().remaining;
                if last_remaining.is_some_and(|prev| remaining > prev) {
                    restarts += 1;
                }
                last_remaining = Some(remaining);
            }
        }
        // The copy is a self-contained file (the source's WAL mode is not carried over).
        let _mode: String = dst.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
        if stop() {
            return Ok(false);
        }
        let problems = alice_core::store::maintenance::quick_check(&dst)?;
        if !problems.is_empty() {
            return Err(AliceError::Other(format!(
                "the backup copy failed its check: {}",
                problems.join("; ")
            )));
        }
        drop(dst);
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        sync_dir(dir);
        Ok(true)
    })();
    match made {
        Ok(true) => Ok(Some(BackupInfo {
            bytes: std::fs::metadata(&path)?.len(),
            name,
            path,
            kind,
            created_at: now,
        })),
        other => {
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(&journal);
            other.map(|_| None)
        }
    }
}

/// Delete temporary files of a copy that was killed (`.alice-….tmp`, `….tmp-journal`) once
/// they are an hour old; a copy in progress is younger.
pub fn sweep_temp(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(".alice-") && name.contains(".tmp")) {
            continue;
        }
        let old = e.metadata().ok().filter(|m| m.is_file()).is_some_and(|m| {
            m.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= STALE_TEMP)
        });
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Room a backup of `db` needs: twice the database and its WAL (the copy, and slack for the
/// writer's growth meanwhile).
pub fn space_needed(db: &Path) -> u64 {
    let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    2 * (len(db) + len(Path::new(&wal)))
}

/// Free bytes on the volume that holds `path` (or its nearest existing parent); `None` where
/// that is not known (not Unix, or the call failed), and the caller then goes ahead.
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut p = path;
        while !p.exists() {
            p = p.parent()?;
        }
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c` is a valid C string and `s` is a zeroed `statvfs` the call fills.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: as above; the call only writes into `s`.
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
        // The field types differ between systems (u32 here, u64 there).
        #[allow(clippy::useless_conversion)]
        (rc == 0).then(|| u64::from(s.f_bavail) * u64::from(s.f_frsize))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Make the rename durable (Unix; best effort).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Backups in `dir`, newest first. Other files (and links, and directories) are ignored.
pub fn list(dir: &Path) -> Result<Vec<BackupInfo>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for e in rd.flatten() {
        // `DirEntry::file_type` does not follow a link: only regular files are backups.
        if !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some((kind, created_at)) = parse_name(&name) {
            out.push(BackupInfo {
                bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
                path: e.path(),
                name,
                kind,
                created_at,
            });
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.name.cmp(&a.name)));
    Ok(out)
}

/// Delete what the policy does not keep; returns the deleted paths. Of the regular
/// (scheduled and manual) backups it keeps the newest of each of the last `daily` days that
/// have one, and the newest of each of the `weekly` most recent ISO weeks that the daily ones
/// do not touch: with enough history exactly `daily + weekly` files (spec §5.6: 7 daily + 4
/// weekly). The newest backup of each kind is always kept, so a policy (or a bug) can never
/// leave a directory without a backup. Stale temporary files are swept too.
pub fn rotate(dir: &Path, now: DateTime<Utc>, daily: u32, weekly: u32) -> Result<Vec<PathBuf>> {
    sweep_temp(dir);
    let all = list(dir)?;
    let mut keep = std::collections::HashSet::new();
    let regular: Vec<&BackupInfo> = all
        .iter()
        .filter(|b| matches!(b.kind, BackupKind::Scheduled | BackupKind::Manual))
        .collect();
    if let Some(newest) = regular.first() {
        keep.insert(newest.path.clone());
    }
    let week_of = |b: &BackupInfo| {
        (
            b.created_at.iso_week().year(),
            b.created_at.iso_week().week(),
        )
    };
    let mut days = Vec::new();
    let mut covered_weeks = std::collections::HashSet::new();
    for b in &regular {
        let day = b.created_at.date_naive();
        if days.len() < daily as usize && !days.contains(&day) {
            days.push(day);
            covered_weeks.insert(week_of(b));
            keep.insert(b.path.clone());
        }
    }
    let mut weeks = Vec::new();
    for b in &regular {
        let week = week_of(b);
        if weeks.len() < weekly as usize && !covered_weeks.contains(&week) && !weeks.contains(&week)
        {
            weeks.push(week);
            keep.insert(b.path.clone());
        }
    }
    for kind in [BackupKind::PreMigration, BackupKind::BeforeRestore] {
        let of_kind: Vec<&BackupInfo> = all.iter().filter(|b| b.kind == kind).collect();
        if let Some(newest) = of_kind.first() {
            keep.insert(newest.path.clone());
        }
        for b in of_kind {
            if now - b.created_at < chrono::Duration::days(KEEP_SPECIAL_DAYS) {
                keep.insert(b.path.clone());
            }
        }
    }
    let mut deleted = Vec::new();
    for b in &all {
        if !keep.contains(&b.path) {
            std::fs::remove_file(&b.path)?;
            deleted.push(b.path.clone());
        }
    }
    Ok(deleted)
}

/// M1 wrote pre-migration backups next to `alice.db` as `alice.pre-migration-<stamp>.db`; move
/// them into `dir`. Only regular files directly in the home are taken: a link (which might point
/// anywhere) is left alone, and an existing backup is never overwritten.
pub fn adopt_legacy(home: &AliceHome, dir: &Path) -> Result<usize> {
    let mut n = 0;
    for e in std::fs::read_dir(home.root())?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(ts) = name
            .strip_prefix("alice.pre-migration-")
            .and_then(|r| r.strip_suffix(".db"))
        else {
            continue;
        };
        if !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        // e.g. 2026-10-13T09-44-23-391Z
        let mut t = NaiveDateTime::parse_from_str(ts.get(..19).unwrap_or(ts), "%Y-%m-%dT%H-%M-%S")
            .map(|t| t.and_utc())
            .unwrap_or_else(|_| Utc::now());
        alice_core::store::create_private_dir(dir)?;
        let dest = loop {
            let d = dir.join(format!("alice-pre-migration-v1-{}.db", stamp(t)));
            if !d.exists() {
                break d;
            }
            t += chrono::Duration::seconds(1);
        };
        move_file(&e.path(), &dest)?;
        alice_core::store::make_private(&dest)?;
        n += 1;
    }
    Ok(n)
}

/// Rename `from` to `to`; across volumes (the backup directory may be elsewhere) copy it and
/// then remove the original.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to).or_else(|_| copy_then_remove(from, to))
}

fn copy_then_remove(from: &Path, to: &Path) -> std::io::Result<()> {
    let tmp = to.with_extension("db.moving");
    let copied = std::fs::copy(from, &tmp).and_then(|_| std::fs::rename(&tmp, to));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn db_with_rows(dir: &Path) -> PathBuf {
        let path = dir.join("alice.db");
        let c = alice_core::store::open_writer(&path).unwrap();
        c.execute("INSERT INTO meta (key, value) VALUES ('x', 'y')", [])
            .unwrap();
        path
    }

    #[test]
    fn a_backup_is_a_checked_copy_and_a_failure_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let db = db_with_rows(d.path());
        let dir = d.path().join("backups");
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 3, 0, 0).unwrap();
        let b = create(&db, &dir, BackupKind::Scheduled, now).unwrap();
        assert_eq!(b.name, "alice-20261013T030000Z.db");
        let c = rusqlite::Connection::open(&b.path).unwrap();
        let v: String = c
            .query_row("SELECT value FROM meta WHERE key = 'x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "y");
        assert!(create(&d.path().join("missing.db"), &dir, BackupKind::Manual, now).is_err());
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![b.name.clone()],
            "no temp or partial file is left"
        );
        assert_eq!(list(&dir).unwrap()[0].kind, BackupKind::Scheduled);
    }

    /// A source that is not a database (a failed copy) leaves no file in `backups/`.
    #[test]
    fn a_copy_that_fails_midway_leaves_no_file() {
        let d = tempfile::tempdir().unwrap();
        let bad = d.path().join("alice.db");
        std::fs::write(&bad, vec![7u8; 4096]).unwrap();
        let dir = d.path().join("backups");
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 3, 0, 0).unwrap();
        assert!(create(&bad, &dir, BackupKind::Manual, now).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// B2: a backup is a copy of the database and nothing else; it is private to its owner.
    #[test]
    fn a_backup_holds_no_secrets_and_is_private() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let db = db_with_rows(d.path());
        std::fs::write(home.secrets_file(), "typesafe_api_key = \"abc\"\n").unwrap();
        std::fs::write(d.path().join("secrets.toml.lock"), b"").unwrap();
        std::fs::write(home.token_file(), "token").unwrap();
        let dir = backup_dir(&home, &BackupSettings::default());
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 3, 0, 0).unwrap();
        let b = create(&db, &dir, BackupKind::Manual, now).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![b.name.clone()], "{names:?}");
        let c = rusqlite::Connection::open(&b.path).unwrap();
        let tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            tables.iter().all(|t| !t.contains("secret")),
            "alice.db has no secrets table: {tables:?}"
        );
        let bytes = std::fs::read(&b.path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("typesafe_api_key"),
            "the secrets file's content is not in the copy"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&b.path), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }
    }

    /// A custom backup directory that does not exist is created private too.
    #[cfg(unix)]
    #[test]
    fn a_new_backup_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let db = db_with_rows(d.path());
        let dir = d.path().join("elsewhere/nested/backups");
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 3, 0, 0).unwrap();
        create(&db, &dir, BackupKind::Manual, now).unwrap();
        for p in [&dir, &d.path().join("elsewhere/nested")] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", p.display());
        }
    }

    fn put_backup(dir: &Path, t: DateTime<Utc>) {
        std::fs::write(
            dir.join(format!("alice-{}.db", t.format("%Y%m%dT%H%M%SZ"))),
            b"x",
        )
        .unwrap();
    }

    fn regular(dir: &Path) -> Vec<BackupInfo> {
        list(dir)
            .unwrap()
            .into_iter()
            .filter(|b| b.kind == BackupKind::Scheduled)
            .collect()
    }

    /// I-5 (spec §5.6): exactly 7 daily and 4 weekly. Daily backups for 60 days leave the newest
    /// of each of the last 7 days and the newest of each of the 4 ISO weeks the daily ones do
    /// not touch: 11 files, whatever the weekday.
    #[test]
    fn rotation_keeps_exactly_seven_daily_and_four_weekly() {
        for hour_shift in 0..7 {
            let d = tempfile::tempdir().unwrap();
            let now = chrono::Utc
                .with_ymd_and_hms(2026, 10, 7 + hour_shift, 12, 0, 0)
                .unwrap();
            for day in 0..60 {
                put_backup(d.path(), now - chrono::Duration::days(day));
            }
            std::fs::write(d.path().join("notes.txt"), b"not a backup").unwrap();
            rotate(d.path(), now, 7, 4).unwrap();
            let kept = regular(d.path());
            assert_eq!(
                kept.len(),
                11,
                "now = {now}: {:?}",
                kept.iter().map(|b| &b.name).collect::<Vec<_>>()
            );
            // The 7 newest days are all there.
            for day in 0..7 {
                let want = (now - chrono::Duration::days(day)).date_naive();
                assert!(
                    kept.iter().any(|b| b.created_at.date_naive() == want),
                    "day {day}"
                );
            }
            // The other 4 are in 4 different ISO weeks, none of them a week of the daily 7.
            let daily_weeks: std::collections::HashSet<_> = (0..7)
                .map(|d| (now - chrono::Duration::days(d)).iso_week().week())
                .collect();
            let older: Vec<_> = kept.iter().skip(7).collect();
            let weeks: std::collections::HashSet<_> = older
                .iter()
                .map(|b| b.created_at.iso_week().week())
                .collect();
            assert_eq!(weeks.len(), 4);
            assert!(weeks.is_disjoint(&daily_weeks));
            assert!(d.path().join("notes.txt").exists());
        }
        // Two backups a day: still one per day.
        let d = tempfile::tempdir().unwrap();
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 13, 12, 0, 0)
            .unwrap();
        for day in 0..60 {
            put_backup(d.path(), now - chrono::Duration::days(day));
            put_backup(
                d.path(),
                now - chrono::Duration::days(day) - chrono::Duration::hours(6),
            );
        }
        rotate(d.path(), now, 7, 4).unwrap();
        assert_eq!(regular(d.path()).len(), 11);
    }

    /// With gaps (a machine that was off) the policy keeps what exists: the newest backup of
    /// each of the last 7 days that have one, then the 4 weeks before them.
    #[test]
    fn rotation_with_gaps_and_short_histories() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 13, 12, 0, 0)
            .unwrap();
        // Backups on days 0, 1, 2, 20-22, 40-43, 60-66, 80 and 90.
        let days = [
            0, 1, 2, 20, 21, 22, 40, 41, 42, 43, 60, 61, 62, 63, 64, 65, 66, 80, 90,
        ];
        let d = tempfile::tempdir().unwrap();
        for day in days {
            put_backup(d.path(), now - chrono::Duration::days(day));
        }
        rotate(d.path(), now, 7, 4).unwrap();
        let kept = regular(d.path());
        // The 7 newest backups (0,1,2,20,21,22,40) are the daily ones; their weeks are covered
        // (days 41-43 share the week of day 40), so the 4 weekly ones are the weeks of days
        // 60, 65, 80 and 90.
        assert_eq!(
            kept.len(),
            11,
            "{:?}",
            kept.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
        for day in [0, 1, 2, 20, 21, 22, 40] {
            let want = (now - chrono::Duration::days(day)).date_naive();
            assert!(
                kept.iter().any(|b| b.created_at.date_naive() == want),
                "day {day}"
            );
        }
        // A short history keeps everything it has, up to the policy.
        let e = tempfile::tempdir().unwrap();
        for day in [0, 3, 9] {
            put_backup(e.path(), now - chrono::Duration::days(day));
        }
        assert!(rotate(e.path(), now, 7, 4).unwrap().is_empty());
        assert_eq!(regular(e.path()).len(), 3);
    }

    #[test]
    fn rotation_prunes_pre_migration_copies_and_never_the_newest() {
        let d = tempfile::tempdir().unwrap();
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 13, 12, 0, 0)
            .unwrap();
        std::fs::write(
            d.path().join("alice-pre-migration-v2-20260801T000000Z.db"),
            b"x",
        )
        .unwrap();
        std::fs::write(
            d.path().join("alice-pre-migration-v1-20260701T000000Z.db"),
            b"x",
        )
        .unwrap();
        std::fs::write(
            d.path().join("alice-pre-migration-v2-20261010T000000Z.db"),
            b"x",
        )
        .unwrap();
        rotate(d.path(), now, 7, 4).unwrap();
        let pre: Vec<_> = list(d.path())
            .unwrap()
            .into_iter()
            .filter(|b| b.kind == BackupKind::PreMigration)
            .collect();
        assert_eq!(
            pre.len(),
            1,
            "only the recent copy is under 30 days; the newest is kept"
        );
        assert!(pre[0].name.contains("20261010"));
        // A single old backup is never deleted.
        let e = tempfile::tempdir().unwrap();
        put_backup(
            e.path(),
            chrono::Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        );
        rotate(e.path(), now, 7, 4).unwrap();
        assert_eq!(list(e.path()).unwrap().len(), 1);
    }

    /// I-4: the temporary file of a killed copy is swept once it is an hour old; a younger one
    /// (a copy in progress) and anything else stay.
    #[test]
    fn stale_temporary_files_are_swept() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join(".alice-20260101T000000Z.db.tmp");
        let old_journal = d.path().join(".alice-20260101T000000Z.db.tmp-journal");
        let fresh = d.path().join(".alice-20261013T000000Z.db.tmp");
        let other = d.path().join(".hidden.tmp");
        for p in [&old, &old_journal, &fresh, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7_200);
        for p in [&old, &old_journal, &other] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }
        let now = chrono::Utc::now();
        rotate(d.path(), now, 7, 4).unwrap();
        assert!(!old.exists() && !old_journal.exists());
        assert!(fresh.exists() && other.exists());
    }

    /// I-4: a backup that is asked to stop leaves no file, however far it got.
    #[test]
    fn a_backup_can_be_stopped_and_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        // A database of a few MB: several steps.
        let db = d.path().join("alice.db");
        {
            let c = alice_core::store::open_writer(&db).unwrap();
            c.execute_batch("CREATE TABLE junk (x BLOB)").unwrap();
            for _ in 0..40 {
                c.execute("INSERT INTO junk VALUES (randomblob(100000))", [])
                    .unwrap();
            }
        }
        let dir = d.path().join("backups");
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 3, 0, 0).unwrap();
        let steps = std::cell::Cell::new(0);
        let stopped = create_stoppable(&db, &dir, BackupKind::Scheduled, now, &|| {
            steps.set(steps.get() + 1);
            steps.get() > 2
        })
        .unwrap();
        assert!(stopped.is_none());
        assert!(steps.get() >= 3, "it ran some steps before it stopped");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // Not stopped: the copy is whole and checked.
        let whole = create_stoppable(&db, &dir, BackupKind::Scheduled, now, &|| false)
            .unwrap()
            .unwrap();
        assert!(whole.bytes > 4_000_000);
    }

    #[test]
    fn the_space_a_backup_needs_and_the_free_space_are_known() {
        let d = tempfile::tempdir().unwrap();
        let db = db_with_rows(d.path());
        let len = std::fs::metadata(&db).unwrap().len();
        assert!(space_needed(&db) >= 2 * len);
        #[cfg(unix)]
        {
            let free = free_space(&d.path().join("not/yet/there")).expect("a volume");
            assert!(free > 1_000_000, "{free}");
        }
    }

    #[test]
    fn moving_across_volumes_copies_then_removes() {
        let d = tempfile::tempdir().unwrap();
        let (from, to) = (d.path().join("a.db"), d.path().join("b.db"));
        std::fs::write(&from, b"payload").unwrap();
        copy_then_remove(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"payload");
        assert!(!d.path().join("b.db.moving").exists());
    }

    #[test]
    fn legacy_pre_migration_files_are_moved_into_backups() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(
            d.path()
                .join("alice.pre-migration-2026-10-13T09-44-23-391Z.db"),
            b"x",
        )
        .unwrap();
        let dir = d.path().join("backups");
        assert_eq!(adopt_legacy(&home, &dir).unwrap(), 1);
        let b = list(&dir).unwrap();
        assert_eq!((b.len(), b[0].kind), (1, BackupKind::PreMigration));
        assert_eq!(
            adopt_legacy(&home, &dir).unwrap(),
            0,
            "nothing is moved twice"
        );
    }

    /// B2: a link named like a legacy backup is not followed out of the home.
    #[cfg(unix)]
    #[test]
    fn adopting_never_follows_a_link_out_of_the_home() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("precious.db");
        std::fs::write(&target, b"precious").unwrap();
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        std::os::unix::fs::symlink(
            &target,
            d.path()
                .join("alice.pre-migration-2026-10-13T09-44-23-391Z.db"),
        )
        .unwrap();
        let dir = d.path().join("backups");
        assert_eq!(adopt_legacy(&home, &dir).unwrap(), 0);
        assert!(target.exists() && std::fs::read(&target).unwrap() == b"precious");
        assert!(list(&dir).unwrap().is_empty());
        // A link inside the backup directory is not a backup either (rotation leaves it).
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("alice-20200101T000000Z.db")).unwrap();
        assert!(list(&dir).unwrap().is_empty());
        rotate(&dir, chrono::Utc::now(), 1, 1).unwrap();
        assert!(dir.join("alice-20200101T000000Z.db").exists());
        assert!(target.exists());
    }
}
