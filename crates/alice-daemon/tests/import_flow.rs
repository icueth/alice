//! The import job end to end: a `Service` in a temp home reads synthetic MemPalace palaces.
//! `Flow` and the fixture palace are shared by the later import tests (knowledge, raw, graph).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::import::fixture::{
    DrawerSpec, TripleSpec, build_kg, build_sqlite_exact, build_sqlite_exact_wal, build_tunnels,
};
use alice_core::import::palace::DrawerMeta;
use alice_core::import::{ImportRequest, ImportState, ImportStatus};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use sha2::{Digest, Sha256};

// ------------------------------------------------------------------------------------ fixtures

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

fn knowledge(id: &str, text: &str, wing: &str, room: &str) -> DrawerSpec {
    DrawerSpec {
        id: id.into(),
        text: text.into(),
        meta: DrawerMeta {
            wing: wing.into(),
            room: room.into(),
            added_by: s("claude-code"),
            filed_at: s("2026-04-10T10:00:00"),
            ..Default::default()
        },
    }
}

fn chunk(id: &str, text: &str, wing: &str, source: &str, index: i64) -> DrawerSpec {
    DrawerSpec {
        id: id.into(),
        text: text.into(),
        meta: DrawerMeta {
            wing: wing.into(),
            room: "technical".into(),
            source_file: s(source),
            ingest_mode: s("convos"),
            chunk_index: Some(index),
            authored_at: s(&format!("2026-04-16T11:{:02}:00", index % 60)),
            filed_at: s("2026-04-16T12:00:00"),
            ..Default::default()
        },
    }
}

const S1: &str = "/h/.claude/projects/-Users-me-Code-pg-ai/s1.jsonl";
const S2: &str = "/h/.claude/projects/-Users-me-Code-devspace/s2.jsonl";

/// The palace Tasks 9-11 share: 117 drawers (5 knowledge, 80 session chunks with one repeat,
/// 30 file chunks, a registry entry and a drawer with unreadable metadata), a closet, a
/// knowledge graph and a tunnels file.
fn fixture_specs() -> Vec<DrawerSpec> {
    let mut v = vec![
        knowledge("k_dec", "decision about the v4 frame", "pg-ai", "decisions"),
        knowledge(
            "k_dup_a",
            "Frames are framed protobuf",
            "pg_ai",
            "knowledge",
        ),
        knowledge(
            "k_dup_b",
            "Frames are framed protobuf",
            "wing_pg_ai",
            "knowledge",
        ),
        DrawerSpec {
            id: "k_diary".into(),
            text: "Set up the import today".into(),
            meta: DrawerMeta {
                wing: "wing_claude_code".into(),
                room: "diary".into(),
                agent: s("claude-code"),
                topic: s("setup"),
                date: s("2026-04-16"),
                filed_at: s("2026-04-16T20:00:00"),
                ..Default::default()
            },
        },
        knowledge(
            "k_agent",
            "Notes from the agent: pg-ai needs the frame, and pg-ai tests pass",
            "wing_claude_code",
            "knowledge",
        ),
    ];
    for i in 0..40 {
        // Chunk 7 of s1 is repeated once, as its last chunk.
        let text = if i == 39 {
            "s1 chunk 7 text".to_string()
        } else {
            format!("s1 chunk {i} text")
        };
        v.push(chunk(&format!("s1_{i:03}"), &text, "sessions", S1, i));
        let text2 = if i == 3 {
            "s2 chunk 3 with the quickstart transcript phrase".to_string()
        } else {
            format!("s2 chunk {i} text")
        };
        v.push(chunk(&format!("s2_{i:03}"), &text2, "sessions", S2, i));
    }
    for i in 0..30 {
        v.push(DrawerSpec {
            id: format!("f_{i:03}"),
            text: format!("func f{i}() {{}}"),
            meta: DrawerMeta {
                wing: "devspace".into(),
                room: "src".into(),
                source_file: s(&format!("/h/Code/devspace/main_{}.go", i % 3)),
                added_by: s("claude"),
                ingest_mode: s("projects"),
                chunk_index: Some(i / 3),
                filed_at: s("2026-04-12T09:00:00"),
                ..Default::default()
            },
        });
    }
    v.push(DrawerSpec {
        id: "_reg_1".into(),
        text: "registry".into(),
        meta: DrawerMeta {
            wing: "devspace".into(),
            room: "_registry".into(),
            ingest_mode: s("registry"),
            ..Default::default()
        },
    });
    v
}

