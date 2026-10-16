//! Capture routes for server tenants (SP2 Task 14; spec §5.2, §5.7, §9.4), end to end through
//! the in-process Caddy emulator: transcript uploads, the capture status and the SessionStart
//! briefing, each with the tenant's bearer token.
//!
//! Requests use `reqwest` against `support::caddy::Server` (one test also runs a local daemon's
//! router on loopback). Transcripts are written inline, line by line.

mod support;

use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::model::Kind;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::caddy::Server;
use tokio_util::sync::CancellationToken;

/// A Claude Code transcript, written line by line. Timestamps advance one second per line.
#[derive(Default, Clone)]
struct Transcript {
    bytes: Vec<u8>,
    clock: u64,
}

impl Transcript {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn line(&mut self, mut v: serde_json::Value) -> &mut Self {
        self.clock += 1;
        let c = self.clock;
        v["timestamp"] = json!(format!(
            "2026-10-16T{:02}:{:02}:{:02}.000Z",
            (c / 3600) % 24,
            (c / 60) % 60,
            c % 60
        ));
        v["isSidechain"] = json!(false);
        self.bytes.extend(serde_json::to_vec(&v).unwrap());
        self.bytes.push(b'\n');
        self
    }

    /// Turn `i` of session `s`: prompt `<s>-p<i>` and reply `<s>-a<i>`, both naming `marker`.
    fn turn(&mut self, s: &str, i: usize, marker: &str) -> &mut Self {
        self.line(json!({"type": "user", "uuid": format!("{s}-p{i}"),
            "message": {"role": "user", "content": format!("Question {i} about {marker}.")}}))
            .line(json!({"type": "assistant", "uuid": format!("{s}-a{i}"),
                "message": {"id": format!("msg-{s}-a{i}"), "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": format!("Answer {i}: {marker} is settled.")}],
                    "stop_reason": "end_turn"}}))
    }
}

fn uri(session: &str) -> String {
    format!("claude-code:session:{session}")
}

/// The query of an upload: agent, session, offset and event.
fn q(session: &str, offset: u64, event: &str) -> Vec<(&'static str, String)> {
    vec![
        ("agent", "claude-code".into()),
        ("session", session.into()),
        ("offset", offset.to_string()),
        ("event", event.into()),
    ]
}

/// `q` with `project=<project>`.
fn q_project(
    session: &str,
    offset: u64,
    event: &str,
    project: &str,
) -> Vec<(&'static str, String)> {
    let mut v = q(session, offset, event);
    v.push(("project", project.into()));
    v
}

/// `pairs` as a URL query (form-encoded: a space is `+`, as curl's `--data-urlencode` sends it).
fn encode<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k.as_ref(), v.as_ref());
    }
    s.finish()
}

