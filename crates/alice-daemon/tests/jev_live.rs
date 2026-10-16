//! Live TypeSafe Jev checks (network, costs a fraction of a cent). The key comes only from the
//! environment; load it from the repo's git-ignored .env without printing it:
//!   set -a; . ./.env; set +a
//!   cargo test -p alice-daemon --test jev_live -- --ignored --nocapture
//! Nothing here prints the key, and the fixtures hold request and response bodies only.
use alice_core::judge::Judge;
use alice_core::judge::jev::{FixedPolicy, JevConfig, JevJudge, MemoryJevStore, scenarios};
use alice_core::redact::Redactor;
use alice_core::settings::SecretValue;
use alice_daemon::judge_http::ReqwestTransport;
use std::sync::Arc;
use std::time::Duration;

fn key() -> String {
    std::env::var("TYPESAFE_API_KEY").expect("set TYPESAFE_API_KEY (set -a; . ./.env; set +a)")
}

fn judge(store: Arc<MemoryJevStore>) -> JevJudge {
    let cfg = JevConfig {
        endpoint: "https://api.typesafe.ai/v1/systemone".into(),
        model: "jev-1.13.0".into(),
        api_key: SecretValue::new(key()),
        rate_per_sec: 5.0,
        timeout: Duration::from_secs(30),
        daily_budget_usd: 0.05,
        max_retries: 3,
    };
    JevJudge::new(
        cfg,
        Arc::new(ReqwestTransport::new().unwrap()),
        store,
        Arc::new(FixedPolicy::allow_all(Redactor::builtin())),
    )
    .with_clock(Arc::new(scenarios::fixed_now))
}

#[test]
#[ignore = "network: needs TYPESAFE_API_KEY"]
fn live_compare_round_trip() {
    let store = Arc::new(MemoryJevStore::default());
    let j = judge(store.clone());
    let (new, existing) = scenarios::compare_case();
    let rel = j.compare(&new, &existing).unwrap();
    for r in &rel {
        let total: f64 = r.probs.0.values().sum();
        assert!((total - 1.0).abs() < 1e-6);
        println!(
            "{}: {:?} (forward/reversed agree: {:?})",
            r.existing_id,
            r.probs.best(),
            r.agreement
        );
    }
    let calls = store.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].input_tokens > 0 && calls[0].usd > 0.0);
    println!(
        "input tokens {}, cost ${:.6}, latency {} ms",
        calls[0].input_tokens, calls[0].usd, calls[0].latency_ms
    );
}

/// Writes crates/alice-core/tests/fixtures/jev/*.json: the request the code built (captured
/// with `last_request`) and the live response body. Headers, and so the key, are never written.
#[test]
#[ignore = "network: re-records the Jev fixtures"]
fn record_fixtures() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../alice-core/tests/fixtures/jev");
    let store = Arc::new(MemoryJevStore::default());
    let j = judge(store.clone());
    let mut requests = Vec::new();
    let (new, existing) = scenarios::compare_case();
    let rel = j.compare(&new, &existing).unwrap();
    println!(
        "compare: {:?}",
        rel.iter().map(|r| r.probs.best()).collect::<Vec<_>>()
    );
    requests.push(("compare", j.last_request().unwrap()));
    let kind = j.classify_kind(&scenarios::kind_case()).unwrap();
    println!("kind: {:?}", kind.best());
    requests.push(("kind", j.last_request().unwrap()));
    let (q, results) = scenarios::relevance_case();
    let grades = j.grade_relevance("alice", &q, &results).unwrap();
    println!(
        "relevance: {:?}",
        grades.iter().map(|g| g.best()).collect::<Vec<_>>()
    );
    requests.push(("relevance", j.last_request().unwrap()));
    let yes = j
        .classify_constant(scenarios::CLASSIFY_STATE, scenarios::CLASSIFY_OPTIONS)
        .unwrap();
    println!("classify: {:?}", yes.best());
    requests.push(("classify", j.last_request().unwrap()));
    let calls = store.calls();
    assert_eq!(calls.len(), 4, "each request went to the API once");
    let total: f64 = calls.iter().map(|c| c.usd).sum();
    println!("four calls, ${total:.6} in all");
    let key = key();
    for ((name, request), call) in requests.into_iter().zip(calls) {
        let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
        let response: serde_json::Value = serde_json::from_str(&call.response_json).unwrap();
        let text = serde_json::to_string_pretty(
            &serde_json::json!({"recorded": true, "request": request, "response": response}),
        )
        .unwrap();
        assert!(!text.contains(&key), "the key never goes into a fixture");
        assert!(
            !text.contains("Bearer") && !text.contains("apikey_"),
            "no header or key-shaped text goes into a fixture"
        );
        std::fs::write(dir.join(format!("{name}.json")), text + "\n").unwrap();
    }
}