/// `root/palace/` holds the palace and its graph; `root/tunnels.json` is where MemPalace keeps tunnels.
fn build_fixture(root: &Path, specs: &[DrawerSpec]) -> PathBuf {
    let palace = root.join("palace");
    std::fs::create_dir_all(&palace).unwrap();
    let file = build_sqlite_exact(&palace, specs);
    add_unreadable_drawer(&file);
    let t = |id: &str, p: &str, o: &str, to: Option<&str>| TripleSpec {
        id: id.into(),
        subject: "alice".into(),
        predicate: p.into(),
        object: o.into(),
        valid_from: s("2026-01-01"),
        valid_to: to.map(str::to_string),
        source_drawer_id: s("k_dec"),
        ..Default::default()
    };
    build_kg(
        &palace,
        &[
            ("alice", "alice"),
            ("bug-a", "bug-a"),
            ("bug-b", "bug-b"),
            ("bug-c", "bug-c"),
            ("bangkok", "bangkok"),
            ("orphan", "orphan"),
        ],
        &[
            t("t1", "has_bug", "bug-a", Some("2026-02-01")),
            t("t2", "has_bug", "bug-b", None),
            t("t3", "had_bug", "bug-c", None),
            t("t4", "located_at", "bangkok", None),
        ],
        false,
    );
    build_tunnels(
        root,
        r#"{"tunnels":[
            {"id":"t1","source":{"wing":"pg-ai","room":"decisions","drawer_id":"k_dec"},
             "target":{"wing":"wing_claude_code","room":"knowledge","drawer_id":"k_agent"},"label":"x","kind":"explicit"},
            {"id":"t2","source":{"wing":"pg-ai","room":"decisions"},
             "target":{"wing":"devspace","room":"src"},"kind":"explicit"}]}"#,
    );
    palace
}

/// A drawer whose metadata cannot be parsed at all.
fn add_unreadable_drawer(db: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO documents (collection_id, id, document, metadata_json, embedding, dim, created_at, updated_at)
         VALUES (1, 'z_unreadable', 'text', '{not json', zeroblob(16), 4, 'x', 'x')",
        [],
    )
    .unwrap();
}

/// `n` file chunks of one source file per 100, for runs that must last a moment.
fn big_specs(n: usize) -> Vec<DrawerSpec> {
    (0..n)
        .map(|i| DrawerSpec {
            id: format!("big_{i:06}"),
            text: format!("chunk number {i} {}", "padding ".repeat(20)),
            meta: DrawerMeta {
                wing: "devspace".into(),
                room: "src".into(),
                source_file: s(&format!("/h/Code/devspace/file_{}.go", i / 100)),
                added_by: s("claude"),
                ingest_mode: s("projects"),
                chunk_index: Some((i % 100) as i64),
                ..Default::default()
            },
        })
        .collect()
}

// ------------------------------------------------------------------------------------- the flow

struct Flow {
    _root: tempfile::TempDir,
    _palace_root: tempfile::TempDir,
    palace: PathBuf,
    home: AliceHome,
    svc: Arc<Service>,
}

trait Req {
    fn dry_run(self, on: bool) -> Self;
}

impl Req for ImportRequest {
    fn dry_run(mut self, on: bool) -> Self {
        self.dry_run = on;
        self
    }
}

fn start_service(home: &AliceHome) -> Arc<Service> {
    Service::start(
        home.clone(),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions::default(),
    )
    .unwrap()
}

#[allow(dead_code)] // not every helper is used by every task's tests
impl Flow {
    async fn new() -> Flow {
        Self::with_specs(&fixture_specs()).await
    }

    async fn with_specs(specs: &[DrawerSpec]) -> Flow {
        let root = tempfile::tempdir().unwrap();
        let palace_root = tempfile::tempdir().unwrap();
        let palace = build_fixture(palace_root.path(), specs);
        let home = AliceHome::new(root.path().join("alice"));
        let svc = start_service(&home);
        Flow {
            _root: root,
            _palace_root: palace_root,
            palace,
            home,
            svc,
        }
    }

    fn home(&self) -> &Path {
        self.home.root()
    }

