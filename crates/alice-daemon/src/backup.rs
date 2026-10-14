//! Backups (spec §5.6): SQLite's online backup API on a dedicated read-only connection (one
//! step: a consistent snapshot, while WAL readers and the writer go on), checked with
//! `quick_check`, then renamed into place — a failed backup leaves no file. Rotation keeps the
//! newest backup of each of the last `daily` days and of each of the last `weekly` ISO weeks,
//! pre-migration and before-restore copies for 30 days, and never deletes the newest backup.
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

/// Copy `db` into `dir` as a checked, private backup file. On any failure nothing is left
/// behind: the copy is made under a temporary name and renamed into place only when whole.
pub fn create(db: &Path, dir: &Path, kind: BackupKind, now: DateTime<Utc>) -> Result<BackupInfo> {
    alice_core::store::create_private_dir(dir)?;
    if !db.exists() {
        return Err(AliceError::NotFound(format!("database {}", db.display())));
    }
    let name = name_for(kind, now, alice_core::store::migrations::CURRENT_VERSION);
    let path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let journal = dir.join(format!(".{name}.tmp-journal"));
    // A temporary file left by a run that was killed.
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&journal);
    let made = (|| -> Result<()> {
        let src = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut dst = Connection::open(&tmp)?;
        alice_core::store::make_private(&tmp)?;
        {
            let b = rusqlite::backup::Backup::new(&src, &mut dst)?;
            // One step: the whole database inside one read transaction (a consistent snapshot;
            // a stepwise backup restarts whenever the writer commits).
            b.step(-1)?;
        }
        // The copy is a self-contained file (the source's WAL mode is not carried over).
        let _mode: String = dst.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
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
        Ok(())
    })();
    if let Err(e) = made {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&journal);
        return Err(e);
    }
    Ok(BackupInfo {
        bytes: std::fs::metadata(&path)?.len(),
        name,
        path,
        kind,
        created_at: now,
    })
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

/// Delete what the policy does not keep; returns the deleted paths. The newest backup of each
/// kind is always kept, so a policy (or a bug) can never leave a directory without a backup.
pub fn rotate(dir: &Path, now: DateTime<Utc>, daily: u32, weekly: u32) -> Result<Vec<PathBuf>> {
    let all = list(dir)?;
    let mut keep = std::collections::HashSet::new();
    let regular: Vec<&BackupInfo> = all
        .iter()
        .filter(|b| matches!(b.kind, BackupKind::Scheduled | BackupKind::Manual))
        .collect();
    if let Some(newest) = regular.first() {
        keep.insert(newest.path.clone());
    }
    let mut days = Vec::new();
    let mut weeks = Vec::new();
    for b in &regular {
        let day = b.created_at.date_naive();
        if days.len() < daily as usize && !days.contains(&day) {
            days.push(day);
            keep.insert(b.path.clone());
        }
        let week = (
            b.created_at.iso_week().year(),
            b.created_at.iso_week().week(),
        );
        if weeks.len() < weekly as usize && !weeks.contains(&week) {
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
        let mut t = NaiveDateTime::parse_from_str(&ts[..19.min(ts.len())], "%Y-%m-%dT%H-%M-%S")
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
        std::fs::rename(e.path(), &dest)?;
        alice_core::store::make_private(&dest)?;
        n += 1;
    }
    Ok(n)
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

    #[test]
    fn rotation_keeps_seven_days_four_weeks_and_never_the_newest() {
        let d = tempfile::tempdir().unwrap();
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 13, 12, 0, 0)
            .unwrap();
        // Two backups a day for 60 days.
        for day in 0..60 {
            for hour in [1, 13] {
                let t = now - chrono::Duration::days(day) - chrono::Duration::hours(12 - hour);
                std::fs::write(
                    d.path()
                        .join(format!("alice-{}.db", t.format("%Y%m%dT%H%M%SZ"))),
                    b"x",
                )
                .unwrap();
            }
        }
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
        std::fs::write(d.path().join("notes.txt"), b"not a backup").unwrap();
        rotate(d.path(), now, 7, 4).unwrap();
        let kept = list(d.path()).unwrap();
        let regular: Vec<&BackupInfo> = kept
            .iter()
            .filter(|b| b.kind == BackupKind::Scheduled)
            .collect();
        // 7 daily (newest of each of the last 7 days) + newest of each of the last 4 ISO weeks
        // that are not already kept: between 7 and 11 files.
        assert!((7..=11).contains(&regular.len()), "{}", regular.len());
        assert_eq!(
            regular[0].created_at.date_naive(),
            now.date_naive(),
            "the newest is kept"
        );
        let pre: Vec<&BackupInfo> = kept
            .iter()
            .filter(|b| b.kind == BackupKind::PreMigration)
            .collect();
        assert_eq!(
            pre.len(),
            1,
            "older than 30 days are pruned, the newest is always kept"
        );
        assert!(
            d.path().join("notes.txt").exists(),
            "unknown files are never touched"
        );
        // A single old backup is never deleted.
        let e = tempfile::tempdir().unwrap();
        std::fs::write(e.path().join("alice-20250101T000000Z.db"), b"x").unwrap();
        rotate(e.path(), now, 7, 4).unwrap();
        assert_eq!(list(e.path()).unwrap().len(), 1);
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
