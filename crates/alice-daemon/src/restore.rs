//! Restore (spec §5.6): verify a backup, then, while holding the daemon lock so that no daemon
//! runs, keep the current database as `alice-before-restore-<stamp>.db`, put the backup in its
//! place and drop the vector index (rebuilt at the next start).
//!
//! What a restore touches: `alice.db` (and the stale `-wal`/`-shm` of the database it replaces)
//! and the files in `vectors/`. Never `secrets.toml` or its lock, the token, the config, the
//! spool (writes made while the daemon was away stay there and are applied, through the normal
//! redaction path, once the restored database is served) or the backups themselves.
//!
//! The invariant: at every instant `alice.db` is either the complete old database or the
//! complete restored one. The backup is staged beside it and verified first; the old database is
//! kept (a checked copy, or, when it is itself unreadable, a link to the damaged file); only then
//! the stale WAL goes aside and one atomic `rename` puts the staged file in place. Any failure
//! before the rename leaves `alice.db` as it was (sidecars are put back).
//!
//! Older backups are accepted: the daemon migrates the restored file when it starts, and takes
//! its own pre-migration backup first. A backup from a newer schema is refused.
//!
//! Caveats (also printed by `alice restore`): memories purged after the backup was taken come
//! back with it, and text stored under older redaction rules is not scanned again.

use crate::backup::{self, BackupKind};
use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};

/// What [`swap`] says when a daemon holds the lock (the CLI stops the daemon and retries).
pub const DAEMON_RUNNING: &str = "a daemon is still running for this data";

/// Days a before-restore copy is kept at least (the newest one is kept for ever); see
/// `backup::rotate`.
pub const KEEP_DAYS: i64 = backup::KEEP_SPECIAL_DAYS;

/// What a restore does not undo, for the CLI output and the docs.
pub const CAVEATS: &str = "A restore brings back memories purged after the backup was taken, and text stored under older redaction rules is not scanned again.";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct VerifiedBackup {
    pub path: PathBuf,
    pub schema_version: i64,
    pub memories: u64,
    pub chunks: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RestoreReport {
    pub restored_from: String,
    /// File name in the backups directory, when there was a current database.
    pub saved_current_as: Option<String>,
    pub memories: u64,
    pub chunks: u64,
    /// Schema of the restored file; the daemon migrates it when it is older than this build.
    pub schema_version: i64,
    /// Things that went less well than usual, in words (the replaced database was moved aside
    /// as it was because it could not be copied, say).
    pub warnings: Vec<String>,
    /// What a restore does not undo ([`CAVEATS`]).
    pub caveats: &'static str,
    /// The command that puts the replaced database back, when one was kept.
    pub undo: Option<String>,
}

/// Open `path` read-only. A valid file whose directory cannot be written (a WAL-mode file needs
/// to create `-shm`) is opened `immutable` instead of being reported as not a backup.
fn open_ro(path: &Path) -> std::result::Result<Connection, String> {
    let probe = |c: Connection| -> rusqlite::Result<Connection> {
        c.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })?;
        Ok(c)
    };
    let first = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(probe);
    match first {
        Ok(c) => Ok(c),
        Err(e) => {
            let text = path
                .to_string_lossy()
                .replace('%', "%25")
                .replace('?', "%3f")
                .replace('#', "%23");
            let uri = format!("file:{text}?immutable=1");
            Connection::open_with_flags(
                uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )
            .and_then(probe)
            .map_err(|_| e.to_string())
        }
    }
}

