use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::http::router;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A daemon's HTTP API on a free port with a throwaway home. This is the one definition of the
/// helpers: later tests (settings, reviews, doctor) use them by these names and add none.
struct Srv {
    _d: tempfile::TempDir,
    #[allow(dead_code)] // read by later tasks' tests
    home: AliceHome,
    #[allow(dead_code)]
    svc: Arc<Service>,
    port: u16,
    token: String,
    shutdown: CancellationToken,
    http: reqwest::Client,
}

#[allow(dead_code)] // not every helper is used by every task's tests
impl Srv {
    async fn start() -> Srv {
        Self::start_with(ServiceOptions::default()).await
    }
    /// `start()` with chosen service options (settings, transports, a short health tick).
    async fn start_with(opts: ServiceOptions) -> Srv {
        alice_daemon::install_tls();
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            opts,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "t".repeat(64);
        let shutdown = CancellationToken::new();
        let app = router(svc.clone(), token.clone(), port, shutdown.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Srv {
            _d: d,
            home,
            svc,
            port,
            token,
            shutdown,
            http: reqwest::Client::new(),
        }
    }
    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.port)
    }
    /// An authorised request with any method; the helpers below are shorthands.
    fn req(&self, method: reqwest::Method, p: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(p))
            .bearer_auth(&self.token)
    }
    async fn post<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.req(reqwest::Method::POST, p)
            .json(body)
            .send()
            .await
            .unwrap()
    }
    async fn get(&self, p: &str) -> reqwest::Response {
        self.req(reqwest::Method::GET, p).send().await.unwrap()
    }
    async fn patch<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.req(reqwest::Method::PATCH, p)
            .json(body)
            .send()
            .await
            .unwrap()
    }
    async fn put<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.req(reqwest::Method::PUT, p)
            .json(body)
            .send()
            .await
            .unwrap()
    }
    async fn delete(&self, p: &str) -> reqwest::Response {
        self.req(reqwest::Method::DELETE, p).send().await.unwrap()
    }
    /// GET that must succeed, decoded.
    async fn get_json<R: serde::de::DeserializeOwned>(&self, p: &str) -> R {
        ok_json(self.get(p).await).await
    }
    /// POST that must succeed, decoded.
    async fn post_json<R: serde::de::DeserializeOwned, T: serde::Serialize>(
        &self,
        p: &str,
        body: &T,
    ) -> R {
        ok_json(self.post(p, body).await).await
    }
}

