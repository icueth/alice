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

#[path = "common/palace.rs"]
mod palace;
use palace::*;

/// `root/palace/` holds the palace and its graph; `root/tunnels.json` is where MemPalace keeps tunnels.
/// The fixture with more triples (subject `alice`, object an entity id that is not a row, so the
/// id is the name).
fn build_fixture_with(root: &Path, specs: &[DrawerSpec], extra: &[TripleSpec]) -> PathBuf {
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
    let mut triples = vec![
        t("t1", "has_bug", "bug-a", Some("2026-02-01")),
        t("t2", "has_bug", "bug-b", None),
        t("t3", "had_bug", "bug-c", None),
        t("t4", "located_at", "bangkok", None),
    ];
    triples.extend(extra.iter().cloned());
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
        &triples,
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
    hook: Option<Arc<ImportHook>>,
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
        Self::with_graph(specs, &[], options).await
    }

    /// `with_options` and more triples in the knowledge graph.
    async fn with_graph(
        specs: &[DrawerSpec],
        extra: &[TripleSpec],
        options: impl FnOnce(&Path) -> ServiceOptions,
    ) -> Flow {
        let root = tempfile::tempdir().unwrap();
        let palace_root = tempfile::tempdir().unwrap();
        let palace = build_fixture_with(palace_root.path(), specs, extra);
        let home = AliceHome::new(root.path().join("alice"));
        let svc = start_service_with(&home, options(palace_root.path()));
        Flow {
            _root: root,
            _palace_root: palace_root,
            palace,
            home,
            svc,
            hook: None,
        }
    }

    /// A flow whose import hook is kept, to count what the planner did.
    async fn with_hook(specs: &[DrawerSpec]) -> Flow {
        let hook = Arc::new(ImportHook::default());
        let h = hook.clone();
        let mut f = Self::with_options(specs, move |_| ServiceOptions {
            import_hook: Some(h),
            ..Default::default()
        })
        .await;
        f.hook = Some(hook);
        f
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

    /// The facts of `subject`, ended ones included.
    async fn facts_timeline(&self, subject: &str) -> Vec<String> {
        let conn = self.db();
        let mut stmt = conn
            .prepare(
                "SELECT r.predicate || ' ' || coalesce(o.name, r.object_literal) || ' ' || coalesce(r.valid_to, '')
                 FROM relations r JOIN entities e ON e.id = r.subject_id
                 LEFT JOIN entities o ON o.id = r.object_entity_id
                 WHERE e.name = ?1 ORDER BY r.valid_from, r.id",
            )
            .unwrap();
        stmt.query_map([subject], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    async fn entity_mentions_exist(&self) -> bool {
        self.count("mentions").await > 0
    }

    async fn meta(&self, key: &str) -> Option<String> {
        self.db()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
    }

    /// Events recorded as `actor action target_id`; the follower of the settings writes its own
    /// asynchronously, so a caller waits with `wait_event`.
    async fn events_of(&self, action: &str) -> Vec<String> {
        let conn = self.db();
        let mut stmt = conn
            .prepare("SELECT actor || ' ' || target_id || ' ' || coalesce(after_json, '') FROM events WHERE action = ?1")
            .unwrap();
        stmt.query_map([action], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    async fn wait_event(&self, action: &str) -> Vec<String> {
        let t = Instant::now();
        loop {
            let got = self.events_of(action).await;
            if !got.is_empty() {
                return got;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "no {action} event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
        // Absolute on every platform (a bare "/x" is not absolute on Windows).
        palace: Some(
            std::env::temp_dir()
                .join("alice-no-such-palace-7f3a")
                .to_string_lossy()
                .into_owned(),
        ),
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
    assert!(
        std::path::Path::new(&r.palace.path).ends_with("palace"),
        "{}",
        r.palace.path
    );
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

fn diary_spec(id: &str, text: &str, wing: &str, topic: &str) -> DrawerSpec {
    DrawerSpec {
        id: id.into(),
        text: text.into(),
        meta: DrawerMeta {
            wing: wing.into(),
            room: "diary".into(),
            agent: s("claude-code"),
            topic: s(topic),
            date: s("2026-04-16"),
            filed_at: s("2026-04-16T20:00:00"),
            ..Default::default()
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn diary_project_rules_show_in_the_import_report_and_checkpoints_follow_them_into_the_raw_tier()
 {
    let mut specs = vec![
        knowledge("k_dev", "devspace note", "devspace", "decisions"),
        knowledge("k_tsm", "tara note", "tara-stock-market", "decisions"),
        knowledge("k_tv", "valley note", "tara_valley", "decisions"),
        knowledge("k_test", "a test wing note", "test", "decisions"),
        knowledge("k_pg", "pg note", "pg_ai", "decisions"),
        // topic names a project (the wing is another project's, or an agent wing)
        diary_spec(
            "diary_1",
            "tara diary, the test suite passed",
            "tara_valley",
            "tara-stock-market",
        ),
        diary_spec(
            "diary_2",
            "devspace diary, test test test",
            "wing_claude_code",
            "devspace-chat-tabs",
        ),
        // agent wing ending in a project
        diary_spec("diary_3", "pg diary one", "wing_claude_code_pgai", "setup"),
        // text full of `test` and no other signal: global, never `test`
        diary_spec(
            "diary_4",
            "test test test, more test",
            "wing_claude_code",
            "deployment",
        ),
        // auto-save checkpoints: raw chunks, same project rules as other diaries
        diary_spec(
            "diary_5",
            "auto-save one",
            "wing_claude_code_pgai",
            "checkpoint",
        ),
        diary_spec("diary_6", "auto-save two", "wing_claude_code", "checkpoint"),
    ];
    specs.push(knowledge("k_x", "x note", "wing_claude_code", "knowledge"));
    let f = Flow::with_specs(&specs).await;
    // One-drawer wings are not trusted by the diary rules; naming them with --map-wing is.
    let request = || ImportRequest {
        map_wing: [
            ("devspace", "devspace"),
            ("tara-stock-market", "tara-stock-market"),
            ("pg_ai", "pg-ai"),
        ]
        .into_iter()
        .map(|(w, p)| (w.to_string(), p.to_string()))
        .collect(),
        ..f.request()
    };
    let st = f.run_import(request().dry_run(true)).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    // Per project: its own wing note, plus the diaries the rules send there.
    assert_eq!(r.projects["tara-stock-market"].memories, 2);
    assert_eq!(r.projects["tara-valley"].memories, 1);
    assert_eq!(r.projects["devspace"].memories, 2);
    // The checkpoints (diary_5, diary_6) are not memories: they are raw chunks of the same projects.
    assert_eq!(r.projects["pg-ai"].memories, 2);
    assert_eq!(r.projects["pg-ai"].chunks, 1);
    assert_eq!(
        r.projects["test"].memories, 1,
        "only the `test` wing's own drawer"
    );
    assert_eq!(r.projects["global"].memories, 2);
    assert_eq!(r.projects["global"].chunks, 1);
    assert_eq!(r.tiers.checkpoint_diaries, 2);

    let st = f.run_import(request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let row = |legacy: &str| -> (String, i64) {
        f.db()
            .query_row(
                "SELECT p.slug, m.importance FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.legacy_id = ?1",
                [legacy],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(row("diary_1"), ("tara-stock-market".to_string(), 2));
    assert_eq!(row("diary_4"), ("global".to_string(), 2));
    let chunk_project = |legacy: &str| -> String {
        f.db()
            .query_row(
                "SELECT p.slug FROM chunks c JOIN projects p ON p.id = c.project_id WHERE c.legacy_id = ?1",
                [legacy],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(chunk_project("diary_5"), "pg-ai");
    assert_eq!(chunk_project("diary_6"), "global");
    for id in ["diary_5", "diary_6"] {
        let as_memory: i64 = f
            .db()
            .query_row(
                "SELECT count(*) FROM memories WHERE legacy_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(as_memory, 0, "{id} is not a knowledge memory");
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
    f.wait_embedded().await;
    let mut before = f.table_counts().await;
    let again = f.run_import(f.request()).await.report.unwrap();
    // What a rerun writes: the tunnel links' batch and the marker's batch (an event each).
    *before.get_mut("events").unwrap() += 2;
    assert_eq!(f.table_counts().await, before);
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

/// After `alice projects merge`, an import run again names the old project (a wing) and the rows
/// land in the project it was merged into: no twin project, no rows under the old name. Drawers
/// that were imported before are not imported again.
#[tokio::test(flavor = "multi_thread")]
async fn a_rerun_after_a_project_merge_lands_in_the_project_it_was_merged_into() {
    let f = Flow::new().await;
    let first = f.run_import(f.request()).await;
    assert_eq!(first.state, ImportState::Done, "{:?}", first.error);
    assert_eq!(f.project_slugs().await, vec!["devspace", "global", "pg-ai"]);
    let memories = f.count("memories").await;
    let chunks = f.count("chunks").await;
    let pg = f.svc.projects().await.unwrap();
    let pg_id = pg.iter().find(|p| p.slug == "pg-ai").unwrap().id.clone();

    let merged = f
        .svc
        .merge_projects(alice_core::api::ProjectMergeRequest {
            from: "devspace".into(),
            into: "pg-ai".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(merged.merged && merged.chunks > 0, "{merged:?}");
    assert_eq!(f.project_slugs().await, vec!["global", "pg-ai"]);

    // Same palace: nothing new. A new drawer in the wing of the merged project, and one in a
    // wing of the target, both land in `pg-ai`.
    let second = f.run_import(f.request()).await.report.unwrap();
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert_eq!(
        f.project_slugs().await,
        vec!["global", "pg-ai"],
        "no devspace twin"
    );
    assert_eq!(
        (f.count("memories").await, f.count("chunks").await),
        (memories, chunks)
    );
    add_drawers(
        &f.palace,
        &[
            knowledge(
                "new_k",
                "devspace decision about the build cache",
                "devspace",
                "decisions",
            ),
            knowledge(
                "new_p",
                "pg-ai decision about the retry budget",
                "pg-ai",
                "decisions",
            ),
        ],
    );
    let third = f.run_import(f.request()).await.report.unwrap();
    assert!(third.errors.is_empty(), "{:?}", third.errors);
    assert_eq!(f.project_slugs().await, vec!["global", "pg-ai"]);
    for legacy in ["new_k", "new_p"] {
        let project: String = f
            .db()
            .query_row(
                "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.legacy_id = ?1",
                [legacy],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(project, "pg-ai", "{legacy}");
    }
    let in_target: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM chunks WHERE project_id = ?1",
            [&pg_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(in_target >= merged.chunks as i64, "{in_target}");
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
    let f = Flow::with_hook(&specs).await;
    let first = f.run_import(f.request()).await.report.unwrap();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let hook = f.hook.clone().unwrap();
    assert!(hook.planned.load(Ordering::SeqCst) > 100);
    hook.planned.store(0, Ordering::SeqCst);
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
    assert_eq!(
        hook.planned.load(Ordering::SeqCst),
        0,
        "stored drawers are not planned again"
    );
    assert_eq!(second.drawers_seen, second.drawers_accounted);
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
    let parts: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM memories WHERE legacy_id LIKE 'big_k%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(parts, 3);
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
    // The survey hands out positions that are free when it looks; a chunk stored by someone else
    // in between (parked here, right after the survey) takes one of them.
    let hook = Arc::new(ImportHook::default());
    hook.hold_planning.store(true, Ordering::SeqCst);
    let h = hook.clone();
    let mut specs = fixture_specs();
    let mut a = chunk(
        "coll_a",
        "a chunk to be turned away",
        "devspace",
        "/h/Code/devspace/collide.go",
        5,
    );
    a.meta.convos_off();
    specs.push(a);
    let f = Flow::with_options(&specs, move |_| ServiceOptions {
        import_hook: Some(h),
        ..Default::default()
    })
    .await;
    f.svc.start_import(f.request()).unwrap();
    let t = Instant::now();
    while !hook.parked.load(Ordering::SeqCst) {
        assert!(t.elapsed() < Duration::from_secs(30), "never parked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    f.svc
        .ingest(alice_core::api::IngestRequest {
            source: alice_core::store::sources::SourceInput {
                uri: "file:///h/Code/devspace/collide.go".into(),
                source_type: alice_core::model::SourceType::File,
                ..Default::default()
            },
            chunks: vec![alice_core::store::sources::ChunkInput {
                ord: 5,
                text: "someone else's chunk".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .unwrap();
    hook.hold_planning.store(false, Ordering::SeqCst);
    let st = f.wait().await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert!(r.errors[0].contains("coll_a"), "{:?}", r.errors);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_drawer_can_ask_for_the_position_a_piece_of_another_takes() {
    // Piece k of an oversize chunk sits at ord + k * 10,000,000: a drawer asking for that
    // position is numbered after the others instead.
    let mut specs = fixture_specs();
    let mut a = chunk(
        "coll_a",
        &"a long chunk of words. ".repeat(2_000),
        "devspace",
        "/h/Code/devspace/collide.go",
        5,
    );
    a.meta.convos_off();
    let mut b = chunk(
        "coll_b",
        "asks for the position a piece takes",
        "devspace",
        "/h/Code/devspace/collide.go",
        10_000_005,
    );
    b.meta.convos_off();
    specs.push(a);
    specs.push(b);
    let f = Flow::with_specs(&specs).await;
    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    for id in ["coll_a", "coll_a#2", "coll_b"] {
        assert!(f.legacy_target(id).await.is_some(), "{id}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drawer_with_a_blank_source_file_is_imported_without_an_error() {
    let mut specs = fixture_specs();
    // The owner's palace has drawers whose source_file is "" (and some that are only blanks): the
    // reader takes them as having no source, so they are knowledge and nothing is refused.
    for (id, blank) in [("blank_a", ""), ("blank_b", "   ")] {
        let mut d = chunk(
            id,
            &format!("text of {id}, a drawer with no source"),
            "devspace",
            blank,
            0,
        );
        d.meta.convos_off();
        specs.push(d);
    }
    let f = Flow::with_specs(&specs).await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    for id in ["blank_a", "blank_b"] {
        let t = f.legacy_target(id).await.unwrap_or_default();
        assert!(t.starts_with("memory:"), "{id}: {t:?}");
    }
    assert_eq!(f.count("chunks").await, 79 + 30, "no chunk was added");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drawer_the_store_refuses_is_an_error_entry_and_the_import_goes_on() {
    // The planner keeps every field inside the store's limits (it redacts, normalises and splits
    // first), so no palace data makes the store refuse a drawer: the test hook does.
    let mut specs = fixture_specs();
    let mut bad = chunk(
        "bad_chunk",
        "text of a chunk the store will refuse",
        "devspace",
        "/h/Code/devspace/bad.go",
        0,
    );
    bad.meta.convos_off();
    specs.push(bad);
    specs.push(knowledge(
        "bad_note",
        "a note the store will refuse",
        "devspace",
        "knowledge",
    ));
    let hook = Arc::new(ImportHook::default());
    hook.refuse
        .lock()
        .unwrap()
        .extend(["bad_chunk".to_string(), "bad_note".to_string()]);
    let h = hook.clone();
    let f = Flow::with_options(&specs, move |_| ServiceOptions {
        import_hook: Some(h),
        ..Default::default()
    })
    .await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!(r.errors.len(), 2, "{:?}", r.errors);
    for id in ["bad_chunk", "bad_note"] {
        assert!(
            r.errors.iter().any(|e| e.contains(id)),
            "{id}: {:?}",
            r.errors
        );
        assert!(f.legacy_target(id).await.is_none(), "{id}");
    }
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    assert_eq!(f.count("chunks").await, 79 + 30, "the others are stored");
    assert!(f.legacy_target("k_dec").await.is_some());
}

fn assert_clean(tables: Vec<String>, what: &str) {
    assert!(tables.is_empty(), "{what} found in {tables:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn key_shaped_wing_names_never_become_project_names() {
    let ghp = SECRET;
    let akia = concat!("AKIA", "IOSFODNN7EXAMPLE");
    let mut specs = fixture_specs();
    specs.push(knowledge(
        "k_ghp",
        "a note under a key-named wing",
        ghp,
        "knowledge",
    ));
    specs.push(knowledge(
        "k_akia",
        "a note under another",
        akia,
        "knowledge",
    ));
    // A chunk under a key-named wing too.
    let mut c = chunk(
        "c_akia",
        "chunk under a key-named wing",
        akia,
        "/h/Code/x/a.go",
        0,
    );
    c.meta.convos_off();
    specs.push(c);
    let f = Flow::with_specs(&specs).await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.clone().unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let status = serde_json::to_string(&st).unwrap() + &r.render_text();
    for needle in [
        ghp.to_string(),
        ghp.to_lowercase().replace('_', "-"),
        ghp.to_lowercase(),
        akia.to_string(),
        akia.to_lowercase(),
    ] {
        assert_clean(f.tables_holding(&needle).await, &needle);
        assert!(!status.contains(&needle), "{needle} in the report");
    }
    assert!(
        f.project_slugs()
            .await
            .iter()
            .any(|s| s.starts_with("redacted"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_import_leaves_whole_batches_and_a_rerun_finishes_it() {
    let f = Flow::with_specs(&big_specs(6_000)).await;
    f.svc.start_import(f.request()).unwrap();
    let t = Instant::now();
    while f.count("chunks").await == 0 {
        assert!(t.elapsed() < Duration::from_secs(60), "nothing was stored");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    f.svc.shutdown();
    let st = f.svc.import_status();
    assert_eq!(st.state, ImportState::Failed, "{st:?}");
    let e = st.error.unwrap();
    assert!(
        e.contains("daemon stopped") && e.contains("stored before it ended"),
        "{e}"
    );
    let stored = f.count("chunks").await;
    assert!(stored > 0 && stored < 6_000, "{stored}");
    assert_eq!(
        stored,
        f.db()
            .query_row("SELECT count(*) FROM legacy_ids", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        "every stored chunk has its old id: only whole batches were written"
    );
    assert_eq!(f.import_tmp_entries(), 0);

    let svc = start_service(&f.home);
    svc.start_import(f.request()).unwrap();
    let t = Instant::now();
    let st = loop {
        let st = svc.import_status();
        if st.state != ImportState::Running {
            break st;
        }
        assert!(t.elapsed() < Duration::from_secs(120));
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!(r.already_imported as i64, stored);
    assert_eq!(f.count("chunks").await, 6_000);
    svc.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_of_a_chunk_that_block_mode_held_back_is_left_for_the_next_run() {
    let mut specs = fixture_specs();
    let text = format!("a chunk that holds {SECRET}");
    specs.push(chunk("blk_a", &text, "sessions", S2, 50));
    specs.push(chunk("blk_b", &text, "sessions", S2, 51));
    let f = Flow::with_specs(&specs).await;
    f.patch("privacy.redact_mode", serde_json::json!("block"))
        .await;
    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.blocked_by_redaction["github_token"], 1);
    assert_eq!(r.skipped["repeat of a chunk that was not stored"], 1);
    assert_eq!(r.duplicates.raw, 1, "only the fixture's own repeat");
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    assert!(
        r.warnings.iter().any(|w| w.contains("next run")),
        "{:?}",
        r.warnings
    );
    assert!(f.legacy_target("blk_b").await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rerun_numbers_new_chunks_below_the_pieces_an_earlier_oversize_chunk_took() {
    const SRC: &str = "/h/Code/devspace/over.go";
    let long = |id: &str, index: Option<i64>| {
        let mut d = chunk(
            id,
            &format!("{id} long words. ").repeat(2_500),
            "devspace",
            SRC,
            0,
        );
        d.meta.chunk_index = index;
        d.meta.convos_off();
        d
    };
    let f = Flow::with_specs(&[long("ov1", Some(5))]).await;
    let first = f.run_import(f.request()).await.report.unwrap();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(first.split.chunks, 1);

    // Run 2: an unnumbered chunk, and a second oversize chunk asking for the next position.
    let mut n1 = chunk("n1", "a chunk with no position", "devspace", SRC, 0);
    n1.meta.chunk_index = None;
    n1.meta.convos_off();
    add_drawers(&f.palace, &[n1, long("ov2", Some(6))]);
    let second = f.run_import(f.request()).await.report.unwrap();
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    let ord = |id: &str| -> i64 {
        f.db()
            .query_row("SELECT ord FROM chunks WHERE legacy_id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert_eq!(ord("ov1"), 5);
    assert_eq!(ord("ov1#2"), 10_000_005);
    assert_eq!(ord("ov2"), 6);
    assert_eq!(ord("ov2#2"), 10_000_006);
    assert_eq!(
        ord("n1"),
        8,
        "numbered below the pieces (7 went to the already stored ov1, whose position 5 was taken)"
    );
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

// ------------------------------------------------------------------- graph, tunnels, accounting

#[tokio::test(flavor = "multi_thread")]
async fn the_whole_fixture_imports_with_an_honest_report_and_the_accounting_law_holds() {
    let f = Flow::new().await;
    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        r.drawers_seen, r.drawers_accounted,
        "every drawer is imported, collapsed, skipped, blocked or already imported"
    );
    assert_eq!(f.facts_timeline("alice").await.len(), 4); // the ended fact is included
    assert_eq!(r.facts.ended, 1);
    assert_eq!(r.facts.added, 4);
    assert_eq!(r.review_items, 1); // has_bug / had_bug, M2 payload {predicate, canonical, p, provider}
    assert_eq!(r.isolated_entities, 1);
    assert_eq!((r.tunnels.linked, r.tunnels.dropped), (1, 1));
    assert_eq!(r.tunnels.ignored_topic, 0);
    assert!(f.entity_mentions_exist().await); // mention links made in the last phase
    assert!(
        f.meta("import.mempalace.last")
            .await
            .unwrap()
            .contains("\"ok\":true")
    );
    let phases: Vec<&str> = r.phases.iter().map(|p| p.phase.as_str()).collect();
    for want in ["graph", "collapsed", "mentions"] {
        assert!(phases.contains(&want), "{phases:?}");
    }
    // The review payload is the M2 shape, with the direction flag.
    let payload: String = f
        .db()
        .query_row("SELECT payload_json FROM review_items", [], |r| r.get(0))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
    for k in ["predicate", "canonical", "p", "provider", "reversed"] {
        assert!(v.get(k).is_some(), "{k} in {payload}");
    }
    let key: String = f
        .db()
        .query_row("SELECT dedupe_key FROM review_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        key,
        format!("predicate:{}", v["predicate"].as_str().unwrap())
    );
    // The tunnel linked the two knowledge memories; the marker names no text.
    assert_eq!(f.count("links").await, 1);
    let marker = f.meta("import.mempalace.last").await.unwrap();
    assert!(
        marker.contains(&r.job_id) && marker.contains("\"drawers_seen\":117"),
        "{marker}"
    );

    // A rerun finds everything stored: the law still holds, nothing is added twice.
    f.wait_embedded().await;
    let again = f.run_import(f.request()).await.report.unwrap();
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert_eq!(again.drawers_seen, again.drawers_accounted);
    assert_eq!(
        (again.facts.added, again.facts.ended, again.facts.skipped),
        (0, 0, 4)
    );
    assert_eq!(again.review_items, 0);
    assert_eq!(f.facts_timeline("alice").await.len(), 4);
    assert_eq!(f.count("links").await, 1);
    assert_eq!(f.count("review_items").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_counts_the_graph_without_writing_it() {
    let f = Flow::new().await;
    let before = f.table_counts().await;
    let r = f
        .run_import(f.request().dry_run(true))
        .await
        .report
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!((r.facts.added, r.facts.ended, r.facts.skipped), (4, 1, 0));
    assert_eq!(r.review_items, 1);
    assert_eq!(r.isolated_entities, 1);
    assert_eq!((r.tunnels.linked, r.tunnels.dropped), (1, 1));
    assert_eq!(f.table_counts().await, before);
    assert!(f.meta("import.mempalace.last").await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn compat_turns_on_by_itself_once_and_respects_an_explicit_off() {
    let f = Flow::new().await;
    assert!(!f.setting_bool("compat.mempalace").await);
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    assert!(f.setting_bool("compat.mempalace").await);
    let report = st.report.as_ref().unwrap();
    assert!(
        report.compat_enabled
            && report
                .render_text()
                .contains("compatibility layer turned on")
    );
    let changed = f.wait_event("settings.change").await;
    assert!(
        changed.iter().any(|e| e.contains("compat.mempalace")),
        "{changed:?}"
    );
    let enabled = f.wait_event("import.compat_enabled").await;
    assert_eq!(enabled.len(), 1);
    assert!(
        enabled[0].starts_with("import compat.mempalace"),
        "{enabled:?}"
    );
    // A second run does not do it again.
    f.run_import(f.request()).await;
    assert_eq!(f.events_of("import.compat_enabled").await.len(), 1);

    // An owner who wrote `false` keeps it.
    let g = Flow::new().await;
    g.patch("compat.mempalace", serde_json::json!(false)).await;
    let st = g.run_import(g.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    assert!(!g.setting_bool("compat.mempalace").await);
    assert!(g.events_of("import.compat_enabled").await.is_empty());
    assert!(!st.report.unwrap().compat_enabled);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_in_a_kg_object_or_a_transcript_path_never_reaches_the_database() {
    let secret = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let leak = TripleSpec {
        id: "t_leak".into(),
        subject: "alice".into(),
        predicate: "keeps".into(),
        object: format!("key {secret}"),
        valid_from: s("2026-01-01"),
        ..Default::default()
    };
    let leak_id = TripleSpec {
        id: format!("t_{secret}"),
        subject: "alice".into(),
        predicate: "uses".into(),
        object: "sqlite".into(),
        valid_from: s("2026-01-02"),
        ..Default::default()
    };
    // Redacted: the fact is kept without the secret.
    let f = Flow::with_graph(&fixture_specs(), &[leak.clone(), leak_id.clone()], |_| {
        ServiceOptions::default()
    })
    .await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.facts.added, 6);
    assert!(r.redacted_fields >= 1);
    for needle in [secret, &secret[..14]] {
        assert_eq!(f.tables_holding(needle).await, Vec::<String>::new());
    }
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains(&secret[..14]) && !r.render_text().contains(&secret[..14]));
    assert!(
        f.facts_timeline("alice")
            .await
            .iter()
            .any(|x| x.contains("REDACTED"))
    );

    // Block mode: only those triples are held back, by name of detector.
    let g = Flow::with_graph(&fixture_specs(), &[leak, leak_id], |_| {
        ServiceOptions::default()
    })
    .await;
    g.patch("privacy.redact_mode", serde_json::json!("block"))
        .await;
    let r = g.run_import(g.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    // The triple whose id holds the key is fine: the id is never stored, only its hash.
    assert_eq!(r.facts.added, 5);
    assert_eq!(r.blocked_by_redaction["github_token"], 1);
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    for needle in [secret, &secret[..14]] {
        assert_eq!(g.tables_holding(needle).await, Vec::<String>::new());
    }
}

/// Parks the job right before its mentions phase (everything else is stored by then).
async fn park_before_mentions(f: &Flow) -> Arc<ImportHook> {
    let hook = f.hook.clone().unwrap();
    hook.hold_mentions.store(true, Ordering::SeqCst);
    f.svc.start_import(f.request()).unwrap();
    let t = Instant::now();
    while !hook.parked_mentions.load(Ordering::SeqCst) {
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the job never got there"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    hook
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_database_mid_import_fails_with_the_resume_sentence() {
    let f = Flow::with_hook(&fixture_specs()).await;
    let hook = park_before_mentions(&f).await;
    f.svc.force_read_only("the disk is full");
    hook.hold_mentions.store(false, Ordering::SeqCst);
    let st = f.wait().await;
    assert_eq!(st.state, ImportState::Failed, "{st:?}");
    let e = st.error.unwrap();
    assert!(
        e.contains("run `alice import mempalace` again") && e.contains("what was imported is kept"),
        "{e}"
    );
    assert_eq!(f.import_tmp_entries(), 0);
    assert_eq!(f.count("chunks").await, 109);
    assert_eq!(f.count("memories").await, 4);
    assert_eq!(f.count("relations").await, 4, "the graph stays");
    assert!(f.meta("import.mempalace.last").await.is_none());
    assert!(
        !f.setting_bool("compat.mempalace").await,
        "a failed run does not opt in"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_stops_before_the_mentions_is_completed_by_the_next_one() {
    let f = Flow::with_hook(&fixture_specs()).await;
    park_before_mentions(&f).await;
    let batches_at_the_stop = f.svc.import_status().batches;
    f.svc.shutdown();
    let st = f.svc.import_status();
    assert_eq!(st.state, ImportState::Failed, "{st:?}");
    // The job sent no mention batch after the stop. (`mentions` rows themselves cannot be the
    // check: the judge worker, which runs beside the job, links the mentions of a memory it
    // activates, and did so in about 1 of 25 runs on a loaded machine.)
    assert_eq!(
        st.batches, batches_at_the_stop,
        "a batch went out after the stop"
    );
    assert_eq!(f.count("memories").await, 4);

    let svc = start_service(&f.home);
    let events_before = f.events_of("import.batch").await.len();
    svc.start_import(f.request()).unwrap();
    let t = Instant::now();
    let st = loop {
        let st = svc.import_status();
        if st.state != ImportState::Running {
            break st;
        }
        assert!(t.elapsed() < Duration::from_secs(120));
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!(r.already_imported, 5 + 80 + 30, "nothing was planned again");
    // The rerun's batches: the closing one and the marker (as in a finished palace) plus at most
    // one mention batch (the embed/judge workers, which run beside the job, may have linked the
    // last memory first, so the batch cannot be required). What the rerun guarantees is that
    // nothing imported is left unlinked.
    let sent = f.events_of("import.batch").await.len() - events_before;
    assert!((2..=3).contains(&sent), "the rerun sent {sent} batches");
    let unlinked: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM memories m WHERE m.legacy_id IS NOT NULL AND EXISTS (
                 SELECT 1 FROM entities e WHERE instr(lower(m.title || char(10) || m.body), lower(e.name)) > 0
                   AND NOT EXISTS (SELECT 1 FROM mentions x WHERE x.memory_id = m.id AND x.entity_id = e.id))",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unlinked, 0, "memories left unlinked after the rerun");
    assert!(f.entity_mentions_exist().await);
    assert!(
        f.meta("import.mempalace.last")
            .await
            .unwrap()
            .contains("\"ok\":true")
    );
    // Once linked, a further run sends no mention batch.
    let events = f.events_of("import.batch").await.len();
    svc.start_import(f.request()).unwrap();
    let t = Instant::now();
    while svc.import_status().state == ImportState::Running {
        assert!(t.elapsed() < Duration::from_secs(120));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(f.events_of("import.batch").await.len(), events + 2);
    svc.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rerun_makes_the_same_predicate_review_keys_as_the_first_run() {
    // Alice has `uses` twice, the graph has `use` twice: a tie that Alice's own predicate wins.
    // Counting the imported facts again on the rerun would make `use` (4) the canonical one.
    let extra = |id: &str, o: &str| TripleSpec {
        id: id.into(),
        subject: "alice".into(),
        predicate: "use".into(),
        object: o.into(),
        valid_from: s("2026-01-03"),
        ..Default::default()
    };
    let f = Flow::with_graph(
        &fixture_specs(),
        &[extra("t_u1", "u1"), extra("t_u2", "u2")],
        |_| ServiceOptions::default(),
    )
    .await;
    for o in ["x1", "x2"] {
        f.svc
            .assert_fact(alice_core::api::AssertFactRequest {
                subject: "alice".into(),
                predicate: "uses".into(),
                object: o.into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let keys = |f: &Flow| -> Vec<String> {
        let conn = f.db();
        let mut st = conn
            .prepare("SELECT dedupe_key FROM review_items ORDER BY 1")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let first = f.run_import(f.request()).await.report.unwrap();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let before = keys(&f);
    assert!(before.contains(&"predicate:use".to_string()), "{before:?}");
    f.wait_embedded().await;
    let again = f.run_import(f.request()).await.report.unwrap();
    assert_eq!(again.review_items, 0);
    assert_eq!(keys(&f), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_in_a_subject_or_a_predicate_never_reaches_reviews_warnings_or_events() {
    let secret = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let t = |id: &str, s_: &str, p: &str| TripleSpec {
        id: id.into(),
        subject: s_.into(),
        predicate: p.into(),
        object: "sqlite".into(),
        valid_from: s("2026-01-01"),
        ..Default::default()
    };
    let extra = [
        t("t_s", &format!("bob {secret}"), "knows"),
        t("t_p", "alice", &format!("has_bugs {secret}")),
    ];
    for block in [false, true] {
        let f = Flow::with_graph(&fixture_specs(), &extra, |_| ServiceOptions::default()).await;
        if block {
            f.patch("privacy.redact_mode", serde_json::json!("block"))
                .await;
        }
        let st = f.run_import(f.request()).await;
        assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
        let r = st.report.unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        if block {
            assert_eq!(r.blocked_by_redaction["github_token"], 2);
            assert_eq!(r.facts.added, 4);
        } else {
            assert_eq!(r.facts.added, 6);
        }
        let json = serde_json::to_string(&r).unwrap();
        for needle in [secret, &secret[..14]] {
            assert!(!json.contains(needle) && !r.render_text().contains(needle));
            assert_eq!(f.tables_holding(needle).await, Vec::<String>::new());
        }
        f.wait_event("import.batch").await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_before_the_first_batch_writes_nothing() {
    let f = Flow::new().await;
    let before = f.table_counts().await;
    f.svc.force_read_only("the disk is full");
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Failed, "{st:?}");
    let e = st.error.unwrap();
    assert!(
        e.contains("read-only") && !e.contains("again to continue"),
        "{e}"
    );
    assert_eq!(f.table_counts().await, before);
    assert_eq!(f.import_tmp_entries(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn re_review_items_resolved_by_the_owner_come_back_zero_times() {
    let f = Flow::new().await;
    let r = f.run_import(f.request()).await.report.unwrap();
    assert_eq!(r.review_items, 1);
    let open = f
        .svc
        .reviews(alice_core::store::reviews::ReviewQuery::default())
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    f.svc
        .resolve_review(alice_core::api::ResolveRequest {
            id: open[0].id.clone(),
            choice: "keep".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    f.wait_embedded().await;
    let again = f.run_import(f.request()).await.report.unwrap();
    assert_eq!(again.review_items, 0);
    let still_open = f
        .svc
        .reviews(alice_core::store::reviews::ReviewQuery::default())
        .await
        .unwrap();
    assert!(still_open.is_empty(), "{}", still_open.len());
}

// ------------------------------------------------------- fragments and checkpoints (follow-up 2)

/// A diary drawer, or one fragment of it when `fragment` is given.
fn diary_part(
    base: &str,
    fragment: Option<i64>,
    text: &str,
    wing: &str,
    topic: &str,
    filed: &str,
) -> DrawerSpec {
    let id = match fragment {
        Some(n) => format!("{base}_chunk_{n:06}"),
        None => base.to_string(),
    };
    DrawerSpec {
        id,
        text: text.into(),
        meta: DrawerMeta {
            wing: wing.into(),
            room: "diary".into(),
            agent: s("claude-code"),
            topic: s(topic),
            date: s(&filed[..10]),
            filed_at: s(filed),
            chunk_index: fragment,
            ..Default::default()
        },
    }
}

const FRAGMENTED: &str = "diary_wing_claude_code_20260416_aa";

fn fragmented_specs() -> Vec<DrawerSpec> {
    let w = "wing_claude_code";
    vec![
        // A diary cut into three fragments (the middle one filed first), at a fixed size.
        diary_part(
            FRAGMENTED,
            Some(0),
            "Reworked the sched",
            w,
            "scheduler",
            "2026-04-16T11:00:00",
        ),
        diary_part(
            FRAGMENTED,
            Some(1),
            "uler; marmot-cobalt-4417 is the ",
            w,
            "scheduler",
            "2026-04-16T10:30:00",
        ),
        diary_part(
            FRAGMENTED,
            Some(2),
            "marker. Done.",
            w,
            "retro",
            "2026-04-16T12:00:00",
        ),
        // A diary that was never cut, and one with a single fragment.
        diary_part(
            "diary_wing_claude_code_20260416_bb",
            None,
            "A short diary.",
            w,
            "short",
            "2026-04-16T13:00:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_cc",
            Some(0),
            "Alone in its drawer.",
            w,
            "solo",
            "2026-04-16T14:00:00",
        ),
    ]
}

async fn memory_count(f: &Flow) -> i64 {
    f.count("memories").await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fragments_of_a_diary_drawer_import_as_one_memory_and_a_rerun_imports_nothing() {
    let f = Flow::with_hook(&fragmented_specs()).await;
    let dry = f
        .run_import(f.request().dry_run(true))
        .await
        .report
        .unwrap();
    assert_eq!((dry.fragments_joined, dry.fragment_groups), (2, 1));
    assert_eq!(memory_count(&f).await, 0, "a dry run stores nothing");

    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!((r.fragments_joined, r.fragment_groups), (2, 1));
    assert_eq!(
        r.drawers_seen, 6,
        "five drawers and the fixture's unreadable one"
    );
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    assert_eq!(r.tiers.knowledge, 5);
    // Three memories: the joined diary, the one that was never cut, the single fragment.
    assert_eq!(memory_count(&f).await, 3);
    let (body, created, title): (String, String, String) = f
        .db()
        .query_row(
            "SELECT body, created_at, title FROM memories WHERE legacy_id = ?1",
            [format!("{FRAGMENTED}_chunk_000000")],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        body,
        "Reworked the scheduler; marmot-cobalt-4417 is the marker. Done."
    );
    assert!(
        created.starts_with("2026-04-16T10:30:00") || created.contains("T03:30:00"),
        "{created}"
    );
    assert_eq!(title, "Diary 2026-04-16: scheduler");
    let topics: Vec<String> = f
        .db()
        .prepare("SELECT topic FROM memory_topics t JOIN memories m ON m.id = t.memory_id WHERE m.legacy_id = ?1 ORDER BY topic")
        .unwrap()
        .query_map([format!("{FRAGMENTED}_chunk_000000")], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(topics, vec!["retro", "scheduler"]);
    // Every fragment id leads to the one memory.
    let head = f.legacy_target(&format!("{FRAGMENTED}_chunk_000000")).await;
    assert!(head.as_deref().is_some_and(|t| t.starts_with("memory:")));
    for n in 1..3 {
        assert_eq!(
            f.legacy_target(&format!("{FRAGMENTED}_chunk_{n:06}")).await,
            head
        );
    }
    // Single drawers are as they always were.
    assert!(
        f.legacy_target("diary_wing_claude_code_20260416_bb")
            .await
            .is_some()
    );
    assert!(
        f.legacy_target("diary_wing_claude_code_20260416_cc_chunk_000000")
            .await
            .is_some()
    );
    assert!(
        f.recall("marmot-cobalt-4417", false)
            .await
            .contains("marmot-cobalt-4417")
    );

    // A rerun plans nothing and stores nothing.
    let planned = f.hook.as_ref().unwrap().planned.load(Ordering::Relaxed);
    let again = f.run_import(f.request()).await.report.unwrap();
    assert_eq!(
        f.hook.as_ref().unwrap().planned.load(Ordering::Relaxed),
        planned
    );
    assert_eq!(again.already_imported, 5);
    assert_eq!((again.fragments_joined, again.fragment_groups), (0, 0));
    assert_eq!(again.drawers_seen, again.drawers_accounted);
    assert_eq!(memory_count(&f).await, 3);
    assert!(
        again.warnings.iter().all(|w| !w.contains("fragments")),
        "{:?}",
        again.warnings
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drawer_with_only_some_of_its_fragment_ids_stored_is_left_alone_and_said_so() {
    for lost in [
        // The tail ids are missing (a run died, or the ids were never filed) ...
        vec![
            format!("{FRAGMENTED}_chunk_000001"),
            format!("{FRAGMENTED}_chunk_000002"),
        ],
        // ... or the first one is (a block-mode hold of the first piece in an earlier version).
        vec![format!("{FRAGMENTED}_chunk_000000")],
    ] {
        let f = Flow::with_hook(&fragmented_specs()).await;
        f.run_import(f.request()).await.report.unwrap();
        for id in &lost {
            f.db()
                .execute("DELETE FROM legacy_ids WHERE legacy_id = ?1", [id])
                .unwrap();
        }
        let before = memory_count(&f).await;
        let planned = f.hook.as_ref().unwrap().planned.load(Ordering::Relaxed);
        let r = f.run_import(f.request()).await.report.unwrap();
        assert_eq!(
            memory_count(&f).await,
            before,
            "no second copy of its text: {lost:?}"
        );
        assert_eq!(
            f.hook.as_ref().unwrap().planned.load(Ordering::Relaxed),
            planned
        );
        assert_eq!(r.already_imported, 5, "{lost:?}");
        assert_eq!(r.drawers_seen, r.drawers_accounted);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("only partly in Alice")),
            "{:?}",
            r.warnings
        );
    }
}

fn checkpoint_specs() -> Vec<DrawerSpec> {
    let w = "wing_claude_code";
    vec![
        knowledge(
            "k_note",
            "devspace note about the scheduler",
            "devspace",
            "decisions",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_c1",
            None,
            "AUTO-SAVE whole quokka-saffron-9021 state",
            w,
            "checkpoint",
            "2026-04-16T09:00:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_c2",
            Some(0),
            "AUTO-SAVE joined pelican-",
            w,
            "Checkpoint ",
            "2026-04-16T09:30:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_c2",
            Some(1),
            "umber-3310 state",
            w,
            "Checkpoint ",
            "2026-04-16T09:30:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260417_c3",
            None,
            "AUTO-SAVE next day heron-teal-5512",
            w,
            "checkpoint",
            "2026-04-17T09:00:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_d1",
            None,
            "A real diary about the roadmap",
            w,
            "roadmap",
            "2026-04-16T15:00:00",
        ),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_diaries_land_in_the_raw_tier_and_never_in_knowledge_hits_or_the_briefing() {
    let f = Flow::with_hook(&checkpoint_specs()).await;
    let dry = f
        .run_import(f.request().dry_run(true))
        .await
        .report
        .unwrap();
    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    for rep in [&dry, &r] {
        assert_eq!(rep.tiers.knowledge, 2, "the note and the real diary");
        assert_eq!(
            rep.tiers.checkpoint_diaries, 4,
            "three drawers, one in two fragments"
        );
        assert_eq!(
            rep.tiers.checkpoint_sources, 2,
            "one source per wing and day"
        );
        assert_eq!((rep.fragments_joined, rep.fragment_groups), (1, 1));
        assert_eq!(rep.tiers.raw_session_transcripts, 0);
        assert_eq!(rep.drawers_seen, rep.drawers_accounted);
    }
    // Memories: the note and the real diary only.
    assert_eq!(memory_count(&f).await, 2);
    let titles: Vec<String> = f.distinct("memories", "title").await;
    assert!(
        titles
            .iter()
            .all(|t| !t.to_lowercase().contains("checkpoint")),
        "{titles:?}"
    );
    // Chunks: three, in two sources, filed as sessions of the checkpoint agent.
    assert_eq!(f.count("chunks").await, 3);
    let sources: Vec<(String, String, Option<String>)> = f
        .db()
        .prepare("SELECT uri, type, agent FROM sources ORDER BY uri")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        sources,
        vec![
            (
                "mempalace-checkpoint:wing_claude_code:2026-04-16".into(),
                "session".into(),
                Some("mempalace checkpoint".into())
            ),
            (
                "mempalace-checkpoint:wing_claude_code:2026-04-17".into(),
                "session".into(),
                Some("mempalace checkpoint".into())
            ),
        ]
    );
    // The joined checkpoint is one chunk with the whole text; its second fragment id leads there.
    let joined: String = f
        .db()
        .query_row("SELECT text FROM chunks WHERE legacy_id = 'diary_wing_claude_code_20260416_c2_chunk_000000'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(joined, "AUTO-SAVE joined pelican-umber-3310 state");
    assert_eq!(
        f.legacy_target("diary_wing_claude_code_20260416_c2_chunk_000001")
            .await,
        f.legacy_target("diary_wing_claude_code_20260416_c2_chunk_000000")
            .await
    );
    assert!(
        f.legacy_target("diary_wing_claude_code_20260416_c2_chunk_000001")
            .await
            .unwrap()
            .starts_with("chunk:")
    );
    // Stable positions inside a source: filing order.
    let ords: Vec<i64> = f
        .db()
        .prepare("SELECT c.ord FROM chunks c JOIN sources s ON s.id = c.source_id WHERE s.uri LIKE '%2026-04-16' ORDER BY c.ord")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ords, vec![0, 1]);

    // Raw recall finds them; knowledge-only recall and the briefing never show them.
    for (marker, text) in [
        (
            "quokka-saffron-9021",
            "AUTO-SAVE whole quokka-saffron-9021 state",
        ),
        (
            "pelican-umber-3310",
            "AUTO-SAVE joined pelican-umber-3310 state",
        ),
        ("heron-teal-5512", "AUTO-SAVE next day heron-teal-5512"),
    ] {
        // Raw recall shows the chunk itself (as a raw hit); the answer text does not echo the query.
        let raw = f.recall(marker, true).await;
        assert!(
            raw.contains("\u{b7} raw \u{b7}") && raw.contains(text),
            "{marker} is found with raw recall: {raw}"
        );
        // Without `include_raw` the knowledge hits are the `results`; the checkpoint is no memory.
        // (The answer text may still add raw excerpts when few memories match: that is the
        // designed fallback, and it is raw, not knowledge.)
        let knowledge = f
            .svc
            .recall(alice_core::api::RecallRequest {
                query: marker.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            knowledge.results.iter().all(|h| {
                let all = format!(
                    "{} {} {}",
                    h.title,
                    h.snippet,
                    h.body.clone().unwrap_or_default()
                );
                !all.contains("AUTO-SAVE")
            }),
            "{marker} leaked into knowledge hits"
        );
        let diaries = f
            .svc
            .recall(alice_core::api::RecallRequest {
                query: marker.to_string(),
                kinds: vec![alice_core::model::Kind::Diary],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            diaries.results.is_empty(),
            "{marker}: no diary memory holds it"
        );
    }
    for project in [
        None,
        Some("global".to_string()),
        Some("devspace".to_string()),
    ] {
        let ctx = f
            .svc
            .context(alice_core::api::ContextRequest {
                project,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!ctx.text.contains("AUTO-SAVE"), "{}", ctx.text);
    }

    // A rerun stores nothing more.
    let planned = f.hook.as_ref().unwrap().planned.load(Ordering::Relaxed);
    let chunks = f.count("chunks").await;
    let again = f.run_import(f.request()).await.report.unwrap();
    assert_eq!(
        f.hook.as_ref().unwrap().planned.load(Ordering::Relaxed),
        planned
    );
    assert_eq!(f.count("chunks").await, chunks);
    assert_eq!(again.already_imported, 6);
    assert_eq!(again.drawers_seen, again.drawers_accounted);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_cut_by_a_fragment_boundary_never_reaches_the_database() {
    let token = concat!("gh", "p_", "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let (a, b) = token.split_at(2);
    let w = "wing_claude_code";
    let specs = vec![
        diary_part(
            "diary_wing_claude_code_20260416_s1",
            Some(0),
            &format!("token {a}"),
            w,
            "keys",
            "2026-04-16T11:00:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_s1",
            Some(1),
            &format!("{b} end"),
            w,
            "keys",
            "2026-04-16T11:00:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_s2",
            Some(0),
            &format!("auto-save {a}"),
            w,
            "checkpoint",
            "2026-04-16T11:00:00",
        ),
        diary_part(
            "diary_wing_claude_code_20260416_s2",
            Some(1),
            &format!("{b} end"),
            w,
            "checkpoint",
            "2026-04-16T11:00:00",
        ),
    ];
    let f = Flow::with_specs(&specs).await;
    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    assert!(r.redacted_fields >= 2, "{}", r.redacted_fields);
    for needle in [token, &token[..14]] {
        assert_eq!(f.tables_holding(needle).await, Vec::<String>::new());
    }
}