/// `POST <base>/api/v1/capture/transcript?<query>` with `body`, the bearer `token` and an
/// `X-Alice-Project` header when given.
async fn upload(
    base: &str,
    token: Option<&str>,
    query: &[(&'static str, String)],
    header: Option<&[u8]>,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut r = reqwest::Client::new()
        .post(format!(
            "{base}/api/v1/capture/transcript?{}",
            encode(query)
        ))
        .header(CONTENT_TYPE, "application/x-ndjson")
        .body(body);
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    if let Some(h) = header {
        r = r.header("x-alice-project", HeaderValue::from_bytes(h).unwrap());
    }
    r.send().await.unwrap()
}

/// The same with a raw query string, for queries `reqwest` would not build.
async fn upload_raw(base: &str, token: &str, query: &str, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/api/v1/capture/transcript?{query}"))
        .bearer_auth(token)
        .header(CONTENT_TYPE, "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// The answer of an upload that must be accepted.
async fn accepted(r: reqwest::Response) -> TranscriptAnswer {
    let status = r.status();
    let body = r.text().await.unwrap();
    assert_eq!(status, 200, "the upload was not accepted: {body}");
    serde_json::from_str(&body).unwrap()
}

/// `GET <base>/api/v1/capture/briefing?<query>`.
async fn briefing(
    base: &str,
    token: Option<&str>,
    query: &[(&str, &str)],
    header: Option<&[u8]>,
) -> reqwest::Response {
    let mut r =
        reqwest::Client::new().get(format!("{base}/api/v1/capture/briefing?{}", encode(query)));
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    if let Some(h) = header {
        r = r.header("x-alice-project", HeaderValue::from_bytes(h).unwrap());
    }
    r.send().await.unwrap()
}

/// The text of a briefing that must be given (`200 text/plain; charset=utf-8`).
async fn briefed(r: reqwest::Response) -> String {
    let status = r.status();
    let ct = r
        .headers()
        .get(CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string());
    let body = r.text().await.unwrap();
    assert_eq!(status, 200, "no briefing: {body}");
    assert_eq!(ct.as_deref(), Some("text/plain; charset=utf-8"));
    body
}

async fn status(base: &str, token: Option<&str>) -> reqwest::Response {
    let mut r = reqwest::Client::new().get(format!("{base}/api/v1/capture/status"));
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    r.send().await.unwrap()
}

fn db(home: &AliceHome) -> rusqlite::Connection {
    alice_core::store::open_reader(&home.db()).unwrap()
}

fn count(home: &AliceHome, sql: &str) -> i64 {
    db(home).query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `(project key, project slug)` of the source `uri`.
fn source_project(home: &AliceHome, uri: &str) -> Option<(String, String)> {
    use rusqlite::OptionalExtension;
    db(home)
        .query_row(
            "SELECT p.key, p.slug FROM sources s JOIN projects p ON p.id = s.project_id
             WHERE s.uri = ?1",
            [uri],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
}

/// `(byte_offset, transcript_path, file_key)` of the source `uri`.
fn capture_row(home: &AliceHome, uri: &str) -> (u64, Option<String>, Option<String>) {
    db(home)
        .query_row(
            "SELECT c.byte_offset, c.transcript_path, c.file_key
             FROM capture_state c JOIN sources s ON s.id = c.source_id WHERE s.uri = ?1",
            [uri],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

fn chunks_of(home: &AliceHome, uri: &str) -> i64 {
    db(home)
        .query_row(
            "SELECT count(*) FROM chunks ch JOIN sources s ON s.id = ch.source_id WHERE s.uri = ?1",
            [uri],
            |r| r.get(0),
        )
        .unwrap()
}

/// An MCP client with no roots.
#[derive(Clone)]
struct Agent;
impl ClientHandler for Agent {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("capture-test", "1.0"),
        )
    }
}

type Conn = RunningService<RoleClient, Agent>;

/// An MCP connection to `url` sending `X-Alice-Project: <project>` on every request (what
/// Claude Code's `headersHelper` does).
async fn connect_with_header(url: &str, token: &str, project: &[u8]) -> Conn {
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    cfg = cfg.auth_header(token.to_string());
    cfg.custom_headers.insert(
        axum::http::HeaderName::from_static("x-alice-project"),
        axum::http::HeaderValue::from_bytes(project).unwrap(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        Agent.serve(StreamableHttpClientTransport::from_config(cfg)),
    )
    .await
    .expect("connect timed out")
    .unwrap()
}

async fn tool(c: &Conn, name: &'static str, args: serde_json::Value) -> CallToolResult {
    let a = args.as_object().unwrap().clone();
    c.call_tool(CallToolRequestParams::new(name).with_arguments(a))
        .await
        .unwrap()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

/// `(project key, project slug)` of the one memory whose body holds `content`.
fn memory_project(home: &AliceHome, content: &str) -> (String, String) {
    db(home)
        .query_row(
            "SELECT p.key, p.slug FROM memories m JOIN projects p ON p.id = m.project_id
             WHERE instr(m.body, ?1) > 0",
            [content],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

// ---- Uploads ----

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_through_the_emulator_honours_x_alice_project() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let mut tr = Transcript::default();
    tr.turn("h1", 1, "hinted by the header")
        .turn("h1", 2, "zanzibarquokka");
    let a = accepted(
        upload(
            &base,
            Some(&t.token),
            &q("h1", 0, "stop"),
            Some(b"Repo One"),
            tr.bytes.clone(),
        )
        .await,
    )
    .await;
    assert_eq!(a.next_offset, tr.len());
    assert_eq!((a.turns_stored, a.turns_skipped), (2, 0));
    assert!(a.chunks_stored >= 2);
    assert!(!a.oversized_line);
    assert_eq!(
        source_project(&t.home, &uri("h1")),
        Some(("import:repo-one".into(), "repo-one".into())),
        "the header named the project: its placeholder"
    );

    // The status lists the session, and says it is a tenant.
    let r = status(&base, Some(&t.token)).await;
    assert_eq!(r.status(), 200);
    let st: CaptureStatus = r.json().await.unwrap();
    assert!(st.tenant && st.enabled);
    let row = st.sessions.iter().find(|r| r.uri == uri("h1")).unwrap();
    assert_eq!(
        (row.project.as_str(), row.byte_offset),
        ("repo-one", tr.len())
    );

    // A later MCP `remember` with the same header lands in the same project.
    let c = connect_with_header(&format!("{base}/mcp"), &t.token, b"Repo One").await;
    let content = "a decision made through MCP in Repo One";
    let r = tool(
        &c,
        "remember",
        json!({"content": content, "kind": "decision"}),
    )
    .await;
    c.cancel().await.ok();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert!(text(&r).contains("repo-one"), "{}", text(&r));
    assert_eq!(
        memory_project(&t.home, content),
        ("import:repo-one".into(), "repo-one".into())
    );

    // Without the header, the `project` query parameter names it.
    let mut t2 = Transcript::default();
    t2.turn("h2", 1, "hinted by the query");
    accepted(
        upload(
            &base,
            Some(&t.token),
            &q_project("h2", 0, "stop", "Repo One"),
            None,
            t2.bytes.clone(),
        )
        .await,
    )
    .await;
    assert_eq!(source_project(&t.home, &uri("h2")).unwrap().1, "repo-one");

    // The header wins over the query parameter.
    let mut t3 = Transcript::default();
    t3.turn("h3", 1, "hinted by both");
    accepted(
        upload(
            &base,
            Some(&t.token),
            &q_project("h3", 0, "stop", "Elsewhere"),
            Some(b"Repo One"),
            t3.bytes.clone(),
        )
        .await,
    )
    .await;
    assert_eq!(source_project(&t.home, &uri("h3")).unwrap().1, "repo-one");
    assert_eq!(
        count(
            &t.home,
            "SELECT count(*) FROM projects WHERE slug LIKE 'repo-one%' OR slug = 'elsewhere'"
        ),
        1,
        "one project, no stray one"
    );
}

/// The header is read as the MCP path reads it: UTF-8 or percent-encoded UTF-8, trimmed, blank
/// as absent, and refused with the same messages otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn the_project_header_is_read_as_the_mcp_path_reads_it() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    // "ร้านค้า" percent-encoded, the way Node's fetch must send it.
    let encoded = "%E0%B8%A3%E0%B9%89%E0%B8%B2%E0%B8%99%E0%B8%84%E0%B9%89%E0%B8%B2";
    let mut tr = Transcript::default();
    tr.turn("th", 1, "the shop");
    accepted(
        upload(
            &base,
            Some(&t.token),
            &q("th", 0, "stop"),
            Some(encoded.as_bytes()),
            tr.bytes.clone(),
        )
        .await,
    )
    .await;
    let (key, slug) = source_project(&t.home, &uri("th")).unwrap();
    assert_eq!(slug, alice_core::normalize::slugify("ร้านค้า"));
    let c = connect_with_header(&format!("{base}/mcp"), &t.token, encoded.as_bytes()).await;
    let content = "a note for the shop through MCP";
    let r = tool(&c, "remember", json!({"content": content})).await;
    c.cancel().await.ok();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert_eq!(memory_project(&t.home, content), (key, slug));

    // A blank header is no header: the query parameter names the project.
    let mut tb = Transcript::default();
    tb.turn("tb", 1, "a blank header");
    accepted(
        upload(
            &base,
            Some(&t.token),
            &q_project("tb", 0, "stop", "Queried"),
            Some(b"   "),
            tb.bytes.clone(),
        )
        .await,
    )
    .await;
    assert_eq!(source_project(&t.home, &uri("tb")).unwrap().1, "queried");

    // "café" as Latin-1: not UTF-8, refused as MCP refuses it; nothing is stored.
    let r = upload(
        &base,
        Some(&t.token),
        &q("tl", 0, "stop"),
        Some(b"caf\xe9"),
        tr.bytes.clone(),
    )
    .await;
    assert_eq!(r.status(), 400);
    let e: serde_json::Value = r.json().await.unwrap();
    assert_eq!(e["kind"], "invalid");
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .contains("the X-Alice-Project header is not UTF-8"),
        "{e}"
    );
    // Over the limit, as MCP says it.
    let long = "h".repeat(250);
    let r = upload(
        &base,
        Some(&t.token),
        &q("tl", 0, "stop"),
        Some(long.as_bytes()),
        tr.bytes.clone(),
    )
    .await;
    assert_eq!(r.status(), 400);
    let e: serde_json::Value = r.json().await.unwrap();
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .contains("the project name in the X-Alice-Project header is too long"),
        "{e}"
    );
    assert!(source_project(&t.home, &uri("tl")).is_none());
}

/// The name of the project the source `uri` (or, with `memory`, the memory holding that text)
/// belongs to.
fn project_name(home: &AliceHome, sql_key: &str, memory: bool) -> String {
    let sql = if memory {
        "SELECT p.name FROM memories m JOIN projects p ON p.id = m.project_id
         WHERE instr(m.body, ?1) > 0"
    } else {
        "SELECT p.name FROM sources s JOIN projects p ON p.id = s.project_id WHERE s.uri = ?1"
    };
    db(home).query_row(sql, [sql_key], |r| r.get(0)).unwrap()
}

/// Every project name holds no control character.
fn assert_no_control_in_names(home: &AliceHome) {
    let c = db(home);
    let mut st = c.prepare("SELECT name FROM projects").unwrap();
    let names: Vec<String> = st
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for n in names {
        assert!(!n.chars().any(char::is_control), "{n:?}");
    }
}

/// Task 14 review: control characters in a hint (`%0A`, `%1B`, a raw tab) are stripped as the
/// hint is decoded, so they never reach a stored project name: the capture routes' header and
/// `project` query.
#[tokio::test(flavor = "multi_thread")]
async fn control_characters_in_a_capture_hint_are_stripped() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    for (session, header, query, expected) in [
        ("cc1", Some(b"Repo%0AOne%1B".as_slice()), None, "RepoOne"),
        ("cc2", None, Some("Repo\nTwo\u{1b}"), "RepoTwo"),
        ("cc3", Some(b"Tab\tRepo".as_slice()), None, "TabRepo"),
    ] {
        let mut tr = Transcript::default();
        tr.turn(session, 1, "a hint with control characters");
        let query = match query {
            Some(p) => q_project(session, 0, "stop", p),
            None => q(session, 0, "stop"),
        };
        accepted(upload(&base, Some(&t.token), &query, header, tr.bytes.clone()).await).await;
        assert_eq!(project_name(&t.home, &uri(session), false), expected);
    }
    // The briefing reads the hint the same way: the stripped name is the project briefed.
    let got = briefed(
        briefing(
            &base,
            Some(&t.token),
            &[("source", "startup")],
            Some(b"Repo%0AOne"),
        )
        .await,
    )
    .await;
    assert!(got.starts_with("# Context · repoone\n"), "{got}");
    assert_no_control_in_names(&t.home);
}

/// The same on the MCP path: the `X-Alice-Project` header and the `?project=` of the URL.
#[tokio::test(flavor = "multi_thread")]
async fn control_characters_in_an_mcp_hint_are_stripped() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let c = connect_with_header(&format!("{base}/mcp"), &t.token, b"Repo%0AThree%1B").await;
    let content = "a note sent with a control character in its header";
    let r = tool(&c, "remember", json!({"content": content})).await;
    c.cancel().await.ok();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert_eq!(project_name(&t.home, content, true), "RepoThree");

    let c = connect_with_header(
        &format!("{base}/mcp?project=Repo%0AFour%07"),
        &t.token,
        b"ignored",
    )
    .await;
    let content = "a note sent with a control character in its query";
    let r = tool(&c, "remember", json!({"content": content})).await;
    c.cancel().await.ok();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert_eq!(project_name(&t.home, content, true), "RepoFour");
    assert_no_control_in_names(&t.home);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lower_offset_answers_409_over_http() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let mut tr = Transcript::default();
    tr.turn("lo", 1, "first").turn("lo", 2, "second");
    let cursor = tr.len();
    let a = accepted(
        upload(
            &base,
            Some(&t.token),
            &q("lo", 0, "stop"),
            Some(b"Repo One"),
            tr.bytes.clone(),
        )
        .await,
    )
    .await;
    assert_eq!(a.next_offset, cursor);
    let chunks = chunks_of(&t.home, &uri("lo"));

    // The client lost its state: everything again from 0, then from just below the cursor.
    for offset in [0, cursor - 1] {
        let body = tr.bytes[offset as usize..].to_vec();
        let r = upload(
            &base,
            Some(&t.token),
            &q("lo", offset, "stop"),
            Some(b"Repo One"),
            body,
        )
        .await;
        assert_eq!(r.status(), 409);
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body, json!({ "next_offset": cursor }));
    }
    assert_eq!(
        chunks_of(&t.home, &uri("lo")),
        chunks,
        "nothing stored twice"
    );

    // From the cursor it goes on.
    tr.turn("lo", 3, "third");
    let a = accepted(
        upload(
            &base,
            Some(&t.token),
            &q("lo", cursor, "stop"),
            Some(b"Repo One"),
            tr.bytes[cursor as usize..].to_vec(),
        )
        .await,
    )
    .await;
    assert_eq!((a.turns_stored, a.next_offset), (1, tr.len()));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_query_answers_400() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let mut tr = Transcript::default();
    tr.turn("iq", 1, "never stored");
    for query in [
        // No offset, a garbage one, a negative one, one beyond any transcript.
        "agent=claude-code&session=iq&event=stop",
        "agent=claude-code&session=iq&offset=abc&event=stop",
        "agent=claude-code&session=iq&offset=-1&event=stop",
        "agent=claude-code&session=iq&offset=9007199254740993&event=stop",
        // Ids that are not ids.
        "agent=claude-code&session=..%2Fetc&offset=0&event=stop",
        "agent=claude-code&session=iq&subagent=a%20b&offset=0&event=stop",
        "agent=claude-code&offset=0&event=stop",
        // An unknown agent or event; flags that are not numbers.
        "agent=gpt&session=iq&offset=0&event=stop",
        "agent=claude-code&session=iq&offset=0&event=nope",
        "agent=claude-code&session=iq&offset=0",
        "agent=claude-code&session=iq&offset=0&event=stop&no_capture=yes",
    ] {
        let r = upload_raw(&base, &t.token, query, tr.bytes.clone()).await;
        assert_eq!(r.status(), 400, "{query}");
        let e: serde_json::Value = r.json().await.unwrap();
        assert_eq!(e["kind"], "invalid", "{query}: {e}");
        assert!(!e["error"].as_str().unwrap().is_empty(), "{query}");
    }
    assert_eq!(count(&t.home, "SELECT count(*) FROM capture_state"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_body_limit_is_eight_mebibytes() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    // 9 MiB: refused before anything is read.
    let r = upload(
        &base,
        Some(&t.token),
        &q("big", 0, "stop"),
        None,
        vec![b'x'; 9 << 20],
    )
    .await;
    assert_eq!(r.status(), 413);
    let e: serde_json::Value = r.json().await.unwrap();
    assert_eq!(e["kind"], "too_large");
    assert!(e["error"].as_str().unwrap().contains("8 MiB"), "{e}");
    // Exactly 8 MiB is taken: one line with no end, which the client skips.
    let a = accepted(
        upload(
            &base,
            Some(&t.token),
            &q("big", 0, "stop"),
            None,
            vec![b'x'; TRANSCRIPT_BODY_LIMIT],
        )
        .await,
    )
    .await;
    assert!(a.oversized_line);
    assert_eq!((a.next_offset, a.chunks_stored), (0, 0));
    // The limit is the route's own: another route still parses a larger body (and refuses the
    // content for its own limit, not for the body's size).
    let r = reqwest::Client::new()
        .post(format!("{base}/api/v1/remember"))
        .bearer_auth(&t.token)
        .json(&json!({"content": "y".repeat(9 << 20)}))
        .send()
        .await
        .unwrap();
    let st = r.status();
    let e: serde_json::Value = r.json().await.unwrap();
    assert_eq!(st, 400, "{e}");
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .contains("the limit is 65536 bytes"),
        "the content's own limit, so the body was read: {e}"
    );
}

/// Spec §9.4: the owner's laptop database becomes their tenant. The tables travel, the client's
/// first upload from 0 jumps to the cursor, and the laptop's paths are never read.
#[tokio::test(flavor = "multi_thread")]
async fn an_adopted_database_is_not_captured_twice() {
    alice_daemon::install_tls();
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().join("home"));
    std::fs::create_dir_all(d.path().join("home")).unwrap();
    let work = d.path().join("laptop-repo");
    std::fs::create_dir_all(&work).unwrap();
    let file = d.path().join("ad.jsonl");
    let mut tr = Transcript::default();
    tr.turn("ad", 1, "captured on the laptop")
        .turn("ad", 2, "also on the laptop");
    std::fs::write(&file, &tr.bytes).unwrap();

    // 1. Captured by a local daemon on the laptop.
    let local = Service::start(
        home.clone(),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions {
            settings_env: Some(Vec::new()),
            ..ServiceOptions::default()
        },
    )
    .unwrap();
    let a = local
        .capture_transcript(
            TranscriptQuery {
                agent: "claude-code".into(),
                session: "ad".into(),
                offset: 0,
                event: "stop".into(),
                cwd: Some(work.to_string_lossy().into_owned()),
                ..Default::default()
            },
            axum::body::Bytes::from(tr.bytes.clone()),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!((a.next_offset, a.turns_stored), (tr.len(), 2));
    local.shutdown();
    drop(local);
    // As the laptop's capture worker leaves it: the file it read, and its identity.
    {
        let c = rusqlite::Connection::open(home.db()).unwrap();
        c.execute(
            "UPDATE capture_state SET transcript_path = ?1, file_key = '16777220:4242:abcdef'",
            [file.to_string_lossy()],
        )
        .unwrap();
    }
    let laptop_project = source_project(&home, &uri("ad")).unwrap();
    let (chunks, turns, projects) = (
        chunks_of(&home, &uri("ad")),
        count(&home, "SELECT count(*) FROM capture_turns"),
        count(&home, "SELECT count(*) FROM projects"),
    );
    // The server has no such file.
    std::fs::remove_file(&file).unwrap();

    // 2. The same database as a tenant, behind the emulator.
    let s = Server::start_on("owner", home.clone()).await;
    let t = s.tenant("owner");
    let base = s.url("owner");
    assert_eq!(
        capture_row(&home, &uri("ad")),
        (tr.len(), None, None),
        "the cursor travels; the laptop's path and file identity are cleared at the tenant start"
    );

    // 3. The tenant script's first upload for the session starts from 0: a 409 jump. Its hint
    // names another project than the laptop's (the checkout was renamed since), so a source that
    // followed the hint would be caught moving.
    assert_ne!(laptop_project.1, "renamed-repo");
    let r = upload(
        &base,
        Some(&t.token),
        &q("ad", 0, "stop"),
        Some(b"Renamed Repo"),
        tr.bytes.clone(),
    )
    .await;
    assert_eq!(r.status(), 409);
    let jump: serde_json::Value = r.json().await.unwrap();
    assert_eq!(jump, json!({ "next_offset": tr.len() }));
    assert_eq!(chunks_of(&home, &uri("ad")), chunks, "no chunk added");
    assert_eq!(count(&home, "SELECT count(*) FROM capture_turns"), turns);

    // 4. A new turn, uploaded from the jump: exactly one turn stored.
    let from = jump["next_offset"].as_u64().unwrap();
    tr.turn("ad", 3, "the first turn on the server");
    let a = accepted(
        upload(
            &base,
            Some(&t.token),
            &q("ad", from, "stop"),
            Some(b"Renamed Repo"),
            tr.bytes[from as usize..].to_vec(),
        )
        .await,
    )
    .await;
    assert_eq!((a.turns_stored, a.turns_skipped), (1, 0));
    assert_eq!(a.next_offset, tr.len());
    assert_eq!(
        count(&home, "SELECT count(*) FROM capture_turns"),
        turns + 1
    );
    assert!(chunks_of(&home, &uri("ad")) > chunks);

    // 5. The path was never needed: the source keeps its project, nothing new was created, and
    // the row still names no path.
    assert_eq!(source_project(&home, &uri("ad")).unwrap(), laptop_project);
    assert_eq!(count(&home, "SELECT count(*) FROM projects"), projects);
    assert_eq!(
        count(
            &home,
            "SELECT count(*) FROM projects WHERE slug = 'renamed-repo' OR key = 'import:renamed-repo'"
        ),
        0,
        "the hint made no project"
    );
    assert_eq!(capture_row(&home, &uri("ad")), (tr.len(), None, None));
}

// ---- The briefing ----

/// Three projects' worth of memories on `svc`: `repo-one` and `elsewhere` (placeholders, from
/// hints) and a global preference.
async fn seed(svc: &Service) {
    for (content, kind, hint) in [
        (
            "Repo One deploys with blue green switching",
            Kind::Decision,
            Some("Repo One"),
        ),
        (
            "Elsewhere keeps its own runbook",
            Kind::Decision,
            Some("Elsewhere"),
        ),
        ("Answer in short sentences", Kind::Preference, None),
    ] {
        svc.remember(RememberRequest {
            content: content.into(),
            kind: Some(kind),
            importance: Some(5),
            project_hint: hint.map(String::from),
            ..Default::default()
        })
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tenant_briefing_resolves_the_hint() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    seed(&t.svc).await;
    let expected = t
        .svc
        .context(ContextRequest {
            project: Some("repo-one".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .text;
    assert!(expected.starts_with("# Context · repo-one\n"), "{expected}");
    assert!(expected.contains("blue green"), "{expected}");

    let tok = Some(t.token.as_str());
    // The header.
    let got =
        briefed(briefing(&base, tok, &[("source", "startup")], Some(b"Repo One")).await).await;
    assert_eq!(got, expected);
    // The query parameter.
    let got = briefed(
        briefing(
            &base,
            tok,
            &[("source", "startup"), ("project", "Repo One")],
            None,
        )
        .await,
    )
    .await;
    assert_eq!(got, expected);
    // The header wins over the query parameter, and a tenant ignores `cwd`.
    let got = briefed(
        briefing(
            &base,
            tok,
            &[
                ("source", "clear"),
                ("project", "Elsewhere"),
                ("cwd", "/etc"),
            ],
            Some(b"Repo One"),
        )
        .await,
    )
    .await;
    assert_eq!(got, expected);
    // `compact`, and no source at all, brief too.
    for query in [&[("source", "compact")][..], &[]] {
        let got = briefed(briefing(&base, tok, query, Some(b"Repo One")).await).await;
        assert_eq!(got, expected);
    }
    // The other project, by its own hint.
    let got =
        briefed(briefing(&base, tok, &[("source", "startup")], Some(b"Elsewhere")).await).await;
    assert!(got.starts_with("# Context · elsewhere\n"), "{got}");
    assert!(got.contains("its own runbook"), "{got}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_briefing_route_answers_204_when_off() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let tok = Some(t.token.as_str());
    seed(&t.svc).await;
    briefed(briefing(&base, tok, &[("source", "startup")], Some(b"Repo One")).await).await;
    // A resumed or forked session already has its context.
    for source in ["resume", "fork"] {
        let r = briefing(&base, tok, &[("source", source)], Some(b"Repo One")).await;
        assert_eq!(r.status(), 204, "{source}");
        assert!(r.bytes().await.unwrap().is_empty());
    }
    // The tenant's setting turns it off, and on again.
    let patch = |on: bool| {
        reqwest::Client::new()
            .patch(format!("{base}/api/v1/settings"))
            .bearer_auth(&t.token)
            .json(&json!({ "capture.briefing": on }))
            .send()
    };
    assert_eq!(patch(false).await.unwrap().status(), 200);
    for header in [Some(b"Repo One".as_slice()), None] {
        let r = briefing(&base, tok, &[("source", "startup")], header).await;
        assert_eq!(r.status(), 204);
        assert!(r.bytes().await.unwrap().is_empty());
    }
    assert_eq!(patch(true).await.unwrap().status(), 200);
    briefed(briefing(&base, tok, &[("source", "startup")], Some(b"Repo One")).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_hint_gives_the_global_briefing_and_creates_nothing() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let tok = Some(t.token.as_str());
    seed(&t.svc).await;
    let expected = t.svc.context(ContextRequest::default()).await.unwrap().text;
    assert!(expected.starts_with("# Context · global\n"), "{expected}");
    assert!(expected.contains("short sentences"), "{expected}");
    let (projects, events) = (
        count(&t.home, "SELECT count(*) FROM projects"),
        count(&t.home, "SELECT count(*) FROM events"),
    );

    let got =
        briefed(briefing(&base, tok, &[("source", "startup")], Some(b"Never Seen")).await).await;
    assert_eq!(got, expected);
    let got = briefed(
        briefing(
            &base,
            tok,
            &[("source", "startup"), ("project", "Never Seen")],
            None,
        )
        .await,
    )
    .await;
    assert_eq!(got, expected);
    // No hint at all: global too.
    let got = briefed(briefing(&base, tok, &[("source", "startup")], None).await).await;
    assert_eq!(got, expected);

    assert_eq!(count(&t.home, "SELECT count(*) FROM projects"), projects);
    assert_eq!(count(&t.home, "SELECT count(*) FROM events"), events);
    assert_eq!(
        count(
            &t.home,
            "SELECT count(*) FROM projects WHERE slug LIKE 'never-seen%' OR key LIKE '%never-seen%'"
        ),
        0,
        "a read creates nothing"
    );
}

// ---- Auth ----

#[tokio::test(flavor = "multi_thread")]
async fn capture_routes_need_the_token() {
    let s = Server::start(&["ann", "bob"]).await;
    let (ann, bob) = (s.tenant("ann"), s.tenant("bob"));
    let base = s.url("ann");
    let mut tr = Transcript::default();
    tr.turn("au", 1, "never stored without the token");
    for token in [None, Some(bob.token.as_str())] {
        let r = upload(&base, token, &q("au", 0, "stop"), None, tr.bytes.clone()).await;
        assert_eq!(r.status(), 401);
        let r = status(&base, token).await;
        assert_eq!(r.status(), 401);
        let r = briefing(&base, token, &[("source", "startup")], None).await;
        assert_eq!(r.status(), 401);
    }
    assert_eq!(count(&ann.home, "SELECT count(*) FROM capture_state"), 0);
    // Each tenant's own token works on its own URL.
    let tok = Some(ann.token.as_str());
    accepted(upload(&base, tok, &q("au", 0, "stop"), None, tr.bytes.clone()).await).await;
    assert_eq!(status(&base, tok).await.status(), 200);
    briefed(briefing(&base, tok, &[("source", "startup")], None).await).await;
    let bob_status = status(&s.url("bob"), Some(&bob.token)).await;
    assert_eq!(bob_status.status(), 200);
    let st: CaptureStatus = bob_status.json().await.unwrap();
    assert!(
        st.sessions.is_empty(),
        "bob's tenant holds nothing of ann's"
    );
}

// ---- A local daemon (plan decision P8) ----

/// A local daemon's routes on loopback: the project comes from `cwd`; a hint means nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_local_routes_follow_cwd() {
    alice_daemon::install_tls();
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().join("home"));
    std::fs::create_dir_all(d.path().join("home")).unwrap();
    let work = d.path().join("local-app");
    std::fs::create_dir_all(&work).unwrap();
    let cwd = work.to_string_lossy().into_owned();
    let svc = Service::start(
        home.clone(),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions {
            settings_env: Some(Vec::new()),
            ..ServiceOptions::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "t".repeat(64);
    let stop = CancellationToken::new();
    let app = alice_daemon::http::router(svc.clone(), token.clone(), port, stop.clone());
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop.cancelled().await })
            .await
            .unwrap()
    });
    let base = format!("http://127.0.0.1:{port}");
    svc.remember(RememberRequest {
        content: "The local app ships on fridays".into(),
        kind: Some(Kind::Decision),
        importance: Some(5),
        cwd: Some(cwd.clone()),
        ..Default::default()
    })
    .await
    .unwrap();
    let expected = svc
        .context(ContextRequest {
            cwd: Some(cwd.clone()),
            ..Default::default()
        })
        .await
        .unwrap()
        .text;
    assert!(
        expected.starts_with("# Context · local-app\n"),
        "{expected}"
    );

    let got = briefed(
        briefing(
            &base,
            Some(&token),
            &[("source", "startup"), ("cwd", &cwd), ("project", "Other")],
            Some(b"Other"),
        )
        .await,
    )
    .await;
    assert_eq!(got, expected);

    let mut tr = Transcript::default();
    tr.turn("lc", 1, "a local session");
    let mut query = q_project("lc", 0, "stop", "Other");
    query.push(("cwd", cwd.clone()));
    // A local daemon reads no header at all, as MCP does not: bytes a tenant refuses are
    // ignored here.
    accepted(
        upload(
            &base,
            Some(&token),
            &query,
            Some(b"caf\xe9"),
            tr.bytes.clone(),
        )
        .await,
    )
    .await;
    assert_eq!(source_project(&home, &uri("lc")).unwrap().1, "local-app");
    let st: CaptureStatus = status(&base, Some(&token)).await.json().await.unwrap();
    assert!(!st.tenant);
}
