//! Capture uploads (SP2 Task 6a; spec §3, §5.2, §5.6, §9.4): transcript bytes handed to
//! `Service::capture_transcript` become chunks, turn keys and a cursor, each exactly once. Local
//! capture (Task 6b; spec §4.2, §10): `Service::capture_notify` has the daemon's worker read the
//! transcript file itself, through the same pipeline.
//!
//! Every test runs at the service level (no HTTP): the route of Task 14 only maps the answer.
//! Transcripts are written inline, line by line. Secret-shaped strings are built with `concat!`,
//! split inside the provider prefix.

use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use axum::body::Bytes;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const GITHUB: &str = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
const GITHUB_CORE: &str = "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
const ANTHROPIC: &str = concat!("sk-", "ant-api03-abcdefghijklmnopqrstuvwxyz012345");
const ANTHROPIC_CORE: &str = "abcdefghijklmnopqrstuvwxyz012345";

fn options(tenant: bool) -> ServiceOptions {
    ServiceOptions {
        secret_env: Some(Arc::new(|_: &str| None)),
        settings_env: Some(Vec::new()),
        settings_poll: Duration::from_millis(50),
        tenant,
        ..Default::default()
    }
}

fn start_in(dir: &Path, tenant: bool) -> Arc<Service> {
    Service::start(
        AliceHome::new(dir.to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        options(tenant),
    )
    .unwrap()
}

fn tenant() -> (tempfile::TempDir, Arc<Service>) {
    let d = tempfile::tempdir().unwrap();
    let s = start_in(d.path(), true);
    (d, s)
}

/// A home whose `config.toml` holds `config`.
fn home_with(config: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(AliceHome::new(d.path().to_path_buf()).config_file(), config).unwrap();
    d
}

/// A Claude Code transcript, written line by line. Timestamps advance one second per line.
#[derive(Default, Clone)]
struct Transcript {
    bytes: Vec<u8>,
    clock: u64,
    /// A subagent's own file: every line is a sidechain carrying this `agentId`.
    agent_id: Option<String>,
}

impl Transcript {
    fn subagent(agent_id: &str) -> Self {
        Transcript {
            agent_id: Some(agent_id.into()),
            ..Default::default()
        }
    }

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
        v["isSidechain"] = json!(self.agent_id.is_some());
        if let Some(a) = &self.agent_id {
            v["agentId"] = json!(a);
        }
        self.bytes.extend(serde_json::to_vec(&v).unwrap());
        self.bytes.push(b'\n');
        self
    }

    fn prompt(&mut self, uuid: &str, text: &str) -> &mut Self {
        self.line(json!({"type": "user", "uuid": uuid,
            "message": {"role": "user", "content": text}}))
    }

    fn reply(&mut self, uuid: &str, text: &str) -> &mut Self {
        self.line(json!({"type": "assistant", "uuid": uuid,
            "message": {"id": format!("msg-{uuid}"), "type": "message", "role": "assistant",
                        "content": [{"type": "text", "text": text}], "stop_reason": "end_turn"}}))
    }

    fn tool_use(&mut self, uuid: &str, tool_id: &str, command: &str) -> &mut Self {
        self.line(json!({"type": "assistant", "uuid": uuid,
            "message": {"id": format!("msg-{uuid}"), "role": "assistant", "content": [
                {"type": "tool_use", "id": tool_id, "name": "Bash", "input": {"command": command}}]}}))
    }

    fn tool_result(&mut self, uuid: &str, tool_id: &str, output: &str) -> &mut Self {
        self.line(json!({"type": "user", "uuid": uuid,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_id, "content": output}]}}))
    }

    /// Turn `i` of session `s`: prompt `<s>-p<i>` and reply `<s>-a<i>`, both naming `marker`.
    fn turn(&mut self, s: &str, i: usize, marker: &str) -> &mut Self {
        self.prompt(
            &format!("{s}-p{i}"),
            &format!("Question {i} about {marker}."),
        )
        .reply(
            &format!("{s}-a{i}"),
            &format!("Answer {i}: {marker} is settled."),
        )
    }
}

fn uri(session: &str) -> String {
    format!("claude-code:session:{session}")
}

fn q(session: &str, offset: u64, event: &str) -> TranscriptQuery {
    TranscriptQuery {
        agent: "claude-code".into(),
        session: session.into(),
        offset,
        event: event.into(),
        ..Default::default()
    }
}

/// An upload that must be accepted.
async fn send(svc: &Service, q: TranscriptQuery, bytes: &[u8]) -> TranscriptAnswer {
    match svc
        .capture_transcript(q, Bytes::copy_from_slice(bytes))
        .await
        .unwrap()
    {
        Ok(a) => a,
        Err(cursor) => panic!("the upload was refused as behind the cursor {cursor}"),
    }
}

fn db(svc: &Service) -> rusqlite::Connection {
    alice_core::store::open_reader(&svc.home().db()).unwrap()
}

