//! SQLite storage. Only the daemon opens the database.

pub mod compat;
pub mod embeddings;
pub mod events;
pub mod fts;
pub mod history;
pub mod import;
pub mod judge;
pub mod kg;
pub mod maintenance;
pub mod memories;
pub mod migrations;
pub mod projects;
pub mod reviews;
pub mod sources;

use crate::error::Result;
use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;

/// Process-wide SQLite settings, applied once before the first connection opens.
///
/// Allocation statistics (`SQLITE_CONFIG_MEMSTATUS`, on by default) put every SQLite allocation
/// behind one process-wide mutex. With the writer tokenizing FTS text while many readers search,
/// that mutex serialised them: in the daemon's concurrency test (20 clients during a bulk ingest)
/// turning it off cut recall and remember p95 by about 40%. Nothing in Alice reads the statistics
/// or sets a soft heap limit.
///
/// `sqlite3_config` only works before SQLite initializes, so this runs ahead of every open in
/// this module; if something else in the process opened SQLite first it is a harmless no-op.
fn configure_sqlite_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: called before this module opens any connection; SQLite rejects the call
        // (SQLITE_MISUSE) without side effects if it is already initialized.
        let rc = unsafe {
            rusqlite::ffi::sqlite3_config(
                rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS,
                0 as std::ffi::c_int,
            )
        };
        if rc != rusqlite::ffi::SQLITE_OK {
            tracing::debug!(
                rc,
                "SQLite was initialized elsewhere; memory statistics stay on"
            );
        }
    });
}

/// How long a connection waits for a lock before it reports `SQLITE_BUSY`.
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Page cache of the writer connection, in KiB.
const WRITER_CACHE_KIB: i64 = 64 * 1024;
/// Page cache of each reader connection, in KiB. Readers are pooled (8 in the daemon), so
/// this times the pool size bounds their memory; the OS file cache serves the rest.
const READER_CACHE_KIB: i64 = 16 * 1024;
/// Page cache of each connection in the small pool that reads chunk vectors for rescoring
/// (a few hundred primary-key reads per raw-tier recall), in KiB.
pub const VECTOR_READER_CACHE_KIB: i64 = 4 * 1024;

/// No memory map: every connection would map the database separately and each map's touched
/// pages count towards the daemon's RSS (with a 2.7 GB database at 1M chunks, eight readers
/// mapping 256 MB each reached gigabytes). Reads through the OS file cache measured the same
/// speed for Alice's queries.
fn configure_common(conn: &Connection, cache_kib: i64) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 0;
         PRAGMA cache_size = -{cache_kib};"
    ))?;
    Ok(())
}

/// Open the single writer connection: creates the file, enables WAL and applies migrations.
pub fn open_writer(path: &Path) -> Result<Connection> {
    configure_sqlite_once();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
    configure_common(&conn, WRITER_CACHE_KIB)?;
    migrations::migrate(&mut conn, path)?;
    // Migration 3 creates these; a database that migrated before they were added gets them once.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS chunks_created ON chunks(created_at);
         CREATE INDEX IF NOT EXISTS judge_cache_created ON judge_cache(created_at);",
    )?;
    Ok(conn)
}

/// Open a read-only connection for queries.
pub fn open_reader(path: &Path) -> Result<Connection> {
    open_reader_with_cache(path, READER_CACHE_KIB)
}

fn open_reader_with_cache(path: &Path, cache_kib: i64) -> Result<Connection> {
    configure_sqlite_once();
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    configure_common(&conn, cache_kib)?;
    Ok(conn)
}

/// Consistent snapshot of a live database. The copy is private to its owner (0600 on Unix): it
/// holds every memory.
pub fn backup_into(conn: &Connection, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
    make_private(dest)?;
    Ok(())
}

/// Create `dir` and any missing parents. What is created here is private to its owner (0700 on
/// Unix); a directory that exists already keeps its mode.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)?;
    Ok(())
}

