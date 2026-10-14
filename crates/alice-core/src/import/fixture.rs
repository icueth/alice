//! Synthetic MemPalace `sqlite_exact` palaces for tests. Built in code; never a copy of real data.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use super::palace::DrawerMeta;

/// One drawer to write into a fixture palace.
#[derive(Debug, Clone)]
pub struct DrawerSpec {
    pub id: String,
    pub text: String,
    pub meta: DrawerMeta,
}

/// File name MemPalace uses for its `sqlite_exact` backend.
pub const MAIN_FILE: &str = "sqlite_exact.sqlite3";

/// A WAL-mode palace with a second connection holding an open write transaction.
pub struct WalPalace {
    pub path: PathBuf,
    /// The connection that built the palace; keeps `-wal` and `-shm` alive.
    _main: Connection,
    /// The "other process": an open write transaction until released or dropped.
    writer: Option<Connection>,
}

impl WalPalace {
    /// Ends the open write transaction (rolling it back) so that a test can write from its own connection.
    pub fn release_writer(&mut self) {
        if let Some(w) = self.writer.take() {
            let _ = w.execute_batch("ROLLBACK");
        }
    }
}

fn metadata_json(m: &DrawerMeta) -> String {
    let mut o = serde_json::Map::new();
    o.insert("wing".into(), m.wing.clone().into());
    o.insert("room".into(), m.room.clone().into());
    let opt = |o: &mut serde_json::Map<String, serde_json::Value>, k: &str, v: &Option<String>| {
        if let Some(v) = v {
            o.insert(k.into(), v.clone().into());
        }
    };
    opt(&mut o, "hall", &m.hall);
    opt(&mut o, "source_file", &m.source_file);
    opt(&mut o, "added_by", &m.added_by);
    opt(&mut o, "ingest_mode", &m.ingest_mode);
    opt(&mut o, "filed_at", &m.filed_at);
    opt(&mut o, "authored_at", &m.authored_at);
    if let Some(i) = m.chunk_index {
        o.insert("chunk_index".into(), i.into());
    }
    opt(&mut o, "agent", &m.agent);
    opt(&mut o, "date", &m.date);
    opt(&mut o, "topic", &m.topic);
    opt(&mut o, "entry_type", &m.entry_type);
    opt(&mut o, "parent_drawer_id", &m.parent_drawer_id);
    serde_json::Value::Object(o).to_string()
}

fn create_schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE collections (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             name TEXT NOT NULL UNIQUE,
             dimension INTEGER,
             created_at TEXT NOT NULL);
         CREATE TABLE documents (
             collection_id INTEGER NOT NULL,
             id TEXT NOT NULL,
             document TEXT NOT NULL,
             metadata_json TEXT NOT NULL,
             embedding BLOB NOT NULL,
             dim INTEGER NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             PRIMARY KEY (collection_id, id),
             FOREIGN KEY(collection_id) REFERENCES collections(id) ON DELETE CASCADE);
         CREATE INDEX idx_documents_collection ON documents(collection_id);
         CREATE TABLE alice_fixture_scratch (n INTEGER);
         INSERT INTO meta VALUES ('embedder_model:mempalace_drawers', 'minilm');
         INSERT INTO collections (name, dimension, created_at) VALUES
             ('mempalace_drawers', 4, '2026-01-01T00:00:00+00:00'),
             ('mempalace_closets', 4, '2026-01-01T00:00:00+00:00');",
    )
    .unwrap();
}

fn insert_doc(conn: &Connection, collection: i64, id: &str, text: &str, meta_json: &str) {
    conn.execute(
        "INSERT INTO documents (collection_id, id, document, metadata_json, embedding, dim, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, zeroblob(16), 4, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
        params![collection, id, text, meta_json],
    )
    .unwrap();
}

fn fill(conn: &Connection, drawers: &[DrawerSpec]) {
    create_schema(conn);
    let tx = conn.unchecked_transaction().unwrap();
    for d in drawers {
        insert_doc(&tx, 1, &d.id, &d.text, &metadata_json(&d.meta));
    }
    insert_doc(
        &tx,
        2,
        "closet_0001",
        "closet row (not a drawer)",
        r#"{"wing":"w","room":"r"}"#,
    );
    tx.commit().unwrap();
}

/// A palace in rollback-journal (DELETE) mode. Returns the path of `sqlite_exact.sqlite3`.
pub fn build_sqlite_exact(dir: &Path, drawers: &[DrawerSpec]) -> PathBuf {
    let path = dir.join(MAIN_FILE);
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    fill(&conn, drawers);
    path
}

/// A palace in WAL mode whose `-wal` and `-shm` files exist and in which a second connection holds an
/// open (uncommitted) write transaction until dropped or released.
pub fn build_sqlite_exact_wal(dir: &Path, drawers: &[DrawerSpec]) -> WalPalace {
    let path = dir.join(MAIN_FILE);
    let main = Connection::open(&path).unwrap();
    let mode: String = main
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    fill(&main, drawers);
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Uncommitted extras: a snapshot that sees these has read a half-written palace.
    for i in 0..5 {
        insert_doc(
            &writer,
            1,
            &format!("zz-uncommitted-{i}"),
            "never committed",
            r#"{"wing":"w","room":"r"}"#,
        );
    }
    WalPalace {
        path,
        _main: main,
        writer: Some(writer),
    }
}
