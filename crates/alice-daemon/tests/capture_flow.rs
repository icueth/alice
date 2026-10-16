//! Capture uploads (SP2 Task 6a; spec §3, §5.2, §5.6, §9.4): transcript bytes handed to
//! `Service::capture_transcript` become chunks, turn keys and a cursor, each exactly once.
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

/// A Codex rollout, built line by line (format: `alice-core/tests/fixtures/capture/codex`).
#[derive(Default)]
struct Rollout {
    bytes: Vec<u8>,
    clock: u64,
}

impl Rollout {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn line(&mut self, kind: &str, payload: serde_json::Value) -> &mut Self {
        self.clock += 1;
        let c = self.clock;
        let v = json!({
            "timestamp": format!("2026-10-16T{:02}:{:02}:{:02}.000Z", (c / 3600) % 24, (c / 60) % 60, c % 60),
            "ordinal": c - 1, "type": kind, "payload": payload,
        });
        self.bytes.extend(serde_json::to_vec(&v).unwrap());
        self.bytes.push(b'\n');
        self
    }

    fn message(&mut self, role: &str, kind: &str, text: &str) -> &mut Self {
        let block = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        self.line(
            "response_item",
            json!({"type": "message", "id": format!("m{}", self.clock), "role": role,
                   "content": [{"type": block, "text": text}],
                   "internal_chat_message_metadata_passthrough": {"content_item_kinds": [kind]}}),
        )
    }

    fn prompt(&mut self, text: &str) -> &mut Self {
        self.message("user", "user.text", text)
    }

    fn reply(&mut self, text: &str) -> &mut Self {
        self.message("assistant", "unknown", text)
    }

    fn exec(&mut self, call: &str, code: &str) -> &mut Self {
        self.line(
            "response_item",
            json!({"type": "custom_tool_call", "call_id": call, "name": "exec", "status": "completed", "input": code}),
        )
    }

    fn output(&mut self, call: &str, text: &str) -> &mut Self {
        self.line(
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": call,
                   "output": [{"type": "input_text", "text": text}]}),
        )
    }
}

fn codex_q(session: &str, offset: u64, event: &str) -> TranscriptQuery {
    TranscriptQuery {
        agent: "codex".into(),
        ..q(session, offset, event)
    }
}

/// Task 17a: a Codex rollout goes through the same upload path as Claude Code's, under its own
/// source uri, and a resend or a Claude session of the same id stores nothing twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_upload_stores_its_turns_once() {
    let (_d, svc) = tenant();
    let mut r = Rollout::default();
    r.line(
        "session_meta",
        json!({"id": "cx1", "cwd": "/home/user/project-x"}),
    )
    .message("developer", "permissions.instructions", "Harness rules.")
    .message(
        "user",
        "environments.environment_context",
        "<environment_context>x</environment_context>",
    )
    .prompt("Where is the quokkabridge config?")
    .reply("It lives in quokkabridge.toml.")
    .exec("call_1", "rg quokkabridge")
    .output("call_1", "quokkabridge.toml:1");
    let second = r.len();
    // The second turn ends on a call with no output yet, so a `stop` leaves it open.
    r.prompt("Thanks, now rename it.")
        .exec("call_2", "mv quokkabridge.toml quokkabridge.yaml");
    let mut body = r.bytes.clone();
    body.extend_from_slice(br#"{"timestamp":"2026-10-16T01:00:00.000Z","type":"respo"#);

    let a = send(&svc, codex_q("cx1", 0, "stop"), &body).await;
    assert_eq!((a.turns_stored, a.turns_skipped), (1, 0));
    assert_eq!(
        a.next_offset, second,
        "the cursor stops at the second prompt's line"
    );
    assert!(a.chunks_stored >= 1);

    let uri = "codex:session:cx1";
    let text = chunks_of(&svc, uri)
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("[user] Where is the quokkabridge config?"),
        "{text}"
    );
    assert!(text.contains("[tool Bash] rg quokkabridge"), "{text}");
    assert!(text.contains("[result] quokkabridge.toml:1"), "{text}");
    assert!(!text.contains("Harness rules") && !text.contains("environment_context"));
    assert!(!source_exists(&svc, "claude-code:session:cx1"));
    let row = state_of(&svc, uri).expect("the codex source has its cursor");
    assert_eq!(row.byte_offset, second);
    assert_eq!(row.session_id, "cx1");

    // A client that lost its state and resends from zero is told the cursor, stores nothing.
    let behind = svc
        .capture_transcript(codex_q("cx1", 0, "stop"), Bytes::from(body.clone()))
        .await
        .unwrap()
        .expect_err("a resend from 0 is behind the cursor");
    assert_eq!(behind, second);
    assert_eq!(chunks_holding(&svc, "quokkabridge.toml:1"), 1);

    // The rest of the rollout completes the second turn at the session end.
    r.output("call_2", "renamed")
        .reply("Renamed to quokkabridge.yaml.");
    let a2 = send(
        &svc,
        codex_q("cx1", second, "session-end"),
        &r.bytes[second as usize..],
    )
    .await;
    assert_eq!(a2.turns_stored, 1);
    assert_eq!(a2.next_offset, r.len());
    assert_eq!(chunks_holding(&svc, "quokkabridge.yaml"), 1);
    assert!(state_of(&svc, uri).unwrap().ended);

    // A zstd file is counted, not read: nothing is stored and the cursor moves past it.
    let mut zst = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01];
    zst.extend_from_slice(b"\x00binary");
    let z = send(&svc, codex_q("cx-zst", 0, "stop"), &zst).await;
    assert_eq!((z.turns_stored, z.chunks_stored), (0, 0));
    assert_eq!(z.next_offset, zst.len() as u64);
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