/// `(ord, text)` of every chunk of the source `uri`, in order.
fn chunks_of(svc: &Service, uri: &str) -> Vec<(i64, String)> {
    let c = db(svc);
    let mut st = c
        .prepare(
            "SELECT ch.ord, ch.text FROM chunks ch JOIN sources s ON s.id = ch.source_id
             WHERE s.uri = ?1 ORDER BY ch.ord",
        )
        .unwrap();
    st.query_map([uri], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn all_chunk_texts(svc: &Service) -> Vec<String> {
    let c = db(svc);
    let mut st = c.prepare("SELECT text FROM chunks").unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// How many chunks (of any source) hold `needle`.
fn chunks_holding(svc: &Service, needle: &str) -> usize {
    all_chunk_texts(svc)
        .iter()
        .filter(|t| t.contains(needle))
        .count()
}

fn known_keys(svc: &Service) -> HashSet<String> {
    let c = db(svc);
    let mut st = c.prepare("SELECT turn_key FROM capture_turns").unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[derive(Debug)]
struct StateRow {
    source_id: String,
    session_id: String,
    byte_offset: u64,
    gaps: i64,
    ended: bool,
    no_distill: bool,
    parent_source_id: Option<String>,
    transcript_path: Option<String>,
    file_key: Option<String>,
    project_slug: String,
    project_key: String,
}

fn state_of(svc: &Service, uri: &str) -> Option<StateRow> {
    use rusqlite::OptionalExtension;
    db(svc)
        .query_row(
            "SELECT c.source_id, c.session_id, c.byte_offset, c.gaps, c.ended, c.no_distill,
                    c.parent_source_id, c.transcript_path, c.file_key, p.slug, p.key
             FROM capture_state c JOIN sources s ON s.id = c.source_id
             JOIN projects p ON p.id = s.project_id WHERE s.uri = ?1",
            [uri],
            |r| {
                Ok(StateRow {
                    source_id: r.get(0)?,
                    session_id: r.get(1)?,
                    byte_offset: r.get::<_, i64>(2)? as u64,
                    gaps: r.get(3)?,
                    ended: r.get(4)?,
                    no_distill: r.get(5)?,
                    parent_source_id: r.get(6)?,
                    transcript_path: r.get(7)?,
                    file_key: r.get(8)?,
                    project_slug: r.get(9)?,
                    project_key: r.get(10)?,
                })
            },
        )
        .optional()
        .unwrap()
}

fn source_exists(svc: &Service, uri: &str) -> bool {
    db(svc)
        .query_row("SELECT count(*) FROM sources WHERE uri = ?1", [uri], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
        > 0
}

/// Waits until `cond` holds (settings are polled every 50 ms in these tests).
async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let t = Instant::now();
    while !cond() {
        assert!(t.elapsed() < Duration::from_secs(20), "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---- From Task 14: the upload protocol ----

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_captures_complete_turns_and_returns_the_cursor() {
    let (_d, svc) = tenant();
    assert!(
        svc.health().await.unwrap().capture.is_none(),
        "no capture health before anything was captured"
    );
    let mut t = Transcript::default();
    t.turn("s1", 1, "alpha").turn("s1", 2, "zanzibarquokka");
    let third = t.len();
    t.prompt("s1-p3", "Run the tests.")
        .tool_use("s1-a3", "toolu_3", "cargo test");
    let mut body = t.bytes.clone();
    body.extend_from_slice(br#"{"type":"user","uuid":"s1-r3","message":{"role":"user","con"#);

    let a = send(
        &svc,
        TranscriptQuery {
            project_header: Some("Repo One".into()),
            ..q("s1", 0, "stop")
        },
        &body,
    )
    .await;
    assert_eq!(
        a.next_offset, third,
        "the cursor stops at the third prompt's line"
    );
    assert_eq!((a.turns_stored, a.turns_skipped), (2, 0));
    assert!(a.chunks_stored >= 2);
    assert!(!a.oversized_line);

    let r = svc
        .recall(RecallRequest {
            query: "zanzibarquokka".into(),
            include_raw: true,
            format: OutputFormat::Json,
            project_hint: Some("Repo One".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        r.results.iter().any(|h| h.tier == Tier::Raw
            && format!("{} {} {:?}", h.title, h.snippet, h.body).contains("zanzibarquokka")),
        "recall finds turn 2: {:?}",
        r.results
    );

    let st = svc.capture_status().await.unwrap();
    assert!(st.tenant && st.enabled);
    let row = st
        .sessions
        .iter()
        .find(|s| s.uri == uri("s1"))
        .expect("the source is listed");
    assert_eq!(row.byte_offset, third);
    assert_eq!(row.project, "repo-one");
    assert_eq!(row.chunks, a.chunks_stored);
    assert!(!row.ended);
    assert_eq!(row.gaps, 0);

    let h = svc
        .health()
        .await
        .unwrap()
        .capture
        .expect("capture health once a session was captured");
    assert_eq!(h.sessions_24h, 1);
    assert_eq!(h.chunks_24h, a.chunks_stored);
    assert_eq!(h.open_sessions, 1);
    assert!(h.last_capture_at.is_some());

    // The next upload starts at the cursor; the rest of turn 3 completes it.
    t.tool_result("s1-r3", "toolu_3", "test result: ok")
        .reply("s1-a3b", "All green.");
    let a2 = send(&svc, q("s1", third, "stop"), &t.bytes[third as usize..]).await;
    assert_eq!(a2.turns_stored, 1);
    assert_eq!(a2.next_offset, t.len());
    assert_eq!(chunks_holding(&svc, "test result: ok"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lower_offset_gets_409_with_the_cursor() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("s2", 1, "one").turn("s2", 2, "two");
    let a = send(&svc, q("s2", 0, "stop"), &t.bytes).await;
    assert_eq!(a.next_offset, t.len());
    let before = all_chunk_texts(&svc).len();

    let answer = svc
        .capture_transcript(q("s2", 10, "stop"), Bytes::from_static(b"not even json\n"))
        .await
        .unwrap();
    match answer {
        Err(cursor) => assert_eq!(cursor, a.next_offset),
        Ok(a) => panic!("a lower offset was accepted: {a:?}"),
    }
    assert_eq!(
        all_chunk_texts(&svc).len(),
        before,
        "nothing parsed or stored"
    );
    assert_eq!(state_of(&svc, &uri("s2")).unwrap().byte_offset, t.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resend_from_zero_stores_nothing_twice() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("s3", 1, "first").turn("s3", 2, "second");
    let cursor = send(&svc, q("s3", 0, "stop"), &t.bytes).await.next_offset;
    let stored = chunks_of(&svc, &uri("s3")).len();
    t.turn("s3", 3, "third");

    // The client lost its state and sends everything from 0: it is told the cursor.
    let jump = svc
        .capture_transcript(q("s3", 0, "stop"), Bytes::from(t.bytes.clone()))
        .await
        .unwrap()
        .expect_err("a resend from 0 is behind the cursor");
    assert_eq!(jump, cursor);
    // Following it stores only the new turn.
    let a = send(&svc, q("s3", jump, "stop"), &t.bytes[jump as usize..]).await;
    assert_eq!(a.turns_stored, 1);
    let chunks = chunks_of(&svc, &uri("s3"));
    assert_eq!(chunks.len(), stored + a.chunks_stored as usize);
    for marker in ["first", "second", "third"] {
        assert_eq!(
            chunks_holding(&svc, &format!("{marker} is settled")),
            1,
            "{marker} once"
        );
    }
    let ords: Vec<i64> = chunks.iter().map(|c| c.0).collect();
    assert_eq!(ords, (0..chunks.len() as i64).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forward_gap_is_accepted_and_counted() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("s4", 1, "before the gap");
    let c1 = t.len();
    t.turn("s4", 2, "inside the gap");
    let c2 = t.len();
    t.turn("s4", 3, "after the gap");
    assert_eq!(
        send(&svc, q("s4", 0, "stop"), &t.bytes[..c1 as usize])
            .await
            .next_offset,
        c1
    );

    // The server's copy is older than the client thinks (a restore, say): accepted as a gap.
    let a = send(&svc, q("s4", c2, "stop"), &t.bytes[c2 as usize..]).await;
    assert_eq!(a.turns_stored, 1);
    assert_eq!(a.next_offset, t.len());
    let row = state_of(&svc, &uri("s4")).unwrap();
    assert_eq!((row.gaps, row.byte_offset), (1, t.len()));
    let listed = svc.capture_status().await.unwrap();
    assert_eq!(
        listed
            .sessions
            .iter()
            .find(|s| s.uri == uri("s4"))
            .unwrap()
            .gaps,
        1
    );
    assert_eq!(chunks_holding(&svc, "after the gap"), 1);
    assert_eq!(chunks_holding(&svc, "inside the gap"), 0);

    // A session the server never saw, sent from beyond 0: registered, then counted as a gap.
    let mut t2 = Transcript::default();
    t2.turn("s4b", 1, "unseen start");
    let c = t2.len();
    t2.turn("s4b", 2, "seen from the middle");
    let a = send(&svc, q("s4b", c, "stop"), &t2.bytes[c as usize..]).await;
    assert_eq!((a.turns_stored, a.next_offset), (1, t2.len()));
    let row = state_of(&svc, &uri("s4b")).unwrap();
    assert_eq!((row.gaps, row.byte_offset), (1, t2.len()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_redacts_and_never_reads_paths() {
    let (d, svc) = tenant();
    let mut t = Transcript::default();
    t.prompt("s5-p1", &format!("My key is {ANTHROPIC}, keep it safe."))
        .reply("s5-a1", "I will not repeat it.");
    let a = send(
        &svc,
        TranscriptQuery {
            cwd: Some("/etc".into()),
            ..q("s5", 0, "stop")
        },
        &t.bytes,
    )
    .await;
    assert_eq!(a.turns_stored, 1);
    let chunks = chunks_of(&svc, &uri("s5"));
    assert!(!chunks.is_empty());
    for (_, text) in &chunks {
        assert!(
            !text.contains(ANTHROPIC_CORE),
            "a stored chunk holds the key"
        );
    }
    assert!(chunks.iter().any(|(_, t)| t.contains("[REDACTED:")));
    // The tenant ignores `cwd`: the source is in `global`, and no project points at /etc.
    let row = state_of(&svc, &uri("s5")).unwrap();
    assert_eq!(row.project_slug, "global");
    assert_eq!((row.transcript_path, row.file_key), (None, None));
    let roots: i64 = db(&svc)
        .query_row(
            "SELECT count(*) FROM projects WHERE root_path LIKE '%etc%' OR slug = 'etc'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(roots, 0);
    svc.shutdown();
    // Nothing under the home holds the key, byte for byte (database, WAL, logs).
    let mut stack = vec![d.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let bytes = std::fs::read(&p).unwrap_or_default();
                assert!(
                    !bytes
                        .windows(ANTHROPIC_CORE.len())
                        .any(|w| w == ANTHROPIC_CORE.as_bytes()),
                    "{} holds the key",
                    p.display()
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_project_hint_shares_the_mcp_placeholder() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("h1", 1, "hinted by header");
    // The hint as the `X-Alice-Project` header carries it.
    send(
        &svc,
        TranscriptQuery {
            project_header: Some("Repo One".into()),
            ..q("h1", 0, "stop")
        },
        &t.bytes,
    )
    .await;
    let row = state_of(&svc, &uri("h1")).unwrap();
    assert_eq!(row.project_slug, "repo-one");
    assert_eq!(row.project_key, "import:repo-one");
    // A later MCP write with the same hint lands in the same project.
    let r = svc
        .remember(RememberRequest {
            content: "a decision made through MCP in Repo One".into(),
            project_hint: Some("Repo One".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(r.project, "repo-one");

    // The hint as the `project` query parameter carries it.
    let mut t2 = Transcript::default();
    t2.turn("h2", 1, "hinted by query");
    send(
        &svc,
        TranscriptQuery {
            project: Some("Repo One".into()),
            ..q("h2", 0, "stop")
        },
        &t2.bytes,
    )
    .await;
    assert_eq!(state_of(&svc, &uri("h2")).unwrap().project_slug, "repo-one");

    // The header wins over the query parameter.
    let mut t3 = Transcript::default();
    t3.turn("h3", 1, "hinted by both");
    send(
        &svc,
        TranscriptQuery {
            project_header: Some("Repo One".into()),
            project: Some("Elsewhere".into()),
            ..q("h3", 0, "stop")
        },
        &t3.bytes,
    )
    .await;
    assert_eq!(state_of(&svc, &uri("h3")).unwrap().project_slug, "repo-one");
    let (repo_one, elsewhere): (i64, i64) = db(&svc)
        .query_row(
            "SELECT (SELECT count(*) FROM projects WHERE slug LIKE 'repo-one%'),
                    (SELECT count(*) FROM projects WHERE slug = 'elsewhere')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((repo_one, elsewhere), (1, 0), "one project, no stray one");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_capture_and_no_distill_flags_are_honoured() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("nc", 1, "never stored")
        .turn("nc", 2, "never stored either");
    let whole = t.len();
    let mut body = t.bytes.clone();
    body.extend_from_slice(b"{\"type\":\"user\",\"half");

    // no_capture: a no-op that answers the offset past the body's last complete line.
    let a = send(
        &svc,
        TranscriptQuery {
            no_capture: Some(1),
            ..q("nc", 0, "stop")
        },
        &body,
    )
    .await;
    assert_eq!(a.next_offset, whole);
    assert_eq!((a.turns_stored, a.chunks_stored), (0, 0));
    let a = send(
        &svc,
        TranscriptQuery {
            no_capture: Some(1),
            ..q("nc", 500, "stop")
        },
        &body,
    )
    .await;
    assert_eq!(a.next_offset, 500 + whole);
    assert!(
        !source_exists(&svc, &uri("nc")),
        "no_capture writes nothing"
    );
    assert_eq!(chunks_holding(&svc, "never stored"), 0);

    // no_distill: captured, and the source is never distilled.
    let mut t2 = Transcript::default();
    t2.turn("nd", 1, "kept but not distilled");
    let a = send(
        &svc,
        TranscriptQuery {
            no_distill: Some(1),
            ..q("nd", 0, "stop")
        },
        &t2.bytes,
    )
    .await;
    assert_eq!(a.turns_stored, 1);
    assert!(state_of(&svc, &uri("nd")).unwrap().no_distill);
    // A later upload without the flag does not switch it off.
    let c = t2.len();
    t2.turn("nd", 2, "still not distilled");
    send(&svc, q("nd", c, "stop"), &t2.bytes[c as usize..]).await;
    assert!(state_of(&svc, &uri("nd")).unwrap().no_distill);
}

// ---- From Task 6: the pipeline ----

#[tokio::test(flavor = "multi_thread")]
async fn a_continuation_file_skips_copied_turns() {
    let (_d, svc) = tenant();
    let mut parent = Transcript::default();
    parent
        .turn("c1", 1, "copied marker one")
        .turn("c1", 2, "copied marker two");
    send(&svc, q("c1", 0, "stop"), &parent.bytes).await;

    // A continued session starts a new file that copies the parent's records (same uuids).
    let mut child = Transcript::default();
    child
        .turn("c1", 1, "copied marker one")
        .turn("c1", 2, "copied marker two")
        .turn("c2", 3, "fresh marker three");
    let a = send(&svc, q("c2", 0, "stop"), &child.bytes).await;
    assert_eq!((a.turns_stored, a.turns_skipped), (1, 2));
    assert_eq!(a.next_offset, child.len());
    let chunks = chunks_of(&svc, &uri("c2"));
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(|(_, t)| t.contains("fresh marker three")));
    assert_eq!(chunks_holding(&svc, "copied marker one"), 1);
    assert_eq!(chunks_holding(&svc, "copied marker two"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn subagent_files_are_their_own_sources() {
    let (_d, svc) = tenant();
    let mut main = Transcript::default();
    main.turn("sa", 1, "the main session");
    send(&svc, q("sa", 0, "stop"), &main.bytes).await;
    let parent = state_of(&svc, &uri("sa")).unwrap();

    let mut agent = Transcript::subagent("agent1");
    agent
        .prompt(
            "sa-agent1-p",
            "Survey the repository layout and report back.",
        )
        .reply(
            "sa-agent1-a",
            &"The repository has a core crate, a daemon crate and a binary package. ".repeat(4),
        );
    let a = send(
        &svc,
        TranscriptQuery {
            subagent: Some("agent1".into()),
            ..q("sa", 0, "subagent-stop")
        },
        &agent.bytes,
    )
    .await;
    assert_eq!(a.turns_stored, 1);
    let sub_uri = "claude-code:session:sa:agent:agent1";
    let row = state_of(&svc, sub_uri).expect("the subagent is a source of its own");
    assert_eq!(
        row.parent_source_id.as_deref(),
        Some(parent.source_id.as_str())
    );
    assert_eq!(
        row.session_id, "sa",
        "a subagent's session is its main session's"
    );
    assert!(chunks_of(&svc, sub_uri)[0].1.contains("core crate"));

    // A subagent whose whole rendering is under 200 characters is dropped.
    let mut tiny = Transcript::subagent("agent2");
    tiny.prompt("sa-agent2-p", "Suggest a title.")
        .reply("sa-agent2-a", "Fine.");
    let a = send(
        &svc,
        TranscriptQuery {
            subagent: Some("agent2".into()),
            ..q("sa", 0, "subagent-stop")
        },
        &tiny.bytes,
    )
    .await;
    assert_eq!((a.turns_stored, a.chunks_stored), (0, 0));
    assert_eq!(
        a.next_offset, 0,
        "the client keeps its place, so later growth is judged whole"
    );
    assert!(!source_exists(&svc, "claude-code:session:sa:agent:agent2"));
    // It grows past 200 characters: the whole file is captured, from its first turn.
    tiny.prompt("sa-agent2-p2", "Now explain the choice.")
        .reply(
            "sa-agent2-a2",
            &"The title names the feature and the reason it exists, nothing more. ".repeat(3),
        );
    let a = send(
        &svc,
        TranscriptQuery {
            subagent: Some("agent2".into()),
            ..q("sa", 0, "subagent-stop")
        },
        &tiny.bytes,
    )
    .await;
    assert_eq!((a.turns_stored, a.next_offset), (2, tiny.len()));
    let row = state_of(&svc, "claude-code:session:sa:agent:agent2").unwrap();
    assert_eq!(row.gaps, 0, "never captured in part as a gap");
    assert_eq!(chunks_holding(&svc, "Suggest a title."), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_excluded_project_moves_the_cursor_and_records_keys() {
    let d = home_with("[capture]\nexcluded_projects = [\"repo-x\"]\n");
    let svc = start_in(d.path(), true);
    let hinted = |q: TranscriptQuery| TranscriptQuery {
        project_header: Some("Repo X".into()),
        ..q
    };
    let mut t = Transcript::default();
    t.turn("ex", 1, "excluded words one")
        .turn("ex", 2, "excluded words two");
    let a = send(&svc, hinted(q("ex", 0, "stop")), &t.bytes).await;
    assert_eq!(a.next_offset, t.len(), "the cursor moves past the turns");
    assert_eq!(
        (a.turns_stored, a.turns_skipped, a.chunks_stored),
        (0, 2, 0)
    );
    let keys = known_keys(&svc);
    assert!(keys.contains("ex-p1") && keys.contains("ex-p2"), "{keys:?}");
    assert_eq!(state_of(&svc, &uri("ex")).unwrap().byte_offset, t.len());
    assert!(chunks_of(&svc, &uri("ex")).is_empty());

    // Removing the exclusion does not backfill the excluded period.
    std::fs::write(svc.home().config_file(), "[capture]\nenabled = true\n").unwrap();
    let settings = svc.settings_handle().clone();
    until("the exclusion is lifted", || {
        settings.get().capture.excluded_projects.is_empty() && !settings.loosening_waiting()
    })
    .await;
    let resend = svc
        .capture_transcript(hinted(q("ex", 0, "stop")), Bytes::from(t.bytes.clone()))
        .await
        .unwrap();
    assert_eq!(resend.unwrap_err(), t.len());
    // A continuation file that copies those records stores none of them.
    let mut child = t.clone();
    child.turn("ex2", 3, "allowed words three");
    let a = send(&svc, hinted(q("ex2", 0, "stop")), &child.bytes).await;
    assert_eq!((a.turns_stored, a.turns_skipped), (1, 2));
    assert_eq!(chunks_holding(&svc, "excluded words"), 0);
    assert_eq!(chunks_holding(&svc, "allowed words three"), 1);
}

/// Ruling 3: the verdict is asked for every name of the source's project. Once `alpha-repo` is
/// merged into `beta-repo`, its name is an alias of `beta-repo`, and an exclusion written as
/// that name excludes `beta-repo`'s sessions, new and existing.
#[tokio::test(flavor = "multi_thread")]
async fn an_alias_in_excluded_projects_excludes_the_project() {
    let d = home_with("[capture]\nexcluded_projects = [\"alpha-repo\"]\n");
    let svc = start_in(d.path(), true);
    let hinted = |name: &str, q: TranscriptQuery| TranscriptQuery {
        project_header: Some(name.into()),
        ..q
    };
    let mut beta = Transcript::default();
    beta.turn("be1", 1, "beta before the merge");
    let a = send(&svc, hinted("Beta Repo", q("be1", 0, "stop")), &beta.bytes).await;
    assert_eq!(a.turns_stored, 1);
    let mut alpha = Transcript::default();
    alpha.turn("al1", 1, "alpha is excluded");
    let a = send(
        &svc,
        hinted("Alpha Repo", q("al1", 0, "stop")),
        &alpha.bytes,
    )
    .await;
    assert_eq!((a.turns_stored, a.turns_skipped), (0, 1));
    svc.merge_projects(ProjectMergeRequest {
        from: "alpha-repo".into(),
        into: "beta-repo".into(),
        ..Default::default()
    })
    .await
    .unwrap();

    let mut beta2 = Transcript::default();
    beta2.turn("be2", 1, "beta session after the merge");
    let a = send(&svc, hinted("Beta Repo", q("be2", 0, "stop")), &beta2.bytes).await;
    assert_eq!((a.turns_stored, a.turns_skipped), (0, 1), "a new session");
    let c = beta.len();
    beta.turn("be1", 2, "beta source after the merge");
    let a = send(&svc, q("be1", c, "stop"), &beta.bytes[c as usize..]).await;
    assert_eq!(
        (a.turns_stored, a.turns_skipped),
        (0, 1),
        "an existing source"
    );
    assert_eq!(chunks_holding(&svc, "after the merge"), 0);
    assert_eq!(chunks_holding(&svc, "beta before the merge"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fail_closed_project_waits_and_keeps_the_cursor() {
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(&d.path().join("home"), false);
    let repo = d.path().join("broken-repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join(".alice.toml"), "[privacy\nno_capture = tru").unwrap();
    let local = |q: TranscriptQuery| TranscriptQuery {
        cwd: Some(repo.to_string_lossy().into_owned()),
        ..q
    };
    let mut t = Transcript::default();
    t.turn("fc", 1, "waiting words one")
        .turn("fc", 2, "waiting words two");

    let a = send(&svc, local(q("fc", 0, "stop")), &t.bytes).await;
    assert_eq!(a.next_offset, 0, "the cursor stays where it was");
    assert_eq!((a.turns_stored, a.chunks_stored), (0, 0));
    assert!(state_of(&svc, &uri("fc")).is_none(), "nothing is stored");
    assert!(known_keys(&svc).is_empty(), "no key is recorded");

    // Fixed, the turns are captured on a later pass.
    std::fs::write(
        repo.join(".alice.toml"),
        "[privacy]\nblock_secrets = false\n",
    )
    .unwrap();
    let t0 = Instant::now();
    loop {
        let a = send(&svc, local(q("fc", 0, "stop")), &t.bytes).await;
        if a.turns_stored > 0 {
            assert_eq!((a.turns_stored, a.next_offset), (2, t.len()));
            break;
        }
        assert_eq!(a.next_offset, 0);
        assert!(t0.elapsed() < Duration::from_secs(30), "never captured");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(chunks_holding(&svc, "waiting words one"), 1);
    assert_eq!(
        state_of(&svc, &uri("fc")).unwrap().project_slug,
        "broken-repo"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn block_mode_withholds_a_chunk_and_keeps_capturing() {
    let d = home_with("[privacy]\nredact_mode = \"block\"\n");
    let svc = start_in(d.path(), false);
    // Long enough that the prompt and the reply are chunks of their own.
    let words = "a plain sentence about the release plan. ".repeat(40);
    let mut t = Transcript::default();
    t.prompt("bm-p1", &format!("Prompt one. {words}"))
        .reply("bm-a1", &format!("Use {GITHUB} for the push. {words}"))
        .turn("bm", 2, "after the secret");
    let a = send(&svc, q("bm", 0, "stop"), &t.bytes).await;
    assert_eq!(a.turns_stored, 2, "the secret does not stop the capture");
    assert_eq!(a.next_offset, t.len());
    let chunks = chunks_of(&svc, &uri("bm"));
    assert!(
        chunks
            .iter()
            .any(|(_, t)| t == "[assistant] [withheld: github_token]"),
        "the withheld chunk keeps its tag: {chunks:?}"
    );
    assert!(chunks.iter().all(|(_, t)| !t.contains(GITHUB_CORE)));
    assert!(
        chunks
            .iter()
            .any(|(_, t)| t.starts_with("[user] Prompt one.")),
        "the turn's other chunk is stored as it was"
    );
    assert_eq!(chunks_holding(&svc, "after the secret is settled"), 1);
}

fn p95(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[(v.len() as f64 * 0.95) as usize - 1]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_sessions_at_once_capture_everything_once_beside_remembers() {
    let (_d, svc) = tenant();
    let mut uploads = Vec::new();
    for s in 0..20 {
        let svc = svc.clone();
        uploads.push(tokio::spawn(async move {
            let session = format!("many{s}");
            let mut t = Transcript::default();
            for i in 0..5 {
                t.turn(&session, i, &format!("marker-{s}-{i}"));
            }
            // Two uploads, the second from the cursor the first returned.
            let half = t.bytes.len() / 2;
            let a = send(&svc, q(&session, 0, "stop"), &t.bytes[..half]).await;
            let b = send(
                &svc,
                q(&session, a.next_offset, "stop"),
                &t.bytes[a.next_offset as usize..],
            )
            .await;
            assert_eq!(b.next_offset, t.len());
            a.turns_stored + b.turns_stored
        }));
    }
    let mut writers = Vec::new();
    for w in 0..5 {
        let svc = svc.clone();
        writers.push(tokio::spawn(async move {
            let mut took = Vec::new();
            for i in 0..10 {
                let t = Instant::now();
                svc.remember(RememberRequest {
                    content: format!("writer {w} decision {i}: keep capture slices short"),
                    ..Default::default()
                })
                .await
                .unwrap();
                took.push(t.elapsed());
            }
            took
        }));
    }
    let mut stored = 0;
    for u in uploads {
        stored += u.await.unwrap();
    }
    let mut took = Vec::new();
    for w in writers {
        took.extend(w.await.unwrap());
    }
    assert_eq!(stored, 100, "every turn stored by exactly one pass");
    let keys = known_keys(&svc);
    for s in 0..20 {
        for i in 0..5 {
            assert!(keys.contains(&format!("many{s}-p{i}")));
            assert_eq!(
                chunks_holding(&svc, &format!("marker-{s}-{i} is settled")),
                1,
                "turn {s}/{i} once"
            );
        }
    }
    assert_eq!(svc.health().await.unwrap().memories, 50, "every remember");
    let p = p95(took);
    assert!(p < Duration::from_millis(500), "remember p95 {p:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forty_megabyte_transcript_is_captured_in_bounded_passes() {
    let (_d, svc) = tenant();
    let output = "x".repeat(9_500);
    let mut t = Transcript::default();
    for i in 0..4_000 {
        t.prompt(&format!("big-p{i}"), &format!("Build step {i}."))
            .tool_use(&format!("big-a{i}"), &format!("toolu_{i}"), "make all")
            .tool_result(&format!("big-r{i}"), &format!("toolu_{i}"), &output);
    }
    assert!(t.len() >= 40_000_000, "{} bytes", t.len());

    // A body larger than a pass is read only up to `MAX_PASS_BYTES`.
    let report = svc
        .capture_upload(q("big", 0, "sweep"), Bytes::from(t.bytes.clone()))
        .await
        .unwrap();
    assert_eq!(
        report.bytes_read,
        alice_core::capture::MAX_PASS_BYTES as u64
    );
    assert!(report.next_offset > 0 && report.next_offset <= report.bytes_read);
    let (mut offset, mut passes) = (report.next_offset, 1);
    while offset < t.len() {
        let end = (offset as usize + TRANSCRIPT_BODY_LIMIT).min(t.bytes.len());
        let body = Bytes::copy_from_slice(&t.bytes[offset as usize..end]);
        let report = svc
            .capture_upload(q("big", offset, "sweep"), body)
            .await
            .unwrap();
        assert!(report.behind.is_none());
        assert!(
            report.bytes_read <= alice_core::capture::MAX_PASS_BYTES as u64,
            "a pass read {} bytes",
            report.bytes_read
        );
        assert!(report.next_offset > offset, "every pass makes progress");
        offset = report.next_offset;
        passes += 1;
        assert!(passes < 50, "too many passes");
    }
    assert!(passes >= 5, "{passes} passes");
    let keys = known_keys(&svc);
    for i in 0..4_000 {
        assert!(keys.contains(&format!("big-p{i}")), "turn {i} is captured");
    }
}

// ---- New with Task 6a ----

#[tokio::test(flavor = "multi_thread")]
async fn a_late_tail_is_stored_once_even_after_a_resend() {
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(d.path(), true);
    let mut t = Transcript::default();
    t.turn("lt", 1, "the first turn");
    let c1 = t.len();
    send(&svc, q("lt", 0, "stop"), &t.bytes).await;
    // A record of turn 1 that arrives after the turn was stored: a late tail.
    t.reply("lt-late", "late words quokkatail arrive after the turn");
    let a = send(&svc, q("lt", c1, "stop"), &t.bytes[c1 as usize..]).await;
    assert_eq!(a.next_offset, t.len());
    assert!(a.chunks_stored >= 1);
    assert_eq!(chunks_holding(&svc, "quokkatail"), 1);
    assert!(known_keys(&svc).contains("tail:lt-late"));

    // The server's copy goes back to before the tail (a restore), and the client resends it.
    svc.shutdown();
    drop(svc);
    {
        let c = rusqlite::Connection::open(AliceHome::new(d.path().to_path_buf()).db()).unwrap();
        c.execute("UPDATE capture_state SET byte_offset = ?1", [c1 as i64])
            .unwrap();
    }
    let svc = start_in(d.path(), true);
    let a = send(&svc, q("lt", c1, "stop"), &t.bytes[c1 as usize..]).await;
    assert_eq!(a.next_offset, t.len());
    assert_eq!(a.chunks_stored, 0, "the tail's key is known");
    assert_eq!(chunks_holding(&svc, "quokkatail"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_uploads_of_one_session_never_interleave() {
    let (_d, svc) = tenant();
    // Several slices: each turn is two chunks of about 1.5 KB.
    let words = "one more sentence for the slice budget. ".repeat(36);
    let mut t = Transcript::default();
    for i in 0..30 {
        t.prompt(&format!("cc-p{i}"), &format!("Prompt {i}. {words}"))
            .reply(&format!("cc-a{i}"), &format!("Reply {i}. {words}"));
    }
    let body = Bytes::from(t.bytes.clone());
    let mut uploads = Vec::new();
    for _ in 0..8 {
        let (svc, body) = (svc.clone(), body.clone());
        uploads.push(tokio::spawn(async move {
            svc.capture_transcript(q("cc", 0, "stop"), body)
                .await
                .unwrap()
        }));
    }
    let mut accepted = Vec::new();
    let mut behind = Vec::new();
    for u in uploads {
        match u.await.unwrap() {
            Ok(a) => accepted.push(a),
            Err(cursor) => behind.push(cursor),
        }
    }
    assert_eq!(accepted.len(), 1, "one pass at a time: {accepted:?}");
    assert_eq!(accepted[0].turns_stored, 30);
    assert_eq!(behind, vec![t.len(); 7], "the others are told the cursor");
    let chunks = chunks_of(&svc, &uri("cc"));
    assert_eq!(chunks.len() as u64, accepted[0].chunks_stored);
    let ords: Vec<i64> = chunks.iter().map(|c| c.0).collect();
    assert_eq!(ords, (0..chunks.len() as i64).collect::<Vec<_>>());
    let distinct: HashSet<&String> = chunks.iter().map(|c| &c.1).collect();
    assert_eq!(distinct.len(), chunks.len(), "no chunk twice");
    // Ruling 5: one `source.capture` event for the pass, though it took several slices.
    let source_id = state_of(&svc, &uri("cc")).unwrap().source_id;
    let events: i64 = db(&svc)
        .query_row(
            "SELECT count(*) FROM events WHERE action = 'source.capture' AND target_id = ?1",
            [&source_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tenant_start_clears_local_paths_from_capture_state() {
    let d = tempfile::tempdir().unwrap();
    let local = start_in(d.path(), false);
    let mut t = Transcript::default();
    t.turn("ad", 1, "captured on the laptop");
    send(&local, q("ad", 0, "stop"), &t.bytes).await;
    local.shutdown();
    drop(local);
    // As a local capture worker would have left it.
    {
        let c = rusqlite::Connection::open(AliceHome::new(d.path().to_path_buf()).db()).unwrap();
        c.execute(
            "UPDATE capture_state SET transcript_path = '/Users/someone/.claude/projects/x/ad.jsonl',
                file_key = '16777220:4242:abcdef'",
            [],
        )
        .unwrap();
    }
    for _ in 0..2 {
        let svc = start_in(d.path(), true);
        let row = state_of(&svc, &uri("ad")).unwrap();
        assert_eq!((row.transcript_path, row.file_key), (None, None));
        assert_eq!(row.byte_offset, t.len(), "the cursor travels");
        svc.shutdown();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_tool_call_keeps_the_turn_open_until_session_end() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("ut", 1, "finished first");
    let c1 = t.len();
    t.prompt("ut-p2", "Watch the build.")
        .tool_use("ut-a2", "toolu_w", "sleep 999");
    let a = send(&svc, q("ut", 0, "stop"), &t.bytes).await;
    assert_eq!((a.turns_stored, a.next_offset), (1, c1));
    let a = send(&svc, q("ut", c1, "stop"), &t.bytes[c1 as usize..]).await;
    assert_eq!(
        (a.turns_stored, a.next_offset),
        (0, c1),
        "a call without its result keeps the turn open"
    );
    let a = send(&svc, q("ut", c1, "session-end"), &t.bytes[c1 as usize..]).await;
    assert_eq!((a.turns_stored, a.next_offset), (1, t.len()));
    assert_eq!(chunks_holding(&svc, "[tool Bash] sleep 999"), 1);
    assert!(state_of(&svc, &uri("ut")).unwrap().ended);
    let st = svc.capture_status().await.unwrap();
    assert!(st.sessions.iter().any(|s| s.uri == uri("ut") && s.ended));
}

// ---- Fix round 1 (Task 6a review) ----

/// An offset beyond any transcript (over 2^53) is refused before anything is read or written:
/// unchecked, it would overflow or be stored saturated and wedge the session.
#[tokio::test(flavor = "multi_thread")]
async fn an_offset_beyond_any_transcript_is_refused_and_leaves_no_state() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("big-off", 1, "an ordinary turn");
    for offset in [(1u64 << 53) + 1, u64::MAX] {
        for no_capture in [None, Some(1)] {
            let e = svc
                .capture_transcript(
                    TranscriptQuery {
                        no_capture,
                        ..q("big-off", offset, "stop")
                    },
                    Bytes::from(t.bytes.clone()),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(e, alice_core::AliceError::Invalid(_)),
                "{offset}: {e}"
            );
        }
    }
    assert!(!source_exists(&svc, &uri("big-off")), "no state is left");

    // An existing source keeps its cursor and counts no gap.
    let cursor = send(&svc, q("big-off", 0, "stop"), &t.bytes)
        .await
        .next_offset;
    for offset in [(1u64 << 53) + 1, u64::MAX] {
        let e = svc
            .capture_transcript(q("big-off", offset, "stop"), Bytes::from_static(b"{}\n"))
            .await
            .unwrap_err();
        assert!(matches!(e, alice_core::AliceError::Invalid(_)), "{e}");
    }
    let row = state_of(&svc, &uri("big-off")).unwrap();
    assert_eq!((row.byte_offset, row.gaps), (cursor, 0));
}

/// Controller ruling: Codex transcripts are refused until their reader exists (Task 17), so a
/// client never re-sends a growing body that is never consumed, and no empty source is made.
#[tokio::test(flavor = "multi_thread")]
async fn codex_uploads_are_refused_until_their_reader_exists() {
    let (_d, svc) = tenant();
    for event in ["session-start", "stop"] {
        let e = svc
            .capture_transcript(
                TranscriptQuery {
                    agent: "codex".into(),
                    ..q("cx1", 0, event)
                },
                Bytes::from_static(b"{\"type\":\"session_meta\"}\n"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&e, alice_core::AliceError::Invalid(m) if m.contains("Codex")),
            "{e}"
        );
    }
    assert!(!source_exists(&svc, "codex:session:cx1"));
    assert!(svc.capture_status().await.unwrap().sessions.is_empty());
}

/// `no_distill=1` turns the flag on for an existing source even when nothing new arrives.
#[tokio::test(flavor = "multi_thread")]
async fn no_distill_turns_on_for_an_existing_source_with_nothing_new() {
    let (_d, svc) = tenant();
    let mut t = Transcript::default();
    t.turn("nd2", 1, "kept and distilled for now");
    let cursor = send(&svc, q("nd2", 0, "stop"), &t.bytes).await.next_offset;
    assert!(!state_of(&svc, &uri("nd2")).unwrap().no_distill);
    let chunks = chunks_of(&svc, &uri("nd2")).len();

    let a = send(
        &svc,
        TranscriptQuery {
            no_distill: Some(1),
            ..q("nd2", cursor, "stop")
        },
        b"",
    )
    .await;
    assert_eq!((a.next_offset, a.turns_stored), (cursor, 0));
    let row = state_of(&svc, &uri("nd2")).unwrap();
    assert!(row.no_distill, "the flag turned on");
    assert_eq!(row.byte_offset, cursor);
    assert_eq!(chunks_of(&svc, &uri("nd2")).len(), chunks);
}

/// `session-start` registers a session the server never saw, even with nothing complete yet.
#[tokio::test(flavor = "multi_thread")]
async fn session_start_registers_a_session_with_nothing_complete() {
    let (_d, svc) = tenant();
    let a = send(
        &svc,
        q("ss1", 0, "session-start"),
        br#"{"type":"user","uuid":"ss1-p1","message":{"role":"us"#,
    )
    .await;
    assert_eq!((a.next_offset, a.turns_stored, a.chunks_stored), (0, 0, 0));
    let row = state_of(&svc, &uri("ss1")).expect("the session is registered");
    assert_eq!((row.byte_offset, row.gaps, row.ended), (0, 0, false));
    assert!(chunks_of(&svc, &uri("ss1")).is_empty());
    let st = svc.capture_status().await.unwrap();
    assert!(
        st.sessions
            .iter()
            .any(|s| s.uri == uri("ss1") && s.chunks == 0)
    );
    // Any other event with nothing complete writes nothing for a session never seen.
    send(
        &svc,
        q("ss2", 0, "stop"),
        br#"{"type":"user","uuid":"ss2-p1","message":{"role":"us"#,
    )
    .await;
    assert!(!source_exists(&svc, &uri("ss2")));
}

/// A pass that waits (the project's rules are unknown) records no gap, even from beyond the
/// cursor; the gap is counted once, by the pass that captures.
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_pass_records_no_gap() {
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(&d.path().join("home"), false);
    let repo = d.path().join("gap-repo");
    std::fs::create_dir(&repo).unwrap();
    let local = |q: TranscriptQuery| TranscriptQuery {
        cwd: Some(repo.to_string_lossy().into_owned()),
        ..q
    };
    let mut t = Transcript::default();
    t.turn("wg", 1, "captured before the break");
    let c1 = t.len();
    t.turn("wg", 2, "skipped by the client");
    let c2 = t.len();
    t.turn("wg", 3, "sent from beyond the cursor");
    assert_eq!(
        send(&svc, local(q("wg", 0, "stop")), &t.bytes[..c1 as usize])
            .await
            .next_offset,
        c1
    );

    std::fs::write(repo.join(".alice.toml"), "[privacy\nno_capture = tru").unwrap();
    let a = send(&svc, local(q("wg", c2, "stop")), &t.bytes[c2 as usize..]).await;
    assert_eq!(
        (a.next_offset, a.turns_stored),
        (c2, 0),
        "the pass waits: nothing consumed"
    );
    let row = state_of(&svc, &uri("wg")).unwrap();
    assert_eq!((row.gaps, row.byte_offset), (0, c1), "no gap is recorded");

    std::fs::write(
        repo.join(".alice.toml"),
        "[privacy]\nblock_secrets = false\n",
    )
    .unwrap();
    let t0 = Instant::now();
    loop {
        let a = send(&svc, local(q("wg", c2, "stop")), &t.bytes[c2 as usize..]).await;
        if a.turns_stored > 0 {
            assert_eq!(a.next_offset, t.len());
            break;
        }
        assert_eq!(a.next_offset, c2);
        assert!(t0.elapsed() < Duration::from_secs(30), "never captured");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let row = state_of(&svc, &uri("wg")).unwrap();
    assert_eq!((row.gaps, row.byte_offset), (1, t.len()), "counted once");
}

// ---- Task 18a: import first, then capture (spec §9.2) ----

#[path = "common/palace.rs"]
mod palace;

/// MemPalace's drawers of `session`'s transcript, one per `(text, authored_at)`: what M3
/// imports as the source `file://…/<session>.jsonl` whose `ended_at` is the newest time.
fn mined(
    session: &str,
    parts: &[(&str, Option<&str>)],
) -> Vec<alice_core::import::fixture::DrawerSpec> {
    let path = format!("/h/.claude/projects/-Users-me-Code-pg-ai/{session}.jsonl");
    parts
        .iter()
        .enumerate()
        .map(|(i, (text, at))| {
            let mut d = palace::chunk(
                &format!("{session}_{i:03}"),
                text,
                "sessions",
                &path,
                i as i64,
            );
            d.meta.authored_at = at.map(str::to_string);
            d
        })
        .collect()
}

/// Imports a palace of `specs`, built under `dir`, and waits for the job.
async fn import_palace(
    svc: &Arc<Service>,
    dir: &Path,
    specs: &[alice_core::import::fixture::DrawerSpec],
) -> alice_core::import::report::ImportReport {
    let palace = dir.join("palace");
    std::fs::create_dir_all(&palace).unwrap();
    alice_core::import::fixture::build_sqlite_exact(&palace, specs);
    svc.start_import(alice_core::import::ImportRequest {
        palace: Some(palace.display().to_string()),
        ..Default::default()
    })
    .unwrap();
    let t = Instant::now();
    loop {
        let st = svc.import_status();
        if st.state != alice_core::import::ImportState::Running {
            let r = st.report.expect("the import reports");
            assert!(r.errors.is_empty(), "{:?}", r.errors);
            return r;
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the import did not end"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `ended_at` of the non-capture source `uri`.
fn ended_at_of(svc: &Service, uri: &str) -> Option<String> {
    db(svc)
        .query_row("SELECT ended_at FROM sources WHERE uri = ?1", [uri], |r| {
            r.get(0)
        })
        .unwrap()
}

fn imported_uri(session: &str) -> String {
    format!("file:///h/.claude/projects/-Users-me-Code-pg-ai/{session}.jsonl")
}

/// Import first, then capture: the first capture of a session MemPalace mined stores only the
/// turns that started after the import's end. The earlier turns' keys are recorded as skipped
/// and the cursor moves past them, so they are never stored again, by any source.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_mempalace_mined_continues_after_the_import() {
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(&d.path().join("home"), false);
    // MemPalace mined the session up to 00:00:04: its first two turns.
    let r = import_palace(
        &svc,
        d.path(),
        &mined(
            "mm1",
            &[
                ("mined text of turn one", Some("2026-10-16T00:00:02Z")),
                ("mined text of turn two", Some("2026-10-16T00:00:04Z")),
            ],
        ),
    )
    .await;
    assert_eq!(r.drawers_seen, r.drawers_accounted);
    assert_eq!(
        ended_at_of(&svc, &imported_uri("mm1")).as_deref(),
        Some("2026-10-16T00:00:04.000Z")
    );

    // Turns start at 00:00:01, :03, :05 and :07.
    let mut t = Transcript::default();
    t.turn("mm1", 1, "first words")
        .turn("mm1", 2, "second words")
        .turn("mm1", 3, "third words")
        .turn("mm1", 4, "fourth words");
    let a = send(&svc, q("mm1", 0, "stop"), &t.bytes).await;
    assert_eq!((a.turns_stored, a.turns_skipped), (2, 2));
    assert_eq!(a.next_offset, t.len(), "the cursor moves past every turn");
    assert_eq!(chunks_holding(&svc, "first words is settled"), 0);
    assert_eq!(chunks_holding(&svc, "second words is settled"), 0);
    assert_eq!(chunks_holding(&svc, "third words is settled"), 1);
    assert_eq!(chunks_holding(&svc, "fourth words is settled"), 1);
    assert_eq!(
        chunks_holding(&svc, "mined text of turn"),
        2,
        "the import's"
    );
    let keys = known_keys(&svc);
    for k in ["mm1-p1", "mm1-p2", "mm1-p3", "mm1-p4"] {
        assert!(keys.contains(k), "{k} is recorded: {keys:?}");
    }
    let row = state_of(&svc, &uri("mm1")).unwrap();
    assert_eq!(row.byte_offset, t.len());

    // A re-upload from 0 stores nothing new: the cursor is past it.
    let again = svc
        .capture_transcript(q("mm1", 0, "stop"), Bytes::copy_from_slice(&t.bytes))
        .await
        .unwrap();
    assert_eq!(again.unwrap_err(), t.len());
    // A forked session that copies the records (same uuids) stores none of them: the skipped
    // turns' keys hold them too.
    let fork = send(&svc, q("mm1-fork", 0, "stop"), &t.bytes).await;
    assert_eq!((fork.turns_stored, fork.turns_skipped), (0, 4));
    assert_eq!(chunks_holding(&svc, "first words is settled"), 0);
    assert_eq!(chunks_holding(&svc, "third words is settled"), 1);
}

/// An imported transcript with no end time is no evidence of what it holds: the first capture
/// stores every turn.
#[tokio::test(flavor = "multi_thread")]
async fn an_imported_source_without_an_end_time_captures_everything() {
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(&d.path().join("home"), false);
    import_palace(
        &svc,
        d.path(),
        &mined("ne1", &[("mined text with no time", None)]),
    )
    .await;
    assert!(source_exists(&svc, &imported_uri("ne1")));
    assert_eq!(ended_at_of(&svc, &imported_uri("ne1")), None);

    let mut t = Transcript::default();
    t.turn("ne1", 1, "first words")
        .turn("ne1", 2, "second words");
    let a = send(&svc, q("ne1", 0, "stop"), &t.bytes).await;
    assert_eq!((a.turns_stored, a.turns_skipped), (2, 0));
    assert_eq!(chunks_holding(&svc, "first words is settled"), 1);
    assert_eq!(chunks_holding(&svc, "second words is settled"), 1);
}

/// The rule applies while the source is new: once it holds a captured turn, a later pass is not
/// filtered again, even by a turn whose time is before the import's end. Times are compared as
/// times: the end here is written with an offset.
#[tokio::test(flavor = "multi_thread")]
async fn the_continue_rule_applies_only_to_a_new_source() {
    use alice_core::store::sources::{ChunkInput, SourceInput};
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(&d.path().join("home"), false);
    // 07:00:04+07:00 is 00:00:04Z; as text it sorts after every turn time below.
    svc.ingest(IngestRequest {
        source: SourceInput {
            uri: imported_uri("cr1"),
            session_id: Some("cr1".into()),
            ended_at: Some("2026-10-16T07:00:04+07:00".into()),
            ..Default::default()
        },
        chunks: vec![ChunkInput {
            ord: 0,
            text: "an earlier transcript of cr1".into(),
            ..Default::default()
        }],
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        ended_at_of(&svc, &imported_uri("cr1")).as_deref(),
        Some("2026-10-16T07:00:04+07:00")
    );

    let mut t = Transcript::default();
    t.turn("cr1", 1, "first words")
        .turn("cr1", 2, "second words")
        .turn("cr1", 3, "third words");
    let a = send(&svc, q("cr1", 0, "stop"), &t.bytes).await;
    assert_eq!(
        (a.turns_stored, a.turns_skipped),
        (1, 2),
        "turns 1 and 2 started by 00:00:04, turn 3 after"
    );
    assert_eq!(chunks_holding(&svc, "third words is settled"), 1);

    // A later turn stamped before the import's end (a clock that went back) is stored: the
    // source is not new any more.
    let from = t.len();
    t.clock = 0;
    t.turn("cr1", 4, "fourth words");
    let b = send(&svc, q("cr1", from, "stop"), &t.bytes[from as usize..]).await;
    assert_eq!((b.turns_stored, b.turns_skipped), (1, 0));
    assert_eq!(chunks_holding(&svc, "fourth words is settled"), 1);
}

/// A first capture can take several uploads (each body is capped, and `session-start` may
/// register the source before any turn is complete). The source stays new until it holds a
/// captured turn, so every imported turn is skipped, whichever upload holds it.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_capture_in_several_uploads_skips_every_imported_turn() {
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(&d.path().join("home"), false);
    import_palace(
        &svc,
        d.path(),
        &mined(
            "mu1",
            &[("mined text of mu1", Some("2026-10-16T00:00:06Z"))],
        ),
    )
    .await;
    // Turns start at 00:00:01, :03, :05 and :07: the first three by the import's end.
    let mut t = Transcript::default();
    t.turn("mu1", 1, "first words")
        .turn("mu1", 2, "second words");
    let third = t.len();
    t.turn("mu1", 3, "third words");
    let fourth = t.len();
    t.turn("mu1", 4, "fourth words");

    // `session-start` registers the session with nothing complete yet.
    let a = send(&svc, q("mu1", 0, "session-start"), &t.bytes[..10]).await;
    assert_eq!((a.next_offset, a.turns_stored), (0, 0));
    assert!(
        state_of(&svc, &uri("mu1")).is_some(),
        "the source is registered"
    );
    // A capped body: turns 1 and 2 are complete, turn 3 stays open.
    let b = send(
        &svc,
        TranscriptQuery {
            more: Some(1),
            ..q("mu1", 0, "stop")
        },
        &t.bytes[..fourth as usize],
    )
    .await;
    assert_eq!(
        (b.next_offset, b.turns_stored, b.turns_skipped),
        (third, 0, 2)
    );
    // The rest: turn 3 is skipped too, turn 4 is stored.
    let c = send(&svc, q("mu1", third, "stop"), &t.bytes[third as usize..]).await;
    assert_eq!(
        (c.next_offset, c.turns_stored, c.turns_skipped),
        (t.len(), 1, 1)
    );
    for (marker, n) in [
        ("first words is settled", 0),
        ("second words is settled", 0),
        ("third words is settled", 0),
        ("fourth words is settled", 1),
    ] {
        assert_eq!(chunks_holding(&svc, marker), n, "{marker}");
    }
    let keys = known_keys(&svc);
    assert!(
        ["mu1-p1", "mu1-p2", "mu1-p3"]
            .iter()
            .all(|k| keys.contains(*k))
    );
}

/// A new source's first upload can start inside a turn (a client that lost its state sends from
/// beyond the cursor): the late tail of a turn the import holds is skipped too, timed by its
/// first record.
#[tokio::test(flavor = "multi_thread")]
async fn the_tail_of_an_imported_turn_is_skipped_too() {
    let d = tempfile::tempdir().unwrap();
    let svc = start_in(&d.path().join("home"), false);
    import_palace(
        &svc,
        d.path(),
        &mined(
            "tl1",
            &[("mined text of tl1", Some("2026-10-16T00:00:04Z"))],
        ),
    )
    .await;
    // Turn 2's reply is stamped 00:00:04, turn 3 starts at 00:00:05.
    let mut t = Transcript::default();
    t.turn("tl1", 1, "first words")
        .prompt("tl1-p2", "Question 2 about second words.");
    let reply = t.len();
    t.reply("tl1-a2", "Answer 2: second words is settled.")
        .turn("tl1", 3, "third words");
    let a = send(&svc, q("tl1", reply, "stop"), &t.bytes[reply as usize..]).await;
    assert_eq!((a.next_offset, a.turns_stored), (t.len(), 1));
    assert_eq!(state_of(&svc, &uri("tl1")).unwrap().gaps, 1);
    assert_eq!(chunks_holding(&svc, "second words is settled"), 0);
    assert_eq!(chunks_holding(&svc, "third words is settled"), 1);
    assert!(
        known_keys(&svc).contains("tail:tl1-a2"),
        "{:?}",
        known_keys(&svc)
    );
}

// ---- Task 6b: local capture (the daemon reads the transcript after a notify) ----

/// Local timings short enough for tests: the settle delay and the sweep period. The quiet and
/// stale ages keep their defaults; tests age a transcript's mtime instead of waiting.
fn local_options() -> ServiceOptions {
    ServiceOptions {
        capture_settle: Duration::from_millis(50),
        capture_sweep_every: Duration::from_millis(100),
        ..options(false)
    }
}

fn start_with(dir: &Path, opts: ServiceOptions) -> Arc<Service> {
    Service::start(
        AliceHome::new(dir.to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        opts,
    )
    .unwrap()
}

/// A local daemon whose home is `<dir>/home`; transcripts and repositories go beside it.
fn local() -> (tempfile::TempDir, Arc<Service>) {
    let d = tempfile::tempdir().unwrap();
    let svc = start_with(&d.path().join("home"), local_options());
    (d, svc)
}

/// What `alice hook <event>` sends for session `session`, whose transcript is `path`.
fn notify(session: &str, path: &Path, event: &str) -> CaptureNotify {
    CaptureNotify {
        agent: "claude-code".into(),
        event: event.into(),
        session_id: session.into(),
        transcript_path: path.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

/// Sets `path`'s modification time to `age` ago (an idle transcript, without waiting).
fn age(path: &Path, age: Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - age)
        .unwrap();
}

/// [`until`] with its own time limit, for a test that reads tens of megabytes.
async fn until_within(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let t = Instant::now();
    while !cond() {
        assert!(t.elapsed() < limit, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Passes the local worker has run.
fn passes(svc: &Service) -> u64 {
    svc.capture_counts().passes
}

fn capture_events(svc: &Service, uri: &str) -> i64 {
    let Some(row) = state_of(svc, uri) else {
        return 0;
    };
    db(svc)
        .query_row(
            "SELECT count(*) FROM events WHERE action = 'source.capture' AND target_id = ?1",
            [&row.source_id],
            |r| r.get(0),
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_notify_captures_the_completed_turn() {
    let (d, svc) = local();
    let repo = d.path().join("notify-repo");
    std::fs::create_dir(&repo).unwrap();
    let path = d.path().join("ns1.jsonl");
    let mut t = Transcript::default();
    t.turn("ns1", 1, "the first marker")
        .turn("ns1", 2, "wombatlantern");
    std::fs::write(&path, &t.bytes).unwrap();

    svc.capture_notify(CaptureNotify {
        cwd: Some(repo.to_string_lossy().into_owned()),
        ..notify("ns1", &path, "stop")
    })
    .await
    .unwrap();
    until("the stop's pass stores turn 2", || {
        chunks_holding(&svc, "wombatlantern is settled") == 1
    })
    .await;

    let st = svc.capture_status().await.unwrap();
    assert!(!st.tenant);
    let row = st
        .sessions
        .iter()
        .find(|s| s.uri == uri("ns1"))
        .expect("the source is listed");
    assert_eq!(
        row.byte_offset,
        t.len(),
        "the stop closed the trailing turn"
    );
    assert_eq!(row.project, "notify-repo", "the project is the cwd's");
    assert!(!row.ended);
    assert_eq!(row.gaps, 0);
    let state = state_of(&svc, &uri("ns1")).unwrap();
    assert_eq!(
        state.transcript_path.as_deref(),
        Some(path.to_string_lossy().as_ref())
    );
    let key = state
        .file_key
        .expect("a local source keeps its file's identity");
    let parts: Vec<&str> = key.split(':').collect();
    assert_eq!(parts.len(), 3, "dev:inode:sha256(first line): {key}");
    assert_eq!(parts[2].len(), 64);
    assert_eq!(chunks_holding(&svc, "the first marker is settled"), 1);

    let r = svc
        .recall(RecallRequest {
            query: "wombatlantern".into(),
            include_raw: true,
            format: OutputFormat::Json,
            cwd: Some(repo.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        r.results.iter().any(|h| h.tier == Tier::Raw
            && format!("{} {} {:?}", h.title, h.snippet, h.body).contains("wombatlantern")),
        "recall finds turn 2: {:?}",
        r.results
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repeating_the_notify_stores_nothing_new() {
    let (d, svc) = local();
    let path = d.path().join("rn.jsonl");
    let mut t = Transcript::default();
    t.turn("rn", 1, "once only").turn("rn", 2, "twice never");
    std::fs::write(&path, &t.bytes).unwrap();
    svc.capture_notify(notify("rn", &path, "stop"))
        .await
        .unwrap();
    until("the first pass", || {
        state_of(&svc, &uri("rn")).is_some_and(|s| s.byte_offset == t.len()) && passes(&svc) == 1
    })
    .await;
    let chunks = chunks_of(&svc, &uri("rn"));
    assert!(!chunks.is_empty());

    for _ in 0..3 {
        let before = passes(&svc);
        svc.capture_notify(notify("rn", &path, "stop"))
            .await
            .unwrap();
        until("the repeated pass ran", || passes(&svc) > before).await;
    }
    assert_eq!(chunks_of(&svc, &uri("rn")), chunks, "nothing new is stored");
    assert_eq!(capture_events(&svc, &uri("rn")), 1, "one pass stored");
    assert_eq!(state_of(&svc, &uri("rn")).unwrap().byte_offset, t.len());
}

/// Spec §10: a pass killed between the slices of one turn neither loses nor doubles a chunk.
/// The test hook fails the pass after its first slice (of a turn three slices long); the next
/// pass renders the turn again and `(source, ord)` drops what landed already.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_between_slices_neither_loses_nor_doubles() {
    // About 75 KB of distinct sentences: some 40 chunks of 2,000 characters, three slices.
    let long: String = (0..2_200)
        .map(|i| format!("Sentence {i} of the long answer. "))
        .collect();
    let mut t = Transcript::default();
    t.turn("kb", 1, "a short first turn")
        .prompt("kb-p2", "Explain everything at length.")
        .reply("kb-a2", &long)
        .turn("kb", 3, "a short last turn");

    let clean_dir = tempfile::tempdir().unwrap();
    let clean = start_with(&clean_dir.path().join("home"), local_options());
    let clean_path = clean_dir.path().join("kb.jsonl");
    std::fs::write(&clean_path, &t.bytes).unwrap();
    clean
        .capture_notify(notify("kb", &clean_path, "stop"))
        .await
        .unwrap();
    until("the clean capture", || {
        state_of(&clean, &uri("kb")).is_some_and(|s| s.byte_offset == t.len())
    })
    .await;
    let expected = chunks_of(&clean, &uri("kb"));
    let distinct: HashSet<&String> = expected.iter().map(|c| &c.1).collect();
    assert_eq!(distinct.len(), expected.len(), "every chunk text differs");

    // No sweep after the one at start: only the notifies below pass the transcript, so the
    // state after the failed pass is the one inspected.
    let d = tempfile::tempdir().unwrap();
    let svc = start_with(
        &d.path().join("home"),
        ServiceOptions {
            capture_fail_after_slices: Some(1),
            capture_sweep_every: Duration::from_secs(3_600),
            ..local_options()
        },
    );
    let path = d.path().join("kb.jsonl");
    std::fs::write(&path, &t.bytes).unwrap();
    svc.capture_notify(notify("kb", &path, "stop"))
        .await
        .unwrap();
    until("the first pass ran", || passes(&svc) >= 1).await;
    let partial = chunks_of(&svc, &uri("kb"));
    assert!(
        !partial.is_empty() && partial.len() < expected.len(),
        "the first slice landed, the rest did not: {} of {}",
        partial.len(),
        expected.len()
    );
    assert!(
        !known_keys(&svc).contains("kb-p2"),
        "the long turn's key waits for its last slice"
    );

    svc.capture_notify(notify("kb", &path, "stop"))
        .await
        .unwrap();
    until("a later pass completes the turn", || {
        chunks_of(&svc, &uri("kb")) == expected
    })
    .await;
    assert!(known_keys(&svc).contains("kb-p2"));
    assert_eq!(state_of(&svc, &uri("kb")).unwrap().byte_offset, t.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replaced_file_resets_and_skips_stored_turns() {
    let (d, svc) = local();
    let path = d.path().join("rf.jsonl");
    let mut first = Transcript::default();
    first
        .prompt(
            "rf-p1",
            &format!(
                "Question 1 about the original layout. {}",
                "padding words ".repeat(40)
            ),
        )
        .reply("rf-a1", "Answer 1: the original layout is settled.")
        .turn("rf", 2, "the second marker");
    std::fs::write(&path, &first.bytes).unwrap();
    svc.capture_notify(notify("rf", &path, "stop"))
        .await
        .unwrap();
    until("turns 1 and 2", || {
        state_of(&svc, &uri("rf")).is_some_and(|s| s.byte_offset == first.len())
    })
    .await;
    let key = state_of(&svc, &uri("rf")).unwrap().file_key;

    // A new file (another inode, another first line) replaces it: turns 1 and 2 written
    // shorter, then a new turn 3, whose first line the old cursor falls inside.
    let mut second = Transcript::default();
    second
        .prompt("rf-p1", "Question 1 about the original layout.")
        .reply("rf-a1", "Answer 1: the original layout is settled.")
        .turn("rf", 2, "the second marker");
    let p3 = second.len();
    second.prompt(
        "rf-p3",
        &format!(
            "Question 3 about the replacement. {}",
            "more words ".repeat(60)
        ),
    );
    let p3_end = second.len();
    second.reply("rf-a3", "Answer 3: replacementmarker is settled.");
    assert!(p3 < first.len() && first.len() < p3_end);
    let tmp = d.path().join("rf.jsonl.tmp");
    std::fs::write(&tmp, &second.bytes).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    svc.capture_notify(notify("rf", &path, "stop"))
        .await
        .unwrap();
    until("turn 3 is stored as a turn", || {
        known_keys(&svc).contains("rf-p3")
    })
    .await;
    assert_eq!(chunks_holding(&svc, "replacementmarker is settled"), 1);
    assert_eq!(chunks_holding(&svc, "Question 3 about the replacement."), 1);
    assert_eq!(chunks_holding(&svc, "the original layout is settled"), 1);
    assert_eq!(chunks_holding(&svc, "the second marker is settled"), 1);
    let row = state_of(&svc, &uri("rf")).unwrap();
    assert_eq!(row.byte_offset, second.len());
    assert_ne!(row.file_key, key, "the new file's identity is kept");
    assert_eq!(row.gaps, 0);

    // Cut in place to fewer bytes than the cursor (same inode, same first line): turn 1 and a
    // new turn 4.
    let mut third = Transcript::default();
    third
        .prompt("rf-p1", "Question 1 about the original layout.")
        .reply("rf-a1", "Answer 1: the original layout is settled.")
        .turn("rf", 4, "truncatedmarker");
    assert!(third.len() < second.len());
    std::fs::write(&path, &third.bytes).unwrap();
    svc.capture_notify(notify("rf", &path, "stop"))
        .await
        .unwrap();
    until("turn 4", || {
        chunks_holding(&svc, "truncatedmarker is settled") == 1
    })
    .await;
    assert_eq!(chunks_holding(&svc, "the original layout is settled"), 1);
    assert_eq!(state_of(&svc, &uri("rf")).unwrap().byte_offset, third.len());
}

/// Ruling 1: the sweep closes a trailing turn once its transcript has been quiet for
/// `capture_quiet` (5 minutes), as a `Stop` would.
#[tokio::test(flavor = "multi_thread")]
async fn the_trailing_turn_is_closed_by_the_quiet_sweep() {
    let (d, svc) = local();
    let path = d.path().join("qs.jsonl");
    let mut t = Transcript::default();
    t.turn("qs", 1, "the opening turn");
    let second = t.len();
    t.turn("qs", 2, "quietmarker");
    std::fs::write(&path, &t.bytes).unwrap();
    // SessionStart closes nothing: turn 1 is complete because turn 2 follows it.
    svc.capture_notify(notify("qs", &path, "session-start"))
        .await
        .unwrap();
    until("turn 1", || {
        state_of(&svc, &uri("qs")).is_some_and(|s| s.byte_offset == second)
    })
    .await;
    // A few sweeps of a fresh transcript leave the trailing turn open.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(chunks_holding(&svc, "quietmarker"), 0);

    age(&path, Duration::from_secs(10 * 60));
    until("the quiet sweep stores turn 2", || {
        chunks_holding(&svc, "quietmarker is settled") == 1
    })
    .await;
    let row = state_of(&svc, &uri("qs")).unwrap();
    assert_eq!(row.byte_offset, t.len());
    assert!(!row.ended, "quiet is not stale");
}

/// Ruling 1: a turn whose tool call has no answer stays open through `Stop` and the quiet
/// sweep; after `capture_stale` (24 hours) without an event the sweep closes it whatever it
/// waits for and ends the source.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_session_is_ended_by_the_sweep() {
    let (d, svc) = local();
    let path = d.path().join("st.jsonl");
    let mut t = Transcript::default();
    t.turn("st", 1, "finished before the crash");
    let second = t.len();
    t.prompt("st-p2", "Watch the build.")
        .tool_use("st-a2", "toolu_st", "sleep 999");
    std::fs::write(&path, &t.bytes).unwrap();
    svc.capture_notify(notify("st", &path, "stop"))
        .await
        .unwrap();
    until("turn 1", || {
        state_of(&svc, &uri("st")).is_some_and(|s| s.byte_offset == second)
    })
    .await;

    age(&path, Duration::from_secs(10 * 60));
    let before = passes(&svc);
    until("the quiet sweep ran", || passes(&svc) > before).await;
    assert_eq!(
        chunks_holding(&svc, "sleep 999"),
        0,
        "the call without its result keeps the turn open"
    );
    assert!(!state_of(&svc, &uri("st")).unwrap().ended);

    age(&path, Duration::from_secs(25 * 3600));
    until("the stale sweep ends the source", || {
        state_of(&svc, &uri("st")).unwrap().ended
    })
    .await;
    assert_eq!(chunks_holding(&svc, "[tool Bash] sleep 999"), 1);
    assert_eq!(state_of(&svc, &uri("st")).unwrap().byte_offset, t.len());
    let st = svc.capture_status().await.unwrap();
    assert!(st.sessions.iter().any(|s| s.uri == uri("st") && s.ended));
}

/// A pass whose project's rules are unknown (a broken `.alice.toml`) stores nothing and keeps
/// the cursor; its job is tried again a sweep period later, with no new event, and captures
/// once the file is fixed. The session-end it carries is not lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_local_pass_is_tried_again_until_the_rules_are_known() {
    let (d, svc) = local();
    let repo = d.path().join("waiting-repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join(".alice.toml"), "[privacy\nno_capture = tru").unwrap();
    let path = d.path().join("wl.jsonl");
    let mut t = Transcript::default();
    t.turn("wl", 1, "waiting words one")
        .turn("wl", 2, "waiting words two");
    std::fs::write(&path, &t.bytes).unwrap();
    svc.capture_notify(CaptureNotify {
        cwd: Some(repo.to_string_lossy().into_owned()),
        ..notify("wl", &path, "session-end")
    })
    .await
    .unwrap();
    until("two waiting passes", || passes(&svc) >= 2).await;
    assert!(state_of(&svc, &uri("wl")).is_none(), "nothing is stored");
    assert!(known_keys(&svc).is_empty());

    std::fs::write(
        repo.join(".alice.toml"),
        "[privacy]\nblock_secrets = false\n",
    )
    .unwrap();
    until_within(
        "captured once the rules are known",
        Duration::from_secs(60),
        || state_of(&svc, &uri("wl")).is_some_and(|s| s.ended),
    )
    .await;
    let row = state_of(&svc, &uri("wl")).unwrap();
    assert_eq!(
        (row.byte_offset, row.project_slug.as_str()),
        (t.len(), "waiting-repo")
    );
    assert_eq!(chunks_holding(&svc, "waiting words two is settled"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spooled_notify_is_drained_into_a_pass() {
    let (d, svc) = local();
    let path = d.path().join("sp.jsonl");
    let mut t = Transcript::default();
    t.turn("sp", 1, "spooledmarker");
    std::fs::write(&path, &t.bytes).unwrap();
    let dir = svc.home().spool_dir();
    alice_daemon::spool::append(
        &dir,
        &alice_daemon::spool::SpoolEntry::CaptureNotify(notify("sp", &path, "stop")),
    )
    .unwrap();
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|x| x == "json"))
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["op"], "capture_notify");

    let r = alice_daemon::spool::drain(&dir, &svc).await;
    assert_eq!((r.processed, r.rejected, r.remaining), (1, 0, 0));
    until("the drained notify's pass", || {
        chunks_holding(&svc, "spooledmarker is settled") == 1
    })
    .await;
    assert_eq!(state_of(&svc, &uri("sp")).unwrap().byte_offset, t.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tenant_daemon_refuses_notify_and_reads_no_files() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let path = d.path().join("tn.jsonl");
    let mut t = Transcript::default();
    t.turn("tn", 1, "captured on the laptop");
    std::fs::write(&path, &t.bytes).unwrap();
    let laptop = start_with(&home, local_options());
    laptop
        .capture_notify(notify("tn", &path, "stop"))
        .await
        .unwrap();
    until("the local capture", || {
        state_of(&laptop, &uri("tn")).is_some_and(|s| s.byte_offset == t.len())
    })
    .await;
    laptop.shutdown();
    drop(laptop);

    // The transcript grows and goes stale: a local daemon would pass it and end the source.
    let cursor = t.len();
    t.turn("tn", 2, "never read by the server");
    std::fs::write(&path, &t.bytes).unwrap();
    age(&path, Duration::from_secs(25 * 3600));

    let svc = start_with(
        &home,
        ServiceOptions {
            tenant: true,
            ..local_options()
        },
    );
    let e = svc
        .capture_notify(notify("tn", &path, "stop"))
        .await
        .unwrap_err();
    match e {
        alice_core::AliceError::Invalid(m) => assert_eq!(
            m,
            "this server cannot read files on your machine; use the capture script (docs/capture.md)"
        ),
        other => panic!("{other}"),
    }
    // A spooled notify is refused the same way.
    let dir = svc.home().spool_dir();
    alice_daemon::spool::append(
        &dir,
        &alice_daemon::spool::SpoolEntry::CaptureNotify(notify("tn", &path, "stop")),
    )
    .unwrap();
    let r = alice_daemon::spool::drain(&dir, &svc).await;
    assert_eq!((r.processed, r.rejected), (0, 1));

    // The path is cleared at start; even put back (as a worker would find it), no tenant thread
    // reads it. Several sweep periods: nothing is read, nothing ends.
    {
        let c = rusqlite::Connection::open(AliceHome::new(home.clone()).db()).unwrap();
        c.execute(
            "UPDATE capture_state SET transcript_path = ?1",
            [path.to_string_lossy()],
        )
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(passes(&svc), 0);
    assert_eq!(chunks_holding(&svc, "never read by the server"), 0);
    let row = state_of(&svc, &uri("tn")).unwrap();
    assert_eq!(row.byte_offset, cursor);
    assert!(!row.ended);
    assert_eq!(row.file_key, None);
}

/// Ruling 2: a line longer than a whole pass (8 MiB) is streamed past. It leaves the stub
/// `[result] [record skipped: too large]` in its turn (the reader's stub for a line over 4 MiB,
/// read from the line's last 4 MiB), the cursor lands after its `\n`, and the next turn is
/// stored. The bytes skipped count one gap, as a tenant client's skip does.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_line_becomes_a_stub_and_the_next_turn_is_stored() {
    let (d, svc) = local();
    let path = d.path().join("ol.jsonl");
    let huge = "x".repeat(alice_core::capture::MAX_PASS_BYTES + (1 << 20));
    let mut t = Transcript::default();
    t.turn("ol", 1, "before the big line")
        .prompt("ol-p2", "Read the huge log.")
        .tool_use("ol-a2", "toolu_big", "cat huge.log")
        .tool_result("ol-r2", "toolu_big", &huge)
        .reply("ol-a2b", "The log was too large to show here.")
        .turn("ol", 3, "after the big line");
    std::fs::write(&path, &t.bytes).unwrap();
    svc.capture_notify(notify("ol", &path, "stop"))
        .await
        .unwrap();
    until("turn 3", || known_keys(&svc).contains("ol-p3")).await;

    assert_eq!(
        chunks_holding(&svc, "[result] [record skipped: too large]"),
        1
    );
    assert_eq!(
        chunks_holding(&svc, "The log was too large to show here."),
        1
    );
    assert_eq!(chunks_holding(&svc, "after the big line is settled"), 1);
    assert_eq!(chunks_holding(&svc, "before the big line is settled"), 1);
    assert_eq!(chunks_holding(&svc, "[tool Bash] cat huge.log"), 1);
    assert_eq!(chunks_holding(&svc, "xxxxxxxxxxxxxxxx"), 0);
    let row = state_of(&svc, &uri("ol")).unwrap();
    assert_eq!(row.byte_offset, t.len());
    assert_eq!(row.gaps, 1);
}

/// A transcript longer than eight passes (64 MiB) is read in passes of at most 8 MiB; the job
/// then goes back in the queue and carries on. A pass that stops short of the end of the file
/// leaves its last turn open, so no turn is split into a turn and a late tail. The turns have
/// no tool call (which would hold a cut turn open anyway): a prompt, then a reply whose long
/// thinking block (not kept) a pass usually ends inside.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_transcript_is_read_in_bounded_passes_that_split_no_turn() {
    let (d, svc) = local();
    let path = d.path().join("lg.jsonl");
    let thinking = "considering the build ".repeat(450);
    let mut t = Transcript::default();
    let turns = 6_800;
    for i in 0..turns {
        t.prompt(&format!("lg-p{i}"), &format!("Build step {i}."))
            .line(json!({"type": "assistant", "uuid": format!("lg-a{i}"),
                "message": {"id": format!("msg-lg-a{i}"), "type": "message", "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": &thinking, "signature": "c2ln"},
                        {"type": "text", "text": format!("Step {i} is built.")}],
                    "stop_reason": "end_turn"}}));
    }
    assert!(t.len() > 8 * alice_core::capture::MAX_PASS_BYTES as u64);
    std::fs::write(&path, &t.bytes).unwrap();
    svc.capture_notify(notify("lg", &path, "stop"))
        .await
        .unwrap();
    until_within("every pass", Duration::from_secs(120), || {
        state_of(&svc, &uri("lg")).is_some_and(|s| s.byte_offset == t.len())
    })
    .await;
    let keys = known_keys(&svc);
    for i in 0..turns {
        assert!(keys.contains(&format!("lg-p{i}")), "turn {i} is captured");
    }
    assert!(
        keys.iter().all(|k| !k.starts_with("tail:")),
        "no turn was cut at a pass's end"
    );
    assert!(passes(&svc) >= 9, "{} passes", passes(&svc));
    assert_eq!(state_of(&svc, &uri("lg")).unwrap().gaps, 0);
    assert_eq!(chunks_holding(&svc, "Step 6799 is built."), 1);
    assert_eq!(chunks_holding(&svc, "considering the build"), 0);
}

/// A tool result's line in Claude Code's own field order, which `json!` would sort away: the
/// result in `message`, then `uuid` and `timestamp`, then the result again in `toolUseResult`,
/// then the session's fields. With an `echo` over 64 KiB, the uuid is out of the reach of the
/// too-large stub's scan at both ends of the line.
fn claude_ordered_tool_result(
    session: &str,
    uuid: &str,
    tool_id: &str,
    result: &str,
    echo: &str,
) -> Vec<u8> {
    let mut line = String::new();
    line.push_str(&format!(
        r#"{{"parentUuid":"{session}-call","isSidechain":false,"promptId":"{session}-prompt","type":"user","#
    ));
    line.push_str(&format!(
        r#""message":{{"role":"user","content":[{{"tool_use_id":"{tool_id}","type":"tool_result","content":"{result}","is_error":false}}]}},"#
    ));
    line.push_str(&format!(
        r#""uuid":"{uuid}","timestamp":"2026-10-16T10:00:00.000Z","#
    ));
    line.push_str(&format!(
        r#""toolUseResult":{{"stdout":"{echo}","stderr":"","interrupted":false,"isImage":false}},"#
    ));
    line.push_str(&format!(
        r#""sourceToolAssistantUUID":"{session}-call","permissionDecision":"allow","userType":"external","entrypoint":"cli","cwd":"/work/repo","sessionId":"{session}","version":"2.0.14","gitBranch":"main"}}"#
    ));
    line.push('\n');
    line.into_bytes()
}

/// Review fix F4: the stub of a line over a pass whose uuid is not near either end (Claude Code's
/// order, above) has a key of its own. Two such lines, in two sessions, each get their stub and
/// keep the reply that follows them; with one constant key the second tail would be skipped as
/// stored.
#[tokio::test(flavor = "multi_thread")]
async fn keyless_oversized_lines_in_two_sessions_keep_both_tails() {
    let (d, svc) = local();
    let result = "x".repeat(alice_core::capture::MAX_PASS_BYTES + (512 << 10));
    let echo = "y".repeat(256 << 10);
    for s in ["ko1", "ko2"] {
        let path = d.path().join(format!("{s}.jsonl"));
        let mut t = Transcript::default();
        t.prompt(&format!("{s}-p1"), "Dump the whole log.")
            .tool_use(&format!("{s}-a1"), &format!("toolu_{s}"), "cat all.log");
        t.bytes.extend(claude_ordered_tool_result(
            s,
            &format!("{s}-r1"),
            &format!("toolu_{s}"),
            &result,
            &echo,
        ));
        t.reply(
            &format!("{s}-a1b"),
            &format!("The reply after {s}'s huge log."),
        )
        .turn(s, 2, &format!("{s} goes on"));
        std::fs::write(&path, &t.bytes).unwrap();
        svc.capture_notify(notify(s, &path, "stop")).await.unwrap();
        until("the session's next turn", || {
            known_keys(&svc).contains(&format!("{s}-p2"))
        })
        .await;
        assert_eq!(state_of(&svc, &uri(s)).unwrap().byte_offset, t.len());
    }
    for s in ["ko1", "ko2"] {
        assert_eq!(
            chunks_holding(&svc, &format!("The reply after {s}'s huge log.")),
            1,
            "{s}'s reply after its oversized line"
        );
        assert_eq!(chunks_holding(&svc, &format!("{s} goes on is settled")), 1);
    }
    assert_eq!(chunks_holding(&svc, "[record skipped: too large]"), 2);
    let tails: Vec<String> = known_keys(&svc)
        .into_iter()
        .filter(|k| k.starts_with("tail:"))
        .collect();
    assert_eq!(tails.len(), 2, "one tail key per stub: {tails:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagent_stop_notify_reads_the_agent_file() {
    let (d, svc) = local();
    let main_path = d.path().join("sb.jsonl");
    let mut main = Transcript::default();
    main.turn("sb", 1, "the main session");
    std::fs::write(&main_path, &main.bytes).unwrap();
    svc.capture_notify(notify("sb", &main_path, "stop"))
        .await
        .unwrap();
    until("the main session", || state_of(&svc, &uri("sb")).is_some()).await;

    let agent_path = d.path().join("agent-sbagent1.jsonl");
    let mut agent = Transcript::subagent("sbagent1");
    agent
        .prompt(
            "sb-agent1-p",
            "Survey the repository layout and report back.",
        )
        .reply(
            "sb-agent1-a",
            &"The repository has a core crate, a daemon crate and a binary package. ".repeat(4),
        );
    std::fs::write(&agent_path, &agent.bytes).unwrap();
    svc.capture_notify(CaptureNotify {
        agent_id: Some("sbagent1".into()),
        agent_transcript_path: Some(agent_path.to_string_lossy().into_owned()),
        ..notify("sb", &main_path, "subagent-stop")
    })
    .await
    .unwrap();
    let sub_uri = "claude-code:session:sb:agent:sbagent1";
    until("the subagent's source", || {
        state_of(&svc, sub_uri).is_some_and(|s| s.byte_offset == agent.len())
    })
    .await;
    let row = state_of(&svc, sub_uri).unwrap();
    assert_eq!(
        row.parent_source_id,
        Some(state_of(&svc, &uri("sb")).unwrap().source_id)
    );
    assert_eq!(
        row.transcript_path.as_deref(),
        Some(agent_path.to_string_lossy().as_ref())
    );
    assert!(chunks_of(&svc, sub_uri)[0].1.contains("core crate"));
}

/// Ruling 6: a Codex hook may give no transcript path. The worker looks for
/// `rollout-*<session id>*.jsonl` in the newest 7 date folders of `$CODEX_HOME/sessions`;
/// compressed rollouts are counted, not read, and a session with no rollout is counted.
#[tokio::test(flavor = "multi_thread")]
async fn a_null_codex_path_is_found_by_session_id() {
    let d = tempfile::tempdir().unwrap();
    let codex = d.path().join("codex-home");
    let day = |y: u32, m: u32, dd: u32| {
        let dir = codex
            .join("sessions")
            .join(format!("{y:04}"))
            .join(format!("{m:02}"))
            .join(format!("{dd:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let new_id = "0199a213-81c0-7800-8aa1-bbab2a035a53";
    let old_id = "0199a213-81c0-7800-8aa1-000000000002";
    let zst_id = "0199a213-81c0-7800-8aa1-00000000000f";
    // Nine date folders across a month end: 2026-09-30 and 2026-10-01 … 10-08.
    day(2026, 9, 30);
    for dd in 1..=8 {
        day(2026, 10, dd);
    }
    std::fs::create_dir_all(codex.join("sessions/2026/10/notes")).unwrap();
    let found = day(2026, 10, 6).join(format!("rollout-2026-10-06T10-00-00-{new_id}.jsonl"));
    std::fs::write(&found, b"{\"type\":\"session_meta\"}\n").unwrap();
    // In the eighth newest folder: too old to look at.
    std::fs::write(
        day(2026, 10, 1).join(format!("rollout-2026-10-01T09-00-00-{old_id}.jsonl")),
        b"{}\n",
    )
    .unwrap();
    std::fs::write(
        day(2026, 10, 8).join(format!("rollout-2026-10-08T09-00-00-{zst_id}.jsonl.zst")),
        b"compressed",
    )
    .unwrap();

    use alice_daemon::capture::find_rollout;
    assert_eq!(find_rollout(&codex, new_id), Some(found.clone()));
    assert_eq!(
        find_rollout(&codex, old_id),
        None,
        "beyond the newest 7 folders"
    );
    assert_eq!(find_rollout(&codex, zst_id), None, "compressed: not read");
    assert_eq!(find_rollout(&codex, "0199a213-nothing"), None);
    assert_eq!(find_rollout(&d.path().join("no-such-home"), new_id), None);

    let svc = start_with(
        &d.path().join("home"),
        ServiceOptions {
            codex_home: Some(codex.clone()),
            ..local_options()
        },
    );
    let codex_notify = |session: &str| CaptureNotify {
        agent: "codex".into(),
        event: "session-start".into(),
        session_id: session.into(),
        transcript_path: String::new(),
        ..Default::default()
    };
    svc.capture_notify(codex_notify(new_id)).await.unwrap();
    let codex_uri = format!("codex:session:{new_id}");
    until("the rollout is found and registered", || {
        state_of(&svc, &codex_uri).is_some()
    })
    .await;
    assert_eq!(
        state_of(&svc, &codex_uri)
            .unwrap()
            .transcript_path
            .as_deref(),
        Some(found.to_string_lossy().as_ref())
    );

    svc.capture_notify(codex_notify(old_id)).await.unwrap();
    until("a missing rollout is counted", || {
        svc.capture_counts().rollouts_not_found == 1
    })
    .await;
    svc.capture_notify(codex_notify(zst_id)).await.unwrap();
    until("a compressed rollout is counted", || {
        svc.capture_counts().rollouts_compressed == 1
    })
    .await;
    assert_eq!(svc.capture_counts().rollouts_not_found, 1);
    assert!(!source_exists(&svc, &format!("codex:session:{old_id}")));
    assert!(!source_exists(&svc, &format!("codex:session:{zst_id}")));
}

/// Review fix F1: a rollout is `rollout-*-<session id>.jsonl` (or `.jsonl.zst`), the whole id at
/// the end of the name. An id that is part of another session's id, at either end, never finds
/// that session's rollout, plain or compressed.
#[tokio::test(flavor = "multi_thread")]
async fn a_rollout_matches_the_whole_session_id() {
    let d = tempfile::tempdir().unwrap();
    let codex = d.path().join("codex-home");
    let newest = codex.join("sessions/2026/10/08");
    let older = codex.join("sessions/2026/10/05");
    std::fs::create_dir_all(&newest).unwrap();
    std::fs::create_dir_all(&older).unwrap();
    let file = |dir: &Path, name: &str| {
        let p = dir.join(name);
        std::fs::write(&p, b"{}\n").unwrap();
        p
    };
    // In the newest folder, rollouts of sessions whose ids hold `abc`, and none of `abc` itself.
    let xabc = file(&newest, "rollout-2026-10-08T09-00-00-xabc.jsonl");
    let abc_1 = file(&newest, "rollout-2026-10-08T10-00-00-abc-1.jsonl");
    file(&newest, "rollout-2026-10-08T11-00-00-xabc.jsonl.zst");
    file(&newest, "rollout-2026-10-08T12-00-00-abc.jsonl.bak");
    // `rollout-` and `-abc.jsonl` would have to share the `-`: not a rollout of `abc`.
    file(&newest, "rollout-abc.jsonl");
    let abc = file(&older, "rollout-2026-10-05T09-00-00-abc.jsonl");

    use alice_daemon::capture::find_rollout;
    assert_eq!(find_rollout(&codex, "abc"), Some(abc));
    assert_eq!(find_rollout(&codex, "xabc"), Some(xabc));
    assert_eq!(find_rollout(&codex, "abc-1"), Some(abc_1));
    // Parts of those ids that the name does not delimit with `-` on the left.
    for part in ["bc", "ab", "c-1", "abc-"] {
        assert_eq!(find_rollout(&codex, part), None, "{part}");
    }

    // Through the worker: `bc` has no rollout, and `xabc`'s compressed one is not counted for it.
    let svc = start_with(
        &d.path().join("home"),
        ServiceOptions {
            codex_home: Some(codex.clone()),
            ..local_options()
        },
    );
    svc.capture_notify(CaptureNotify {
        agent: "codex".into(),
        event: "session-start".into(),
        session_id: "bc".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    until("the missing rollout is counted", || {
        svc.capture_counts().rollouts_not_found == 1
    })
    .await;
    assert_eq!(svc.capture_counts().rollouts_compressed, 0);
    assert!(!source_exists(&svc, "codex:session:bc"));
}

/// Review fix F2: a compressed rollout named by the hook itself is skipped by its file name,
/// whatever the source's cursor: it is never opened (no pass runs and the stored identity and
/// cursor stay), nothing is stored, and it is counted.
#[tokio::test(flavor = "multi_thread")]
async fn a_compressed_rollout_is_skipped_by_its_name() {
    let d = tempfile::tempdir().unwrap();
    // No periodic sweep: only the notifies below run passes.
    let svc = start_with(
        &d.path().join("home"),
        ServiceOptions {
            capture_sweep_every: Duration::from_secs(3_600),
            ..local_options()
        },
    );
    let session = "0199a213-81c0-7800-8aa1-0000000000z1";
    let plain = d
        .path()
        .join(format!("rollout-2026-10-08T09-00-00-{session}.jsonl"));
    std::fs::write(&plain, "{\"type\":\"session_meta\"}\n".repeat(100)).unwrap();
    let codex_notify = |path: &Path, event: &str| CaptureNotify {
        agent: "codex".into(),
        event: event.into(),
        session_id: session.into(),
        transcript_path: path.to_string_lossy().into_owned(),
        ..Default::default()
    };
    // The session is registered from its plain rollout, then its cursor is put past 0.
    svc.capture_notify(codex_notify(&plain, "session-start"))
        .await
        .unwrap();
    let codex_uri = format!("codex:session:{session}");
    until("the session is registered", || {
        state_of(&svc, &codex_uri).is_some() && passes(&svc) == 1
    })
    .await;
    {
        let c = rusqlite::Connection::open(svc.home().db()).unwrap();
        c.execute("UPDATE capture_state SET byte_offset = 1000", [])
            .unwrap();
    }
    let before = state_of(&svc, &codex_uri).unwrap();
    assert_eq!(before.byte_offset, 1000);
    let passes_before = passes(&svc);

    let zst = d
        .path()
        .join(format!("rollout-2026-10-08T09-00-00-{session}.jsonl.zst"));
    std::fs::write(&zst, b"\x28\xb5\x2f\xfd compressed frames").unwrap();
    svc.capture_notify(codex_notify(&zst, "stop"))
        .await
        .unwrap();
    until("the compressed rollout is counted", || {
        svc.capture_counts().rollouts_compressed == 1
    })
    .await;
    assert_eq!(passes(&svc), passes_before, "no pass read it");
    let after = state_of(&svc, &codex_uri).unwrap();
    assert_eq!(after.byte_offset, 1000);
    assert_eq!(after.file_key, before.file_key, "never opened and keyed");
    assert_eq!(after.transcript_path, before.transcript_path);
    assert!(chunks_of(&svc, &codex_uri).is_empty());
    assert_eq!(svc.capture_counts().rollouts_not_found, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notify_is_checked_before_it_is_queued() {
    let (d, svc) = local();
    let path = d.path().join("ck.jsonl");
    let bad = [
        CaptureNotify {
            agent: "cursor".into(),
            ..notify("ck", &path, "stop")
        },
        notify("ck", &path, "teatime"),
        notify("../ck", &path, "stop"),
        notify("ck", Path::new("relative/ck.jsonl"), "stop"),
        notify("ck", Path::new(""), "stop"),
        CaptureNotify {
            agent_transcript_path: Some(path.to_string_lossy().into_owned()),
            ..notify("ck", &path, "subagent-stop")
        },
    ];
    for n in bad {
        let e = svc.capture_notify(n.clone()).await.unwrap_err();
        assert!(
            matches!(e, alice_core::AliceError::Invalid(_)),
            "{n:?}: {e}"
        );
    }
    // A path that is not a `.jsonl` transcript is queued, then refused unread.
    let other = d.path().join("ck.txt");
    std::fs::write(&other, b"{}\n").unwrap();
    svc.capture_notify(notify("ck", &other, "session-start"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!source_exists(&svc, &uri("ck")));
}