    fn request(&self) -> ImportRequest {
        ImportRequest {
            palace: Some(self.palace.display().to_string()),
            ..Default::default()
        }
    }

    async fn wait(&self) -> ImportStatus {
        let t = Instant::now();
        loop {
            let st = self.svc.import_status();
            if st.state != ImportState::Running {
                return st;
            }
            assert!(t.elapsed() < Duration::from_secs(120), "import did not end");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn run_import(&self, req: ImportRequest) -> ImportStatus {
        self.svc.start_import(req).unwrap();
        self.wait().await
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.home.db()).unwrap()
    }

    /// Row counts of every table, the virtual ones and the event and meta tables included.
    async fn table_counts(&self) -> BTreeMap<String, i64> {
        let conn = self.db();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        names
            .into_iter()
            .map(|n| {
                let c: i64 = conn
                    .query_row(&format!("SELECT count(*) FROM \"{n}\""), [], |r| r.get(0))
                    .unwrap();
                (n, c)
            })
            .collect()
    }

    async fn setting_bool(&self, key: &str) -> bool {
        let mut v = serde_json::to_value(&*self.svc.settings()).unwrap();
        for part in key.split('.') {
            v = v[part].clone();
        }
        v.as_bool().unwrap()
    }

    async fn patch(&self, key: &str, value: serde_json::Value) {
        let mut m = serde_json::Map::new();
        m.insert(key.to_string(), value);
        self.svc.patch_settings(m).unwrap();
    }

    fn import_tmp_entries(&self) -> usize {
        std::fs::read_dir(self.home().join("import-tmp"))
            .map(|d| d.count())
            .unwrap_or(0)
    }
}

fn hash_dir(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            let mut h = Sha256::new();
            h.update(std::fs::read(&p).unwrap());
            out.insert(
                p.file_name().unwrap().to_string_lossy().into_owned(),
                hex::encode(h.finalize()),
            );
        }
    }
    out
}

