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
    async fn start_with(mut opts: ServiceOptions) -> Srv {
        alice_daemon::install_tls();
        // No `ALICE_*` variable of the developer's shell reaches the settings under test.
        opts.settings_env.get_or_insert_with(Vec::new);
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

// ------------------------------------------------------------------ Task 6: the settings API

/// Options whose secret status ignores the machine's environment (a developer's own
/// `TYPESAFE_API_KEY` must not change what a test sees).
fn quiet() -> ServiceOptions {
    ServiceOptions {
        secret_env: Some(Arc::new(|_| None)),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_api_round_trip_validation_schema_and_secrets() {
    let s = Srv::start_with(quiet()).await;
    let view: serde_json::Value = s.get_json("/api/v1/settings").await;
    assert_eq!(view["settings"]["judge"]["provider"], "heuristic");
    assert!(view["keys"].as_array().unwrap().len() >= 50);
    let schema: serde_json::Value = s.get_json("/api/v1/settings/schema").await;
    assert!(schema["properties"]["judge"].is_object());
    let ok: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"judge.review_threshold": 0.6}),
        )
        .await,
    )
    .await;
    assert_eq!(ok["view"]["settings"]["judge"]["review_threshold"], 0.6);
    assert_eq!(ok["changed"], serde_json::json!(["judge.review_threshold"]));
    let bad = s
        .patch(
            "/api/v1/settings",
            &serde_json::json!({"judge.review_threshold": 0.95}),
        )
        .await;
    assert_eq!(bad.status(), 400);
    let body: serde_json::Value = bad.json().await.unwrap();
    assert_eq!(body["kind"], "invalid");
    assert_eq!(body["errors"][0]["key"], "judge.review_threshold");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("judge.review_threshold"),
        "{body}"
    );
    let put: serde_json::Value = ok_json(
        s.put(
            "/api/v1/secrets/jev_api_key",
            &serde_json::json!({"value": concat!("ts_", "secret_value_1234")}),
        )
        .await,
    )
    .await;
    assert_eq!(put["hint"], "••••1234");
    let view = s.get("/api/v1/settings").await.text().await.unwrap();
    assert!(
        !view.contains("ts_secret_value"),
        "the API never returns a secret"
    );
    assert!(view.contains("jev_api_key"), "its status is there");
    let unknown = s
        .put("/api/v1/secrets/github", &serde_json::json!({"value": "x"}))
        .await;
    assert_eq!(unknown.status(), 400);
    let gone = s.delete("/api/v1/secrets/jev_api_key").await;
    assert!(gone.status().is_success(), "{}", gone.status());
    let gone: serde_json::Value = gone.json().await.unwrap();
    assert_eq!(gone["deleted"], true);
    let view: serde_json::Value = s.get_json("/api/v1/settings").await;
    assert_eq!(view["secrets"]["jev_api_key"]["set"], false);
}

/// Every key is writable through the API with validation: an unknown key, a wrong type, a
/// read-only key and a body that is not an object are each a 400 that names the problem; a
/// `null` removes the key from config.toml.
#[tokio::test(flavor = "multi_thread")]
async fn patch_validates_every_shape_and_null_removes_a_key() {
    let s = Srv::start_with(quiet()).await;
    for (body, key) in [
        (serde_json::json!({"judge.nope": 1}), "judge.nope"),
        (
            serde_json::json!({"judge.review_threshold": "high"}),
            "judge.review_threshold",
        ),
        (
            serde_json::json!({"storage.data_dir": "/tmp"}),
            "storage.data_dir",
        ),
        (
            serde_json::json!({"search.relevance_floor": 7}),
            "search.relevance_floor",
        ),
    ] {
        let r = s.patch("/api/v1/settings", &body).await;
        assert_eq!(r.status(), 400, "{body}");
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["kind"], "invalid", "{v}");
        assert_eq!(v["errors"][0]["key"], key, "{v}");
    }
    // Not an object: the API's own error shape.
    let r = s
        .patch("/api/v1/settings", &serde_json::json!([1, 2]))
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["kind"],
        "invalid"
    );
    // Nothing above touched the settings or the file.
    assert!(!s.home.config_file().exists());
    // Set, then remove: back to the default, out of config.toml.
    let r: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"mcp.admin_tools": true}),
        )
        .await,
    )
    .await;
    assert_eq!(r["view"]["settings"]["mcp"]["admin_tools"], true);
    assert_eq!(r["view"]["sources"]["mcp.admin_tools"]["kind"], "file");
    assert!(
        std::fs::read_to_string(s.home.config_file())
            .unwrap()
            .contains("admin_tools = true")
    );
    let r: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"mcp.admin_tools": null}),
        )
        .await,
    )
    .await;
    assert_eq!(r["view"]["settings"]["mcp"]["admin_tools"], false);
    assert_eq!(r["view"]["sources"]["mcp.admin_tools"]["kind"], "default");
    assert!(
        !std::fs::read_to_string(s.home.config_file())
            .unwrap()
            .contains("admin_tools")
    );
    // Keys that need a restart say so.
    let r: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"server.port": 4545}),
        )
        .await,
    )
    .await;
    assert_eq!(r["restart_required"], serde_json::json!(["server.port"]));
    assert_eq!(
        r["view"]["restart_pending"],
        serde_json::json!(["server.port"])
    );
}

