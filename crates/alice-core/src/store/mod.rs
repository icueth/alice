//! SQLite storage. Only the daemon opens the database.

pub mod migrations;

use crate::error::Result;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn configure_common(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 268435456;
         PRAGMA cache_size = -65536;",
    )?;
    Ok(())
}

/// Open the single writer connection: creates the file, enables WAL and applies migrations.
pub fn open_writer(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
    configure_common(&conn)?;
    migrations::migrate(&mut conn, path)?;
    Ok(conn)
}

/// Open a read-only connection for queries.
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    configure_common(&conn)?;
    Ok(conn)
}

/// Consistent snapshot of a live database.
pub fn backup_into(conn: &Connection, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
    Ok(())
}

/// Small pool of read-only connections. Connections are opened lazily and reused.
pub struct ReaderPool {
    path: PathBuf,
    max_idle: usize,
    idle: Mutex<Vec<Connection>>,
}

impl ReaderPool {
    pub fn new(path: PathBuf, max_idle: usize) -> Self {
        Self {
            path,
            max_idle,
            idle: Mutex::new(Vec::new()),
        }
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let pooled = self.idle.lock().pop();
        let conn = match pooled {
            Some(c) => c,
            None => open_reader(&self.path)?,
        };
        let out = f(&conn);
        let mut idle = self.idle.lock();
        if idle.len() < self.max_idle {
            idle.push(conn);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AliceError;

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alice.db");
        (dir, path)
    }

    #[test]
    fn writer_creates_schema_in_wal_mode() {
        let (_d, path) = temp_db();
        let conn = open_writer(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM projects WHERE slug = 'global'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn migrations_are_idempotent() {
        let (_d, path) = temp_db();
        drop(open_writer(&path).unwrap());
        let mut conn = open_writer(&path).unwrap();
        assert_eq!(migrations::migrate(&mut conn, &path).unwrap(), 0);
    }

    #[test]
    fn trigram_fts_matches_thai_substring() {
        let (_d, path) = temp_db();
        let conn = open_writer(&path).unwrap();
        conn.execute(
            "INSERT INTO memories_tri(rowid, title, body) VALUES (1, 'ระบบ', 'ระบบความจำสำหรับเอเจนต์เขียนโค้ด')",
            [],
        )
        .unwrap();
        let hit: i64 = conn
            .query_row(
                "SELECT rowid FROM memories_tri WHERE memories_tri MATCH '\"ความจำ\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1);
    }

    #[test]
    fn readers_see_committed_writes_and_cannot_write() {
        let (_d, path) = temp_db();
        let writer = open_writer(&path).unwrap();
        writer
            .execute("INSERT INTO meta(key, value) VALUES ('k', 'v')", [])
            .unwrap();
        let pool = ReaderPool::new(path.clone(), 2);
        let v: String = pool
            .with(|c| Ok(c.query_row("SELECT value FROM meta WHERE key='k'", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(v, "v");
        let err =
            pool.with(|c| Ok(c.execute("INSERT INTO meta(key, value) VALUES ('x','y')", [])?));
        assert!(
            matches!(&err, Err(AliceError::Db(rusqlite::Error::SqliteFailure(e, _))) if e.code == rusqlite::ErrorCode::ReadOnly),
            "expected a read-only error, got {err:?}"
        );
    }

    fn pre_migration_backups(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("alice.pre-migration-")
            })
            .collect()
    }

    #[test]
    fn migrating_an_existing_database_backs_it_up_first() {
        let (fresh_dir, fresh_path) = temp_db();
        drop(open_writer(&fresh_path).unwrap());
        assert!(
            pre_migration_backups(fresh_dir.path()).is_empty(),
            "a new database needs no backup"
        );

        // A database from an older build: it recorded an earlier migration and has data,
        // but migration 1 is still pending.
        let (old_dir, old_path) = temp_db();
        Connection::open(&old_path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
                 INSERT INTO schema_migrations VALUES (0, '2026-01-01T00:00:00.000Z');
                 CREATE TABLE legacy (x INTEGER);
                 INSERT INTO legacy VALUES (42);",
            )
            .unwrap();
        let conn = open_writer(&old_path).unwrap();
        let version: i64 = conn
            .query_row("SELECT max(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, migrations::CURRENT_VERSION);

        let backups = pre_migration_backups(old_dir.path());
        assert_eq!(backups.len(), 1, "{backups:?}");
        let backup = Connection::open(&backups[0]).unwrap();
        let x: i64 = backup
            .query_row("SELECT x FROM legacy", [], |r| r.get(0))
            .unwrap();
        assert_eq!(x, 42);
        let migrated: i64 = backup
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'projects'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            migrated, 0,
            "the backup must hold the state before migrating"
        );
    }

    #[test]
    fn backup_into_produces_a_readable_copy() {
        let (d, path) = temp_db();
        let conn = open_writer(&path).unwrap();
        let dest = d.path().join("copy.db");
        backup_into(&conn, &dest).unwrap();
        let copy = rusqlite::Connection::open(&dest).unwrap();
        let n: i64 = copy
            .query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