/// Check that `path` is an Alice backup this build can restore: a SQLite file that passes
/// `quick_check`, has Alice's tables and a schema version not newer than this build's.
///
/// ```no_run
/// let v = alice_daemon::restore::verify(std::path::Path::new("alice-manual-20261014T120000Z.db"))?;
/// println!("{} memories", v.memories);
/// # Ok::<(), alice_core::error::AliceError>(())
/// ```
pub fn verify(path: &Path) -> Result<VerifiedBackup> {
    let not_ours = |why: String| {
        AliceError::invalid(format!(
            "{} is not an Alice backup ({why}); pick one from `alice backup list`",
            path.display()
        ))
    };
    if !path.is_file() {
        return Err(AliceError::NotFound(format!("backup {}", path.display())));
    }
    let c = open_ro(path).map_err(not_ours)?;
    let problems =
        alice_core::store::maintenance::quick_check(&c).map_err(|e| not_ours(e.to_string()))?;
    if !problems.is_empty() {
        return Err(AliceError::invalid(format!(
            "{} is damaged ({}); pick another backup",
            path.display(),
            problems[0]
        )));
    }
    let version: Option<i64> = c
        .query_row("SELECT max(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| not_ours(e.to_string()))?
        .flatten();
    let version = version.ok_or_else(|| not_ours("no schema version".into()))?;
    if version > alice_core::store::migrations::CURRENT_VERSION {
        return Err(AliceError::invalid(format!(
            "{} was made by a newer Alice (schema v{version}, this build reads up to v{}); upgrade Alice first",
            path.display(),
            alice_core::store::migrations::CURRENT_VERSION
        )));
    }
    let count = |t: &str| -> Result<u64> {
        let n: i64 = c
            .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
            .map_err(|e| not_ours(e.to_string()))?;
        Ok(n.max(0) as u64)
    };
    Ok(VerifiedBackup {
        path: path.to_path_buf(),
        schema_version: version,
        memories: count("memories")?,
        chunks: count("chunks")?,
    })
}

/// A backup by file name (in `dir`), by path, or the newest scheduled or manual one.
pub fn find_backup(_home: &AliceHome, dir: &Path, name_or_path: Option<&str>) -> Result<PathBuf> {
    match name_or_path {
        None => backup::list(dir)?
            .into_iter()
            .find(|b| matches!(b.kind, BackupKind::Scheduled | BackupKind::Manual))
            .map(|b| b.path)
            .ok_or_else(|| {
                AliceError::NotFound(format!(
                    "backups in {}; make one with `alice backup now`",
                    dir.display()
                ))
            }),
        Some(s) => {
            let p = PathBuf::from(s);
            // A bare name is a backup in the backups directory first (a file of that name in
            // the current directory must not shadow it).
            let bare = p.components().count() == 1;
            let named = dir.join(s);
            if bare && named.is_file() {
                return Ok(named);
            }
            if p.is_file() {
                return Ok(p);
            }
            if named.is_file() {
                return Ok(named);
            }
            Err(AliceError::NotFound(format!(
                "backup {s}; see `alice backup list`"
            )))
        }
    }
}

/// The staged copy of the backup and the WAL files put aside during a swap. They live in the
/// home, beside `alice.db` (a rename needs one volume) and are swept by [`sweep_staged`].
const STAGED_PREFIX: &str = ".alice-restore";

fn staged_path(home: &AliceHome) -> PathBuf {
    home.root().join(format!("{STAGED_PREFIX}.tmp"))
}

/// Remove what a killed restore left in the home (`.alice-restore*`). Only call it while holding
/// the daemon lock: the restore that owned those files is then gone. A daemon does at start-up.
pub fn sweep_staged(home: &AliceHome) {
    let Ok(rd) = std::fs::read_dir(home.root()) else {
        return;
    };
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with(STAGED_PREFIX)
            && e.file_type().is_ok_and(|t| t.is_file())
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Only one `alice restore` at a time, and a mark clients can see: while it is held, clients
/// do not start a daemon (it could not get the lock anyway) and spool writes at once instead of
/// waiting for one.
pub struct RestoreGuard {
    _file: std::fs::File,
}

fn restore_lock(home: &AliceHome) -> PathBuf {
    home.root().join("restore.lock")
}

impl RestoreGuard {
    pub fn acquire(home: &AliceHome) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(restore_lock(home))?;
        // A client probing `in_progress` holds the lock for an instant: try a few times.
        for _ in 0..5 {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        Err(AliceError::Other(
            "a restore is already running for this data; wait for it to finish".into(),
        ))
    }
}

/// Whether an `alice restore` is running now.
pub fn in_progress(home: &AliceHome) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .open(restore_lock(home))
    else {
        return false;
    };
    matches!(
        file.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    )
}

/// [`swap_verified`] after verifying `backup`.
pub fn swap(
    home: &AliceHome,
    dir: &Path,
    backup: &Path,
    now: DateTime<Utc>,
) -> Result<RestoreReport> {
    let verified = verify(backup)?;
    swap_verified(home, dir, &verified, now)
}

/// Replace `alice.db` by the verified backup. Needs the daemon lock (an error that starts with
/// [`DAEMON_RUNNING`] when a daemon has it), refuses a volume without room before it changes
/// anything, keeps the current database as a before-restore backup in `dir` and removes the
/// vector index files. See the module doc for the order and the invariant it keeps.
pub fn swap_verified(
    home: &AliceHome,
    dir: &Path,
    backup: &VerifiedBackup,
    now: DateTime<Utc>,
) -> Result<RestoreReport> {
    swap_with(
        home,
        dir,
        backup,
        now,
        &Env {
            free: &backup::free_space,
            fault: &|_| Ok(()),
        },
    )
}

/// [`swap_verified`] stamped with the time of the call.
pub fn swap_now(home: &AliceHome, dir: &Path, backup: &VerifiedBackup) -> Result<RestoreReport> {
    swap_verified(home, dir, backup, Utc::now())
}

/// The steps of a swap, for tests that make one fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Stage,
    Save,
    Index,
    Sidecars,
    Rename,
}

/// What a swap asks of the world: free space, and a hook called before each step that a test
/// can make fail.
struct Env<'a> {
    free: &'a dyn Fn(&Path) -> Option<u64>,
    fault: &'a dyn Fn(Step) -> std::io::Result<()>,
}

