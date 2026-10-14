//! The import job end to end: a `Service` in a temp home reads synthetic MemPalace palaces.
//! `Flow` and the fixture palace are shared by the later import tests (knowledge, raw, graph).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::import::fixture::{
    DrawerSpec, TripleSpec, build_kg, build_sqlite_exact, build_sqlite_exact_wal, build_tunnels,
};
use alice_core::import::palace::DrawerMeta;
use alice_core::import::{ImportRequest, ImportState, ImportStatus};
use alice_daemon::importer::ImportHook;
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
    start_service_with(home, ServiceOptions::default())
}

fn start_service_with(home: &AliceHome, opts: ServiceOptions) -> Arc<Service> {
    Service::start(
        home.clone(),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        opts,
    )
    .unwrap()
}

#[allow(dead_code)] // not every helper is used by every task's tests
impl Flow {
    async fn new() -> Flow {
        Self::with_specs(&fixture_specs()).await
    }

    async fn with_specs(specs: &[DrawerSpec]) -> Flow {
        Self::with_options(specs, |_| ServiceOptions::default()).await
    }

    /// `options` gets the folder that holds `palace/` (a stand-in for `~/.mempalace`).
    async fn with_options(
        specs: &[DrawerSpec],
        options: impl FnOnce(&Path) -> ServiceOptions,
    ) -> Flow {
        let root = tempfile::tempdir().unwrap();
        let palace_root = tempfile::tempdir().unwrap();
        let palace = build_fixture(palace_root.path(), specs);
        let home = AliceHome::new(root.path().join("alice"));
        let svc = start_service_with(&home, options(palace_root.path()));
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

    async fn count(&self, table: &str) -> i64 {
        self.db()
            .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    async fn project_slugs(&self) -> Vec<String> {
        let conn = self.db();
        let mut stmt = conn
            .prepare("SELECT slug FROM projects ORDER BY slug")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// Where an old drawer id went (`type:id`).
    async fn legacy_target(&self, id: &str) -> Option<String> {
        self.db()
            .query_row(
                "SELECT target_type || ':' || target_id FROM legacy_ids WHERE legacy_id = ?1",
                [id],
                |r| r.get(0),
            )
            .ok()
    }

    async fn recall(&self, query: &str, raw: bool) -> String {
        let r = self
            .svc
            .recall(alice_core::api::RecallRequest {
                query: query.to_string(),
                include_raw: raw,
                ..Default::default()
            })
            .await
            .unwrap();
        let text = r.text.clone().unwrap_or_default();
        let hits = r
            .results
            .iter()
            .map(|h| {
                format!(
                    "{} {} {}",
                    h.title,
                    h.snippet,
                    h.body.clone().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("{text}\n{hits}")
    }

    async fn diary_author(&self) -> Option<String> {
        self.db()
            .query_row(
                "SELECT author_agent FROM memories WHERE legacy_id = 'k_diary'",
                [],
                |r| r.get(0),
            )
            .ok()
            .flatten()
    }

    async fn distinct(&self, table: &str, column: &str) -> Vec<String> {
        let conn = self.db();
        let mut stmt = conn
            .prepare(&format!("SELECT DISTINCT {column} FROM {table} ORDER BY 1"))
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    async fn health(&self) -> alice_core::api::Health {
        self.svc.health().await.unwrap()
    }

    async fn wait_embedded(&self) {
        let t = Instant::now();
        while self.health().await.embed_backlog > 0 {
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "embedding did not drain"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Tables (and columns) of the database that hold `needle` anywhere.
    async fn tables_holding(&self, needle: &str) -> Vec<String> {
        let conn = self.db();
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut hits = Vec::new();
        for t in tables {
            let cols: Vec<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{t}')"))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for c in cols {
                let sql = format!(
                    "SELECT count(*) FROM \"{t}\" WHERE instr(CAST(\"{c}\" AS TEXT), ?1) > 0"
                );
                let n: i64 = match conn.query_row(&sql, [needle], |r| r.get(0)) {
                    Ok(n) => n,
                    Err(_) => continue, // a column a virtual table cannot be scanned for
                };
                if n > 0 {
                    hits.push(format!("{t}.{c}"));
                }
            }
        }
        hits
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
    let hook = Arc::new(ImportHook::default());
    hook.hold.store(true, Ordering::SeqCst);
    let h = hook.clone();
    let f = Flow::with_options(&fixture_specs(), move |_| ServiceOptions {
        import_hook: Some(h),
        ..Default::default()
    })
    .await;
    f.svc.start_import(f.request().dry_run(true)).unwrap();
    // The job is parked after its snapshot, so this cannot race with its end.
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
    hook.hold.store(false, Ordering::SeqCst);
    let st = f.wait().await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    assert_eq!(st.report.unwrap().drawers_seen, 117);
    // Done: the next one may start.
    assert_eq!(
        f.run_import(f.request().dry_run(true)).await.state,
        ImportState::Done
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_snapshot_folder_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let hook = Arc::new(ImportHook::default());
    hook.hold.store(true, Ordering::SeqCst);
    let h = hook.clone();
    let f = Flow::with_options(&fixture_specs(), move |_| ServiceOptions {
        import_hook: Some(h),
        ..Default::default()
    })
    .await;
    let job = f
        .svc
        .start_import(f.request().dry_run(true))
        .unwrap()
        .job_id
        .unwrap();
    // Parked after the snapshot: the copy of the owner's history is on disk right now.
    let tmp = f.home().join("import-tmp");
    let t = Instant::now();
    while !tmp.join(&job).join("sqlite_exact.sqlite3").is_file() {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "no snapshot appeared"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&tmp), 0o700);
    assert_eq!(mode(&tmp.join(&job)), 0o700);
    assert!(tmp.join(&job).join("sqlite_exact.sqlite3").is_file());
    hook.hold.store(false, Ordering::SeqCst);
    assert_eq!(f.wait().await.state, ImportState::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_in_the_job_fails_it_and_releases_everything() {
    let hook = Arc::new(ImportHook::default());
    hook.panic.store(true, Ordering::SeqCst);
    let h = hook.clone();
    let f = Flow::with_options(&fixture_specs(), move |_| ServiceOptions {
        import_hook: Some(h),
        ..Default::default()
    })
    .await;
    let st = f.run_import(f.request().dry_run(true)).await;
    assert_eq!(st.state, ImportState::Failed);
    assert!(st.error.unwrap().contains("unexpectedly"));
    assert!(!f.svc.activity().is_busy(), "the guard was released");
    assert_eq!(f.import_tmp_entries(), 0, "the snapshot folder was removed");
    // The slot is usable again.
    hook.panic.store(false, Ordering::SeqCst);
    assert_eq!(
        f.run_import(f.request().dry_run(true)).await.state,
        ImportState::Done
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_palace_the_default_under_mempalace_home_is_read() {
    let f = Flow::with_options(&fixture_specs(), |home| ServiceOptions {
        mempalace_home: Some(home.to_path_buf()),
        ..Default::default()
    })
    .await;
    let st = f
        .run_import(ImportRequest {
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!(r.tiers.knowledge, 5);
    assert!(r.palace.path.ends_with("/palace"), "{}", r.palace.path);
    assert_eq!(f.import_tmp_entries(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_default_palace_that_is_missing_names_the_fix_and_an_empty_palace_is_refused() {
    let none = tempfile::tempdir().unwrap();
    let dir = none.path().to_path_buf();
    let f = Flow::with_options(&fixture_specs(), move |_| ServiceOptions {
        mempalace_home: Some(dir),
        ..Default::default()
    })
    .await;
    let e = f
        .svc
        .start_import(ImportRequest::default())
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("does not exist") && e.contains("--palace"),
        "{e}"
    );
    for blank in ["", "   "] {
        let e = f
            .svc
            .start_import(ImportRequest {
                palace: Some(blank.into()),
                ..Default::default()
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("--palace is empty"), "{e}");
    }
    assert_eq!(f.import_tmp_entries(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn names_that_look_like_secrets_never_reach_the_status_or_the_text() {
    // Lower-case and hyphenated, so slugify leaves them as they are.
    let plain = concat!("s", "k-", "abcdefghijklmnopqrstuvwx1234");
    let agent = concat!("s", "k-", "zyxwvutsrqponmlkjihgfedc9876");
    let mut specs = fixture_specs();
    specs.push(knowledge("k_p", "a note", plain, "knowledge"));
    specs.push(knowledge(
        "k_a",
        "another note",
        &format!("wing_{agent}"),
        "knowledge",
    ));
    let f = Flow::with_specs(&specs).await;
    let st = f.run_import(f.request().dry_run(true)).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let json = serde_json::to_string(&st).unwrap();
    let text = st.report.as_ref().unwrap().render_text();
    for secret in [plain, agent] {
        assert!(
            !json.contains(secret),
            "status JSON holds a name that looks like a key"
        );
        assert!(
            !text.contains(secret),
            "text report holds a name that looks like a key"
        );
    }
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

// ------------------------------------------------------------------ Task 10: knowledge and raw

const SECRET: &str = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");

fn secret_specs() -> Vec<DrawerSpec> {
    let mut v = fixture_specs();
    // A transcript chunk whose source path also holds the key.
    v.push(chunk(
        "sec_t",
        &format!("transcript with {SECRET} inside"),
        "sessions",
        &format!("/h/.claude/projects/-Users-me-Code-pg-ai/{SECRET}.jsonl"),
        0,
    ));
    // The diary text, under an id that holds the key as well.
    v.push(DrawerSpec {
        id: format!("diary_wing_claude_code_{SECRET}"),
        text: format!("diary says {SECRET}"),
        meta: DrawerMeta {
            wing: "wing_claude_code".into(),
            room: "diary".into(),
            agent: s("claude-code"),
            topic: s("keys"),
            date: s("2026-04-17"),
            filed_at: s("2026-04-17T20:00:00"),
            ..Default::default()
        },
    });
    // A code chunk, and one whose key straddles the 32,000-byte split point.
    v.push(DrawerSpec {
        id: "sec_f".into(),
        text: format!("const KEY = \"{SECRET}\""),
        meta: DrawerMeta {
            wing: "devspace".into(),
            room: "src".into(),
            source_file: s("/h/Code/devspace/keys.go"),
            added_by: s("claude"),
            ingest_mode: s("projects"),
            chunk_index: Some(0),
            ..Default::default()
        },
    });
    v.push(DrawerSpec {
        id: "sec_big".into(),
        text: format!("{}{SECRET}{}", "a ".repeat(15_995), " b".repeat(1_000)),
        meta: DrawerMeta {
            wing: "devspace".into(),
            room: "src".into(),
            source_file: s("/h/Code/devspace/keys.go"),
            added_by: s("claude"),
            ingest_mode: s("projects"),
            chunk_index: Some(1),
            ..Default::default()
        },
    });
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn a_palace_imports_into_both_tiers_with_projects_topics_and_authors() {
    let f = Flow::new().await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(f.project_slugs().await, vec!["devspace", "global", "pg-ai"]);
    assert_eq!(r.wing_map["wing_pg_ai"].to, "pg-ai");
    assert_eq!(r.wing_map["wing_claude_code"].via, "inferred");
    assert_eq!(f.count("memories").await, 4);
    assert_eq!(f.count("chunks").await, 79 + 30);
    assert_eq!(
        f.legacy_target("k_dup_a").await,
        f.legacy_target("k_dup_b").await
    );
    assert!(f.legacy_target("k_dup_a").await.is_some());
    assert!(
        f.recall("decision about the v4 frame", false)
            .await
            .contains("v4 frame")
    );
    assert!(
        f.recall("quickstart transcript phrase", true)
            .await
            .contains("transcript phrase")
    );
    assert_eq!(f.diary_author().await.as_deref(), Some("claude-code"));
    assert_eq!(f.distinct("memories", "judge_state").await, vec!["skipped"]);
    assert_eq!(f.distinct("memories", "status").await, vec!["active"]);
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    f.wait_embedded().await;
    assert_eq!(f.health().await.embed_backlog, 0);
    // The collapsed repeat points at the chunk it repeats.
    assert_eq!(
        f.legacy_target("s1_039").await,
        f.legacy_target("s1_007").await
    );
    assert_eq!(f.import_tmp_entries(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_in_imported_history_never_reach_the_database_the_report_or_the_meta_marker() {
    let plain = concat!("s", "k-", "abcdefghijklmnopqrstuvwx1234");
    let mut specs = secret_specs();
    // A wing and a room named like a key.
    specs.push(knowledge(
        "k_plain",
        "a note about keys",
        plain,
        "knowledge",
    ));
    specs.push(knowledge("k_room", "a note in a room", "pg-ai", plain));
    let f = Flow::with_specs(&specs).await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    for needle in [SECRET, plain, &SECRET[..20]] {
        assert_eq!(f.tables_holding(needle).await, Vec::<String>::new());
    }
    let json = serde_json::to_string(&r).unwrap();
    for needle in [SECRET, plain] {
        assert!(!json.contains(needle) && !r.render_text().contains(needle));
    }
    let marked: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM chunks WHERE text LIKE '%[REDACTED:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(marked >= 3, "{marked}");
    assert!(r.redacted_fields >= 4, "{}", r.redacted_fields);
    // A second run recognises the drawers whose ids held the key (the stored id is the hashed one).
    let again = f.run_import(f.request()).await.report.unwrap();
    assert_eq!(again.redacted_fields, 0);
    assert_eq!(again.tiers.knowledge, 0);
    assert_eq!(again.tiers.raw_session_chunks, 0);
    assert_eq!(f.tables_holding(SECRET).await, Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn redaction_runs_even_when_the_privacy_setting_is_off_and_the_report_says_so() {
    let f = Flow::with_specs(&secret_specs()).await;
    f.patch("privacy.redact", serde_json::json!(false)).await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.redaction_note.is_some());
    assert_eq!(f.tables_holding(SECRET).await, Vec::<String>::new());
    assert!(r.redacted_fields >= 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_mode_skips_the_drawer_names_the_detector_and_keeps_going() {
    let f = Flow::with_specs(&secret_specs()).await;
    f.patch("privacy.redact_mode", serde_json::json!("block"))
        .await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    // Three chunk drawers and the diary hold the key.
    assert_eq!(r.blocked_by_redaction["github_token"], 4);
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    assert_eq!(f.count("memories").await, 4);
    assert_eq!(f.count("chunks").await, 79 + 30);
    assert_eq!(f.tables_holding(SECRET).await, Vec::<String>::new());
}

fn add_drawers(palace: &Path, specs: &[DrawerSpec]) {
    let conn = rusqlite::Connection::open(palace.join("sqlite_exact.sqlite3")).unwrap();
    for d in specs {
        let m = &d.meta;
        let mut o = serde_json::Map::new();
        o.insert("wing".into(), m.wing.clone().into());
        o.insert("room".into(), m.room.clone().into());
        for (k, v) in [
            ("source_file", &m.source_file),
            ("added_by", &m.added_by),
            ("ingest_mode", &m.ingest_mode),
            ("filed_at", &m.filed_at),
            ("authored_at", &m.authored_at),
        ] {
            if let Some(v) = v {
                o.insert(k.into(), v.clone().into());
            }
        }
        if let Some(i) = m.chunk_index {
            o.insert("chunk_index".into(), i.into());
        }
        conn.execute(
            "INSERT INTO documents (collection_id, id, document, metadata_json, embedding, dim, created_at, updated_at)
             VALUES (1, ?1, ?2, ?3, zeroblob(16), 4, 'x', 'x')",
            rusqlite::params![d.id, d.text, serde_json::Value::Object(o).to_string()],
        )
        .unwrap();
    }
}

fn unindexed(id: &str, text: &str) -> DrawerSpec {
    let mut d = chunk(id, text, "sessions", S1, 0);
    d.meta.chunk_index = None;
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn running_again_imports_nothing_does_no_planning_work_and_new_drawers_are_picked_up() {
    let mut specs = fixture_specs();
    specs.push(unindexed("auto_1", "an unnumbered chunk"));
    let f = Flow::with_specs(&specs).await;
    let first = f.run_import(f.request()).await.report.unwrap();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let counts = f.table_counts().await;
    let (memories, chunks) = (counts["memories"], counts["chunks"]);

    let second = f.run_import(f.request()).await.report.unwrap();
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert_eq!(
        (
            second.tiers.knowledge,
            second.tiers.raw_session_chunks,
            second.tiers.raw_file_chunks
        ),
        (0, 0, 0)
    );
    assert_eq!(
        second.already_imported,
        5 + 81 + 30,
        "every stored drawer, the repeat and the duplicate included"
    );
    assert_eq!(second.redacted_fields, 0);
    assert_eq!(second.drawers_seen, second.drawers_accounted);
    let work: f64 = second
        .phases
        .iter()
        .filter(|p| p.phase == "knowledge" || p.phase == "raw")
        .map(|p| p.seconds)
        .sum();
    assert!(work < 1.0, "{work}");
    let counts = f.table_counts().await;
    assert_eq!((counts["memories"], counts["chunks"]), (memories, chunks));

    // Two new drawers: one unnumbered, one that asks for the position "auto_1" was given.
    let asked: i64 = f
        .db()
        .query_row(
            "SELECT c.ord FROM chunks c WHERE c.legacy_id = 'auto_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut wants = chunk("new_b", "asks for a taken position", "sessions", S1, asked);
    wants.meta.filed_at = s("2026-04-18T12:00:00");
    add_drawers(
        &f.palace,
        &[unindexed("new_a", "another unnumbered chunk"), wants],
    );
    let third = f.run_import(f.request()).await.report.unwrap();
    assert!(third.errors.is_empty(), "{:?}", third.errors);
    assert_eq!(third.tiers.raw_session_chunks, 2);
    assert_eq!(f.count("chunks").await, chunks + 2);
    let clash: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM (SELECT source_id, ord FROM chunks GROUP BY 1, 2 HAVING count(*) > 1)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(clash, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversize_knowledge_drawer_becomes_parts_that_are_stored_together() {
    let mut specs = fixture_specs();
    let text = (0..13_000)
        .map(|i| format!("word{i:05}"))
        .collect::<Vec<_>>()
        .join(" "); // about 130 KB
    assert!(text.len() > 125_000);
    specs.push(knowledge("big_k", &text, "pg-ai", "knowledge"));
    let f = Flow::with_specs(&specs).await;
    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    for id in ["big_k", "big_k#2", "big_k#3"] {
        assert!(f.legacy_target(id).await.is_some(), "{id}");
    }
    assert!(f.legacy_target("big_k#4").await.is_none());
    assert_eq!(r.split.memories, 1);
    let events: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM memories WHERE legacy_id LIKE 'big_k%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn odd_drawers_are_counted_not_fatal() {
    let mut specs = fixture_specs();
    let k = |id: &str, text: &str, wing: &str| knowledge(id, text, wing, "knowledge");
    specs.push(k("odd_empty", "", "pg-ai"));
    specs.push(k("odd_blank", "   \n\t", "pg-ai"));
    specs.push(k("odd_nul", "has a nul\0inside", "pg-ai"));
    let big = (0..13_000)
        .map(|i| format!("word{i:05}"))
        .collect::<Vec<_>>()
        .join(" ");
    specs.push(k("odd_big", &big, "pg-ai"));
    specs.push(k("odd_nowing", "no wing at all", ""));
    specs.push(k("odd_dashes", "wing of dashes", "---"));
    let mut forty = chunk(
        "odd_40k",
        &"forty kilobytes of words. ".repeat(1_600),
        "sessions",
        S1,
        100,
    );
    forty.meta.filed_at = s("2026-04-16T12:00:00");
    specs.push(forty);
    specs.push(unindexed("odd_noindex", "chunk with no index"));
    let f = Flow::with_specs(&specs).await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.skipped["empty text"], 2);
    assert_eq!(r.split.memories, 1);
    assert_eq!(r.split.chunks, 1);
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    let conn = f.db();
    let body: i64 = conn
        .query_row(
            "SELECT max(length(CAST(body AS BLOB))) FROM memories",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let text: i64 = conn
        .query_row(
            "SELECT max(length(CAST(text AS BLOB))) FROM chunks",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(body <= 65_536 && text <= 32_768, "{body} {text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ignored_chunk_insert_is_an_error_entry_never_silent() {
    // The second piece of an oversize chunk takes position 10,000,000 + its ord; another drawer
    // of the same source asks for exactly that.
    let mut specs = fixture_specs();
    let mut a = chunk(
        "coll_a",
        &"a long chunk of words. ".repeat(2_000),
        "devspace",
        "/h/Code/devspace/collide.go",
        5,
    );
    a.meta.ingest_mode = s("projects");
    a.meta.convos_off();
    let mut b = chunk(
        "coll_b",
        "asks for the position a piece took",
        "devspace",
        "/h/Code/devspace/collide.go",
        10_000_005,
    );
    b.meta.convos_off();
    specs.push(a);
    specs.push(b);
    let f = Flow::with_specs(&specs).await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert!(r.errors[0].contains("coll_b"), "{:?}", r.errors);
}

trait ConvosOff {
    fn convos_off(&mut self);
}
impl ConvosOff for DrawerMeta {
    /// A file chunk from a curated-free room: the raw file tier.
    fn convos_off(&mut self) {
        self.ingest_mode = s("projects");
        self.added_by = s("claude");
        self.room = "src".into();
    }
}