/// Make `path` readable and writable by its owner only (0600 on Unix; Windows has no mode bits).
pub fn make_private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Pool of read-only connections, opened lazily and reused. At most `max` exist at once; a
/// caller beyond that waits until one is handed back. Every connection has its own page cache,
/// so the cap bounds the daemon's memory however many requests arrive together (64 concurrent
/// recalls used to open 64 connections). `f` must not take a second connection from the same
/// pool: with every connection lent out it would wait for itself.
pub struct ReaderPool {
    path: PathBuf,
    max: usize,
    /// Page cache of each connection, in KiB.
    cache_kib: i64,
    state: Mutex<PoolState>,
    returned: Condvar,
}

struct PoolState {
    idle: Vec<Connection>,
    /// Connections that exist: idle ones plus those lent out.
    open: usize,
}

/// A lent connection; dropping it hands the connection back (or, after a panic, closes it).
struct Lease<'a> {
    pool: &'a ReaderPool,
    conn: Option<Connection>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let conn = self.conn.take();
        let mut st = self.pool.state.lock();
        match conn {
            // A panic may have left a statement or transaction open: do not reuse that one.
            // Neither may a connection that comes back inside a transaction or with a statement
            // half-read: its read snapshot would pin the write-ahead log, and with it every
            // restart of the log, for as long as the connection idles in the pool.
            Some(c) if !std::thread::panicking() && c.is_autocommit() && !c.is_busy() => {
                st.idle.push(c)
            }
            Some(_) if !std::thread::panicking() => {
                tracing::warn!(
                    "a pooled reader came back inside a transaction or mid-statement; closed"
                );
                st.open -= 1;
            }
            _ => st.open -= 1,
        }
        drop(st);
        self.pool.returned.notify_one();
    }
}

impl ReaderPool {
    pub fn new(path: PathBuf, max: usize) -> Self {
        Self::with_cache(path, max, READER_CACHE_KIB)
    }

