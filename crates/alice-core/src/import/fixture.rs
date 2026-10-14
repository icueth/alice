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

/// File name Chroma uses for its SQLite store.
pub const CHROMA_FILE: &str = "chroma.sqlite3";

/// A Chroma-backend palace (only the SQLite store; the HNSW folders hold vectors and are ignored)
/// with one collection called `mempalace_drawers`. Returns the path of `chroma.sqlite3`.
pub fn build_chroma(dir: &Path, drawers: &[DrawerSpec]) -> PathBuf {
    build_chroma_in(dir, "mempalace_drawers", drawers)
}

/// Like [`build_chroma`] with a chosen collection name.
pub fn build_chroma_in(dir: &Path, collection: &str, drawers: &[DrawerSpec]) -> PathBuf {
    let path = dir.join(CHROMA_FILE);
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    conn.execute_batch(
        "CREATE TABLE databases (id TEXT PRIMARY KEY, name TEXT NOT NULL, tenant_id TEXT NOT NULL DEFAULT 'default_tenant');
         CREATE TABLE collections (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             dimension INTEGER,
             database_id TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
             config_json_str TEXT,
             schema_str TEXT,
             UNIQUE (name, database_id));
         CREATE TABLE segments (
             id TEXT PRIMARY KEY,
             type TEXT NOT NULL,
             scope TEXT NOT NULL,
             collection TEXT REFERENCES collections(id));
         CREATE TABLE embeddings (
             id INTEGER PRIMARY KEY,
             segment_id TEXT NOT NULL,
             embedding_id TEXT NOT NULL,
             seq_id BLOB NOT NULL,
             created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
             UNIQUE (segment_id, embedding_id));
         CREATE TABLE embedding_metadata (
             id INTEGER REFERENCES embeddings(id),
             key TEXT NOT NULL,
             string_value TEXT,
             int_value INTEGER,
             float_value REAL,
             bool_value INTEGER,
             PRIMARY KEY (id, key));
         INSERT INTO databases (id, name) VALUES ('db1', 'default_database');",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO collections (id, name, dimension, database_id) VALUES ('c1', ?1, 4, 'db1')",
        [collection],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO segments (id, type, scope, collection) VALUES
             ('seg-vec', 'urn:chroma:segment/vector/hnsw-local-persisted', 'VECTOR', 'c1'),
             ('seg-meta', 'urn:chroma:segment/metadata/sqlite', 'METADATA', 'c1');",
    )
    .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    for (i, d) in drawers.iter().enumerate() {
        let rowid = i as i64 + 1;
        for seg in ["seg-vec", "seg-meta"] {
            // Chroma keeps an embeddings row per segment; only the metadata segment has metadata rows.
            let id = if seg == "seg-meta" {
                rowid
            } else {
                rowid + 1_000_000
            };
            tx.execute(
                "INSERT INTO embeddings (id, segment_id, embedding_id, seq_id) VALUES (?1, ?2, ?3, zeroblob(8))",
                params![id, seg, d.id],
            )
            .unwrap();
        }
        let put_s = |k: &str, v: &str| {
            tx.execute(
                "INSERT INTO embedding_metadata (id, key, string_value) VALUES (?1, ?2, ?3)",
                params![rowid, k, v],
            )
            .unwrap();
        };
        put_s("chroma:document", &d.text);
        let m = &d.meta;
        put_s("wing", &m.wing);
        put_s("room", &m.room);
        for (k, v) in [
            ("hall", &m.hall),
            ("source_file", &m.source_file),
            ("added_by", &m.added_by),
            ("ingest_mode", &m.ingest_mode),
            ("filed_at", &m.filed_at),
            ("authored_at", &m.authored_at),
            ("agent", &m.agent),
            ("date", &m.date),
            ("topic", &m.topic),
            ("entry_type", &m.entry_type),
            ("parent_drawer_id", &m.parent_drawer_id),
        ] {
            if let Some(v) = v {
                put_s(k, v);
            }
        }
        if let Some(n) = m.chunk_index {
            tx.execute(
                "INSERT INTO embedding_metadata (id, key, int_value) VALUES (?1, 'chunk_index', ?2)",
                params![rowid, n],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
    path
}

/// One triple row for [`build_kg`]; `None` columns are stored as NULL.
#[derive(Debug, Clone, Default)]
pub struct TripleSpec {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub source_drawer_id: Option<String>,
    pub extracted_at: Option<String>,
}

/// A MemPalace `knowledge_graph.sqlite3` with entities `(id, name)` and triples, using the
/// schema `mempalace/knowledge_graph.py` creates. `legacy_schema` omits `source_drawer_id`,
/// `adapter_name` and `extracted_at`.
pub fn build_kg(
    dir: &Path,
    entities: &[(&str, &str)],
    triples: &[TripleSpec],
    legacy_schema: bool,
) -> PathBuf {
    let path = dir.join("knowledge_graph.sqlite3");
    let conn = Connection::open(&path).unwrap();
    let tail = if legacy_schema {
        ""
    } else {
        ", source_drawer_id TEXT, adapter_name TEXT, extracted_at TEXT DEFAULT CURRENT_TIMESTAMP"
    };
    conn.execute_batch(&format!(
        "CREATE TABLE entities (id TEXT PRIMARY KEY, name TEXT NOT NULL, type TEXT DEFAULT 'unknown',
             properties TEXT DEFAULT '{{}}', created_at TEXT DEFAULT CURRENT_TIMESTAMP);
         CREATE TABLE triples (id TEXT PRIMARY KEY, subject TEXT NOT NULL, predicate TEXT NOT NULL,
             object TEXT NOT NULL, valid_from TEXT, valid_to TEXT, confidence REAL DEFAULT 1.0,
             source_closet TEXT, source_file TEXT{tail});"
    ))
    .unwrap();
    for (id, name) in entities {
        conn.execute(
            "INSERT INTO entities (id, name) VALUES (?1, ?2)",
            params![id, name],
        )
        .unwrap();
    }
    for t in triples {
        conn.execute(
            "INSERT INTO triples (id, subject, predicate, object, valid_from, valid_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                t.id,
                t.subject,
                t.predicate,
                t.object,
                t.valid_from,
                t.valid_to
            ],
        )
        .unwrap();
        if !legacy_schema {
            conn.execute(
                "UPDATE triples SET source_drawer_id = ?2, extracted_at = ?3 WHERE id = ?1",
                params![t.id, t.source_drawer_id, t.extracted_at],
            )
            .unwrap();
        }
    }
    path
}

/// Writes `json` verbatim to `dir/tunnels.json` (tests pass real, flat, or corrupt content).
pub fn build_tunnels(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join("tunnels.json");
    std::fs::write(&path, json).unwrap();
    path
}
