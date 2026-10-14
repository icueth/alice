//! The MemPalace compatibility tools, called as plain functions of the service (no MCP, no
//! HTTP). `Flow::imported()` imports the synthetic palace of the import tests (same drawers,
//! including the knowledge duplicate pair `k_dup_a`/`k_dup_b`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alice_core::api::GetRequest;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::import::fixture::build_sqlite_exact;
use alice_core::import::{ImportRequest, ImportState};
use alice_core::model::{Kind, Memory, Status};
use alice_daemon::compat::CompatCall;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use serde_json::{Value, json};

use alice_daemon::client::Launcher;
use alice_daemon::discovery::{DaemonInfo, write_info};
use alice_daemon::mcp::{AliceMcp, Backend, RemoteBackend};
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio_util::sync::CancellationToken;

// ------------------------------------------------------------------------------------ fixtures

#[path = "common/palace.rs"]
mod palace;
use palace::fixture_specs;

struct Flow {
    _root: tempfile::TempDir,
    _palace_root: tempfile::TempDir,
    _palace: PathBuf,
    svc: Arc<Service>,
}

impl Flow {
    /// A service whose store holds the imported fixture palace.
    async fn imported() -> Flow {
        let root = tempfile::tempdir().unwrap();
        let palace_root = tempfile::tempdir().unwrap();
        let palace = palace_root.path().join("palace");
        std::fs::create_dir_all(&palace).unwrap();
        build_sqlite_exact(&palace, &fixture_specs());
        let home = AliceHome::new(root.path().join("alice"));
        let svc = Service::start(
            home,
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap();
        svc.start_import(ImportRequest {
            palace: Some(palace.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        let t = Instant::now();
        loop {
            let st = svc.import_status();
            if st.state != ImportState::Running {
                assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(120), "import did not end");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Raw chunks are searchable by meaning only once the embedding worker has done them; a
        // search made before that sees the chunks embedded so far (a loaded machine showed s2's
        // and not s1's).
        let t = Instant::now();
        while svc.health().await.unwrap().embed_backlog > 0 {
            assert!(
                t.elapsed() < Duration::from_secs(120),
                "embedding did not drain"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Flow {
            _root: root,
            _palace_root: palace_root,
            _palace: palace,
            svc,
        }
    }

    async fn memory(&self, id: &str) -> Memory {
        self.svc
            .get(GetRequest {
                ids: vec![id.to_string()],
                include_provenance: false,
            })
            .await
            .unwrap()
            .memories
            .remove(0)
            .memory
    }

    async fn is_forgotten(&self, id: &str) -> bool {
        self.memory(id).await.status == Status::Trashed
    }

    /// The actor of the newest event of an action.
    fn last_actor(&self, action: &str) -> String {
        let c = rusqlite::Connection::open(self.svc.home().db()).unwrap();
        c.query_row(
            "SELECT actor FROM events WHERE action = ?1 ORDER BY ts DESC, rowid DESC LIMIT 1",
            [action],
            |r| r.get(0),
        )
        .unwrap()
    }

    async fn call(&self, tool: &str, arguments: Value) -> Value {
        self.call_with(tool, arguments, None).await
    }

    async fn call_with(&self, tool: &str, arguments: Value, cwd: Option<&str>) -> Value {
        self.svc
            .compat_call(CompatCall {
                tool: tool.into(),
                arguments,
                cwd: cwd.map(String::from),
                agent: None,
            })
            .await
            .expect("a compat call never fails at the protocol level")
            .value
    }
}

// ------------------------------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread")]
async fn old_wing_names_resolve_through_aliases() {
    let f = Flow::imported().await;
    let r = f
        .call(
            "mempalace_search",
            json!({"query": "v4 frame", "wing": "wing_pg_ai"}),
        )
        .await;
    assert!(
        r["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["text"].as_str().unwrap().contains("v4 frame")),
        "{r}"
    );
    assert_eq!(
        (
            r["results"][0]["wing"].as_str(),
            r["results"][0]["similarity"].as_f64()
        ),
        (Some("pg-ai"), Some(1.0))
    );
    let w = f.call("mempalace_list_wings", json!({})).await;
    assert!(
        w["wings"]["pg-ai"].as_u64().unwrap() > 0 && w["wings"].get("wing_pg_ai").is_none(),
        "{w}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_fills_room_and_source_file_for_knowledge_and_raw_hits() {
    let f = Flow::imported().await;
    let r = f
        .call(
            "mempalace_search",
            json!({"query": "decision about the v4 frame"}),
        )
        .await;
    let k = r["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["text"].as_str().unwrap().contains("v4 frame"))
        .unwrap_or_else(|| panic!("{r}"));
    assert_eq!(
        (k["room"].as_str(), k["source_file"].as_str()),
        (Some("decisions"), Some(""))
    );
    assert_eq!(
        k["drawer_id"], "k_dec",
        "an imported memory answers with its old id"
    );
    for key in ["created_at", "matched", "similarity", "text", "wing"] {
        assert!(k.get(key).is_some(), "{key}");
    }
    // MemPalace's own top-level keys are all there.
    for key in ["query", "filters", "total_before_filter", "results"] {
        assert!(r.get(key).is_some(), "{key} in {r}");
    }

    let raw = f
        .call(
            "mempalace_search",
            json!({"query": "quickstart transcript phrase"}),
        )
        .await;
    let hit = raw["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["text"].as_str().unwrap().contains("quickstart"))
        .unwrap_or_else(|| panic!("{raw}"));
    assert!(
        hit["source_file"].as_str().unwrap().ends_with("s2.jsonl"),
        "{hit}"
    );
    assert_eq!(hit["room"], "");

    let s1 = f
        .call("mempalace_search", json!({"query": "s1 chunk 7 text"}))
        .await;
    assert!(
        s1["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["source_file"].as_str().unwrap().ends_with("s1.jsonl")),
        "{s1}"
    );

    let roomed = f
        .call(
            "mempalace_search",
            json!({"query": "decision about the v4 frame", "room": "decisions"}),
        )
        .await;
    let rows = roomed["results"].as_array().unwrap();
    assert!(
        !rows.is_empty() && rows.iter().all(|x| x["room"] == "decisions"),
        "{roomed}"
    );
    assert!(
        roomed["note"]
            .as_str()
            .unwrap()
            .contains("raw chunks carry no rooms"),
        "{roomed}"
    );

    let nope = f
        .call(
            "mempalace_search",
            json!({"query": "frame", "wing": "nope"}),
        )
        .await;
    assert_eq!(nope["results"], json!([]));
    assert!(
        nope["note"]
            .as_str()
            .unwrap()
            .contains("no project or alias named nope"),
        "{nope}"
    );

    let ignored = f
        .call(
            "mempalace_search",
            json!({"query": "frame", "max_distance": 0.5, "candidate_strategy": "union"}),
        )
        .await;
    assert!(
        ignored["note"].as_str().unwrap().contains("max_distance"),
        "{ignored}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_rooms_gives_topics_of_a_project_and_an_empty_answer_with_a_note_for_an_unknown_wing()
{
    let f = Flow::imported().await;
    let r = f
        .call("mempalace_list_rooms", json!({"wing": "pg_ai"}))
        .await;
    assert_eq!(r["wing"], "pg-ai");
    assert!(r["rooms"]["decisions"].as_u64().unwrap() >= 1, "{r}");
    assert!(r["rooms"]["knowledge"].as_u64().unwrap() >= 1, "{r}");
    let all = f.call("mempalace_list_rooms", json!({})).await;
    assert_eq!(all["wing"], "all");
    assert!(all["rooms"].as_object().unwrap().len() >= r["rooms"].as_object().unwrap().len());
    let nope = f
        .call("mempalace_list_rooms", json!({"wing": "nope"}))
        .await;
    assert_eq!(nope["rooms"], json!({}));
    assert!(
        nope["note"]
            .as_str()
            .unwrap()
            .contains("no project or alias named nope"),
        "{nope}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_drawer_resolves_alice_ids_old_ids_collapsed_ids_and_raw_chunks() {
    let f = Flow::imported().await;
    let a = f
        .call("mempalace_get_drawer", json!({"drawer_id": "k_dup_a"}))
        .await;
    let b = f
        .call("mempalace_get_drawer", json!({"drawer_id": "k_dup_b"}))
        .await;
    assert!(
        a["content"]
            .as_str()
            .unwrap()
            .contains("Frames are framed protobuf"),
        "{a}"
    );
    assert_eq!(
        (&a["content"], &a["drawer_id"]),
        (&b["content"], &b["drawer_id"]),
        "{a} {b}"
    );
    let alice_id = a["drawer_id"].as_str().unwrap().to_string();
    let by_alice = f
        .call("mempalace_get_drawer", json!({"drawer_id": alice_id}))
        .await;
    assert_eq!(by_alice["content"], a["content"]);
    assert_eq!(a["wing"], "pg-ai");
    assert_eq!(a["room"], "knowledge");
    assert!(a.get("metadata").is_some());

    let raw = f
        .call("mempalace_get_drawer", json!({"drawer_id": "s1_005"}))
        .await;
    assert!(
        raw["content"].as_str().unwrap().contains("s1 chunk 5"),
        "{raw}"
    );
    assert!(
        raw["metadata"]["source_file"]
            .as_str()
            .unwrap()
            .ends_with("s1.jsonl"),
        "{raw}"
    );
    assert_eq!(
        (
            raw["room"].as_str(),
            raw["metadata"]["chunk_index"].as_i64()
        ),
        (Some(""), Some(5))
    );

    let missing = f
        .call("mempalace_get_drawer", json!({"drawer_id": "nope_1"}))
        .await;
    assert!(
        missing["error"].as_str().unwrap().contains("not found"),
        "{missing}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_gives_counts_and_the_wing_map_and_stays_small() {
    let f = Flow::imported().await;
    let s = f.call("mempalace_status", json!({})).await;
    let wings: u64 = s["wings"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(s["total_drawers"].as_u64().unwrap(), wings, "{s}");
    assert!(s["wings"]["pg-ai"].as_u64().unwrap() > 0, "{s}");
    assert_eq!(s["alice"], true);
    assert!(!s["context"].as_str().unwrap().is_empty(), "{s}");
    for key in [
        "rooms",
        "protocol",
        "aaak_dialect",
        "backend",
        "wing_project_map",
    ] {
        assert!(s.get(key).is_some(), "{key}");
    }
    assert_eq!(s["wing_project_map"]["wing-pg-ai"], "pg-ai", "{s}");
    assert!(serde_json::to_vec(&s).unwrap().len() < 6000);
    let runs = f.svc.compat_overview_runs();
    let again = f.call("mempalace_status", json!({})).await;
    assert_eq!(again["total_drawers"], s["total_drawers"]);
    f.call("mempalace_list_wings", json!({})).await;
    assert_eq!(
        f.svc.compat_overview_runs(),
        runs,
        "within 5 seconds the overview is not re-run"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_tools_bad_arguments_and_numbers_as_strings_get_helpful_answers() {
    let f = Flow::imported().await;
    let m = f.call("mempalace_mine", json!({})).await;
    assert_eq!(m["supported"], false);
    assert!(
        m["message"]
            .as_str()
            .unwrap()
            .contains("alice import mempalace"),
        "{m}"
    );
    assert_eq!(m["tool"], "mempalace_mine");
    let n = f.call("mempalace_totally_new", json!({})).await;
    assert_eq!(n["supported"], false);
    assert!(
        n["message"]
            .as_str()
            .unwrap()
            .contains("recall, remember, context"),
        "{n}"
    );
    let t = f.call("mempalace_find_tunnels", json!({})).await;
    assert!(t["message"].as_str().unwrap().contains("no tunnels"), "{t}");

    let e = f.call("mempalace_search", json!({})).await;
    assert_eq!(e["error"], "missing required argument: query");
    let e = f.call("mempalace_search", json!({"query": 42})).await;
    assert!(e["error"].as_str().unwrap().contains("query"), "{e}");
    let e = f
        .call("mempalace_search", json!({"query": "x", "limit": "abc"}))
        .await;
    assert!(e["error"].as_str().unwrap().contains("limit"), "{e}");
    let e = f
        .call("mempalace_search", json!({"query": "x", "limit": "-3"}))
        .await;
    assert!(e["error"].as_str().unwrap().contains("limit"), "{e}");
    let ok = f
        .call("mempalace_search", json!({"query": "frame", "limit": "3"}))
        .await;
    assert!(ok["results"].as_array().unwrap().len() <= 3, "{ok}");
    assert!(ok.get("error").is_none(), "{ok}");
    let ok = f
        .call("mempalace_search", json!("{\"query\":\"frame\"}"))
        .await;
    assert!(ok.get("results").is_some(), "{ok}");
    let e = f.call("mempalace_search", json!("not an object")).await;
    assert!(e.get("error").is_some(), "{e}");
    let e = f.call("mempalace_search", json!([1, 2])).await;
    assert!(e.get("error").is_some(), "{e}");
    let e = f.call("mempalace_list_rooms", json!({"wing": 5})).await;
    assert!(e["error"].as_str().unwrap().contains("wing"), "{e}");
    // A tool that is not a mempalace_ name.
    let r = f.call("recall", json!({})).await;
    assert!(r.get("error").is_some(), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limit_is_capped_at_fifty() {
    let f = Flow::imported().await;
    let r = f
        .call("mempalace_search", json!({"query": "chunk", "limit": 500}))
        .await;
    assert!(r["results"].as_array().unwrap().len() <= 50, "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_stays_under_the_budget_with_hundreds_of_rooms() {
    let f = Flow::imported().await;
    for i in 0..250 {
        f.svc
            .remember(alice_core::api::RememberRequest {
                content: format!("a memory about subject number {i}"),
                project: Some("pg-ai".into()),
                topics: vec![format!("a-rather-long-room-name-for-subject-{i:04}")],
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let s = f.call("mempalace_status", json!({})).await;
    assert!(
        serde_json::to_vec(&s).unwrap().len() < 6000,
        "{} bytes",
        serde_json::to_vec(&s).unwrap().len()
    );
    assert_eq!(s["total_drawers"].as_u64().unwrap(), 113 + 250);
    assert!(s["note"].as_str().unwrap().contains("list_rooms"), "{s}");
    let rooms = f.call("mempalace_list_rooms", json!({})).await;
    assert!(
        rooms["rooms"].as_object().unwrap().len() > 100,
        "list_rooms has the full set"
    );
}

fn results(v: &Value) -> &Vec<Value> {
    v["results"].as_array().unwrap_or_else(|| panic!("{v}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wing_filter_excludes_other_projects() {
    let f = Flow::imported().await;
    let all = f
        .call("mempalace_search", json!({"query": "chunk", "limit": 50}))
        .await;
    let wings: std::collections::HashSet<&str> = results(&all)
        .iter()
        .map(|r| r["wing"].as_str().unwrap())
        .collect();
    assert!(
        wings.len() > 1,
        "the unfiltered search spans projects: {wings:?}"
    );
    let one = f
        .call(
            "mempalace_search",
            json!({"query": "chunk", "limit": 50, "wing": "devspace"}),
        )
        .await;
    assert!(!results(&one).is_empty());
    assert!(
        results(&one).iter().all(|r| r["wing"] == "devspace"),
        "{one}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn since_and_before_bound_the_results_and_a_bad_date_is_an_error_answer() {
    let f = Flow::imported().await;
    let q = |extra: Value| {
        let mut a = json!({"query": "frame", "limit": 50});
        a.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        a
    };
    let open = f.call("mempalace_search", q(json!({}))).await;
    assert!(!results(&open).is_empty());
    let future = f
        .call("mempalace_search", q(json!({"since": "2099-01-01"})))
        .await;
    assert_eq!(results(&future).len(), 0, "{future}");
    let past = f
        .call("mempalace_search", q(json!({"before": "2000-01-01"})))
        .await;
    assert_eq!(results(&past).len(), 0, "{past}");
    let within = f
        .call(
            "mempalace_search",
            q(json!({"since": "2020-01-01", "before": "2099-01-01"})),
        )
        .await;
    assert_eq!(results(&within).len(), results(&open).len(), "{within}");
    let bad = f
        .call("mempalace_search", q(json!({"since": "last tuesday"})))
        .await;
    assert!(bad.get("error").is_some(), "{bad}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_room_that_does_not_normalise_matches_nothing_and_a_room_hit_reports_that_room() {
    let f = Flow::imported().await;
    let bad = f
        .call("mempalace_search", json!({"query": "frame", "room": "!!!"}))
        .await;
    assert_eq!(bad["results"], json!([]));
    assert!(
        bad["note"]
            .as_str()
            .unwrap()
            .contains("not a valid room name"),
        "{bad}"
    );
    let ok = f
        .call(
            "mempalace_search",
            json!({"query": "frame", "room": "Decisions"}),
        )
        .await;
    assert!(!results(&ok).is_empty(), "{ok}");
    assert!(
        results(&ok).iter().all(|r| r["room"] == "decisions"),
        "{ok}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_old_id_says_so_and_a_trashed_memory_is_marked_and_left_out_of_search() {
    let f = Flow::imported().await;
    let merged = f
        .call("mempalace_get_drawer", json!({"drawer_id": "k_dup_b"}))
        .await;
    assert!(
        merged["note"].as_str().unwrap().contains("merged into"),
        "{merged}"
    );
    let direct = f
        .call("mempalace_get_drawer", json!({"drawer_id": "k_dup_a"}))
        .await;
    assert!(direct.get("note").is_none(), "{direct}");

    let saved = f
        .svc
        .remember(alice_core::api::RememberRequest {
            content: "zebracrossing is the unique word of this memory".into(),
            project: Some("pg-ai".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let id = saved.id.unwrap();
    let hit = f
        .call("mempalace_search", json!({"query": "zebracrossing"}))
        .await;
    assert_eq!(results(&hit).len(), 1, "{hit}");
    let before = f.call("mempalace_status", json!({})).await["memories"]
        .as_u64()
        .unwrap();
    f.svc
        .update(alice_core::api::UpdateRequest {
            id: id.clone(),
            update: alice_core::store::memories::MemoryUpdate {
                action: alice_core::store::memories::UpdateAction::Forget,
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let gone = f
        .call("mempalace_search", json!({"query": "zebracrossing"}))
        .await;
    assert_eq!(results(&gone).len(), 0, "{gone}");
    let shown = f
        .call("mempalace_get_drawer", json!({"drawer_id": id}))
        .await;
    assert_eq!(shown["trashed"], true, "{shown}");
    assert!(shown["note"].as_str().unwrap().contains("trash"), "{shown}");
    // The overview is cached for 5 s, so count with a store query instead.
    let live: i64 = rusqlite::Connection::open(f.svc.home().db())
        .unwrap()
        .query_row(
            "SELECT count(*) FROM memories WHERE status <> 'trashed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live as u64, before - 1, "one live memory fewer");
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_inputs_never_panic_and_no_path_separator_reaches_source_file() {
    let f = Flow::imported().await;
    let huge = "word ".repeat(200_000);
    let queries = [
        "\"unbalanced".to_string(),
        "NEAR(a b, 2)".to_string(),
        "a AND OR NOT (".to_string(),
        "*".to_string(),
        "col:val -x ^y".to_string(),
        "nul\u{0}byte".to_string(),
        "\u{202e}rtl\u{200b}zw\u{fe0f} 🧪 ก็ไม่เป็นไร e\u{301}".to_string(),
        "' OR 1=1; --".to_string(),
        huge,
    ];
    for q in queries {
        let r = f.call("mempalace_search", json!({"query": q})).await;
        assert!(r.is_object());
        for hit in r["results"].as_array().into_iter().flatten() {
            let sf = hit["source_file"].as_str().unwrap();
            assert!(!sf.contains('/') && !sf.contains('\\'), "{sf}");
        }
    }
    for limit in [
        json!(0),
        json!("0"),
        json!(1_000_000_000u64),
        json!("99999999999999999999"),
        json!(1.5),
        json!(-1),
        json!(true),
    ] {
        let r = f
            .call(
                "mempalace_search",
                json!({"query": "frame", "limit": limit}),
            )
            .await;
        assert!(r.is_object());
        assert!(r["results"].as_array().is_none_or(|a| a.len() <= 50), "{r}");
    }
    for arg in [
        json!(null),
        json!(7),
        json!([]),
        json!("{"),
        json!({"drawer_id": ["a"]}),
        json!({"drawer_id": "\u{0}"}),
    ] {
        let r = f.call("mempalace_get_drawer", arg).await;
        assert!(r.get("error").is_some(), "{r}");
    }
    // The raw hits do carry a file name.
    let raw = f
        .call("mempalace_search", json!({"query": "s1 chunk 7 text"}))
        .await;
    assert!(
        results(&raw).iter().any(|r| r["source_file"] == "s1.jsonl"),
        "{raw}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_read_tools_write_nothing_but_the_access_counts_of_what_search_returns() {
    let f = Flow::imported().await;
    let counts = || -> Vec<i64> {
        let c = rusqlite::Connection::open(f.svc.home().db()).unwrap();
        [
            "memories",
            "chunks",
            "events",
            "links",
            "legacy_ids",
            "projects",
        ]
        .iter()
        .map(|t| {
            c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        })
        .collect()
    };
    let before = counts();
    for (tool, args) in [
        ("mempalace_status", json!({})),
        ("mempalace_list_wings", json!({})),
        ("mempalace_list_rooms", json!({"wing": "pg_ai"})),
        ("mempalace_get_drawer", json!({"drawer_id": "k_dec"})),
        ("mempalace_get_drawer", json!({"drawer_id": "s1_001"})),
        ("mempalace_search", json!({"query": "frame"})),
        ("mempalace_mine", json!({})),
    ] {
        f.call(tool, args).await;
    }
    assert_eq!(counts(), before);
}

// ------------------------------------------------------------------------------- write tools

#[tokio::test(flavor = "multi_thread")]
async fn add_get_update_delete_round_trip_with_old_style_arguments() {
    let f = Flow::imported().await;
    let a = f
        .call(
            "mempalace_add_drawer",
            json!({"wing": "pg_ai", "room": "decisions", "content": "We chose X because Y", "added_by": "codex"}),
        )
        .await;
    assert_eq!(a["success"], true, "{a}");
    assert_eq!(
        (a["wing"].as_str(), a["room"].as_str()),
        (Some("pg-ai"), Some("decisions"))
    );
    let id = a["drawer_id"].as_str().unwrap().to_string();
    let m = f.memory(&id).await;
    assert_eq!(
        (m.kind, m.topics.clone(), m.author.agent.as_deref()),
        (Kind::Decision, vec!["decisions".to_string()], Some("codex"))
    );
    assert_eq!(m.project, "pg-ai");
    assert!(
        f.call("mempalace_get_drawer", json!({"drawer_id": id}))
            .await["content"]
            .as_str()
            .unwrap()
            .contains("We chose X")
    );
    let again = f
        .call(
            "mempalace_add_drawer",
            json!({"wing": "pg_ai", "room": "decisions", "content": "We chose X because Y"}),
        )
        .await;
    assert_eq!(
        (again["reason"].as_str(), again["drawer_id"].as_str()),
        (Some("already_exists"), Some(id.as_str())),
        "{again}"
    );
    let u = f
        .call(
            "mempalace_update_drawer",
            json!({"drawer_id": id, "content": "We chose Z"}),
        )
        .await;
    assert_eq!(u["success"], true, "{u}");
    assert!(
        f.call("mempalace_get_drawer", json!({"drawer_id": id}))
            .await["content"]
            .as_str()
            .unwrap()
            .contains("We chose Z")
    );
    let u = f
        .call(
            "mempalace_update_drawer",
            json!({"drawer_id": id, "room": "Bug Classes"}),
        )
        .await;
    assert_eq!(u["success"], true, "{u}");
    assert_eq!(f.memory(&id).await.topics, vec!["bug-classes".to_string()]);
    let d = f
        .call("mempalace_delete_drawer", json!({"drawer_id": id}))
        .await;
    assert_eq!(
        (d["success"].as_bool(), d["drawer_id"].as_str()),
        (Some(true), Some(id.as_str())),
        "{d}"
    );
    let got = f
        .call("mempalace_get_drawer", json!({"drawer_id": id}))
        .await;
    assert!(
        got.get("error").is_some() || f.is_forgotten(&id).await,
        "{got}"
    );
    assert!(f.is_forgotten(&id).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn add_drawer_appends_the_source_file_and_updates_with_no_change_are_noops() {
    let f = Flow::imported().await;
    let a = f
        .call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "a note", "source_file": "/home/me/docs/a.md"}),
        )
        .await;
    let id = a["drawer_id"].as_str().unwrap().to_string();
    let m = f.memory(&id).await;
    assert!(m.body.ends_with("\nSource: a.md"), "{:?}", m.body.len());
    let n = f
        .call("mempalace_update_drawer", json!({"drawer_id": id}))
        .await;
    assert_eq!(
        (n["success"].as_bool(), n["noop"].as_bool()),
        (Some(true), Some(true)),
        "{n}"
    );
    let missing = f
        .call(
            "mempalace_update_drawer",
            json!({"drawer_id": "nope", "content": "x"}),
        )
        .await;
    assert!(
        missing["error"].as_str().unwrap().contains("not found"),
        "{missing}"
    );
    let missing = f
        .call("mempalace_delete_drawer", json!({"drawer_id": "nope"}))
        .await;
    assert!(
        missing["error"].as_str().unwrap().contains("not found"),
        "{missing}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_wing_on_a_write_files_into_the_current_project_with_a_note() {
    let f = Flow::imported().await;
    let cwd = tempfile::tempdir().unwrap();
    let cwd_s = cwd.path().display().to_string();
    let a = f
        .call_with(
            "mempalace_add_drawer",
            json!({"wing": "nope", "room": "Ideas", "content": "an idea"}),
            Some(&cwd_s),
        )
        .await;
    assert_eq!(a["success"], true, "{a}");
    let wing = a["wing"].as_str().unwrap().to_string();
    assert_ne!(wing, "nope");
    let note = a["note"].as_str().unwrap();
    assert!(
        note.contains("wing nope is not an Alice project")
            && note.contains(&format!("saved in {wing}")),
        "{note}"
    );
    let m = f.memory(a["drawer_id"].as_str().unwrap()).await;
    assert_eq!((m.project, m.topics), (wing, vec!["ideas".to_string()]));
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_and_deletes_of_raw_chunks_and_moves_between_wings_are_refused_with_a_sentence() {
    let f = Flow::imported().await;
    for (tool, args) in [
        (
            "mempalace_update_drawer",
            json!({"drawer_id": "s1_001", "content": "x"}),
        ),
        ("mempalace_delete_drawer", json!({"drawer_id": "s1_001"})),
    ] {
        let r = f.call(tool, args).await;
        assert!(r["error"].as_str().unwrap().contains("raw"), "{tool}: {r}");
        assert_eq!(r["success"], false, "{r}");
    }
    let r = f
        .call(
            "mempalace_update_drawer",
            json!({"drawer_id": "k_dec", "wing": "devspace"}),
        )
        .await;
    assert!(
        r["error"].as_str().unwrap().contains("not supported"),
        "{r}"
    );
    assert_eq!(f.memory("k_dec").await.project, "pg-ai");
    // naming the wing the memory is already in is not a move
    let same = f
        .call(
            "mempalace_update_drawer",
            json!({"drawer_id": "k_dec", "wing": "wing_pg_ai", "content": "decision, reworded"}),
        )
        .await;
    assert_eq!(same["success"], true, "{same}");
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_are_redacted_and_carry_the_calling_agent() {
    let f = Flow::imported().await;
    let secret = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
    let a = f
        .call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": format!("token {secret} in a note")}),
        )
        .await;
    let id = a["drawer_id"].as_str().unwrap().to_string();
    let m = f.memory(&id).await;
    assert!(!m.body.contains(secret), "{}", m.body);
    let r = f
        .svc
        .compat_call(CompatCall {
            tool: "mempalace_update_drawer".into(),
            arguments: json!({"drawer_id": id, "content": format!("again {secret}")}),
            cwd: None,
            agent: Some("codex".into()),
        })
        .await
        .unwrap()
        .value;
    assert_eq!(r["success"], true, "{r}");
    assert!(!f.memory(&id).await.body.contains(secret));
    assert_eq!(f.last_actor("memory.edit"), "agent:codex");
}

#[tokio::test(flavor = "multi_thread")]
async fn diary_write_and_read_round_trip_for_an_agent() {
    let f = Flow::imported().await;
    let w = f
        .call(
            "mempalace_diary_write",
            json!({"agent_name": "Claude-Code", "entry": "did X", "topic": "setup"}),
        )
        .await;
    assert_eq!(w["success"], true, "{w}");
    assert_eq!(
        (w["agent"].as_str(), w["topic"].as_str()),
        (Some("claude-code"), Some("setup"))
    );
    let m = f.memory(w["entry_id"].as_str().unwrap()).await;
    assert_eq!(
        (
            m.kind,
            m.title.ends_with("claude-code: setup"),
            m.topics.clone(),
            m.author.agent.as_deref()
        ),
        (
            Kind::Diary,
            true,
            vec!["setup".to_string()],
            Some("claude-code")
        )
    );
    let r = f
        .call(
            "mempalace_diary_read",
            json!({"agent_name": "claude-code", "last_n": "3"}),
        )
        .await;
    let entries = r["entries"].as_array().unwrap();
    assert!(r["total"].as_u64().unwrap() >= 2, "{r}");
    assert_eq!(r["showing"].as_u64(), Some(entries.len() as u64));
    assert_eq!(entries[0]["content"], "did X", "newest first: {r}");
    assert_eq!(entries[0]["topic"], "setup");
    assert!(
        entries[0]["timestamp"]
            .as_str()
            .unwrap()
            .starts_with(entries[0]["date"].as_str().unwrap())
    );
    // the imported diary entry is the older one
    assert!(
        entries
            .iter()
            .any(|e| e["content"].as_str().unwrap().contains("Set up the import"))
    );
    let other = f
        .call("mempalace_diary_read", json!({"agent_name": "other"}))
        .await;
    assert_eq!(other["entries"].as_array().unwrap().len(), 0, "{other}");
    let nowing = f
        .call(
            "mempalace_diary_read",
            json!({"agent_name": "claude-code", "wing": "nope"}),
        )
        .await;
    assert_eq!(nowing["entries"].as_array().unwrap().len(), 0);
    assert!(
        nowing["note"].as_str().unwrap().contains("nope"),
        "{nowing}"
    );
    // the same words again, by the same agent or by another, are new entries
    let again = f
        .call(
            "mempalace_diary_write",
            json!({"agent_name": "claude-code", "entry": "did X", "topic": "setup"}),
        )
        .await;
    assert!(
        again.get("reason").is_none() && again["entry_id"] != w["entry_id"],
        "{again}"
    );
    let b = f
        .call(
            "mempalace_diary_write",
            json!({"agent_name": "agent-b", "entry": "did X", "topic": "setup"}),
        )
        .await;
    assert!(b.get("reason").is_none(), "{b}");
    let rb = f
        .call("mempalace_diary_read", json!({"agent_name": "agent-b"}))
        .await;
    assert_eq!(
        (rb["total"].as_u64(), rb["entries"][0]["content"].as_str()),
        (Some(1), Some("did X")),
        "{rb}"
    );
    let ra = f
        .call("mempalace_diary_read", json!({"agent_name": "claude-code"}))
        .await;
    assert_eq!(ra["total"].as_u64(), Some(3), "{ra}");
}

#[tokio::test(flavor = "multi_thread")]
async fn diary_write_to_an_unknown_wing_files_into_the_current_project() {
    let f = Flow::imported().await;
    let cwd = tempfile::tempdir().unwrap();
    let cwd_s = cwd.path().display().to_string();
    let w = f
        .call_with(
            "mempalace_diary_write",
            json!({"agent_name": "sub-agent", "entry": "journal", "wing": "wing_sub_agent"}),
            Some(&cwd_s),
        )
        .await;
    assert_eq!(w["success"], true, "{w}");
    assert!(
        w["note"]
            .as_str()
            .unwrap()
            .contains("wing wing_sub_agent is not an Alice project"),
        "{w}"
    );
    let m = f.memory(w["entry_id"].as_str().unwrap()).await;
    assert!(m.title.ends_with("sub-agent: general"), "{}", m.title);
    let r = f
        .call("mempalace_diary_read", json!({"agent_name": "sub-agent"}))
        .await;
    assert_eq!(r["entries"][0]["topic"], "general", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn kg_add_and_query_show_an_active_fact() {
    let f = Flow::imported().await;
    let a = f
        .call(
            "mempalace_kg_add",
            json!({"subject": "alice", "predicate": "uses", "object": "sqlite", "valid_from": "2026-01-01"}),
        )
        .await;
    assert_eq!(a["success"], true, "{a}");
    assert_eq!(a["fact"], "alice → uses → sqlite");
    assert!(a["triple_id"].as_str().is_some());
    let q = f
        .call("mempalace_kg_query", json!({"entity": "alice"}))
        .await;
    let active = q["active_facts"].as_array().unwrap();
    assert_eq!(active.len(), 1, "{q}");
    assert_eq!(
        (
            active[0]["predicate"].as_str(),
            active[0]["object"].as_str(),
            active[0]["direction"].as_str(),
            active[0]["current"].as_bool()
        ),
        (Some("uses"), Some("sqlite"), Some("outgoing"), Some(true))
    );
    assert_eq!(
        (
            q["historical_facts"].as_array().unwrap().len(),
            q["count"].as_u64()
        ),
        (0, Some(1))
    );
    // direction
    let inc = f
        .call(
            "mempalace_kg_query",
            json!({"entity": "sqlite", "direction": "incoming"}),
        )
        .await;
    assert_eq!(inc["active_facts"][0]["subject"], "alice", "{inc}");
    let out = f
        .call(
            "mempalace_kg_query",
            json!({"entity": "sqlite", "direction": "outgoing"}),
        )
        .await;
    assert_eq!(out["count"], 0, "{out}");
    let bad = f
        .call(
            "mempalace_kg_query",
            json!({"entity": "sqlite", "direction": "sideways"}),
        )
        .await;
    assert!(
        bad["error"].as_str().unwrap().contains("direction"),
        "{bad}"
    );
    // as_of before the start sees nothing
    let early = f
        .call(
            "mempalace_kg_query",
            json!({"entity": "alice", "as_of": "2025-06-01"}),
        )
        .await;
    assert_eq!(early["count"], 0, "{early}");
}

#[tokio::test(flavor = "multi_thread")]
async fn kg_supersede_closes_the_old_value() {
    let f = Flow::imported().await;
    f.call(
        "mempalace_kg_add",
        json!({"subject": "alice", "predicate": "uses", "object": "sqlite", "valid_from": "2026-01-01"}),
    )
    .await;
    let s = f
        .call(
            "mempalace_kg_supersede",
            json!({"subject": "alice", "predicate": "uses", "old_object": "sqlite", "new_object": "postgres", "at": "2026-06-01"}),
        )
        .await;
    assert_eq!(
        (s["success"].as_bool(), s["superseded"].as_str()),
        (Some(true), Some("sqlite")),
        "{s}"
    );
    assert!(s.get("note").is_none(), "{s}");
    let q = f
        .call("mempalace_kg_query", json!({"entity": "alice"}))
        .await;
    assert_eq!(q["active_facts"][0]["object"], "postgres", "{q}");
    assert_eq!(q["historical_facts"][0]["object"], "sqlite", "{q}");
    // a mismatch is reported, not refused
    let m = f
        .call(
            "mempalace_kg_supersede",
            json!({"subject": "alice", "predicate": "uses", "old_object": "mysql", "new_object": "cockroach", "at": "2026-07-01"}),
        )
        .await;
    assert_eq!(m["success"], true, "{m}");
    assert!(m["note"].as_str().unwrap().contains("mysql"), "{m}");
    let q = f
        .call("mempalace_kg_query", json!({"entity": "alice"}))
        .await;
    assert_eq!(q["active_facts"][0]["object"], "cockroach", "{q}");
}

#[tokio::test(flavor = "multi_thread")]
async fn kg_invalidate_ends_the_fact() {
    let f = Flow::imported().await;
    f.call(
        "mempalace_kg_add",
        json!({"subject": "alice", "predicate": "uses", "object": "sqlite", "valid_from": "2026-01-01"}),
    )
    .await;
    let i = f
        .call(
            "mempalace_kg_invalidate",
            json!({"subject": "alice", "predicate": "uses", "object": "sqlite", "ended": "2026-07-01"}),
        )
        .await;
    assert_eq!(
        (i["success"].as_bool(), i["ended"].as_str()),
        (Some(true), Some("2026-07-01")),
        "{i}"
    );
    let q = f
        .call("mempalace_kg_query", json!({"entity": "alice"}))
        .await;
    assert_eq!(q["active_facts"].as_array().unwrap().len(), 0, "{q}");
    assert_eq!(q["historical_facts"].as_array().unwrap().len(), 1, "{q}");
    // nothing left to end: said, not an error
    let again = f
        .call(
            "mempalace_kg_invalidate",
            json!({"subject": "alice", "predicate": "uses", "object": "sqlite"}),
        )
        .await;
    assert!(
        again.get("error").is_none() && again["note"].as_str().is_some(),
        "{again}"
    );
    assert!(again["ended"].as_str().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn kg_timeline_lists_entries_in_time_order_with_limit_and_offset() {
    let f = Flow::imported().await;
    f.call(
        "mempalace_kg_add",
        json!({"subject": "alice", "predicate": "uses", "object": "sqlite", "valid_from": "2026-01-01"}),
    )
    .await;
    f.call(
        "mempalace_kg_supersede",
        json!({"subject": "alice", "predicate": "uses", "old_object": "sqlite", "new_object": "postgres", "at": "2026-06-01"}),
    )
    .await;
    let t = f
        .call("mempalace_kg_timeline", json!({"entity": "alice"}))
        .await;
    let objs: Vec<&str> = t["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["object"].as_str().unwrap())
        .collect();
    assert_eq!(objs, ["sqlite", "postgres"], "{t}");
    assert_eq!(
        (
            t["count"].as_u64(),
            t["total"].as_u64(),
            t["entity"].as_str()
        ),
        (Some(2), Some(2), Some("alice"))
    );
    let p = f
        .call(
            "mempalace_kg_timeline",
            json!({"entity": "alice", "limit": "1", "offset": "1"}),
        )
        .await;
    assert_eq!(p["timeline"][0]["object"], "postgres", "{p}");
    assert_eq!(
        (
            p["count"].as_u64(),
            p["total"].as_u64(),
            p["offset"].as_u64()
        ),
        (Some(1), Some(2), Some(1))
    );
    let none = f.call("mempalace_kg_timeline", json!({})).await;
    assert!(none["error"].as_str().unwrap().contains("entity"), "{none}");
}

#[tokio::test(flavor = "multi_thread")]
async fn kg_add_with_valid_to_creates_an_ended_fact() {
    let f = Flow::imported().await;
    let a = f
        .call(
            "mempalace_kg_add",
            json!({"subject": "alice", "predicate": "used", "object": "csv", "valid_from": "2026-01-01", "valid_to": "2026-03-01"}),
        )
        .await;
    assert_eq!(a["success"], true, "{a}");
    let q = f
        .call("mempalace_kg_query", json!({"entity": "alice"}))
        .await;
    assert_eq!(q["active_facts"].as_array().unwrap().len(), 0, "{q}");
    assert_eq!(q["historical_facts"][0]["object"], "csv", "{q}");
    let bad = f
        .call(
            "mempalace_kg_add",
            json!({"subject": "alice", "predicate": "p", "object": "o", "valid_from": "yesterday"}),
        )
        .await;
    assert_eq!(bad["success"], false, "{bad}");
    assert!(
        bad["error"].as_str().unwrap().contains("valid_from"),
        "{bad}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compat_delete_can_be_undone_through_its_event() {
    let f = Flow::imported().await;
    let d = f
        .call("mempalace_delete_drawer", json!({"drawer_id": "k_dec"}))
        .await;
    assert!(f.is_forgotten("k_dec").await);
    let undone = f
        .svc
        .undo(alice_core::api::UndoRequest {
            event_id: d["event_id"]
                .as_str()
                .expect("the answer names the event")
                .into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(undone.undone);
    assert!(!f.is_forgotten("k_dec").await);
}

// ---------------------------------------------------------------------- fix round 1 tests

const MARK: &str = "[REDACTED";

fn secret() -> &'static str {
    concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789")
}

impl Flow {
    fn scalar(&self, sql: &str, p: &str) -> String {
        let c = rusqlite::Connection::open(self.svc.home().db()).unwrap();
        c.query_row(sql, [p], |r| r.get(0)).unwrap()
    }

    async fn call_as(&self, agent: &str, tool: &str, arguments: Value) -> Value {
        self.svc
            .compat_call(CompatCall {
                tool: tool.into(),
                arguments,
                cwd: None,
                agent: Some(agent.into()),
            })
            .await
            .unwrap()
            .value
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_are_redacted_in_diary_and_never_echoed_by_the_graph_tools() {
    let f = Flow::imported().await;
    let sec = secret();
    let w = f
        .call(
            "mempalace_diary_write",
            json!({"agent_name": "a", "entry": format!("key {sec}"), "topic": format!("t {sec}")}),
        )
        .await;
    assert_eq!(w["success"], true, "{w}");
    assert!(!w.to_string().contains(sec), "{w}");
    let m = f.memory(w["entry_id"].as_str().unwrap()).await;
    for text in [&m.body, &m.title, &m.topics.join(" ")] {
        assert!(!text.contains(sec), "{text}");
    }
    assert!(m.body.contains(MARK), "{}", m.body);

    let added = f
        .call(
            "mempalace_kg_add",
            json!({"subject": "svc", "predicate": "holds", "object": format!("token {sec}"), "valid_from": "2026-01-01"}),
        )
        .await;
    assert_eq!(added["success"], true, "{added}");
    assert!(
        !added.to_string().contains(sec) && added["fact"].as_str().unwrap().contains(MARK),
        "{added}"
    );
    let sup = f
        .call(
            "mempalace_kg_supersede",
            json!({"subject": "svc", "predicate": "holds", "old_object": format!("old {sec}"), "new_object": format!("new {sec}"), "at": "2026-02-01"}),
        )
        .await;
    assert!(!sup.to_string().contains(sec), "{sup}");
    assert!(
        sup["fact"].as_str().unwrap().contains(MARK)
            && sup["superseded"].as_str().unwrap().contains(MARK),
        "{sup}"
    );
    assert!(sup["note"].as_str().unwrap().contains(MARK), "{sup}");
    let inv = f
        .call(
            "mempalace_kg_invalidate",
            json!({"subject": "svc", "predicate": "holds", "object": format!("new {sec}")}),
        )
        .await;
    assert!(
        !inv.to_string().contains(sec) && inv["fact"].as_str().unwrap().contains(MARK),
        "{inv}"
    );
    let q = f.call("mempalace_kg_query", json!({"entity": "svc"})).await;
    assert!(
        !q.to_string().contains(sec) && q.to_string().contains(MARK),
        "{q}"
    );
    let t = f
        .call("mempalace_kg_timeline", json!({"entity": "svc"}))
        .await;
    assert!(!t.to_string().contains(sec), "{t}");
    let nf = f
        .call("mempalace_get_drawer", json!({"drawer_id": sec}))
        .await;
    assert!(!nf.to_string().contains(sec), "{nf}");
    let nf = f
        .call("mempalace_delete_drawer", json!({"drawer_id": sec}))
        .await;
    assert!(!nf.to_string().contains(sec), "{nf}");
}

#[tokio::test(flavor = "multi_thread")]
async fn events_are_attributed_and_agent_writes_wait_for_the_judge() {
    let f = Flow::imported().await;
    let a = f
        .call_as(
            "codex",
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "attributed"}),
        )
        .await;
    let id = a["drawer_id"].as_str().unwrap().to_string();
    assert_eq!(f.last_actor("memory.create"), "agent:codex");
    // An agent's write is stored pending (only the judge activates it): read from the create
    // event, which records the state at insert. The row itself may already be judged by now.
    let created: String = f.scalar(
        "SELECT json_extract(after_json, '$.status') FROM events WHERE action = 'memory.create' AND target_id = ?1",
        &id,
    );
    assert_eq!(
        created, "pending",
        "the compat layer never activates an agent's write"
    );
    let judge = f.scalar("SELECT judge_state FROM memories WHERE id = ?1", &id);
    assert!(
        ["pending", "done", "retry", "skipped"].contains(&judge.as_str()),
        "judge_state {judge}"
    );
    // added_by names the author of the creation
    f.call_as(
        "codex",
        "mempalace_add_drawer",
        json!({"wing": "pg-ai", "room": "notes", "content": "by someone", "added_by": "gemini"}),
    )
    .await;
    assert_eq!(f.last_actor("memory.create"), "agent:gemini");
    // no agent at all is the user's write
    f.call(
        "mempalace_add_drawer",
        json!({"wing": "pg-ai", "room": "notes", "content": "by nobody"}),
    )
    .await;
    assert_eq!(f.last_actor("memory.create"), "user");
    f.call_as("codex", "mempalace_delete_drawer", json!({"drawer_id": id}))
        .await;
    assert_eq!(f.last_actor("memory.forget"), "agent:codex");
    f.call_as(
        "codex",
        "mempalace_kg_add",
        json!({"subject": "x", "predicate": "is", "object": "y", "valid_from": "2026-01-01"}),
    )
    .await;
    assert_eq!(f.last_actor("relation.add"), "agent:codex");
    f.call_as(
        "reviewer",
        "mempalace_kg_invalidate",
        json!({"subject": "x", "predicate": "is", "object": "y", "ended": "2026-02-01"}),
    )
    .await;
    assert_eq!(f.last_actor("relation.close"), "agent:reviewer");
}

#[tokio::test(flavor = "multi_thread")]
async fn kg_query_shows_future_facts_windows_and_ends_only_the_matching_relation() {
    let f = Flow::imported().await;
    for (o, from) in [("a", "2026-01-01"), ("b", "2026-01-01")] {
        f.call(
            "mempalace_kg_add",
            json!({"subject": "s", "predicate": "has", "object": o, "valid_from": from}),
        )
        .await;
    }
    f.call("mempalace_kg_add", json!({"subject": "s", "predicate": "will_use", "object": "later", "valid_from": "2999-01-01"})).await;
    f.call(
        "mempalace_kg_invalidate",
        json!({"subject": "s", "predicate": "has", "object": "a", "ended": "2026-03-01"}),
    )
    .await;
    let q = f.call("mempalace_kg_query", json!({"entity": "s"})).await;
    let objs = |k: &str| -> Vec<String> {
        q[k].as_array()
            .unwrap()
            .iter()
            .map(|x| x["object"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(objs("active_facts"), ["b"], "{q}");
    assert_eq!(objs("historical_facts"), ["a"], "{q}");
    assert_eq!(objs("future_facts"), ["later"], "{q}");
    // a point inside the closed span sees both; after it only b
    let inside = f
        .call(
            "mempalace_kg_query",
            json!({"entity": "s", "as_of": "2026-02-01"}),
        )
        .await;
    assert_eq!(inside["count"], 2, "{inside}");
    let after = f
        .call(
            "mempalace_kg_query",
            json!({"entity": "s", "as_of": "2026-04-01"}),
        )
        .await;
    assert_eq!(after["count"], 1, "{after}");
    assert_eq!(after["active_facts"][0]["object"], "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn kg_add_refuses_a_reversed_or_unanchored_range_and_a_trashed_memory_is_not_edited() {
    let f = Flow::imported().await;
    let r = f.call("mempalace_kg_add", json!({"subject": "s", "predicate": "p", "object": "o", "valid_from": "2026-05-01", "valid_to": "2026-04-01"})).await;
    assert!(
        r["error"].as_str().unwrap().contains("before valid_from"),
        "{r}"
    );
    let r = f
        .call(
            "mempalace_kg_add",
            json!({"subject": "s", "predicate": "p", "object": "o", "valid_to": "2026-04-01"}),
        )
        .await;
    assert!(r["error"].as_str().unwrap().contains("valid_from"), "{r}");
    let q = f.call("mempalace_kg_query", json!({"entity": "s"})).await;
    assert_eq!(q["count"], 0, "nothing was added: {q}");
    f.call("mempalace_delete_drawer", json!({"drawer_id": "k_dec"}))
        .await;
    for (tool, args) in [
        ("mempalace_delete_drawer", json!({"drawer_id": "k_dec"})),
        (
            "mempalace_update_drawer",
            json!({"drawer_id": "k_dec", "content": "x"}),
        ),
    ] {
        let r = f.call(tool, args).await;
        assert!(
            r["error"].as_str().unwrap().contains("already forgotten"),
            "{tool}: {r}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_room_is_noted_and_big_graph_and_diary_answers_are_capped() {
    let f = Flow::imported().await;
    let a = f
        .call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "!!!", "content": "roomless"}),
        )
        .await;
    assert_eq!(a["success"], true, "{a}");
    assert!(
        a["note"].as_str().unwrap().contains("not a usable room"),
        "{a}"
    );
    for i in 0..230 {
        f.call("mempalace_kg_add", json!({"subject": "hub", "predicate": "links", "object": format!("n{i}"), "valid_from": "2026-01-01"})).await;
    }
    let q = f.call("mempalace_kg_query", json!({"entity": "hub"})).await;
    assert_eq!(
        (
            q["truncated"].clone(),
            q["total"].as_u64(),
            q["count"].as_u64()
        ),
        (json!(["active_facts"]), Some(230), Some(200)),
        "{}",
        q["note"]
    );
    assert_eq!(q["facts_omitted"], true);
    assert_eq!(q["facts"].as_array().unwrap().len(), 0);
    let long = "x".repeat(20_000);
    f.call(
        "mempalace_diary_write",
        json!({"agent_name": "big", "entry": long}),
    )
    .await;
    let r = f
        .call("mempalace_diary_read", json!({"agent_name": "big"}))
        .await;
    assert_eq!(r["entries"][0]["truncated"], true);
    assert!(
        r["entries"][0]["content"].as_str().unwrap().len() <= 4_000,
        "{}",
        r["note"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn while_read_only_new_memories_are_queued_and_other_writes_say_so_plainly() {
    let f = Flow::imported().await;
    f.svc.force_read_only("the disk is full");
    let a = f
        .call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "queued note"}),
        )
        .await;
    assert_eq!(
        (a["success"].as_bool(), a["queued"].as_bool()),
        (Some(true), Some(true)),
        "{a}"
    );
    let d = f
        .call(
            "mempalace_diary_write",
            json!({"agent_name": "a", "entry": "queued diary"}),
        )
        .await;
    assert_eq!(d["queued"], true, "{d}");
    assert_eq!(
        std::fs::read_dir(f.svc.home().spool_dir()).unwrap().count(),
        2
    );
    for (tool, args) in [
        ("mempalace_delete_drawer", json!({"drawer_id": "k_dec"})),
        (
            "mempalace_kg_add",
            json!({"subject": "a", "predicate": "b", "object": "c"}),
        ),
    ] {
        let r = f.call(tool, args).await;
        let e = r["error"].as_str().unwrap();
        assert!(
            e.contains("read-only") && e.contains("not saved") && !e.contains("disk is full"),
            "{tool}: {r}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capped_kg_query_keeps_current_facts_and_drops_the_oldest_ended_ones() {
    let f = Flow::imported().await;
    // 215 ended facts (n000 oldest), then 3 active and 1 future
    for i in 0..215 {
        f.call(
            "mempalace_kg_add",
            json!({"subject": "hub", "predicate": "saw", "object": format!("n{i:03}"),
                   "valid_from": format!("2020-01-{:02}T00:{:02}:00Z", 1 + i / 60, i % 60),
                   "valid_to": "2021-01-01"}),
        )
        .await;
    }
    for o in ["live1", "live2", "live3"] {
        f.call(
            "mempalace_kg_add",
            json!({"subject": "hub", "predicate": "is", "object": o, "valid_from": "2024-01-01"}),
        )
        .await;
    }
    f.call("mempalace_kg_add", json!({"subject": "hub", "predicate": "will", "object": "soon", "valid_from": "2999-01-01"})).await;
    let q = f.call("mempalace_kg_query", json!({"entity": "hub"})).await;
    assert_eq!(q["total"], 219, "{}", q["note"]);
    assert_eq!(q["count"], 200);
    assert_eq!(q["truncated"], json!(["historical_facts"]));
    assert_eq!(q["active_facts"].as_array().unwrap().len(), 3);
    assert_eq!(q["future_facts"][0]["object"], "soon");
    let hist: Vec<&str> = q["historical_facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["object"].as_str().unwrap())
        .collect();
    assert_eq!(hist.len(), 196);
    assert_eq!(
        (hist[0], *hist.last().unwrap()),
        ("n019", "n214"),
        "oldest dropped, newest kept"
    );
    assert!(q["facts"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_shaped_agent_name_is_stored_and_shown_redacted_and_diary_titles_are_one_clean_line()
 {
    let f = Flow::imported().await;
    let sec = secret();
    let agent = format!("agent-{sec}");
    let w = f
        .call("mempalace_diary_write", json!({"agent_name": agent, "entry": "hello", "topic": format!("line one\nline\u{7}two {sec}")}))
        .await;
    assert_eq!(w["success"], true, "{w}");
    let a = f
        .call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "x", "added_by": agent}),
        )
        .await;
    let r = f
        .call("mempalace_diary_read", json!({"agent_name": agent}))
        .await;
    assert_eq!(r["total"], 1, "{r}");
    let u = f
        .call_as(
            &agent,
            "mempalace_update_drawer",
            json!({"drawer_id": a["drawer_id"], "content": "y"}),
        )
        .await;
    for v in [&w, &a, &r, &u] {
        assert!(!v.to_string().contains(sec), "{v}");
    }
    assert!(w["agent"].as_str().unwrap().contains("redacted"), "{w}");
    let m = f.memory(w["entry_id"].as_str().unwrap()).await;
    assert!(
        !m.title.contains(sec) && !m.title.contains('\n') && !m.title.chars().any(char::is_control),
        "{:?}",
        m.title
    );
    assert!(!m.title.contains("ghp_"), "{}", m.title);
    // nothing in the database or the event log holds it
    let c = rusqlite::Connection::open(f.svc.home().db()).unwrap();
    for sql in [
        "SELECT count(*) FROM memories WHERE instr(coalesce(author_agent,'') || title || body, ?1) > 0",
        "SELECT count(*) FROM events WHERE instr(actor || coalesce(before_json,'') || coalesce(after_json,''), ?1) > 0",
    ] {
        let n: i64 = c.query_row(sql, [sec], |r| r.get(0)).unwrap();
        if n != 0 {
            let rows: Vec<String> = c
                .prepare("SELECT action || ' ' || actor || ' ' || coalesce(before_json,'') || ' ' || coalesce(after_json,'') FROM events")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .filter(|t: &String| t.contains(sec))
                .collect();
            panic!("{sql}: {rows:?}");
        }
    }
}

// ------------------------------------------------------------------------------------- MCP

#[derive(Clone)]
struct Named(&'static str);
impl ClientHandler for Named {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new(self.0, "1.0"),
        )
    }
}

/// The imported service served over HTTP (as the daemon does) and published for `alice mcp`.
struct Served {
    port: u16,
    token: String,
    stop: CancellationToken,
}

impl Flow {
    async fn serve(&self) -> Served {
        alice_daemon::install_tls();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let home = self.svc.home().clone();
        let token = alice_daemon::discovery::load_or_create_token(&home).unwrap();
        write_info(
            &home,
            &DaemonInfo {
                pid: std::process::id(),
                port,
                instance: self.svc.instance().into(),
                version: "t".into(),
                started_at: "t".into(),
            },
        )
        .unwrap();
        let stop = CancellationToken::new();
        let app = alice_daemon::http::router(self.svc.clone(), token.clone(), port, stop.clone());
        let ct = stop.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct.cancelled().await })
                .await
                .unwrap()
        });
        Served { port, token, stop }
    }

    fn patch(&self, key: &str, on: bool) {
        let mut change = serde_json::Map::new();
        change.insert(key.into(), json!(on));
        self.svc.settings_handle().patch(&change).unwrap();
    }
    fn set_compat(&self, on: bool) {
        self.patch("compat.mempalace", on);
    }
}

impl Served {
    fn transport(&self) -> StreamableHttpClientTransport<reqwest::Client> {
        let cfg = StreamableHttpClientTransportConfig::with_uri(format!(
            "http://127.0.0.1:{}/mcp",
            self.port
        ))
        .auth_header(self.token.clone());
        StreamableHttpClientTransport::from_config(cfg)
    }
}

type Client<C> = rmcp::service::RunningService<RoleClient, C>;

async fn names<C: ClientHandler>(c: &Client<C>) -> Vec<String> {
    let mut v: Vec<String> = c
        .list_all_tools()
        .await
        .unwrap()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    v.sort();
    v
}

fn compat_count(v: &[String]) -> usize {
    v.iter().filter(|n| n.starts_with("mempalace_")).count()
}

fn tool_call(name: &str, v: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name.to_string()).with_arguments(v.as_object().unwrap().clone())
}

fn first_text(r: &rmcp::model::CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

fn value_of(r: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(&first_text(r)).unwrap_or_else(|_| panic!("not JSON: {}", first_text(r)))
}

fn error_text(e: rmcp::ServiceError) -> String {
    match e {
        rmcp::ServiceError::McpError(d) => format!("{} {}", d.message, d.data.unwrap_or_default()),
        other => other.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn compat_tools_appear_and_vanish_with_the_setting_without_a_restart() {
    let f = Flow::imported().await;
    let s = f.serve().await;
    let c = Named("test-agent").serve(s.transport()).await.unwrap();
    f.set_compat(false);
    assert_eq!(compat_count(&names(&c).await), 0);
    f.set_compat(true);
    let on = names(&c).await;
    assert_eq!(compat_count(&on), 15, "{on:?}");
    for core in [
        "recall",
        "remember",
        "context",
        "get_memory",
        "update_memory",
        "query_facts",
        "assert_fact",
    ] {
        assert!(on.contains(&core.to_string()), "{core}");
    }
    f.set_compat(false);
    assert_eq!(compat_count(&names(&c).await), 0);
    c.cancel().await.ok();
    s.stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_while_compat_is_off_is_refused_with_the_way_to_turn_it_on() {
    let f = Flow::imported().await;
    let s = f.serve().await;
    let c = Named("test-agent").serve(s.transport()).await.unwrap();
    f.set_compat(false);
    for name in ["mempalace_search", "mempalace_mine"] {
        let err = error_text(
            c.call_tool(tool_call(name, json!({"query": "x"})))
                .await
                .unwrap_err(),
        );
        assert!(err.contains("tool not found"), "{err}");
        assert!(err.contains("compat.mempalace"), "{err}");
    }
    f.set_compat(true);
    let r = c
        .call_tool(tool_call("mempalace_list_wings", json!({})))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{}", first_text(&r));
    assert!(value_of(&r).get("wings").is_some(), "{}", first_text(&r));
    let r = c
        .call_tool(tool_call("mempalace_search", json!({"query": "the"})))
        .await
        .unwrap();
    assert!(value_of(&r).is_object());
    c.cancel().await.ok();
    s.stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_names_are_answered_but_not_listed() {
    let f = Flow::imported().await;
    f.set_compat(true);
    let s = f.serve().await;
    let c = Named("test-agent").serve(s.transport()).await.unwrap();
    assert!(!names(&c).await.contains(&"mempalace_mine".to_string()));
    let r = c
        .call_tool(tool_call("mempalace_mine", json!({})))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true));
    assert!(
        first_text(&r).contains("alice import mempalace"),
        "{}",
        first_text(&r)
    );
    c.cancel().await.ok();
    s.stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn every_listed_schema_names_mempalaces_arguments() {
    let expected: &[(&str, &[&str])] = &[
        ("mempalace_status", &[]),
        ("mempalace_search", &["query"]),
        ("mempalace_add_drawer", &["wing", "room", "content"]),
        ("mempalace_diary_write", &["agent_name"]),
        ("mempalace_diary_read", &["agent_name"]),
        ("mempalace_get_drawer", &["drawer_id"]),
        ("mempalace_update_drawer", &["drawer_id"]),
        ("mempalace_delete_drawer", &["drawer_id"]),
        ("mempalace_kg_query", &["entity"]),
        ("mempalace_kg_add", &["subject", "predicate", "object"]),
        (
            "mempalace_kg_invalidate",
            &["subject", "predicate", "object"],
        ),
        (
            "mempalace_kg_supersede",
            &["subject", "predicate", "old_object", "new_object"],
        ),
        ("mempalace_kg_timeline", &[]),
        ("mempalace_list_wings", &[]),
        ("mempalace_list_rooms", &[]),
    ];
    let tools = alice_daemon::compat_schema::tools();
    assert_eq!(tools.len(), alice_daemon::compat::SUPPORTED.len());
    for name in alice_daemon::compat::SUPPORTED {
        assert!(tools.iter().any(|t| t.name == name), "{name}");
    }
    for t in &tools {
        assert_eq!(t.input_schema["type"], "object", "{}", t.name);
        assert!(t.description.as_deref().is_some_and(|d| !d.is_empty()));
        let want = expected.iter().find(|(n, _)| *n == t.name).unwrap().1;
        let got: Vec<&str> = t
            .input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().map(|v| v.as_str().unwrap()).collect())
            .unwrap_or_default();
        assert_eq!(got, want, "required of {}", t.name);
    }
    // A call built from the schema's own properties succeeds for the three writers.
    let f = Flow::imported().await;
    f.set_compat(true);
    let s = f.serve().await;
    let c = Named("test-agent").serve(s.transport()).await.unwrap();
    let listed = c.list_all_tools().await.unwrap();
    for (name, sample) in [
        ("mempalace_search", json!({"query": "the"})),
        (
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "a schema-built call"}),
        ),
        (
            "mempalace_kg_add",
            json!({"subject": "alpha", "predicate": "uses", "object": "beta"}),
        ),
    ] {
        let t = listed.iter().find(|t| t.name == name).unwrap();
        let props = t.input_schema["properties"].as_object().unwrap();
        for k in sample.as_object().unwrap().keys() {
            assert!(props.contains_key(k), "{name} lacks {k}");
        }
        let r = c.call_tool(tool_call(name, sample)).await.unwrap();
        assert_ne!(r.is_error, Some(true));
        let v = value_of(&r);
        assert!(v.get("error").is_none(), "{name}: {v}");
    }
    c.cancel().await.ok();
    s.stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn author_is_the_mcp_client_name_for_compat_writes() {
    let f = Flow::imported().await;
    f.set_compat(true);
    let s = f.serve().await;
    let c = Named("codex").serve(s.transport()).await.unwrap();
    let a = c
        .call_tool(tool_call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "written without added_by"}),
        ))
        .await
        .unwrap();
    let id = value_of(&a)["drawer_id"].as_str().unwrap().to_string();
    assert_eq!(f.memory(&id).await.author.agent.as_deref(), Some("codex"));
    let b = c
        .call_tool(tool_call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "written with added_by", "added_by": "claude"}),
        ))
        .await
        .unwrap();
    let id = value_of(&b)["drawer_id"].as_str().unwrap().to_string();
    assert_eq!(f.memory(&id).await.author.agent.as_deref(), Some("claude"));
    c.cancel().await.ok();
    s.stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_two_switches_are_independent_and_the_admin_one_is_unchanged() {
    let f = Flow::imported().await;
    let s = f.serve().await;
    let c = Named("test-agent").serve(s.transport()).await.unwrap();
    f.patch("mcp.admin_tools", true);
    f.set_compat(false);
    let n = names(&c).await;
    assert_eq!((n.len(), compat_count(&n)), (11, 0), "{n:?}");
    f.set_compat(true);
    let n = names(&c).await;
    assert_eq!((n.len(), compat_count(&n)), (26, 15), "{n:?}");
    f.patch("mcp.admin_tools", false);
    let n = names(&c).await;
    assert_eq!((n.len(), compat_count(&n)), (22, 15), "{n:?}");
    assert!(!n.contains(&"stats".to_string()));
    let err = error_text(
        c.call_tool(tool_call("stats", json!({})))
            .await
            .unwrap_err(),
    );
    assert!(err.contains("tool not found"), "{err}");
    c.cancel().await.ok();
    s.stop.cancel();
}

/// `alice mcp`'s backend asks the daemon for both switches in one question, fails closed when
/// none answers, and sends the calls through the daemon's route.
#[tokio::test(flavor = "multi_thread")]
async fn the_stdio_backend_follows_both_switches_through_the_daemon_and_fails_closed() {
    let f = Flow::imported().await;
    let s = f.serve().await;
    let server = AliceMcp::new(Backend::Remote(Arc::new(
        RemoteBackend::new(f.svc.home().clone(), Launcher::None, None)
            .with_admin_ttl(Duration::ZERO),
    )));
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    let c = Named("shim-agent").serve(b).await.unwrap();
    f.set_compat(false);
    assert_eq!(compat_count(&names(&c).await), 0);
    let err = error_text(
        c.call_tool(tool_call("mempalace_list_wings", json!({})))
            .await
            .unwrap_err(),
    );
    assert!(err.contains("compat.mempalace"), "{err}");
    f.set_compat(true);
    assert_eq!(compat_count(&names(&c).await), 15);
    let r = c
        .call_tool(tool_call("mempalace_list_wings", json!({})))
        .await
        .unwrap();
    assert!(value_of(&r).get("wings").is_some(), "{}", first_text(&r));
    let w = c
        .call_tool(tool_call(
            "mempalace_add_drawer",
            json!({"wing": "pg-ai", "room": "notes", "content": "through the shim"}),
        ))
        .await
        .unwrap();
    let id = value_of(&w)["drawer_id"].as_str().unwrap().to_string();
    assert_eq!(
        f.memory(&id).await.author.agent.as_deref(),
        Some("shim-agent")
    );
    // The daemon goes away: both switches read off.
    s.stop.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(compat_count(&names(&c).await), 0, "no daemon answers: off");
    c.cancel().await.ok();
}
