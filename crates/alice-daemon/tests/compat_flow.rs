//! The MemPalace compatibility tools, called as plain functions of the service (no MCP, no
//! HTTP). `Flow::imported()` imports the synthetic palace of the import tests (same drawers,
//! including the knowledge duplicate pair `k_dup_a`/`k_dup_b`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::import::fixture::{DrawerSpec, build_sqlite_exact};
use alice_core::import::palace::DrawerMeta;
use alice_core::import::{ImportRequest, ImportState};
use alice_daemon::compat::CompatCall;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use serde_json::{Value, json};

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
            text: "Set up the import today with alice".into(),
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