fn swap_with(
    home: &AliceHome,
    dir: &Path,
    verified: &VerifiedBackup,
    now: DateTime<Utc>,
    env: &Env,
) -> Result<RestoreReport> {
    let source = &verified.path;
    let db = home.db();
    if let (Ok(a), Ok(b)) = (source.canonicalize(), db.canonicalize())
        && a == b
    {
        return Err(AliceError::invalid(
            "that is the live database itself, not a backup; pick one from `alice backup list`",
        ));
    }
    if source
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with(STAGED_PREFIX))
    {
        return Err(AliceError::invalid(
            "that file is left over from an interrupted restore, not a backup; pick one from `alice backup list`",
        ));
    }
    let Some(lock) = crate::daemon::try_lock_home(home)? else {
        return Err(AliceError::Other(format!(
            "{DAEMON_RUNNING}; run `alice daemon stop` first"
        )));
    };
    ensure_room(home, dir, source, env.free)?;
    alice_core::store::create_private_dir(dir)?;
    sweep_staged(home);
    let staged = staged_path(home);
    let mut warnings = Vec::new();

    // 1. Stage the backup beside alice.db and check the staged file itself. Nothing of the
    // current database has been touched.
    let staged_ok = (|| -> Result<()> {
        (env.fault)(Step::Stage)?;
        copy_private(source, &staged)?;
        let v = verify(&staged)?;
        if (v.schema_version, v.memories, v.chunks)
            != (verified.schema_version, verified.memories, verified.chunks)
        {
            return Err(AliceError::Other(format!(
                "{} changed after it was checked",
                source.display()
            )));
        }
        Ok(())
    })();
    if let Err(e) = staged_ok {
        let _ = std::fs::remove_file(&staged);
        return Err(AliceError::Other(format!(
            "cannot stage the backup ({e}); nothing was changed"
        )));
    }

    // 2. Keep what is there.
    let kept = if db.exists() {
        match save_current(&db, dir, now, env, &mut warnings) {
            Ok(k) => Some(k),
            Err(e) => {
                let _ = std::fs::remove_file(&staged);
                return Err(e);
            }
        }
    } else {
        None
    };
    let abort = |e: AliceError| -> AliceError {
        let _ = std::fs::remove_file(&staged);
        if let Some(k) = &kept {
            for suffix in ["", "-wal", "-shm"] {
                let _ =
                    std::fs::remove_file(PathBuf::from(format!("{}{suffix}", k.path.display())));
            }
        }
        AliceError::Other(format!("{e}; the current database is unchanged"))
    };

    // 3. The vector index belongs to the replaced database. Gone before the swap (a stale
    // index would be trusted by key), and a file that cannot be removed stops the restore.
    if let Err(e) = remove_index(home, env) {
        return Err(abort(e));
    }

    // 4. Fold the WAL into the database where it can be read, then put the sidecars aside.
    if kept.as_ref().is_some_and(|k| !k.moved_aside) {
        checkpoint(&db);
    }
    let mut aside = Vec::new();
    if let Err(e) = (|| -> Result<()> {
        (env.fault)(Step::Sidecars)?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", db.display()));
            if side.exists() {
                let bak = home.root().join(format!("{STAGED_PREFIX}{suffix}.bak"));
                std::fs::rename(&side, &bak)?;
                aside.push((side, bak));
            }
        }
        Ok(())
    })() {
        put_back(&aside);
        return Err(abort(e));
    }

    // 5. One atomic rename: alice.db is the old database until this call, the restored one after.
    if let Err(e) = (env.fault)(Step::Rename).and_then(|_| std::fs::rename(&staged, &db)) {
        put_back(&aside);
        return Err(abort(e.into()));
    }
    backup::sync_dir(home.root());
    for (_, bak) in &aside {
        let _ = std::fs::remove_file(bak);
    }
    drop(lock);
    let name = kept.map(|k| k.name);
    Ok(RestoreReport {
        restored_from: source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        undo: name.as_ref().map(|n| format!("alice restore {n}")),
        saved_current_as: name,
        memories: verified.memories,
        chunks: verified.chunks,
        schema_version: verified.schema_version,
        warnings,
        caveats: CAVEATS,
    })
}

fn put_back(aside: &[(PathBuf, PathBuf)]) {
    for (side, bak) in aside {
        let _ = std::fs::rename(bak, side);
    }
}

/// Fold a WAL into its database (best effort; a busy or damaged database is left as it is).
fn checkpoint(db: &Path) {
    if let Ok(c) = Connection::open(db) {
        let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    }
}

fn remove_index(home: &AliceHome, env: &Env) -> Result<()> {
    (env.fault)(Step::Index)?;
    let dir = home.root().join("vectors");
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for e in rd.flatten() {
        if e.file_type().is_ok_and(|t| !t.is_dir()) {
            std::fs::remove_file(e.path()).map_err(|err| {
                AliceError::Other(format!("cannot remove {}: {err}", e.path().display()))
            })?;
        }
    }
    Ok(())
}

/// The volume must hold the restored copy (in the home) and the before-restore copy (in `dir`);
/// both land on one volume when it is the same. Where the free space is not known, go ahead.
/// The CLI asks this before it stops the daemon.
pub fn check_room(home: &AliceHome, dir: &Path, source: &Path) -> Result<()> {
    ensure_room(home, dir, source, &backup::free_space)
}

