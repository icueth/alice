use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::http::router;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Srv {
    _d: tempfile::TempDir,
    port: u16,
    token: String,
    shutdown: CancellationToken,
    http: reqwest::Client,
}

impl Srv {
    async fn start() -> Srv {
        alice_daemon::install_tls();
        let d = tempfile::tempdir().unwrap();
        let svc = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "t".repeat(64);
        let shutdown = CancellationToken::new();
        let app = router(svc, token.clone(), port, shutdown.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Srv {
            _d: d,
            port,
            token,
            shutdown,
            http: reqwest::Client::new(),
        }
    }
    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.port)
    }
    async fn post<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.http
            .post(self.url(p))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_is_public_everything_else_needs_the_token() {
    let s = Srv::start().await;
    let ping: serde_json::Value = s
        .http
        .get(s.url("/api/v1/ping"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ping["service"], "alice");
    assert!(ping["instance"].as_str().unwrap().len() == 26);
    assert_eq!(
        s.http
            .get(s.url("/api/v1/health"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        s.http
            .get(s.url("/api/v1/health"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        s.http
            .get(s.url("/api/v1/health"))
            .bearer_auth(&s.token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn host_and_origin_are_checked() {
    let s = Srv::start().await;
    let r = s
        .http
        .get(s.url("/api/v1/health"))
        .bearer_auth(&s.token)
        .header("Host", "evil.example:80")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = s
        .http
        .get(s.url("/api/v1/ping"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = s
        .http
        .get(s.url("/api/v1/health"))
        .bearer_auth(&s.token)
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = s
        .http
        .get(s.url("/api/v1/health"))
        .bearer_auth(&s.token)
        .header("Origin", format!("http://127.0.0.1:{}", s.port))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = s
        .http
        .get(s.url("/api/v1/health"))
        .bearer_auth(&s.token)
        .header("Host", format!("localhost:{}", s.port))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "localhost:<port> is an allowed Host");
}

#[tokio::test(flavor = "multi_thread")]
async fn remember_recall_and_errors_over_http() {
    let s = Srv::start().await;
    let r: RememberResponse = s
        .post(
            "/api/v1/remember",
            &RememberRequest {
                content: "HTTP round trip works".into(),
                ..Default::default()
            },
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r.status, RememberStatus::New);
    let recall: RecallResponse = s
        .post(
            "/api/v1/recall",
            &RecallRequest {
                query: "round trip".into(),
                format: OutputFormat::Json,
                ..Default::default()
            },
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(recall.results[0].id, r.id.unwrap());
    let bad = s
        .post(
            "/api/v1/remember",
            &RememberRequest {
                content: " ".into(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(bad.status(), 400);
    let body: serde_json::Value = bad.json().await.unwrap();
    assert_eq!(body["kind"], "invalid");
    assert!(body["error"].as_str().unwrap().contains("empty"));
    let missing = s
        .post(
            "/api/v1/update",
            &serde_json::json!({"id": "NOPE", "action": "archive"}),
        )
        .await;
    assert_eq!(missing.status(), 404);
    let body: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(body["kind"], "not_found");
}

/// Every documented route answers with the token (and a typed body where one is expected).
#[tokio::test(flavor = "multi_thread")]
async fn every_route_is_served() {
    let s = Srv::start().await;
    let saved: RememberResponse = s
        .post(
            "/api/v1/remember",
            &RememberRequest {
                content: "routes are wired".into(),
                ..Default::default()
            },
        )
        .await
        .json()
        .await
        .unwrap();
    let id = saved.id.unwrap();
    let health: Health = s
        .http
        .get(s.url("/api/v1/health"))
        .bearer_auth(&s.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health.memories, 1);
    let projects: serde_json::Value = s
        .http
        .get(s.url("/api/v1/projects"))
        .bearer_auth(&s.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(projects[0]["slug"], "global");
    let ctx: ContextResponse = s
        .post("/api/v1/context", &ContextRequest::default())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ctx.project, "global");
    let got: GetResponse = s
        .post(
            "/api/v1/get",
            &GetRequest {
                ids: vec![id.clone()],
                include_provenance: false,
            },
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(got.memories[0].memory.id, id);
    let updated = s
        .post(
            "/api/v1/update",
            &serde_json::json!({"id": id, "action": "archive"}),
        )
        .await;
    assert_eq!(updated.status(), 200);
    let asserted = s
        .post(
            "/api/v1/facts/assert",
            &serde_json::json!({"subject": "alice", "predicate": "uses", "object": "axum"}),
        )
        .await;
    assert_eq!(asserted.status(), 200);
    let facts: FactsResponse = s
        .post(
            "/api/v1/facts/query",
            &serde_json::json!({"entity": "alice"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(facts.facts.len(), 1);
    let ingested: serde_json::Value = s
        .post(
            "/api/v1/ingest",
            &IngestRequest {
                source: SourceInput {
                    uri: "session-1".into(),
                    ..Default::default()
                },
                chunks: vec![ChunkInput {
                    ord: 0,
                    text: "raw transcript line".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ingested["inserted"], 1);
}

/// Bodies the API cannot parse get the same JSON error shape as every other error.
#[tokio::test(flavor = "multi_thread")]
async fn unparseable_bodies_get_actionable_json_errors() {
    let s = Srv::start().await;
    for (body, content_type) in [
        ("{", "application/json"),
        ("{\"kind\": \"decision\"}", "application/json"),
        ("{\"content\": \"x\"}", "text/plain"),
    ] {
        let r = s
            .http
            .post(s.url("/api/v1/remember"))
            .bearer_auth(&s.token)
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{body}");
        let e: serde_json::Value = r.json().await.unwrap();
        assert_eq!(e["kind"], "invalid", "{e}");
        assert!(e["error"].as_str().unwrap().contains("JSON"), "{e}");
    }
}

/// Guard refusals are JSON too and say what to do next.
#[tokio::test(flavor = "multi_thread")]
async fn guard_refusals_explain_themselves() {
    let s = Srv::start().await;
    let r = s.http.get(s.url("/api/v1/health")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer");
    let e: serde_json::Value = r.json().await.unwrap();
    assert_eq!(e["kind"], "unauthorized");
    assert!(e["error"].as_str().unwrap().contains("token"), "{e}");
    let r = s
        .http
        .get(s.url("/api/v1/ping"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let e: serde_json::Value = r.json().await.unwrap();
    assert_eq!(e["kind"], "forbidden");
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_shutdown_needs_the_token_and_cancels() {
    let s = Srv::start().await;
    let r = s
        .http
        .post(s.url("/api/v1/admin/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(!s.shutdown.is_cancelled());
    let r = s
        .http
        .post(s.url("/api/v1/admin/shutdown"))
        .bearer_auth(&s.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    assert!(s.shutdown.is_cancelled());
}