/// An invalid edit of config.toml leaves the previous settings in force; the error shows in
/// the settings view and in health, and a good edit clears it.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_edit_shows_in_the_view_and_in_health() {
    let s = Srv::start_with(ServiceOptions {
        settings_poll: std::time::Duration::from_millis(50),
        ..quiet()
    })
    .await;
    let _: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"judge.review_threshold": 0.6}),
        )
        .await,
    )
    .await;
    std::fs::write(
        s.home.config_file(),
        "[judge]\nreview_threshold = 0.9\nauto_apply_threshold = 0.6\n",
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let view = loop {
        let v: serde_json::Value = s.get_json("/api/v1/settings").await;
        if !v["errors"].as_array().unwrap().is_empty() {
            break v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the watcher never showed the invalid edit: {v}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(view["errors"][0]["key"], "judge.review_threshold");
    assert_eq!(
        view["settings"]["judge"]["review_threshold"], 0.6,
        "the previous settings stay in force"
    );
    let health: serde_json::Value = s.get_json("/api/v1/health").await;
    assert_eq!(
        health["settings_errors"][0]["key"],
        "judge.review_threshold"
    );
    assert!(health.get("config_degraded").is_none(), "{health}");
    std::fs::write(s.home.config_file(), "[judge]\nreview_threshold = 0.7\n").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let v: serde_json::Value = s.get_json("/api/v1/settings").await;
        if v["errors"].as_array().unwrap().is_empty() {
            assert_eq!(v["settings"]["judge"]["review_threshold"], 0.7);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the watcher never cleared the error after the good edit: {v}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let health: serde_json::Value = s.get_json("/api/v1/health").await;
    assert!(health.get("settings_errors").is_none(), "{health}");
}

/// A change pushes a `settings` item on the event stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_settings_change_is_pushed_on_the_stream() {
    let s = Srv::start_with(quiet()).await;
    let mut stream = s.get("/api/v1/events/stream").await;
    let mut buf = String::new();
    assert_eq!(next_item(&mut stream, &mut buf).await["type"], "health");
    let _: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"search.relevance_floor": 0.75}),
        )
        .await,
    )
    .await;
    let item = loop {
        let v = next_item(&mut stream, &mut buf).await;
        if v["type"] == "settings" {
            break v;
        }
    };
    assert_eq!(
        item["changed"],
        serde_json::json!(["search.relevance_floor"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_provider_test_and_secret_errors_speak_plainly() {
    let s = Srv::start_with(quiet()).await;
    let r: serde_json::Value = ok_json(
        s.post("/api/v1/settings/test/embedding", &serde_json::json!({}))
            .await,
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["latency_ms"].is_u64());
    let r: serde_json::Value = ok_json(
        s.post("/api/v1/settings/test/judge", &serde_json::json!({}))
            .await,
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let r = s
        .post("/api/v1/settings/test/dropbox", &serde_json::json!({}))
        .await;
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("judge or embedding"),
        "{v}"
    );
    // A secret that is not one line is refused, and the refusal never repeats it.
    let marker = "ZQXMARKER-line-one\nline-two";
    let r = s
        .put(
            "/api/v1/secrets/embedding_api_key",
            &serde_json::json!({"value": marker}),
        )
        .await;
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(!text.contains("ZQXMARKER"), "{text}");
    // A value of the wrong type is refused without quoting it either.
    let r = s
        .put(
            "/api/v1/secrets/embedding_api_key",
            &serde_json::json!({"value": 9_876_543_210_u64}),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert!(!r.text().await.unwrap().contains("9876543210"));
    // Wrong body shape: the API's error body.
    let r = s
        .put(
            "/api/v1/secrets/embedding_api_key",
            &serde_json::json!({"key": "x"}),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["kind"],
        "invalid"
    );
}

/// The new routes sit behind the same token, Host and Origin guard as the rest.
#[tokio::test(flavor = "multi_thread")]
async fn the_settings_routes_are_guarded() {
    let s = Srv::start_with(quiet()).await;
    let routes: [(reqwest::Method, &str); 12] = [
        (reqwest::Method::GET, "/api/v1/reviews"),
        (reqwest::Method::GET, "/api/v1/reviews/X"),
        (reqwest::Method::POST, "/api/v1/reviews/X/resolve"),
        (reqwest::Method::POST, "/api/v1/memories/merge"),
        (reqwest::Method::GET, "/api/v1/stats"),
        (reqwest::Method::GET, "/api/v1/settings"),
        (reqwest::Method::PATCH, "/api/v1/settings"),
        (reqwest::Method::GET, "/api/v1/settings/schema"),
        (reqwest::Method::POST, "/api/v1/settings/test/judge"),
        (reqwest::Method::PUT, "/api/v1/secrets/jev_api_key"),
        (reqwest::Method::DELETE, "/api/v1/secrets/jev_api_key"),
        (reqwest::Method::POST, "/api/v1/settings/test/embedding"),
    ];
    for (method, path) in routes {
        let anon = s
            .http
            .request(method.clone(), s.url(path))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(anon.status(), 401, "{method} {path} without a token");
        let wrong_host = s
            .req(method.clone(), path)
            .header("Host", "evil.example:80")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            wrong_host.status(),
            403,
            "{method} {path} with a foreign Host"
        );
        let wrong_origin = s
            .req(method.clone(), path)
            .header("Origin", "https://evil.example")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            wrong_origin.status(),
            403,
            "{method} {path} from a web page"
        );
    }
    assert!(
        !s.home.config_file().exists(),
        "no refused request changed anything"
    );
}

/// Over HTTP: a loosening applies at once and the answer says so; a tightening and an
/// unrelated change do not; keys nothing reads yet are marked.
#[tokio::test(flavor = "multi_thread")]
async fn the_patch_answer_discloses_a_loosening_and_unused_keys() {
    let s = Srv::start_with(quiet()).await;
    let r: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"privacy.redact": false}),
        )
        .await,
    )
    .await;
    assert_eq!(r["view"]["settings"]["privacy"]["redact"], false);
    let notes = r["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("privacy.redact")
                && n.as_str().unwrap().contains("at once")),
        "{notes:?}"
    );
    let r: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"privacy.redact": true}),
        )
        .await,
    )
    .await;
    assert_eq!(r["notes"], serde_json::json!([]));
    let r: serde_json::Value = ok_json(
        s.patch(
            "/api/v1/settings",
            &serde_json::json!({"embedding.endpoint": "http://127.0.0.1:11434"}),
        )
        .await,
    )
    .await;
    assert!(
        r["notes"][0].as_str().unwrap().contains("not used yet"),
        "{r}"
    );
    let view: serde_json::Value = s.get_json("/api/v1/settings").await;
    let flag = |k: &str| {
        view["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == k)
            .unwrap()["not_used_yet"]
            .clone()
    };
    assert_eq!(flag("embedding.provider"), true);
    assert_eq!(flag("embedding.batch_size"), false);
}