fn ensure_room(
    home: &AliceHome,
    dir: &Path,
    source: &Path,
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Result<()> {
    let db = home.db();
    let restored = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
    let kept = if db.exists() {
        backup::space_needed(&db)
    } else {
        0
    };
    let mut checks = vec![(home.root(), restored)];
    if same_volume(home.root(), dir) {
        checks[0].1 += kept;
    } else {
        checks.push((dir, kept));
    }
    for (path, need) in checks {
        if let Some(f) = free(path).filter(|f| *f < need) {
            return Err(AliceError::Other(format!(
                "{} {} MB free where {} is, a restore needs {} MB (a checked copy of the current database kept in backups, and the restored file); free some space or point backup.dir at another volume. Nothing was changed",
                backup::DISK_SPACE,
                f / 1_000_000,
                path.display(),
                need / 1_000_000
            )));
        }
    }
    Ok(())
}

fn nearest_existing(p: &Path) -> &Path {
    let mut p = p;
    while !p.exists() {
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    p
}

fn same_volume(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (
            std::fs::metadata(nearest_existing(a)),
            std::fs::metadata(nearest_existing(b)),
        ) {
            (Ok(x), Ok(y)) => x.dev() == y.dev(),
            _ => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (nearest_existing(a), nearest_existing(b));
        true
    }
}

/// The before-restore copy of the current database.
struct Kept {
    name: String,
    path: PathBuf,
    /// The current database could not be read, so it was linked (or copied) as it is.
    moved_aside: bool,
}

/// Whether `db` cannot be read as a database at all (open or `quick_check` fails).
fn unreadable(db: &Path) -> bool {
    match open_ro(db) {
        Err(_) => true,
        Ok(c) => alice_core::store::maintenance::quick_check(&c).map_or(true, |p| !p.is_empty()),
    }
}

/// Hard link, or copy where a link is not possible (another volume).
fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::hard_link(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map(|_| ())
}

/// Keep the database about to be replaced as a before-restore backup: a checked, private copy
/// through Task 14's `backup::create`, so it is listed and swept like every other backup, and
/// rotation keeps it for `KEEP_DAYS` (the newest one for ever). Only a database that cannot be read
/// at all (damaged, which is a common reason to restore) is kept as it is, as a link to the
/// file (`alice.db` itself stays in place until the swap). Any other failure (no space, an I/O
/// error) stops the restore with nothing changed.
fn save_current(
    db: &Path,
    dir: &Path,
    now: DateTime<Utc>,
    env: &Env,
    warnings: &mut Vec<String>,
) -> Result<Kept> {
    // Two restores in one second must not overwrite the first's saved copy.
    let mut stamp = now;
    for _ in 0..3600 {
        if !dir
            .join(backup::name_for(BackupKind::BeforeRestore, stamp, 0))
            .exists()
        {
            break;
        }
        stamp += chrono::Duration::seconds(1);
    }
    let attempt = (env.fault)(Step::Save)
        .map_err(AliceError::from)
        .and_then(|_| backup::create(db, dir, BackupKind::BeforeRestore, stamp));
    match attempt {
        Ok(info) => Ok(Kept {
            name: info.name,
            path: info.path,
            moved_aside: false,
        }),
        Err(copy_error) => {
            if !unreadable(db) {
                return Err(AliceError::Other(format!(
                    "cannot keep a copy of the current database first ({copy_error}); nothing was changed"
                )));
            }
            let name = backup::name_for(BackupKind::BeforeRestore, stamp, 0);
            let dest = dir.join(&name);
            link_or_copy(db, &dest).map_err(|e| {
                AliceError::Other(format!(
                    "cannot keep the unreadable current database ({e}); nothing was changed"
                ))
            })?;
            for suffix in ["-wal", "-shm"] {
                let side = PathBuf::from(format!("{}{suffix}", db.display()));
                if side.exists()
                    && let Err(e) =
                        link_or_copy(&side, &PathBuf::from(format!("{}{suffix}", dest.display())))
                {
                    warnings.push(format!("could not keep {}: {e}", side.display()));
                }
            }
            warnings.push(format!(
                "the current database could not be read ({copy_error}); it was kept as it was"
            ));
            Ok(Kept {
                name,
                path: dest,
                moved_aside: true,
            })
        }
    }
}

/// Copy `from` to `to`, created 0600 (it holds every memory) and synced. Refuses a destination
/// that is the source: the copy starts by removing an old `to`.
fn copy_private(from: &Path, to: &Path) -> Result<()> {
    use std::io::Write;
    if from == to || matches!((from.canonicalize(), to.canonicalize()), (Ok(a), Ok(b)) if a == b) {
        return Err(AliceError::invalid(format!(
            "{} cannot be copied onto itself",
            from.display()
        )));
    }
    let _ = std::fs::remove_file(to);
    let mut src = std::fs::File::open(from)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut dst = opts.open(to)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.flush()?;
    dst.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn home_with_db() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        h.ensure_dirs().unwrap();
        let c = alice_core::store::open_writer(&h.db()).unwrap();
        c.execute(
            "INSERT INTO meta (key, value) VALUES ('which', 'current')",
            [],
        )
        .unwrap();
        (d, h)
    }

    fn which(path: &Path) -> String {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key = 'which'", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn set_which(path: &Path, v: &str) {
        Connection::open(path)
            .unwrap()
            .execute("UPDATE meta SET value = ?1 WHERE key = 'which'", [v])
            .unwrap();
    }

    fn manual(h: &AliceHome, at: DateTime<Utc>) -> backup::BackupInfo {
        backup::create(&h.db(), &h.backups_dir(), BackupKind::Manual, at).unwrap()
    }

    #[test]
    fn verify_accepts_a_backup_and_refuses_damage_and_newer_schemas() {
        let (d, h) = home_with_db();
        let b = backup::create(&h.db(), &h.backups_dir(), BackupKind::Manual, Utc::now()).unwrap();
        let v = verify(&b.path).unwrap();
        assert_eq!(
            v.schema_version,
            alice_core::store::migrations::CURRENT_VERSION
        );
        let bytes = std::fs::read(&b.path).unwrap();
        let truncated = d.path().join("half.db");
        std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        assert!(verify(&truncated).is_err(), "truncated");
        let text = d.path().join("notes.db");
        std::fs::write(&text, b"just some text").unwrap();
        assert!(
            verify(&text)
                .unwrap_err()
                .to_string()
                .contains("not an Alice backup")
        );
        let newer = d.path().join("newer.db");
        std::fs::copy(&b.path, &newer).unwrap();
        Connection::open(&newer)
            .unwrap()
            .execute("INSERT INTO schema_migrations VALUES (99, 'x')", [])
            .unwrap();
        assert!(verify(&newer).unwrap_err().to_string().contains("newer"));
        // A SQLite file that is not Alice's.
        let other = d.path().join("other.db");
        Connection::open(&other)
            .unwrap()
            .execute("CREATE TABLE t (x)", [])
            .unwrap();
        assert!(
            verify(&other)
                .unwrap_err()
                .to_string()
                .contains("not an Alice backup")
        );
        assert!(verify(&d.path().join("missing.db")).is_err());
    }

    #[test]
    fn swap_needs_the_lock_saves_the_current_database_and_drops_the_index() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = backup::create(&h.db(), &dir, BackupKind::Manual, Utc::now()).unwrap();
        set_which(&h.db(), "later");
        std::fs::write(h.root().join("vectors/fake.memories.usearch"), b"x").unwrap();
        let held = crate::daemon::try_lock_home(&h).unwrap().expect("free");
        let err = swap(&h, &dir, &b.path, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("still running"), "{err}");
        assert_eq!(which(&h.db()), "later", "nothing changed while locked out");
        drop(held);
        let r = swap(&h, &dir, &b.path, Utc::now()).unwrap();
        assert_eq!(
            which(&h.db()),
            "current",
            "the backup's content is in place"
        );
        let saved = dir.join(r.saved_current_as.unwrap());
        assert_eq!(which(&saved), "later", "the replaced database was kept");
        assert!(
            !h.root().join("vectors/fake.memories.usearch").exists(),
            "the index is rebuilt on start"
        );
        assert!(
            crate::daemon::try_lock_home(&h).unwrap().is_some(),
            "the lock is released"
        );
    }

    #[test]
    fn find_backup_takes_a_name_a_path_or_the_newest() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        assert!(
            find_backup(&h, &dir, None)
                .unwrap_err()
                .to_string()
                .contains("alice backup now")
        );
        let a = backup::create(
            &h.db(),
            &dir,
            BackupKind::Manual,
            Utc::now() - chrono::Duration::days(1),
        )
        .unwrap();
        let b = backup::create(&h.db(), &dir, BackupKind::Manual, Utc::now()).unwrap();
        assert_eq!(find_backup(&h, &dir, None).unwrap(), b.path);
        assert_eq!(find_backup(&h, &dir, Some(&a.name)).unwrap(), a.path);
        assert_eq!(
            find_backup(&h, &dir, Some(a.path.to_str().unwrap())).unwrap(),
            a.path
        );
        assert!(find_backup(&h, &dir, Some("nope.db")).is_err());
    }

    #[test]
    fn a_before_restore_copy_is_a_listed_private_backup_and_never_the_default_pick() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now() - chrono::Duration::hours(2));
        set_which(&h.db(), "later");
        let r = swap(&h, &dir, &b.path, Utc::now()).unwrap();
        let name = r.saved_current_as.clone().unwrap();
        assert!(name.starts_with("alice-before-restore-"), "{name}");
        let listed = backup::list(&dir).unwrap();
        let saved = listed.iter().find(|i| i.name == name).expect("listed");
        assert_eq!(saved.kind, BackupKind::BeforeRestore);
        assert_eq!(
            verify(&saved.path).unwrap().schema_version,
            alice_core::store::migrations::CURRENT_VERSION,
            "a self-contained, checked file"
        );
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        // The default pick stays the newest scheduled or manual backup.
        assert_eq!(find_backup(&h, &dir, None).unwrap(), b.path);
        // A scheduled run's rotation does not take it away, however many scheduled ones follow.
        let now = Utc::now();
        for i in 0..40 {
            backup::create(
                &h.db(),
                &dir,
                BackupKind::Scheduled,
                now - chrono::Duration::days(i),
            )
            .unwrap();
        }
        backup::rotate(&dir, now, 7, 4).unwrap();
        assert!(
            dir.join(&name).exists(),
            "rotation keeps a before-restore copy"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&saved.path), 0o600);
            assert_eq!(mode(&h.db()), 0o600, "the restored file is private too");
        }
    }

    #[test]
    fn two_restores_in_one_second_keep_both_saved_copies() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now() - chrono::Duration::hours(1));
        let now = Utc::now();
        set_which(&h.db(), "first");
        let r1 = swap(&h, &dir, &b.path, now).unwrap();
        set_which(&h.db(), "second");
        let r2 = swap(&h, &dir, &b.path, now).unwrap();
        let (n1, n2) = (r1.saved_current_as.unwrap(), r2.saved_current_as.unwrap());
        assert_ne!(n1, n2);
        assert_eq!(which(&dir.join(n1)), "first");
        assert_eq!(which(&dir.join(n2)), "second");
    }

    #[test]
    fn a_restore_leaves_secrets_config_token_and_spool_alone() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        let files = [
            ("secrets.toml", "[keys]\njev = \"kept\"\n"),
            ("secrets.toml.lock", ""),
            ("token", "tok"),
            ("config.toml", "[server]\nport = 1\n"),
            ("spool/0001.json", "{\"content\":\"written meanwhile\"}"),
        ];
        for (name, body) in files {
            std::fs::write(h.root().join(name), body).unwrap();
        }
        let stamp = |name: &str| {
            let m = std::fs::metadata(h.root().join(name)).unwrap();
            (m.len(), m.modified().unwrap())
        };
        let before: Vec<_> = files.iter().map(|(n, _)| stamp(n)).collect();
        swap(&h, &dir, &b.path, Utc::now()).unwrap();
        for ((name, body), was) in files.iter().zip(before) {
            assert_eq!(
                std::fs::read_to_string(h.root().join(name)).unwrap(),
                *body,
                "{name}"
            );
            assert_eq!(stamp(name), was, "{name} was not even rewritten");
        }
        // No backup carries them either.
        for i in backup::list(&dir).unwrap() {
            let bytes = std::fs::read(&i.path).unwrap();
            assert!(!bytes.windows(4).any(|w| w == b"kept"), "{}", i.name);
        }
    }

    #[test]
    fn the_replaced_databases_wal_does_not_meet_the_restored_file() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        std::fs::write(format!("{}-wal", h.db().display()), b"stale wal").unwrap();
        std::fs::write(format!("{}-shm", h.db().display()), b"stale shm").unwrap();
        swap(&h, &dir, &b.path, Utc::now()).unwrap();
        assert!(!Path::new(&format!("{}-wal", h.db().display())).exists());
        assert!(!Path::new(&format!("{}-shm", h.db().display())).exists());
        assert_eq!(which(&h.db()), "current");
        assert!(!h.root().join("alice.db.restore.tmp").exists());
    }

    #[test]
    fn a_volume_without_room_refuses_before_anything_changes() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        set_which(&h.db(), "later");
        let v = verify(&b.path).unwrap();
        let err = swap_with(&h, &dir, &v, Utc::now(), &env_free(&|_| Some(1_000)))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with(backup::DISK_SPACE), "{err}");
        assert!(err.contains("Nothing was changed"), "{err}");
        assert_eq!(which(&h.db()), "later");
        assert_eq!(
            backup::list(&dir)
                .unwrap()
                .iter()
                .filter(|i| i.kind == BackupKind::BeforeRestore)
                .count(),
            0
        );
        // Room enough, or unknown: it goes ahead.
        swap_with(&h, &dir, &v, Utc::now(), &env_free(&|_| None)).unwrap();
        assert_eq!(which(&h.db()), "current");
    }

    #[test]
    fn a_damaged_current_database_is_moved_aside_and_the_restore_goes_ahead() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        std::fs::write(h.db(), b"X".repeat(8192)).unwrap();
        let r = swap(&h, &dir, &b.path, Utc::now()).unwrap();
        assert_eq!(which(&h.db()), "current");
        let saved = dir.join(r.saved_current_as.unwrap());
        assert_eq!(std::fs::read(&saved).unwrap(), b"X".repeat(8192));
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    }

    #[test]
    fn a_missing_current_database_restores_without_a_saved_copy() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        std::fs::remove_file(h.db()).unwrap();
        let _ = std::fs::remove_file(format!("{}-wal", h.db().display()));
        let r = swap(&h, &dir, &b.path, Utc::now()).unwrap();
        assert_eq!(r.saved_current_as, None);
        assert_eq!(which(&h.db()), "current");
    }

    #[test]
    fn the_live_database_is_not_a_backup() {
        let (_d, h) = home_with_db();
        let v = VerifiedBackup {
            path: h.db(),
            schema_version: 3,
            memories: 0,
            chunks: 0,
        };
        let err = swap_verified(&h, &h.backups_dir(), &v, Utc::now()).unwrap_err();
        assert!(err.to_string().contains("live database"), "{err}");
    }

    #[test]
    fn a_newer_backup_is_refused_before_the_lock_is_even_tried() {
        let (d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        let newer = d.path().join("newer.db");
        std::fs::copy(&b.path, &newer).unwrap();
        Connection::open(&newer)
            .unwrap()
            .execute("INSERT INTO schema_migrations VALUES (99, 'x')", [])
            .unwrap();
        set_which(&h.db(), "later");
        let _held = crate::daemon::try_lock_home(&h).unwrap().unwrap();
        let err = swap(&h, &dir, &newer, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("newer"), "{err}");
        assert!(!err.contains("still running"), "{err}");
    }

    #[test]
    fn an_older_backup_is_migrated_when_the_daemon_opens_it() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        // Make the backup what a v2 build wrote: migration 3 undone.
        {
            let c = Connection::open(&b.path).unwrap();
            c.execute_batch(
                "DROP INDEX memories_judge; DROP INDEX chunks_created; DROP INDEX judge_cache_created;
                 ALTER TABLE memories DROP COLUMN kind_explicit;
                 ALTER TABLE memories DROP COLUMN project_defaulted;
                 ALTER TABLE memories DROP COLUMN judge_attempts;
                 ALTER TABLE memories DROP COLUMN judge_unavailable;
                 ALTER TABLE memories DROP COLUMN judge_next_try_at;
                 DELETE FROM schema_migrations WHERE version = 3;",
            )
            .unwrap();
        }
        let v = verify(&b.path).unwrap();
        assert_eq!(v.schema_version, 2);
        swap_verified(&h, &dir, &v, Utc::now()).unwrap();
        let c = alice_core::store::open_writer(&h.db()).unwrap();
        let newest: i64 = c
            .query_row("SELECT max(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(newest, alice_core::store::migrations::CURRENT_VERSION);
        let pre = backup::list(&dir)
            .unwrap()
            .into_iter()
            .filter(|i| i.kind == BackupKind::PreMigration)
            .count();
        assert_eq!(
            pre, 1,
            "the migration took its own backup of the restored file"
        );
    }

    fn env_free<'a>(free: &'a dyn Fn(&Path) -> Option<u64>) -> Env<'a> {
        Env {
            free,
            fault: &|_| Ok(()),
        }
    }

    /// A current database with committed rows that live only in its WAL (the connection is
    /// leaked, so nothing checkpoints on close), as after a daemon that was killed.
    fn live_wal(db: &Path) {
        let c = Connection::open(db).unwrap();
        c.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .unwrap();
        c.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
        c.execute(
            "INSERT INTO meta (key, value) VALUES ('walrow', 'in the wal')",
            [],
        )
        .unwrap();
        std::mem::forget(c);
    }

    fn walrow(db: &Path) -> Option<String> {
        Connection::open(db)
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key = 'walrow'", [], |r| {
                r.get(0)
            })
            .ok()
    }

    fn leftovers(h: &AliceHome) -> Vec<String> {
        std::fs::read_dir(h.root())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".alice-restore"))
            .collect()
    }

    #[test]
    fn a_failure_at_any_step_leaves_alice_db_the_complete_old_database() {
        for step in [
            Step::Stage,
            Step::Save,
            Step::Index,
            Step::Sidecars,
            Step::Rename,
        ] {
            let (_d, h) = home_with_db();
            let dir = h.backups_dir();
            let b = manual(&h, Utc::now());
            set_which(&h.db(), "later");
            live_wal(&h.db());
            std::fs::write(h.root().join("vectors/fake.memories.usearch"), b"x").unwrap();
            let v = verify(&b.path).unwrap();
            let fail = |s: Step| {
                if s == step {
                    Err(std::io::Error::other("injected"))
                } else {
                    Ok(())
                }
            };
            let err = swap_with(
                &h,
                &dir,
                &v,
                Utc::now(),
                &Env {
                    free: &|_| None,
                    fault: &fail,
                },
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("injected"), "{step:?}: {err}");
            assert!(
                err.contains("unchanged") || err.contains("nothing was changed"),
                "{step:?}: {err}"
            );
            assert!(h.db().exists(), "{step:?}: alice.db is there");
            assert_eq!(which(&h.db()), "later", "{step:?}: the old content");
            assert_eq!(
                walrow(&h.db()).as_deref(),
                Some("in the wal"),
                "{step:?}: its WAL too"
            );
            assert_eq!(leftovers(&h), Vec::<String>::new(), "{step:?}");
            assert!(
                backup::list(&dir)
                    .unwrap()
                    .iter()
                    .all(|i| i.kind != BackupKind::BeforeRestore),
                "{step:?}: no half-finished kept copy"
            );
            assert!(
                crate::daemon::try_lock_home(&h).unwrap().is_some(),
                "{step:?}: lock released"
            );
            // The same restore then goes through.
            swap_verified(&h, &dir, &v, Utc::now()).unwrap();
            assert_eq!(which(&h.db()), "current");
        }
    }

    #[test]
    fn a_failing_copy_of_a_healthy_database_aborts_instead_of_setting_it_aside() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        set_which(&h.db(), "later");
        let v = verify(&b.path).unwrap();
        let fail = |s: Step| {
            if s == Step::Save {
                Err(std::io::Error::other("no space left on device"))
            } else {
                Ok(())
            }
        };
        let err = swap_with(
            &h,
            &dir,
            &v,
            Utc::now(),
            &Env {
                free: &|_| None,
                fault: &fail,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing was changed"), "{err}");
        assert_eq!(which(&h.db()), "later");
    }

    #[test]
    fn a_damaged_database_stays_in_place_when_the_swap_fails_after_it_was_kept() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        std::fs::write(h.db(), b"X".repeat(8192)).unwrap();
        let v = verify(&b.path).unwrap();
        let fail = |s: Step| {
            if s == Step::Rename {
                Err(std::io::Error::other("injected"))
            } else {
                Ok(())
            }
        };
        let err = swap_with(
            &h,
            &dir,
            &v,
            Utc::now(),
            &Env {
                free: &|_| None,
                fault: &fail,
            },
        );
        assert!(err.is_err());
        assert_eq!(
            std::fs::read(h.db()).unwrap(),
            b"X".repeat(8192),
            "alice.db is not absent"
        );
        assert_eq!(leftovers(&h), Vec::<String>::new());
        assert!(
            backup::list(&dir)
                .unwrap()
                .iter()
                .all(|i| i.kind != BackupKind::BeforeRestore)
        );
    }

    #[test]
    fn rows_that_live_only_in_the_current_wal_are_in_the_kept_copy() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        live_wal(&h.db());
        let r = swap(&h, &dir, &b.path, Utc::now()).unwrap();
        let kept = dir.join(r.saved_current_as.unwrap());
        assert_eq!(walrow(&kept).as_deref(), Some("in the wal"));
        assert_eq!(
            walrow(&h.db()),
            None,
            "the restored database has none of it"
        );
        assert!(!Path::new(&format!("{}-wal", h.db().display())).exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_index_file_that_cannot_be_removed_stops_the_restore() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        set_which(&h.db(), "later");
        let vectors = h.root().join("vectors");
        std::fs::write(vectors.join("fake.chunks.bq"), b"x").unwrap();
        std::fs::set_permissions(&vectors, std::fs::Permissions::from_mode(0o500)).unwrap();
        let err = swap(&h, &dir, &b.path, Utc::now());
        std::fs::set_permissions(&vectors, std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = err.unwrap_err().to_string();
        assert!(err.contains("cannot remove"), "{err}");
        assert_eq!(which(&h.db()), "later", "the old database is untouched");
        // Index files are removed before the rename, never after it.
        swap(&h, &dir, &b.path, Utc::now()).unwrap();
        assert!(!vectors.join("fake.chunks.bq").exists());
    }

    #[test]
    fn copy_private_never_deletes_its_own_source() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.db");
        std::fs::write(&p, b"precious").unwrap();
        assert!(copy_private(&p, &p).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"precious");
    }

    #[test]
    fn a_leftover_staged_file_is_not_a_backup_and_is_swept() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = manual(&h, Utc::now());
        let staged = h.root().join(".alice-restore.tmp");
        std::fs::copy(&b.path, &staged).unwrap();
        let err = swap(&h, &dir, &staged, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("interrupted restore"), "{err}");
        assert!(
            staged.exists(),
            "the file named as the source was not deleted"
        );
        std::fs::write(h.root().join(".alice-restore-wal.bak"), b"w").unwrap();
        let held = crate::daemon::try_lock_home(&h).unwrap().unwrap();
        sweep_staged(&h);
        drop(held);
        assert!(leftovers(&h).is_empty());
        assert!(h.db().exists());
    }

    #[test]
    fn only_one_restore_runs_and_clients_can_see_it() {
        let (_d, h) = home_with_db();
        assert!(!in_progress(&h), "no marker yet");
        let g = RestoreGuard::acquire(&h).unwrap();
        assert!(in_progress(&h));
        let err = RestoreGuard::acquire(&h).err().unwrap().to_string();
        assert!(err.contains("already running"), "{err}");
        drop(g);
        assert!(!in_progress(&h));
        drop(RestoreGuard::acquire(&h).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_backup_in_a_directory_that_cannot_be_written_still_verifies() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (d, h) = home_with_db();
        let ro = d.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        let b = manual(&h, Utc::now());
        let copy = ro.join("alice-manual-20200101T000000Z.db");
        std::fs::copy(&b.path, &copy).unwrap();
        // Make the copy a WAL-mode database, as a backup made by hand might be.
        Connection::open(&copy)
            .unwrap()
            .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        let v = verify(&copy);
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
        v.unwrap();
    }
}