/// The body of a 2xx response, decoded; any other status fails the test, showing the body.
async fn ok_json<R: serde::de::DeserializeOwned>(r: reqwest::Response) -> R {
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "{status}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// The next SSE message's JSON (its `data:` line) within 5 s; keep-alive comments are skipped.
async fn next_item(stream: &mut reqwest::Response, buf: &mut String) -> serde_json::Value {
    loop {
        if let Some(end) = buf.find("\n\n") {
            let msg: String = buf.drain(..end + 2).collect();
            if let Some(data) = msg.lines().find_map(|l| l.strip_prefix("data:")) {
                return serde_json::from_str(data.trim()).unwrap();
            }
            continue;
        }
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.chunk())
            .await
            .expect("an SSE message within 5 s")
            .unwrap()
            .expect("the stream is open");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn events_list_undo_and_stream() {
    let s = Srv::start().await;
    let mut stream = s.get("/api/v1/events/stream").await;
    assert_eq!(stream.status(), 200);
    assert_eq!(
        stream.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let mut buf = String::new();
    assert_eq!(
        next_item(&mut stream, &mut buf).await["type"],
        "health",
        "the stream opens with the health"
    );
    let r: RememberResponse = s
        .post_json(
            "/api/v1/remember",
            &serde_json::json!({"content": "stream me"}),
        )
        .await;
    let id = r.id.unwrap();
    let item = loop {
        let v = next_item(&mut stream, &mut buf).await;
        if v["type"] == "event" {
            break v;
        }
    };
    assert_eq!(item["event"]["target_id"], id.as_str(), "{item}");
    assert_eq!(item["event"]["actor"], "user", "{item}");
    let evs: Vec<EventRow> = s.get_json(&format!("/api/v1/events?target_id={id}")).await;
    assert_eq!(evs[0].action, "memory.create");
    let u: UndoResponse = s
        .post_json("/api/v1/undo", &serde_json::json!({"event_id": evs[0].id}))
        .await;
    assert!(u.undone, "{}", u.message);
    assert_eq!(u.reverted, vec![evs[0].id.clone()]);
    let again = s
        .post("/api/v1/undo", &serde_json::json!({"event_id": evs[0].id}))
        .await;
    assert_eq!(again.status(), 400);
    let again: serde_json::Value = again.json().await.unwrap();
    assert_eq!(again["kind"], "invalid");
    assert!(again["error"].as_str().unwrap().contains("already undone"));
    // The undo itself is pushed: what it reverted and its marker.
    let mut pushed = Vec::new();
    while pushed.len() < 2 {
        let v = next_item(&mut stream, &mut buf).await;
        if v["type"] == "event" {
            pushed.push(v["event"]["action"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(pushed, ["undo:memory.create", "undo"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stream_sends_health_again_only_when_it_changes() {
    let s = Srv::start_with(ServiceOptions {
        health_tick: std::time::Duration::from_millis(100),
        ..Default::default()
    })
    .await;
    let mut stream = s.get("/api/v1/events/stream").await;
    let mut buf = String::new();
    // The stream opens with the health; start-up may still flip `index_ready` once.
    let mut first = next_item(&mut stream, &mut buf).await;
    assert_eq!(first["type"], "health");
    while first["health"]["index_ready"] != true {
        first = next_item(&mut stream, &mut buf).await;
    }
    assert_eq!(first["health"]["memories"], 0);
    // Nothing changes now: several ticks pass without another health item.
    let quiet = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        next_item(&mut stream, &mut buf),
    )
    .await;
    assert!(quiet.is_err(), "unchanged health was sent again: {quiet:?}");
    let _: RememberResponse = s
        .post_json(
            "/api/v1/remember",
            &serde_json::json!({"content": "health moves"}),
        )
        .await;
    let changed = loop {
        let v = next_item(&mut stream, &mut buf).await;
        if v["type"] == "health" && v["health"]["memories"] == 1 {
            break v;
        }
    };
    assert_eq!(changed["health"]["memories"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_returns_its_event_id() {
    let s = Srv::start().await;
    let r: RememberResponse = s
        .post_json(
            "/api/v1/remember",
            &serde_json::json!({"content": "archive me"}),
        )
        .await;
    let id = r.id.unwrap();
    let full: MemoryFull = s
        .post_json(
            "/api/v1/update",
            &serde_json::json!({"id": id, "action": "archive"}),
        )
        .await;
    let ev = full.event_id.expect("update answers with its event id");
    let evs: Vec<EventRow> = s.get_json(&format!("/api/v1/events?target_id={id}")).await;
    assert_eq!(
        (evs[0].id.as_str(), evs[0].action.as_str()),
        (ev.as_str(), "memory.archive")
    );
    let get: serde_json::Value = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [id]}))
        .await;
    assert!(
        get["memories"][0].get("event_id").is_none(),
        "only update responses carry it"
    );
}

/// Undo never overwrites a later change: the answer is a review item, not an error.
#[tokio::test(flavor = "multi_thread")]
async fn undo_after_a_later_change_answers_with_a_review_item() {
    let s = Srv::start().await;
    let r: RememberResponse = s
        .post_json(
            "/api/v1/remember",
            &serde_json::json!({"content": "first text"}),
        )
        .await;
    let id = r.id.unwrap();
    let edit = |body: &'static str| serde_json::json!({"id": id, "action": "edit", "body": body});
    let first: MemoryFull = s.post_json("/api/v1/update", &edit("second text")).await;
    let _: MemoryFull = s.post_json("/api/v1/update", &edit("third text")).await;
    let u: UndoResponse = s
        .post_json(
            "/api/v1/undo",
            &serde_json::json!({"event_id": first.event_id.unwrap()}),
        )
        .await;
    assert!(!u.undone && u.reverted.is_empty(), "{u:?}");
    let review = u.review_id.expect("a review item asks");
    assert!(
        u.message.contains(&review) && u.message.contains(&id),
        "{}",
        u.message
    );
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [id]}))
        .await;
    assert_eq!(got.memories[0].memory.body, "third text");
    let missing = s
        .post(
            "/api/v1/undo",
            &serde_json::json!({"event_id": "01K7NOSUCHEVENT00000000000"}),
        )
        .await;
    assert_eq!(missing.status(), 404);
}

/// History filters by actor and pages with `before`; a bad query says how to write one.
#[tokio::test(flavor = "multi_thread")]
async fn events_filter_page_and_explain_bad_queries() {
    let s = Srv::start().await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let r: RememberResponse = s
            .post_json(
                "/api/v1/remember",
                &serde_json::json!({"content": format!("note {i}"), "author": {"agent": "codex"}}),
            )
            .await;
        ids.push(r.id.unwrap());
    }
    let mine: Vec<EventRow> = s.get_json("/api/v1/events?actor=agent%3Acodex").await;
    assert_eq!(mine.len(), 3);
    let users: Vec<EventRow> = s.get_json("/api/v1/events?actor=user").await;
    assert!(users.is_empty());
    let page: Vec<EventRow> = s
        .get_json(&format!("/api/v1/events?limit=1&before={}", mine[0].id))
        .await;
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, mine[1].id);
    for bad in [
        "/api/v1/events?limit=many",
        "/api/v1/events?before=01K7NOSUCHEVENT00000000000",
    ] {
        let r = s.get(bad).await;
        assert_eq!(r.status(), 400, "{bad}");
        let e: serde_json::Value = r.json().await.unwrap();
        assert_eq!(e["kind"], "invalid", "{e}");
    }
    let e: serde_json::Value = s
        .get("/api/v1/events?limit=many")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        e["error"].as_str().unwrap().contains("/api/v1/events?"),
        "{e}"
    );
}

/// The stream sits behind the same guard as every route, and ends when the daemon stops so
/// a graceful shutdown never waits for a UI that stays connected.
#[tokio::test(flavor = "multi_thread")]
async fn the_stream_is_guarded_and_ends_on_shutdown() {
    let s = Srv::start().await;
    let r = s
        .http
        .get(s.url("/api/v1/events/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = s
        .req(reqwest::Method::GET, "/api/v1/events/stream")
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let mut stream = s.get("/api/v1/events/stream").await;
    let mut buf = String::new();
    assert_eq!(next_item(&mut stream, &mut buf).await["type"], "health");
    s.shutdown.cancel();
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match stream.chunk().await {
                Ok(Some(_)) => continue,
                other => return other.map(|_| ()),
            }
        }
    })
    .await;
    assert!(
        matches!(end, Ok(Ok(()))),
        "the stream ends on shutdown: {end:?}"
    );
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

/// Review focus #3: the ping proves the daemon knows the token (HMAC over the caller's nonce)
/// without revealing it, so a process replaying a recorded instance id cannot pass for it.
#[tokio::test(flavor = "multi_thread")]
async fn ping_proves_knowledge_of_the_token() {
    use alice_daemon::discovery::{ping_proof, verify_ping_proof};
    use alice_daemon::http::PingResponse;
    let s = Srv::start().await;
    let nonce = "00112233445566778899aabbccddeeff";
    let ping: PingResponse = s
        .http
        .get(s.url(&format!("/api/v1/ping?nonce={nonce}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let proof = ping.proof.expect("a nonce gets a proof");
    assert_eq!(proof, ping_proof(&s.token, nonce, &ping.instance));
    assert!(verify_ping_proof(&s.token, nonce, &ping.instance, &proof));
    assert_ne!(proof, ping_proof("wrong-token", nonce, &ping.instance));
    assert!(!verify_ping_proof(
        "wrong-token",
        nonce,
        &ping.instance,
        &proof
    ));
    assert!(!proof.contains(&s.token));
    // Relay: an impostor on a stale port forwards the client's nonce here and answers with
    // the stale instance id from daemon.json plus this daemon's proof. It must not verify.
    assert!(!verify_ping_proof(
        &s.token,
        nonce,
        "01K7STALESTALESTALESTALEST",
        &proof
    ));

    // Nonce length boundaries: 16 and 64 hex characters are accepted.
    for n in ["0123456789abcdef".to_string(), "ab".repeat(32)] {
        let p: PingResponse = s
            .http
            .get(s.url(&format!("/api/v1/ping?nonce={n}")))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            verify_ping_proof(&s.token, &n, &p.instance, &p.proof.unwrap()),
            "nonce of {} chars",
            n.len()
        );
    }
    // Two nonces are ambiguous: refused.
    let r = s
        .http
        .get(s.url(
            "/api/v1/ping?nonce=00112233445566778899aabbccddeeff&nonce=ffeeddccbbaa99887766554433221100",
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "duplicate nonce");
    let e: serde_json::Value = r.json().await.unwrap();
    assert_eq!(e["kind"], "invalid", "{e}");

    let plain: serde_json::Value = s
        .http
        .get(s.url("/api/v1/ping"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(plain.get("proof").is_none(), "{plain}");

    for bad in [
        "0123456789abcde".to_string(), // 15 hex chars
        "0".repeat(65),
        "zz112233445566778899aabbccddeeff".to_string(),
        String::new(),
    ] {
        let r = s
            .http
            .get(s.url(&format!("/api/v1/ping?nonce={bad}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "nonce {bad:?}");
        let e: serde_json::Value = r.json().await.unwrap();
        assert_eq!(e["kind"], "invalid", "{e}");
        assert!(e["error"].as_str().unwrap().contains("nonce"), "{e}");
    }
}

/// DNS rebinding: a page on evil.example resolved to 127.0.0.1 sends its own Host and Origin.
#[tokio::test(flavor = "multi_thread")]
async fn dns_rebinding_shapes_are_refused() {
    let s = Srv::start().await;
    for path in ["/api/v1/ping", "/api/v1/health", "/api/v1/events/stream"] {
        let r = s
            .http
            .get(s.url(path))
            .bearer_auth(&s.token)
            .header("Host", format!("evil.example:{}", s.port))
            .header("Origin", format!("http://evil.example:{}", s.port))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{path}");
        let r = s
            .http
            .get(s.url(path))
            .bearer_auth(&s.token)
            .header("Origin", format!("http://localhost:{}", s.port))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            403,
            "only http://127.0.0.1:<port> is an allowed origin ({path})"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_prefix_or_extension_is_refused() {
    let s = Srv::start().await;
    for wrong in [
        s.token[..63].to_string(),
        format!("{}t", s.token),
        s.token[..1].to_string(),
    ] {
        let r = s
            .http
            .get(s.url("/api/v1/health"))
            .bearer_auth(&wrong)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "token of length {}", wrong.len());
    }
}