// ------------------------------------------------------------------------------------- the tests

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_reports_what_a_real_run_would_add_and_writes_nothing() {
    let f = Flow::new().await;
    let before = f.table_counts().await;
    let st = f.run_import(f.request().dry_run(true)).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!(
        (
            r.tiers.knowledge,
            r.tiers.raw_session_chunks,
            r.tiers.raw_file_chunks
        ),
        (5, 80, 30)
    );
    assert_eq!((r.duplicates.knowledge, r.duplicates.raw), (1, 1));
    assert_eq!(r.skipped["MemPalace registry entry"], 1);
    assert_eq!(r.skipped["unreadable metadata"], 1);
    assert_eq!(r.closets_dropped, 1);
    assert_eq!(r.drawers_seen, 117);
    assert_eq!(r.drawers_seen, r.drawers_accounted, "{:?}", r.errors);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.tiers.raw_session_transcripts, 2);
    assert_eq!(r.already_imported, 0);
    assert_eq!(r.palace.backend, "sqlite_exact");
    assert_eq!(r.wing_map["wing_pg_ai"].to, "pg-ai");
    assert_eq!(r.wing_map["wing_pg_ai"].via, "alias");
    assert_eq!(r.wing_map["wing_claude_code"].via, "inferred");
    assert_eq!(r.projects["pg-ai"].memories, 3);
    assert_eq!(f.table_counts().await, before, "a dry run writes nothing");
    assert!(
        !f.setting_bool("compat.mempalace").await,
        "and does not turn compat on"
    );
    assert_eq!(f.import_tmp_entries(), 0, "snapshot removed");
    assert!(r.phases.iter().any(|p| p.phase == "survey"));
    assert!(r.render_text().contains("[dry run]"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_run_is_not_silently_empty_in_this_build() {
    let f = Flow::new().await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Failed);
    assert!(
        st.error.unwrap().contains("--dry-run"),
        "says what to do instead"
    );
    assert_eq!(f.import_tmp_entries(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_one_clear_sentence_each_and_create_no_files() {
    let f = Flow::new().await;
    let refused = |req: ImportRequest| f.svc.start_import(req).unwrap_err().to_string();

    let e = refused(ImportRequest::default());
    assert!(e.contains("--palace"), "{e}");
    let e = refused(ImportRequest {
        palace: Some("/nonexistent/palace".into()),
        ..Default::default()
    });
    assert!(e.contains("does not exist"), "{e}");
    let e = refused(ImportRequest {
        since: Some("last tuesday".into()),
        ..f.request()
    });
    assert!(e.contains("--since"), "{e}");

    f.patch("embedding.provider", serde_json::json!("ollama"))
        .await;
    let e = refused(f.request());
    assert!(
        e.contains("--allow-remote-embedding") && e.contains("ollama"),
        "{e}"
    );
    assert_eq!(f.import_tmp_entries(), 0, "refusals create no files");
    let ok = ImportRequest {
        allow_remote_embedding: true,
        ..f.request().dry_run(true)
    };
    assert_eq!(f.run_import(ok).await.state, ImportState::Done);

    // A folder that only looks like a palace: the failure names the damage.
    let g = Flow::new().await;
    let fake = tempfile::tempdir().unwrap();
    std::fs::write(fake.path().join("sqlite_exact.sqlite3"), "this is text").unwrap();
    let st = g
        .run_import(ImportRequest {
            palace: Some(fake.path().display().to_string()),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert_eq!(st.state, ImportState::Failed);
    assert!(
        st.error.as_deref().unwrap_or("").contains("damaged"),
        "{:?}",
        st.error
    );
    assert_eq!(
        g.import_tmp_entries(),
        0,
        "the snapshot folder of a failed job is removed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_source_palace_is_byte_identical_afterwards_and_a_wal_palace_works() {
    let f = Flow::new().await;
    let before = hash_dir(&f.palace);
    let st = f.run_import(f.request().dry_run(true)).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    assert_eq!(hash_dir(&f.palace), before, "the palace must not change");

    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("palace");
    std::fs::create_dir_all(&dir).unwrap();
    let specs = fixture_specs();
    let mut wal = build_sqlite_exact_wal(&dir, &specs);
    wal.release_writer();
    let h = AliceHome::new(root.path().join("alice"));
    let svc = start_service(&h);
    let before = hash_dir(&dir);
    svc.start_import(ImportRequest {
        palace: Some(dir.display().to_string()),
        dry_run: true,
        ..Default::default()
    })
    .unwrap();
    let t = Instant::now();
    let st = loop {
        let st = svc.import_status();
        if st.state != ImportState::Running {
            break st;
        }
        assert!(t.elapsed() < Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    assert_eq!(st.report.unwrap().tiers.knowledge, 5);
    let after = hash_dir(&dir);
    // Only an empty -shm / -wal may be new; the database itself is untouched.
    assert_eq!(
        after.get("sqlite_exact.sqlite3"),
        before.get("sqlite_exact.sqlite3")
    );
    for (k, v) in &after {
        if !before.contains_key(k) {
            assert!(k.ends_with("-shm") || k.ends_with("-wal"), "new file {k}");
            let _ = v;
        }
    }
    drop(wal);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_start_while_one_runs_is_refused_with_the_progress() {
    let f = Flow::with_specs(&big_specs(20_000)).await;
    f.svc.start_import(f.request().dry_run(true)).unwrap();
    let e = f
        .svc
        .start_import(f.request().dry_run(true))
        .unwrap_err()
        .to_string();
    assert!(e.contains("already running"), "{e}");
    assert!(
        ["starting", "snapshot", "survey", "knowledge", "raw"]
            .iter()
            .any(|p| e.contains(p)),
        "names the phase: {e}"
    );
    let st = f.wait().await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    assert_eq!(
        st.report.unwrap().drawers_seen,
        20_001,
        "20,000 chunks and the unreadable drawer"
    );
    // Done: the next one may start.
    assert_eq!(
        f.run_import(f.request().dry_run(true)).await.state,
        ImportState::Done
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_joins_the_import_thread_and_the_daemon_start_sweeps_import_tmp() {
    let f = Flow::with_specs(&big_specs(30_000)).await;
    f.svc.start_import(f.request().dry_run(true)).unwrap();
    let t = Instant::now();
    f.svc.shutdown();
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let st = f.svc.import_status();
    assert_eq!(st.state, ImportState::Failed, "{st:?}");
    assert!(st.error.unwrap().contains("daemon stopped"));
    assert_eq!(f.import_tmp_entries(), 0, "no snapshot folder left");

    // A folder an earlier daemon left behind is gone after the next start.
    let root = tempfile::tempdir().unwrap();
    let home = AliceHome::new(root.path().join("alice"));
    std::fs::create_dir_all(home.import_tmp().join("old-job")).unwrap();
    std::fs::write(home.import_tmp().join("old-job").join("x"), "x").unwrap();
    let svc = start_service(&home);
    assert!(!home.import_tmp().join("old-job").exists());
    svc.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_import_holds_the_activity_guard_so_maintenance_stays_off_the_writer() {
    let f = Flow::with_specs(&big_specs(20_000)).await;
    assert!(!f.svc.activity().is_busy());
    f.svc.start_import(f.request().dry_run(true)).unwrap();
    assert!(f.svc.activity().is_busy(), "busy while the job runs");
    let st = f.wait().await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    assert!(!f.svc.activity().is_busy(), "released when the job is over");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_in_a_transcript_path_never_reaches_the_report() {
    let secret = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
    let mut specs = fixture_specs();
    specs.push(chunk(
        "s3_000",
        "text",
        "sessions",
        &format!("/h/.claude/projects/-Users-me-Code-pg-ai/{secret}.jsonl"),
        0,
    ));
    specs.push(knowledge(
        "k_secret",
        &format!("the key is {secret}"),
        &format!("notes {secret}"),
        "knowledge",
    ));
    let f = Flow::with_specs(&specs).await;
    let st = f.run_import(f.request().dry_run(true)).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    let json = serde_json::to_string(&r).unwrap();
    let at = json
        .find(secret)
        .map(|i| json[i.saturating_sub(40)..i + 20].to_string());
    assert!(at.is_none(), "{at:?}");
    assert!(!r.render_text().contains(secret));
    assert!(r.redacted_fields >= 2, "{}", r.redacted_fields);
}

#[tokio::test(flavor = "multi_thread")]
async fn drawers_stored_by_an_earlier_run_are_counted_not_planned_again() {
    let f = Flow::new().await;
    let conn = f.db();
    for id in ["k_dec", "s1_000", "f_000"] {
        conn.execute(
            "INSERT INTO legacy_ids (legacy_id, target_type, target_id) VALUES (?1, 'chunk', 'x')",
            [id],
        )
        .unwrap();
    }
    drop(conn);
    let r = f
        .run_import(f.request().dry_run(true))
        .await
        .report
        .unwrap();
    assert_eq!(r.already_imported, 3);
    assert_eq!(
        (
            r.tiers.knowledge,
            r.tiers.raw_session_chunks,
            r.tiers.raw_file_chunks
        ),
        (4, 79, 29)
    );
    assert_eq!(r.drawers_seen, r.drawers_accounted, "{:?}", r.errors);
}

/// The survey asks for the raw `file://` uri; chunks are stored under the redacted one. A lookup
/// that used the raw uri would find nothing and hand out positions that are taken.
#[tokio::test(flavor = "multi_thread")]
async fn the_reserved_lookup_applies_the_same_redaction_as_storage() {
    use alice_core::api::IngestRequest;
    use alice_core::settings::PrivacySettings;
    use alice_core::store::ReaderPool;
    use alice_core::store::sources::{ChunkInput, SourceInput};
    use alice_daemon::importer::{RedactFilter, reserved_ords};
    use alice_daemon::redaction::Redactors;

    let f = Flow::new().await;
    let secret = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
    let raw_uri = format!("file:///h/.claude/projects/-Users-me-Code-pg-ai/{secret}.jsonl");
    f.svc
        .ingest(IngestRequest {
            source: SourceInput {
                uri: raw_uri.clone(),
                ..Default::default()
            },
            chunks: (0..3)
                .map(|i| ChunkInput {
                    ord: i * 2,
                    text: format!("stored chunk {i}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
        .await
        .unwrap();
    let readers = ReaderPool::new(f.home.db(), 1);
    let filter =
        RedactFilter::new(Redactors::fixed(PrivacySettings::default()).for_root_always_on(None));
    let got = reserved_ords(&readers, &filter, &raw_uri).unwrap();
    assert_eq!(got.into_iter().collect::<Vec<_>>(), vec![0, 2, 4]);
    assert!(
        reserved_ords(&readers, &filter, "file:///no/such.jsonl")
            .unwrap()
            .is_empty()
    );
}