    /// A pool whose connections have a page cache of `cache_kib` KiB each.
    pub fn with_cache(path: PathBuf, max: usize, cache_kib: i64) -> Self {
        Self {
            path,
            max: max.max(1),
            cache_kib,
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                open: 0,
            }),
            returned: Condvar::new(),
        }
    }

    /// Run `f` on a pooled connection, waiting for one if all `max` are in use.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let lease = self.lease()?;
        f(lease.conn.as_ref().expect("a lease holds its connection"))
    }

    fn lease(&self) -> Result<Lease<'_>> {
        let mut st = self.state.lock();
        loop {
            if let Some(conn) = st.idle.pop() {
                return Ok(Lease {
                    pool: self,
                    conn: Some(conn),
                });
            }
            if st.open < self.max {
                st.open += 1;
                break;
            }
            self.returned.wait(&mut st);
        }
        drop(st);
        match open_reader_with_cache(&self.path, self.cache_kib) {
            Ok(conn) => Ok(Lease {
                pool: self,
                conn: Some(conn),
            }),
            Err(e) => {
                self.state.lock().open -= 1;
                self.returned.notify_one();
                Err(e)
            }
        }
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

    #[test]
    fn connections_do_not_map_the_file_and_readers_have_a_small_cache() {
        let (_d, path) = temp_db();
        let w = open_writer(&path).unwrap();
        let r = open_reader(&path).unwrap();
        let pragma = |c: &Connection, name: &str| -> i64 {
            c.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(pragma(&w, "mmap_size"), 0);
        assert_eq!(pragma(&r, "mmap_size"), 0);
        assert_eq!(pragma(&w, "cache_size"), -WRITER_CACHE_KIB);
        assert_eq!(pragma(&r, "cache_size"), -READER_CACHE_KIB);
    }

    /// The chunk vector list reads on its own small pool: same read-only connections, a 4 MiB
    /// page cache each.
    #[test]
    fn a_reader_pool_can_give_its_connections_a_smaller_cache() {
        let (_d, path) = temp_db();
        drop(open_writer(&path).unwrap());
        let pool = ReaderPool::with_cache(path, 2, VECTOR_READER_CACHE_KIB);
        let cache: i64 = pool
            .with(|c| Ok(c.query_row("PRAGMA cache_size", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(cache, -VECTOR_READER_CACHE_KIB);
        assert_eq!(VECTOR_READER_CACHE_KIB, 4 * 1024);
        assert!(
            pool.with(|c| Ok(c.execute("INSERT INTO meta(key, value) VALUES ('x','y')", [])?))
                .is_err(),
            "read-only like every reader"
        );
    }

    #[test]
    fn reader_pool_never_opens_more_than_max_and_waiters_are_served() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_d, path) = temp_db();
        drop(open_writer(&path).unwrap());
        let pool = Arc::new(ReaderPool::new(path, 3));
        let (busy, peak, served) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (pool, busy, peak, served) =
                    (pool.clone(), busy.clone(), peak.clone(), served.clone());
                std::thread::spawn(move || {
                    pool.with(|c| {
                        let now = busy.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        let n: i64 =
                            c.query_row("SELECT count(*) FROM projects", [], |r| r.get(0))?;
                        std::thread::sleep(Duration::from_millis(20));
                        busy.fetch_sub(1, Ordering::SeqCst);
                        served.fetch_add(n as usize, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(served.load(Ordering::SeqCst), 16, "every caller was served");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            3,
            "at most max connections at once"
        );
        assert_eq!(pool.state.lock().open, 3);
        assert_eq!(pool.state.lock().idle.len(), 3);
    }

    /// A reader that keeps a read snapshot while it idles would stop the writer from ever
    /// restarting the log (Task 20), so such a connection is closed, not pooled.
    #[test]
    fn a_connection_returned_inside_a_transaction_or_mid_statement_is_not_reused() {
        let (_d, path) = temp_db();
        let w = open_writer(&path).unwrap();
        w.execute_batch("INSERT INTO meta (key, value) VALUES ('a', '1'), ('b', '2')")
            .unwrap();
        let pool = ReaderPool::new(path, 1);
        // Back to back, as clients do: each reader sees an idle connection, no snapshot held.
        pool.with(|c| {
            assert!(c.is_autocommit() && !c.is_busy());
            Ok(())
        })
        .unwrap();
        pool.with(|c| {
            c.execute_batch("BEGIN")?;
            c.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))?;
            Ok(())
        })
        .unwrap();
        pool.with(|c| {
            assert!(c.is_autocommit(), "the open transaction was not handed on");
            Ok(())
        })
        .unwrap();
        // A statement stepped once and then left: its snapshot is open too. The statement is
        // leaked on purpose (a dropped one would be reset), so SQLite cannot close this
        // connection: the pool discards it and it stays allocated for the life of the test.
        // Only the transaction case above is actually closed.
        let half = pool.lease().unwrap();
        {
            let c = half.conn.as_ref().unwrap();
            let mut stmt = c.prepare("SELECT key FROM meta").unwrap();
            let mut rows = stmt.query([]).unwrap();
            assert!(rows.next().unwrap().is_some());
            assert!(c.is_busy());
            std::mem::forget(rows);
            std::mem::forget(stmt);
        }
        // Hand the busy connection back through the pool's own path.
        drop(half);
        pool.with(|c| {
            assert!(!c.is_busy(), "a half-read statement was not handed on");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn reader_pool_recovers_the_slot_of_a_panicking_caller_and_a_failed_open() {
        use std::sync::Arc;
        let (d, path) = temp_db();
        drop(open_writer(&path).unwrap());
        let pool = Arc::new(ReaderPool::new(path, 1));
        let p = pool.clone();
        let panicked = std::thread::spawn(move || {
            p.with(|_| -> Result<()> { panic!("boom") }).unwrap();
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(
            pool.state.lock().open,
            0,
            "the panicking caller's connection was closed"
        );
        let n: i64 = pool
            .with(|c| Ok(c.query_row("SELECT count(*) FROM projects", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1, "the only slot is usable again");

        let missing = ReaderPool::new(d.path().join("nope/missing.db"), 1);
        assert!(missing.with(|_| Ok(())).is_err());
        assert!(
            missing.with(|_| Ok(())).is_err(),
            "a failed open gives its slot back"
        );
        assert_eq!(missing.state.lock().open, 0);
    }

    /// Pre-migration backups live in `<dir>/backups/`, named `alice-pre-migration-v<from>-<stamp>.db`.
    fn pre_migration_backups(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir.join("backups")) else {
            return Vec::new();
        };
        rd.map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("alice-pre-migration-")
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
    fn a_v1_database_migrates_to_v2_after_a_backup() {
        let (d, path) = temp_db();
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)").unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(include_str!("sql/0001_init.sql")).unwrap();
            tx.execute("INSERT INTO schema_migrations VALUES (1, 'x')", [])
                .unwrap();
            tx.commit().unwrap();
        }
        let conn = open_writer(&path).unwrap();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('events')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(cols.contains(&"group_id".to_string()));
        let backups = pre_migration_backups(d.path());
        assert_eq!(backups.len(), 1, "a backup was taken before migrating");
        let name = backups[0].file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("alice-pre-migration-v1-"), "{name}");
        assert!(name.ends_with(".db"), "{name}");
        // It holds every memory: private to its owner, in a private directory.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&backups[0]), 0o600);
            assert_eq!(mode(&d.path().join("backups")), 0o700);
        }
    }

    /// An M1 database with data keeps every row through migration 2, gains the new columns
    /// and indexes, and the dedupe index allows one open review item per key.
    #[test]
    fn migration_2_keeps_m1_rows_and_adds_the_indexes() {
        let (_d, path) = temp_db();
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)").unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(include_str!("sql/0001_init.sql")).unwrap();
            tx.execute("INSERT INTO schema_migrations VALUES (1, 'x')", [])
                .unwrap();
            tx.execute(
                "INSERT INTO events (id, ts, hlc, actor, action, target_type, target_id)
                 VALUES ('E1', '2026-10-13T00:00:00.000Z', 'h', 'user', 'memory.edit', 'memory', 'M1')",
                [],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO review_items (id, type, payload_json, status, created_at)
                 VALUES ('R1', 'merge', '{}', 'open', '2026-10-13T00:00:00.000Z')",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let conn = open_writer(&path).unwrap();
        let (events, reviews): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM events WHERE group_id IS NULL),
                        (SELECT count(*) FROM review_items WHERE dedupe_key IS NULL)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (events, reviews),
            (1, 1),
            "M1 rows survive with NULL in the new columns"
        );
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name IN ('events_actor', 'events_target', 'events_ts', 'events_group', 'review_open_dedupe', 'review_status') ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            indexes,
            [
                "events_actor",
                "events_group",
                "events_target",
                "events_ts",
                "review_open_dedupe",
                "review_status"
            ]
        );
        let add = |id: &str, status: &str| {
            conn.execute(
                "INSERT INTO review_items (id, type, payload_json, status, created_at, dedupe_key)
                 VALUES (?1, 'undo', '{}', ?2, '2026-10-13T00:00:00.000Z', 'undo:E1')",
                [id, status],
            )
        };
        add("R2", "open").unwrap();
        assert!(add("R3", "open").is_err(), "one open item per dedupe key");
        add("R4", "resolved").unwrap();
    }

    /// A v2 database migrates to v3: live memories that were never judged are marked done
    /// (they are not judged after the fact), a memory still waiting for its embedding stays
    /// pending, and imported (`skipped`) and `retry` rows keep their state.
    #[test]
    fn migration_3_marks_live_unjudged_memories_done_and_leaves_the_rest() {
        let (_d, path) = temp_db();
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)").unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(include_str!("sql/0001_init.sql")).unwrap();
            tx.execute_batch(include_str!("sql/0002_events_reviews.sql"))
                .unwrap();
            tx.execute("INSERT INTO schema_migrations VALUES (1, 'x')", [])
                .unwrap();
            tx.execute("INSERT INTO schema_migrations VALUES (2, 'x')", [])
                .unwrap();
            for (id, status, judge) in [
                ("M-ACTIVE", "active", "pending"),
                ("M-PENDING", "pending", "pending"),
                ("M-SKIPPED", "active", "skipped"),
                ("M-RETRY", "active", "retry"),
            ] {
                tx.execute(
                    "INSERT INTO memories (id, project_id, kind, title, body, status, content_hash, created_at, updated_at, hlc, judge_state)
                     VALUES (?1, '00000000000000000000GLOBAL', 'note', 't', 'b', ?2, ?1,
                             '2026-10-13T00:00:00.000Z', '2026-10-13T00:00:00.000Z', 'h', ?3)",
                    [id, status, judge],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let conn = open_writer(&path).unwrap();
        let state = |id: &str| -> String {
            conn.query_row(
                "SELECT judge_state FROM memories WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            state("M-ACTIVE"),
            "done",
            "an M1-era live memory is not judged after the fact"
        );
        assert_eq!(
            state("M-PENDING"),
            "pending",
            "a memory still waiting is judged"
        );
        assert_eq!(state("M-SKIPPED"), "skipped", "imports are never judged");
        assert_eq!(state("M-RETRY"), "retry");
        let (explicit, defaulted, attempts): (i64, i64, i64) = conn
            .query_row(
                "SELECT kind_explicit, project_defaulted, judge_attempts FROM memories WHERE id = 'M-ACTIVE'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (explicit, defaulted, attempts),
            (1, 0, 0),
            "old rows count as explicit"
        );
        let index: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'memories_judge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
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

    #[test]
    fn a_database_from_a_newer_build_is_refused_untouched() {
        let (d, path) = temp_db();
        drop(open_writer(&path).unwrap());
        let newer = migrations::CURRENT_VERSION + 1;
        Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, '2027-01-01T00:00:00.000Z')",
                [newer],
            )
            .unwrap();
        let expected = format!(
            "database schema v{newer} is newer than this build (v{}); upgrade Alice",
            migrations::CURRENT_VERSION
        );

        match open_writer(&path) {
            Err(AliceError::ReadOnly(msg)) => assert_eq!(msg, expected),
            other => panic!("open_writer: expected ReadOnly, got {other:?}"),
        }
        let mut conn = Connection::open(&path).unwrap();
        match migrations::migrate(&mut conn, &path) {
            Err(AliceError::ReadOnly(msg)) => assert_eq!(msg, expected),
            other => panic!("migrate: expected ReadOnly, got {other:?}"),
        }

        assert!(pre_migration_backups(d.path()).is_empty());
        let versions: Vec<i64> = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let mut expected: Vec<i64> = (1..=migrations::CURRENT_VERSION).collect();
        expected.push(newer);
        assert_eq!(versions, expected);
    }

    #[test]
    fn purged_seqs_are_never_reused() {
        let (_d, path) = temp_db();
        let conn = open_writer(&path).unwrap();
        let add_memory = |id: &str| -> i64 {
            conn.execute(
                "INSERT INTO memories (id, project_id, kind, title, body, status, content_hash, created_at, updated_at, hlc)
                 VALUES (?1, '00000000000000000000GLOBAL', 'note', 't', 'b', 'active', ?1,
                         '2026-10-13T00:00:00.000Z', '2026-10-13T00:00:00.000Z', 'h')",
                [id],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        add_memory("m1");
        let purged = add_memory("m2");
        conn.execute("DELETE FROM memories WHERE seq = ?1", [purged])
            .unwrap();
        let next = add_memory("m3");
        assert!(next > purged, "memories.seq {purged} was reused");

        conn.execute(
            "INSERT INTO sources (id, project_id, type, uri, created_at)
             VALUES ('s1', '00000000000000000000GLOBAL', 'file', 'f', '2026-10-13T00:00:00.000Z')",
            [],
        )
        .unwrap();
        let add_chunk = |ord: i64| -> i64 {
            conn.execute(
                "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at)
                 VALUES ('c' || ?1, 's1', '00000000000000000000GLOBAL', ?1, 't', 'h', '2026-10-13T00:00:00.000Z')",
                [ord],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        add_chunk(1);
        let purged = add_chunk(2);
        conn.execute("DELETE FROM chunks WHERE seq = ?1", [purged])
            .unwrap();
        let next = add_chunk(3);
        assert!(next > purged, "chunks.seq {purged} was reused");
    }
}