/// A write that `block` mode refuses is a 400 naming the detector, not a server error.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_secret_is_a_400_that_names_the_detector() {
    let s = Srv::start().await;
    let mut change = serde_json::Map::new();
    change.insert("privacy.redact_mode".into(), serde_json::json!("block"));
    s.svc.settings_handle().patch(&change).unwrap();
    let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let resp = s
        .post(
            "/api/v1/remember",
            &serde_json::json!({"content": format!("key {token}")}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("github_token") && !body.contains(token),
        "{body}"
    );
}

/// Task 11: the review inbox over HTTP. An undo that finds the target changed asks; the item
/// lists its choices; "apply" restores the first text; a second answer is refused.
#[tokio::test(flavor = "multi_thread")]
async fn the_review_inbox_lists_resolves_once_and_stats_follow() {
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
    assert!(!u.undone);
    let review = u.review_id.unwrap();

    let stats: Stats = s.get_json("/api/v1/stats").await;
    assert_eq!(stats.reviews_open, 1);
    let ctx: ContextResponse = s.post_json("/api/v1/context", &serde_json::json!({})).await;
    assert!(
        ctx.text.contains("1 item(s) wait for a decision"),
        "{}",
        ctx.text
    );

    let list: Vec<ReviewItem> = s.get_json("/api/v1/reviews").await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, review);
    assert_eq!(
        list[0]
            .choices
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["apply", "dismiss"]
    );
    assert_eq!(list[0].subjects[0].id, id, "the memory the item is about");
    let one: ReviewItem = s.get_json(&format!("/api/v1/reviews/{review}")).await;
    assert_eq!(one, list[0]);
    assert_eq!(s.get("/api/v1/reviews/NOSUCHITEM").await.status(), 404);
    let bad = s.get("/api/v1/reviews?limit=lots").await;
    assert_eq!(bad.status(), 400);
    assert!(
        bad.text()
            .await
            .unwrap()
            .contains("/api/v1/reviews?status=")
    );

    // A choice the type does not offer is refused and the item stays open.
    let wrong = s
        .post(
            &format!("/api/v1/reviews/{review}/resolve"),
            &serde_json::json!({"choice": "merge"}),
        )
        .await;
    assert_eq!(wrong.status(), 400);
    assert!(wrong.text().await.unwrap().contains("apply, dismiss"));

    let done: ResolveResponse = s
        .post_json(
            &format!("/api/v1/reviews/{review}/resolve"),
            &serde_json::json!({"choice": "apply"}),
        )
        .await;
    assert_eq!(done.review.resolution.as_deref(), Some("apply"));
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [id]}))
        .await;
    assert_eq!(got.memories[0].memory.body, "first text");
    let again = s
        .post(
            &format!("/api/v1/reviews/{review}/resolve"),
            &serde_json::json!({"choice": "apply"}),
        )
        .await;
    assert_eq!(again.status(), 400);
    let again: serde_json::Value = again.json().await.unwrap();
    assert!(
        again["error"]
            .as_str()
            .unwrap()
            .contains("already resolved"),
        "{again}"
    );

    assert!(
        s.get_json::<Vec<ReviewItem>>("/api/v1/reviews")
            .await
            .is_empty()
    );
    let resolved: Vec<ReviewItem> = s
        .get_json("/api/v1/reviews?status=resolved&review_type=undo")
        .await;
    assert_eq!(resolved.len(), 1);
    let stats: Stats = s.get_json("/api/v1/stats").await;
    assert_eq!(stats.reviews_open, 0);
    let ctx: ContextResponse = s.post_json("/api/v1/context", &serde_json::json!({})).await;
    assert!(!ctx.text.contains("wait for a decision"), "{}", ctx.text);
    // The decision is in the history.
    let evs: Vec<EventRow> = s.get_json("/api/v1/events?limit=50").await;
    assert!(
        evs.iter()
            .any(|e| e.action == "review.resolve" && e.target_id == review)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn merged_duplicates_answer_with_their_survivor() {
    let s = Srv::start().await;
    let mut ids = Vec::new();
    for t in ["survivor text", "dup one", "dup two"] {
        let r: RememberResponse = s
            .post_json("/api/v1/remember", &serde_json::json!({"content": t}))
            .await;
        ids.push(r.id.unwrap());
    }
    let m: MergeResponse = s
        .post_json(
            "/api/v1/memories/merge",
            &serde_json::json!({"survivor": ids[0], "others": [ids[1], ids[2], ids[0]]}),
        )
        .await;
    assert_eq!(m.merged, vec![ids[1].clone(), ids[2].clone()]);
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [ids[1]]}))
        .await;
    assert_eq!(got.memories.len(), 1);
    assert_eq!(
        got.memories[0].memory.id, ids[0],
        "the survivor is returned instead"
    );
    assert_eq!(
        got.redirects,
        vec![Redirect {
            from: ids[1].clone(),
            to: ids[0].clone()
        }]
    );
    assert!(got.missing.is_empty());
    // Asking for both shows the survivor once.
    let got: GetResponse = s
        .post_json(
            "/api/v1/get",
            &serde_json::json!({"ids": [ids[0], ids[2], "NOSUCH"]}),
        )
        .await;
    assert_eq!(got.memories.len(), 1);
    assert_eq!(got.redirects.len(), 1);
    assert_eq!(got.missing, vec!["NOSUCH".to_string()]);
    // A response without a merge has no `redirects` field at all.
    let plain: serde_json::Value = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [ids[0]]}))
        .await;
    assert!(plain.get("redirects").is_none(), "{plain}");
    // Bad ids and a trashed survivor.
    let bad = s
        .post(
            "/api/v1/memories/merge",
            &serde_json::json!({"survivor": "NOSUCH", "others": []}),
        )
        .await;
    assert_eq!(bad.status(), 404);
    let stats: Stats = s.get_json("/api/v1/stats").await;
    assert_eq!(stats.memories_by_status.get("trashed"), Some(&2));
    assert_eq!(stats.reviews_open, 0);
    assert_eq!(stats.judge_provider, "heuristic");
    // The merge is one undoable group.
    let evs: Vec<EventRow> = s
        .get_json(&format!("/api/v1/events?target_id={}", ids[1]))
        .await;
    assert_eq!(evs[0].action, "memory.merge");
    let u: UndoResponse = s
        .post_json("/api/v1/undo", &serde_json::json!({"event_id": evs[0].id}))
        .await;
    assert!(u.undone, "{}", u.message);
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [ids[1]]}))
        .await;
    assert!(got.redirects.is_empty() && got.memories[0].memory.id == ids[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_follow_chains_stop_at_eight_and_a_hand_restore_ends_them() {
    let s = Srv::start().await;
    let mut ids = Vec::new();
    for i in 0..11 {
        let r: RememberResponse = s
            .post_json(
                "/api/v1/remember",
                &serde_json::json!({"content": format!("version {i}")}),
            )
            .await;
        ids.push(r.id.unwrap());
    }
    // A -> B -> C.
    for w in ids[..3].windows(2) {
        let _: MergeResponse = s
            .post_json(
                "/api/v1/memories/merge",
                &serde_json::json!({"survivor": w[1], "others": [w[0]]}),
            )
            .await;
    }
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [ids[0]]}))
        .await;
    assert_eq!(got.memories.len(), 1);
    assert_eq!(got.memories[0].memory.id, ids[2], "the end of the chain");
    assert_eq!(
        got.redirects,
        vec![
            Redirect {
                from: ids[0].clone(),
                to: ids[1].clone()
            },
            Redirect {
                from: ids[1].clone(),
                to: ids[2].clone()
            },
        ]
    );
    assert!(got.notes.is_empty());
    // A chain of ten steps is cut at eight, and says so.
    for w in ids[2..].windows(2) {
        let _: MergeResponse = s
            .post_json(
                "/api/v1/memories/merge",
                &serde_json::json!({"survivor": w[1], "others": [w[0]]}),
            )
            .await;
    }
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [ids[0]]}))
        .await;
    assert_eq!(got.redirects.len(), 8, "{:?}", got.redirects);
    assert_eq!(got.memories[0].memory.id, ids[8]);
    assert!(got.notes[0].contains("longer than 8"), "{:?}", got.notes);
    // Restoring a duplicate by hand makes it itself again: no link, no redirect.
    let restored: MemoryFull = s
        .post_json(
            "/api/v1/update",
            &serde_json::json!({"id": ids[0], "action": "restore"}),
        )
        .await;
    assert_eq!(restored.memory.id, ids[0]);
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [ids[0]]}))
        .await;
    assert!(got.redirects.is_empty() && got.memories[0].memory.id == ids[0]);
    // It can be forgotten and fetched again without being sent to its old survivor.
    let _: MemoryFull = s
        .post_json(
            "/api/v1/update",
            &serde_json::json!({"id": ids[0], "action": "forget"}),
        )
        .await;
    let got: GetResponse = s
        .post_json("/api/v1/get", &serde_json::json!({"ids": [ids[0]]}))
        .await;
    assert!(got.redirects.is_empty() && got.memories[0].memory.id == ids[0]);
    // The merge request is capped.
    let many: Vec<String> = (0..51).map(|i| format!("X{i}")).collect();
    let r = s
        .post(
            "/api/v1/memories/merge",
            &serde_json::json!({"survivor": ids[10], "others": many}),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("limit of 50"));
}
