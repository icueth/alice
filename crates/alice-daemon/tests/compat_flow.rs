//! The MemPalace compatibility tools, called as plain functions of the service (no MCP, no
//! HTTP). `Flow::imported()` imports the synthetic palace of the import tests (same drawers,
//! including the knowledge duplicate pair `k_dup_a`/`k_dup_b`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::import::fixture::build_sqlite_exact;
use alice_core::import::{ImportRequest, ImportState};
use alice_daemon::compat::CompatCall;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use serde_json::{Value, json};

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
        Flow {
            _root: root,
            _palace_root: palace_root,
            _palace: palace,
            svc,
        }
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
        (
            "mempalace_add_drawer",
            json!({"wing": "pg_ai", "room": "x", "content": "y"}),
        ),
        ("mempalace_mine", json!({})),
    ] {
        f.call(tool, args).await;
    }
    assert_eq!(counts(), before);
}
