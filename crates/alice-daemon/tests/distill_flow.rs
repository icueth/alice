//! The distill worker (SP2 Task 12; spec §6.2, §6.4–§6.7, §8.4): captured sessions become
//! judged memories in the background, behind the Jev judge, the cloud gate, a breaker and the
//! distillation share of the budget.
//!
//! Every test runs a local daemon (`Service`, no HTTP unless the route is the subject) with its
//! own scripted Jev transport, [`DistillScript`]: it answers each distillation question (`c<n>`)
//! from a phrase → option map (all the probability on that option) and every other question
//! (the judge's own) with a neutral answer, counts its calls, and can be switched to answer 500
//! or to be unreachable. Nothing touches the network. Transcripts are written inline, line by
//! line, and uploaded through `Service::capture_transcript` with a `cwd` naming the project.

use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::judge::jev::{HttpReply, JevTransport, TransportError};
use alice_daemon::doctor::{Check, CheckStatus, daemon_checks};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use axum::body::Bytes;
use parking_lot::Mutex;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A test key, split inside its prefix so no scanner takes it for a real one.
const KEY: &str = concat!("ts_", "live_distill_flow_key_0001");
/// Jev judges and distils.
const JEV: &str = "[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n";

// ---- The scripted transport ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outage {
    None,
    /// Distillation requests are answered with 500 (and retried by the client).
    Status500,
    /// Distillation requests cannot reach Jev.
    Unreachable,
}

struct DistillScript {
    /// `(phrase, option)`: a distillation question whose text holds `phrase` is answered with
    /// `option`; any other with `ephemeral`.
    answers: Vec<(&'static str, &'static str)>,
    outage: Mutex<Outage>,
    /// How long a distillation request takes to answer.
    delay: Mutex<Duration>,
    /// Distillation requests answered (with a 200).
    answered: AtomicUsize,
    calls: AtomicUsize,
    distill_calls: AtomicUsize,
    /// The question texts of every distillation request, one list per request.
    questions: Mutex<Vec<Vec<String>>>,
    /// Every request body received (the judge's and distillation's).
    bodies: Mutex<Vec<String>>,
}

fn is_distill_question(id: &str) -> bool {
    id.len() > 1 && id.starts_with('c') && id[1..].chars().all(|c| c.is_ascii_digit())
}

impl DistillScript {
    fn new(answers: &[(&'static str, &'static str)]) -> Arc<Self> {
        Arc::new(Self {
            answers: answers.to_vec(),
            outage: Mutex::new(Outage::None),
            delay: Mutex::new(Duration::ZERO),
            answered: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            distill_calls: AtomicUsize::new(0),
            questions: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
        })
    }

    fn set_outage(&self, o: Outage) {
        *self.outage.lock() = o;
    }

    /// Every request posted (the judge's and distillation's, retries included).
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Distillation requests answered with a 200.
    fn answered(&self) -> usize {
        self.answered.load(Ordering::SeqCst)
    }

    /// Distillation requests posted, retries included.
    fn distill_calls(&self) -> usize {
        self.distill_calls.load(Ordering::SeqCst)
    }

    /// Whether any request received so far holds `needle`.
    fn received(&self, needle: &str) -> bool {
        self.bodies.lock().iter().any(|b| b.contains(needle))
    }

    /// Every distillation question asked so far.
    fn questions(&self) -> Vec<String> {
        self.questions.lock().iter().flatten().cloned().collect()
    }
}

impl JevTransport for DistillScript {
    fn post(
        &self,
        _endpoint: &str,
        key: &str,
        body: &[u8],
        _timeout: Duration,
    ) -> Result<HttpReply, TransportError> {
        assert_eq!(key, KEY, "the configured key is the one sent");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let req: serde_json::Value = serde_json::from_slice(body).unwrap();
        self.bodies
            .lock()
            .push(String::from_utf8_lossy(body).into_owned());
        let questions = req["questions"].as_object().unwrap().clone();
        if questions.keys().any(|k| is_distill_question(k)) {
            self.distill_calls.fetch_add(1, Ordering::SeqCst);
            let delay = *self.delay.lock();
            std::thread::sleep(delay);
            self.questions.lock().push(
                questions
                    .values()
                    .map(|q| q["instructions"].as_str().unwrap_or_default().to_string())
                    .collect(),
            );
            match *self.outage.lock() {
                Outage::Status500 => {
                    return Ok(HttpReply {
                        status: 500,
                        body: b"{}".to_vec(),
                        retry_after: Some(Duration::ZERO),
                    });
                }
                Outage::Unreachable => {
                    return Err(TransportError::Connect("the scripted outage".into()));
                }
                Outage::None => {}
            }
        }
        let mut answers = serde_json::Map::new();
        for (qid, q) in &questions {
            let options: Vec<String> = q["criteria"].as_object().unwrap().keys().cloned().collect();
            let offered = |o: &str| options.iter().any(|x| x == o);
            let text = q["instructions"].as_str().unwrap_or_default();
            let choice: String = if is_distill_question(qid) {
                let want = self
                    .answers
                    .iter()
                    .find(|(phrase, _)| text.contains(phrase))
                    .map_or("ephemeral", |(_, o)| *o);
                if offered(want) { want } else { "ephemeral" }.to_string()
            } else {
                // The judge's own questions: nothing to act on.
                ["unrelated", "none", "note", "global"]
                    .into_iter()
                    .find(|o| offered(o))
                    .map_or_else(|| options[0].clone(), str::to_string)
            };
            let probs: serde_json::Map<String, serde_json::Value> = options
                .iter()
                .map(|o| (o.clone(), json!(if *o == choice { 1.0 } else { 0.0 })))
                .collect();
            answers.insert(
                qid.clone(),
                json!({"type": "choice", "choice": choice, "confidence": 1.0, "probabilities": probs}),
            );
        }
        let resp = json!({"model": "jev-1.13.0", "answers": answers,
            "usage": {"input_tokens": 500, "output_tokens": 2}});
        if questions.keys().any(|k| is_distill_question(k)) {
            self.answered.fetch_add(1, Ordering::SeqCst);
        }
        Ok(HttpReply {
            status: 200,
            body: serde_json::to_vec(&resp).unwrap(),
            retry_after: None,
        })
    }
}

// ---- The daemon ----

#[derive(Clone)]
struct Opts {
    config: &'static str,
    key: bool,
    idle: Option<Duration>,
    /// A server tenant: sessions name their project by a hint, not a working directory.
    tenant: bool,
}

impl Opts {
    fn jev() -> Self {
        Opts {
            config: JEV,
            key: true,
            idle: None,
            tenant: false,
        }
    }
}

/// A local daemon in `<dir>/home`; projects are folders `<dir>/<slug>`.
struct Daemon {
    dir: tempfile::TempDir,
    svc: Arc<Service>,
    script: Arc<DistillScript>,
    opts: Opts,
}

fn start_service(home: &Path, script: &Arc<DistillScript>, o: &Opts) -> Arc<Service> {
    let key = o.key;
    Service::start(
        AliceHome::new(home.to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions {
            secret_env: Some(Arc::new(move |v: &str| {
                (key && v == "TYPESAFE_API_KEY").then(|| KEY.to_string())
            })),
            settings_env: Some(Vec::new()),
            settings_poll: Duration::from_millis(50),
            jev_transport: Some(script.clone()),
            distill_tick: Duration::from_millis(100),
            distill_idle_override: o.idle,
            tenant: o.tenant,
            judge_retry_every: Duration::from_millis(200),
            // The local capture worker (Task 6b) runs beside the distill worker on a local
            // daemon; uploaded sources have no transcript path, so its sweep leaves them alone.
            capture_settle: Duration::from_millis(50),
            capture_sweep_every: Duration::from_millis(100),
            ..Default::default()
        },
    )
    .unwrap()
}

impl Daemon {
    fn start(o: Opts, script: Arc<DistillScript>) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(AliceHome::new(home.clone()).config_file(), o.config).unwrap();
        let svc = start_service(&home, &script, &o);
        Daemon {
            dir,
            svc,
            script,
            opts: o,
        }
    }

    /// Replaces `config.toml` and waits until the daemon holds `check` (a loosening takes three
    /// polls of 50 ms).
    async fn write_config(
        &self,
        text: &str,
        check: impl Fn(&alice_core::settings::Settings) -> bool,
    ) {
        std::fs::write(self.svc.home().config_file(), text).unwrap();
        until("the daemon reads the new settings", || {
            check(&self.svc.settings())
        })
        .await;
    }

    /// The working tree of project `slug` (created on first use).
    fn repo(&self, slug: &str) -> PathBuf {
        let p = self.dir.path().join(slug);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn db(&self) -> rusqlite::Connection {
        alice_core::store::open_reader(&self.svc.home().db()).unwrap()
    }

    /// Stops the daemon (as a crash between two writes would leave it), runs `sql` on its
    /// database, and starts it again with the same transport.
    async fn restart_with(&mut self, sql: &str) {
        let svc = self.svc.clone();
        tokio::task::spawn_blocking(move || svc.shutdown())
            .await
            .unwrap();
        {
            let c = rusqlite::Connection::open(self.svc.home().db()).unwrap();
            c.execute_batch(sql).unwrap();
        }
        let home = self.dir.path().join("home");
        self.svc = start_service(&home, &self.script, &self.opts);
    }

    /// Uploads `t` from `offset` for session `session` of project `slug`.
    async fn send(
        &self,
        slug: &str,
        session: &str,
        t: &Transcript,
        offset: u64,
        event: &str,
    ) -> TranscriptAnswer {
        self.send_q(
            TranscriptQuery {
                agent: "claude-code".into(),
                session: session.into(),
                offset,
                event: event.into(),
                cwd: (!self.opts.tenant).then(|| self.repo(slug).to_string_lossy().into_owned()),
                project_header: self.opts.tenant.then(|| slug.to_string()),
                ..Default::default()
            },
            &t.bytes[offset as usize..],
        )
        .await
    }

    async fn send_q(&self, q: TranscriptQuery, bytes: &[u8]) -> TranscriptAnswer {
        match self
            .svc
            .capture_transcript(q, Bytes::copy_from_slice(bytes))
            .await
            .unwrap()
        {
            Ok(a) => a,
            Err(cursor) => panic!("the upload was refused as behind the cursor {cursor}"),
        }
    }

    fn state(&self, uri: &str) -> SourceState {
        self.db()
            .query_row(
                "SELECT c.source_id, c.distill_state, c.distill_ord, c.next_ord, c.distill_attempts,
                        c.distill_next_try_at, c.ended, c.byte_offset
                 FROM capture_state c JOIN sources s ON s.id = c.source_id WHERE s.uri = ?1",
                [uri],
                |r| {
                    Ok(SourceState {
                        source_id: r.get(0)?,
                        state: r.get(1)?,
                        distill_ord: r.get(2)?,
                        next_ord: r.get(3)?,
                        attempts: r.get(4)?,
                        next_try_at: r.get(5)?,
                        ended: r.get(6)?,
                        byte_offset: r.get(7)?,
                    })
                },
            )
            .unwrap_or_else(|e| panic!("no capture state for {uri}: {e}"))
    }

    /// Whether `uri` has a capture state yet.
    fn has_state(&self, uri: &str) -> bool {
        self.db()
            .query_row(
                "SELECT count(*) FROM capture_state c JOIN sources s ON s.id = c.source_id
                 WHERE s.uri = ?1",
                [uri],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0
    }

    /// Writes `t` as session `session`'s transcript file, beside the home, for the local
    /// capture worker to read.
    fn write_transcript(&self, session: &str, t: &Transcript) -> PathBuf {
        let path = self.dir.path().join(format!("{session}.jsonl"));
        std::fs::write(&path, &t.bytes).unwrap();
        path
    }

    /// What `alice hook <event>` sends for a Claude Code session of project `slug` whose
    /// transcript is `path`: the local worker reads the file later.
    async fn notify(&self, slug: &str, session: &str, path: &Path, event: &str) {
        self.svc
            .capture_notify(CaptureNotify {
                agent: "claude-code".into(),
                event: event.into(),
                session_id: session.into(),
                transcript_path: path.to_string_lossy().into_owned(),
                cwd: Some(self.repo(slug).to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    fn distilled(&self) -> Vec<Distilled> {
        let c = self.db();
        let mut st = c
            .prepare(
                "SELECT m.id, m.kind, m.importance, m.title, m.body, m.author_agent,
                        m.author_session, m.judge_state, m.status, p.slug
                 FROM memories m JOIN projects p ON p.id = m.project_id
                 WHERE m.origin = 'distill' ORDER BY m.seq",
            )
            .unwrap();
        st.query_map([], |r| {
            Ok(Distilled {
                id: r.get(0)?,
                kind: r.get(1)?,
                importance: r.get(2)?,
                title: r.get(3)?,
                body: r.get(4)?,
                agent: r.get(5)?,
                session: r.get(6)?,
                judge_state: r.get(7)?,
                status: r.get(8)?,
                project: r.get(9)?,
            })
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// The chunk ids recorded as `memory`'s provenance, sorted.
    fn provenance(&self, memory: &str) -> Vec<String> {
        let c = self.db();
        let mut st = c
            .prepare("SELECT chunk_id FROM provenance WHERE memory_id = ?1 ORDER BY chunk_id")
            .unwrap();
        st.query_map([memory], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The ids of the chunks of `uri` whose text holds `needle`, sorted.
    fn chunks_with(&self, uri: &str, needle: &str) -> Vec<String> {
        let c = self.db();
        let mut st = c
            .prepare(
                "SELECT ch.id, ch.text FROM chunks ch JOIN sources s ON s.id = ch.source_id
                 WHERE s.uri = ?1 ORDER BY ch.id",
            )
            .unwrap();
        st.query_map([uri], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .filter(|(_, text)| text.contains(needle))
        .map(|(id, _)| id)
        .collect()
    }

    async fn distill_check(&self) -> Check {
        let checks = daemon_checks(&self.svc).await.unwrap();
        checks
            .into_iter()
            .find(|c| c.name == "distill")
            .expect("the doctor has a distill line")
    }

    async fn capture_health(&self) -> CaptureHealth {
        *self
            .svc
            .health()
            .await
            .unwrap()
            .capture
            .expect("capture ran, so health has its line")
    }

    fn shutdown(&self) {
        self.svc.shutdown();
    }
}

#[derive(Debug)]
#[allow(dead_code)] // every field is read by some test
struct SourceState {
    source_id: String,
    state: String,
    distill_ord: i64,
    next_ord: i64,
    attempts: i64,
    next_try_at: Option<String>,
    ended: bool,
    byte_offset: i64,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Distilled {
    id: String,
    kind: String,
    importance: i64,
    title: String,
    body: String,
    agent: Option<String>,
    session: Option<String>,
    judge_state: String,
    status: String,
    project: String,
}

fn uri(session: &str) -> String {
    format!("claude-code:session:{session}")
}

/// Waits until `cond` holds.
async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let t = Instant::now();
    while !cond() {
        assert!(t.elapsed() < Duration::from_secs(30), "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Lets the worker run a few of its 100 ms rounds.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(600)).await;
}

// ---- Transcripts ----

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

    fn tool(
        &mut self,
        uuid: &str,
        tool_id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> &mut Self {
        self.line(json!({"type": "assistant", "uuid": uuid,
            "message": {"id": format!("msg-{uuid}"), "role": "assistant", "content": [
                {"type": "tool_use", "id": tool_id, "name": name, "input": input}],
                "stop_reason": "tool_use"}}))
    }

    fn tool_result(&mut self, uuid: &str, tool_id: &str, output: &str) -> &mut Self {
        self.line(json!({"type": "user", "uuid": uuid,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_id, "content": output}]}}))
    }
}

const DECISION_PROMPT: &str = "Which package manager should the workspace use for its builds?";
const DECISION: &str = "We decided to use pnpm workspaces for every package, because npm hoisting broke the lockfile twice last week and pnpm keeps the dependency tree strict.";
const BUGFIX_PROMPT: &str =
    "The lease renewal test fails on CI again, can you find out why it breaks?";
const BUGFIX: &str = "Found the bug: the lease renewal test failed because the clock mock was never reset between cases, so the expiry check read a stale time. I fixed it by resetting the mock in the shared setup hook.";

/// What the scripted Jev says about the passages above.
const ANSWERS: &[(&str, &str)] = &[
    ("decided to use pnpm workspaces", "decision"),
    ("clock mock was never reset", "bugfix"),
    ("Never run the schema migration", "warning"),
    ("The cache service listens on port", "fact"),
    ("Where things stand:", "handoff"),
    ("always answer in British English", "preference_user"),
];

const HANDOFF: &str = "Where things stand: the cache settings are written down in deploy/cache.toml, and next we move the nightly restart to 04:00 UTC.";

/// A session (ids prefixed `s`) with a decision and a bugfix, one turn each.
fn decision_and_bugfix(s: &str) -> Transcript {
    let mut t = Transcript::default();
    t.prompt(&format!("{s}-p1"), DECISION_PROMPT)
        .reply(&format!("{s}-a1"), DECISION)
        .prompt(&format!("{s}-p2"), BUGFIX_PROMPT)
        .reply(&format!("{s}-a2"), BUGFIX);
    t
}

/// A one-turn session holding a single passage Jev keeps (the decision).
fn one_decision(s: &str) -> Transcript {
    let mut t = Transcript::default();
    t.prompt(&format!("{s}-p1"), DECISION_PROMPT)
        .reply(&format!("{s}-a1"), DECISION);
    t
}

// ---- The tests the plan names ----

/// Spec §6.2–§6.5: a session that ends is distilled once: one Jev call, a memory per kept
/// passage with its kind, `origin = distill`, the source's agent and session as the author,
/// `distill.importance`, and the exact chunks as provenance; the cursor moves to the end and
/// the memories are found once the judge has seen them.
#[tokio::test(flavor = "multi_thread")]
async fn an_ended_session_is_distilled_into_judged_memories() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let t = decision_and_bugfix("s1");
    let a = d.send("shop", "s1", &t, 0, "session-end").await;
    assert_eq!(a.turns_stored, 2);

    until("the session is distilled", || {
        d.state(&uri("s1")).state == "done"
    })
    .await;
    let st = d.state(&uri("s1"));
    assert_eq!(st.distill_ord, st.next_ord, "the cursor is at the end");
    assert_eq!(st.attempts, 0);
    assert_eq!(d.script.distill_calls(), 1, "one request for the run");

    let mems = d.distilled();
    assert_eq!(mems.len(), 2, "{mems:?}");
    let decision = mems
        .iter()
        .find(|m| m.kind == "decision")
        .expect("a decision");
    let bugfix = mems.iter().find(|m| m.kind == "bugfix").expect("a bugfix");
    for m in [decision, bugfix] {
        assert_eq!(m.importance, 2, "distill.importance");
        assert_eq!(m.agent.as_deref(), Some("claude-code"));
        assert_eq!(m.session.as_deref(), Some("s1"));
        assert_eq!(m.project, "shop");
    }
    assert!(
        decision.body.contains("pnpm workspaces"),
        "{}",
        decision.body
    );
    assert!(
        decision
            .body
            .ends_with(&format!("\n\nContext: {DECISION_PROMPT}")),
        "a conclusion carries its prompt: {}",
        decision.body
    );
    assert!(bugfix.body.contains("clock mock was never reset"));
    assert_eq!(
        d.provenance(&decision.id),
        d.chunks_with(&uri("s1"), "decided to use pnpm workspaces")
    );
    assert_eq!(
        d.provenance(&bugfix.id),
        d.chunks_with(&uri("s1"), "clock mock was never reset")
    );
    assert_ne!(d.provenance(&decision.id), d.provenance(&bugfix.id));

    // The judge sees them (spec §6.6); then recall finds them.
    until("the judge has seen both", || {
        d.distilled().iter().all(|m| m.judge_state == "done")
    })
    .await;
    assert!(d.distilled().iter().all(|m| m.status == "active"));
    let r = d
        .svc
        .recall(RecallRequest {
            query: "pnpm workspaces lockfile hoisting".into(),
            project: Some("shop".into()),
            mode: Some(RecallMode::Fast),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        r.results.iter().any(|h| h.id == decision.id),
        "recall finds the distilled decision: {:?}",
        r.results
    );

    let h = d.capture_health().await;
    assert_eq!(h.distill_state, "ready");
    assert_eq!(h.distilled_24h, 2);
    assert_eq!(h.distill_due, 0);
    let stats = d.svc.stats().await.unwrap();
    assert_eq!(stats.distilled_today, 2);
    assert!(stats.distill_cost_today_usd > 0.0);
    let line = d.distill_check().await;
    assert_eq!(line.status, CheckStatus::Ok, "{line:?}");
    assert!(line.detail.contains("jev, 2 distilled today"), "{line:?}");
    let status = d.svc.capture_status().await.unwrap();
    assert_eq!(status.distill_state, "ready");
    assert_eq!(
        status
            .sessions
            .iter()
            .find(|s| s.uri == uri("s1"))
            .unwrap()
            .distill_state,
        "done"
    );
    d.shutdown();
}

/// Spec §6.2, R14: without the Jev judge nothing is distilled (the judge is what removes
/// doubles): the source waits, nothing is sent, and the doctor names the fix.
#[tokio::test(flavor = "multi_thread")]
async fn distillation_waits_for_the_jev_judge() {
    let d = Daemon::start(
        Opts {
            config: "[judge]\nprovider = \"heuristic\"\n\n[distill]\nprovider = \"jev\"\n",
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let t = decision_and_bugfix("w1");
    d.send("shop", "w1", &t, 0, "session-end").await;
    until("the source waits for the judge", || {
        d.state(&uri("w1")).state == "waiting_judge"
    })
    .await;
    settle().await;
    assert_eq!(d.state(&uri("w1")).state, "waiting_judge");
    assert_eq!(d.script.calls(), 0, "nothing is sent");
    assert!(d.distilled().is_empty());
    let h = d.capture_health().await;
    assert_eq!(h.distill_state, "waiting_judge");
    let line = d.distill_check().await;
    assert_eq!(line.status, CheckStatus::Warn, "{line:?}");
    assert!(
        line.line().contains("alice config set judge.provider jev"),
        "{}",
        line.line()
    );

    // Jev becomes the judge: the waiting source goes on by itself.
    d.write_config(JEV, |s| {
        s.judge.provider == alice_core::settings::JudgeProvider::Jev
    })
    .await;
    until("the waiting session is distilled", || {
        d.state(&uri("w1")).state == "done"
    })
    .await;
    assert_eq!(d.distilled().len(), 2);
    assert_eq!(d.capture_health().await.distill_state, "ready");
    d.shutdown();
}

/// Spec §6.7: a project the cloud policy keeps at home is never sent; its source says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_cloud_denied_project_is_never_sent() {
    let config = "[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n\n[privacy]\ncloud_denied_projects = [\"work\"]\n";
    let d = Daemon::start(
        Opts {
            config,
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let t = decision_and_bugfix("cd1");
    let a = d.send("work", "cd1", &t, 0, "session-end").await;
    assert_eq!(a.turns_stored, 2, "raw capture goes on");
    until("the source is skipped for the cloud", || {
        d.state(&uri("cd1")).state == "skipped_cloud"
    })
    .await;
    settle().await;
    assert_eq!(d.script.distill_calls(), 0, "no distillation request");
    assert_eq!(
        d.script.calls(),
        0,
        "nothing at all is sent for the project"
    );
    assert!(d.distilled().is_empty());
    let st = d.state(&uri("cd1"));
    assert!(
        st.next_try_at.is_some(),
        "the policy is asked again later: {st:?}"
    );

    // The policy allows the project again: the source is distilled at the next check.
    d.write_config(JEV, |s| s.privacy.cloud_denied_projects.is_empty())
        .await;
    until("the allowed session is distilled", || {
        d.state(&uri("cd1")).state == "done"
    })
    .await;
    assert_eq!(d.script.distill_calls(), 1);
    assert_eq!(d.distilled().len(), 2);
    d.shutdown();
}

/// Spec §6.7: three failed runs in a row open the breaker; a fourth source is not sent while
/// it is open, and raw capture never waits on it.
#[tokio::test(flavor = "multi_thread")]
async fn three_failures_open_the_breaker() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    d.script.set_outage(Outage::Status500);
    for s in ["b1", "b2", "b3"] {
        let t = one_decision(s);
        d.send("shop", s, &t, 0, "session-end").await;
    }
    until("three runs failed", || {
        ["b1", "b2", "b3"]
            .iter()
            .all(|s| d.state(&uri(s)).attempts == 1)
    })
    .await;
    // One try and three retries each.
    assert_eq!(d.script.distill_calls(), 12);
    for s in ["b1", "b2", "b3"] {
        let st = d.state(&uri(s));
        assert_eq!(st.state, "due", "a failure waits and tries again: {st:?}");
        assert!(st.next_try_at.is_some(), "with a back-off: {st:?}");
    }
    let h = d.capture_health().await;
    assert_eq!(h.distill_state, "breaker_open");
    let line = d.distill_check().await;
    assert_eq!(line.status, CheckStatus::Warn);
    assert!(
        line.line()
            .contains("Jev failed 3 times; it is tried again at"),
        "{}",
        line.line()
    );

    // A fourth source: due, not sent until the breaker's time.
    let mut t = one_decision("b4");
    let first = d.send("shop", "b4", &t, 0, "stop").await;
    assert_eq!(first.turns_stored, 1, "raw capture goes on");
    let off = t.len();
    t.prompt(
        "b4-p2",
        "And one more question about the build cache settings?",
    )
    .reply(
        "b4-a2",
        "The cache directory is cleared on every release build.",
    );
    let a = d.send("shop", "b4", &t, off, "session-end").await;
    assert_eq!(
        a.turns_stored, 1,
        "raw capture goes on while the breaker is open"
    );
    until("the fourth source is put off", || {
        d.state(&uri("b4")).next_try_at.is_some()
    })
    .await;
    settle().await;
    let st = d.state(&uri("b4"));
    assert_eq!(st.state, "due");
    assert_eq!(
        st.attempts, 0,
        "a source the breaker holds back has not failed"
    );
    let at = chrono::DateTime::parse_from_rfc3339(st.next_try_at.as_deref().unwrap()).unwrap();
    assert!(
        at > chrono::Utc::now() + chrono::Duration::minutes(10),
        "it waits for the breaker (15 minutes): {st:?}"
    );
    assert_eq!(
        d.script.distill_calls(),
        12,
        "the fourth source is not sent"
    );
    d.shutdown();
}

/// Spec §6.7: an unreachable Jev is a failure like a 500 (sent once: nothing reached Jev, so
/// the client does not retry). Three of them open the breaker.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_jev_counts_toward_the_breaker() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    d.script.set_outage(Outage::Unreachable);
    for s in ["u1", "u2", "u3"] {
        let t = one_decision(s);
        d.send("shop", s, &t, 0, "session-end").await;
    }
    until("three runs failed", || {
        ["u1", "u2", "u3"]
            .iter()
            .all(|s| d.state(&uri(s)).attempts == 1)
    })
    .await;
    assert_eq!(d.script.distill_calls(), 3, "one request each, no retry");
    assert_eq!(d.capture_health().await.distill_state, "breaker_open");
    // Jev is back, but the breaker holds until its time.
    d.script.set_outage(Outage::None);
    let t = one_decision("u4");
    d.send("shop", "u4", &t, 0, "session-end").await;
    until("the fourth source is put off", || {
        d.state(&uri("u4")).next_try_at.is_some()
    })
    .await;
    assert_eq!(d.script.distill_calls(), 3);
    assert!(d.distilled().is_empty());
    d.shutdown();
}

/// Spec §6.7: when distillation's share of the day's budget is used up, the source waits for
/// the next UTC day; that is not a failure, and nothing is sent.
#[tokio::test(flavor = "multi_thread")]
async fn the_share_used_up_waits_for_tomorrow() {
    let config = "[judge]\nprovider = \"jev\"\ndaily_budget_usd = 0.001\n\n[distill]\nprovider = \"jev\"\nbudget_share = 0.05\n";
    let d = Daemon::start(
        Opts {
            config,
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let t = decision_and_bugfix("bu1");
    d.send("shop", "bu1", &t, 0, "session-end").await;
    until("the source waits for its budget", || {
        d.state(&uri("bu1")).state == "waiting_budget"
    })
    .await;
    let st = d.state(&uri("bu1"));
    let tomorrow = (chrono::Utc::now().date_naive() + chrono::Duration::days(1))
        .format("%Y-%m-%dT00:00:00.000Z")
        .to_string();
    assert_eq!(st.next_try_at.as_deref(), Some(tomorrow.as_str()));
    assert_eq!(st.attempts, 0, "not a failure");
    assert_eq!(d.script.distill_calls(), 0, "nothing is sent");
    let h = d.capture_health().await;
    assert_eq!(h.distill_state, "waiting_budget");
    let line = d.distill_check().await;
    assert_eq!(line.status, CheckStatus::Warn);
    let text = line.line();
    assert!(
        text.contains("today's distillation share is used up")
            && text.contains("raise distill.budget_share or judge.daily_budget_usd"),
        "{text}"
    );
    assert!(
        !text.contains("judge's daily budget is used up"),
        "our own words, not the judge error's: {text}"
    );

    // A raised share lets it go on the same day.
    d.write_config(
        "[judge]\nprovider = \"jev\"\ndaily_budget_usd = 0.001\n\n[distill]\nprovider = \"jev\"\nbudget_share = 1.0\n",
        |s| s.distill.budget_share == 1.0,
    )
    .await;
    until("the session is distilled after the raise", || {
        d.state(&uri("bu1")).state == "done"
    })
    .await;
    assert_eq!(d.script.distill_calls(), 1);
    assert_eq!(d.distilled().len(), 2);
    d.shutdown();
}

/// Spec §6.5: a crash between the memories and the cursor repeats the run, and the repeat
/// stores nothing twice: the request is answered from the cache and every memory comes back
/// as a duplicate through its idempotency key.
#[tokio::test(flavor = "multi_thread")]
async fn a_rerun_after_a_crash_stores_nothing_twice() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let t = decision_and_bugfix("cr1");
    d.send("shop", "cr1", &t, 0, "session-end").await;
    until("the first run", || d.state(&uri("cr1")).state == "done").await;
    let before: Vec<String> = d.distilled().into_iter().map(|m| m.id).collect();
    assert_eq!(before.len(), 2);
    assert_eq!(d.script.distill_calls(), 1);

    // The cursor never moved (as after a crash right after the memories were written). One of
    // the memories was trashed meanwhile: its text alone would no longer stop a second copy.
    d.restart_with(
        "UPDATE capture_state SET distill_ord = 0, distill_state = 'due', distill_next_try_at = NULL;
         UPDATE memories SET status = 'trashed' WHERE origin = 'distill' AND kind = 'decision';",
    )
    .await;
    until("the run is repeated", || {
        d.svc.distill_runs().iter().any(|r| r.uri == uri("cr1"))
    })
    .await;
    let run = d
        .svc
        .distill_runs()
        .into_iter()
        .find(|r| r.uri == uri("cr1"))
        .unwrap();
    assert_eq!(run.state, "done", "{run:?}");
    assert_eq!((run.stored, run.duplicates), (0, 2), "{run:?}");
    assert_eq!(
        d.script.distill_calls(),
        1,
        "the answer came from the cache"
    );
    let after: Vec<String> = d.distilled().into_iter().map(|m| m.id).collect();
    assert_eq!(after, before, "nothing is stored twice");
    assert!(
        d.distilled()
            .iter()
            .any(|m| m.kind == "decision" && m.status == "trashed"),
        "a trashed memory is not brought back"
    );
    let st = d.state(&uri("cr1"));
    assert_eq!((st.state.as_str(), st.distill_ord), ("done", st.next_ord));
    d.shutdown();
}

/// Spec §6.3: a passage that says what the agent already saved in the session is filtered
/// before Jev is asked: no question about it, and no second memory.
#[tokio::test(flavor = "multi_thread")]
async fn a_memory_the_agent_saved_is_not_stored_again() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    // The agent saved it during the session.
    let saved = d
        .svc
        .remember(RememberRequest {
            content: "The owner prefers pnpm to npm in this repository.".into(),
            title: Some("Use pnpm, not npm".into()),
            kind: Some(alice_core::model::Kind::Preference),
            cwd: Some(d.repo("shop").to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut t = Transcript::default();
    t.prompt("sv-p1", "Use pnpm, not npm please")
        .tool(
            "sv-a1",
            "tu_mem",
            "mcp__alice__remember",
            json!({"title": "Use pnpm, not npm", "content": "The owner prefers pnpm to npm in this repository.", "kind": "preference"}),
        )
        .tool_result("sv-r1", "tu_mem", "saved")
        .reply("sv-a1b", "Saved.")
        .prompt("sv-p2", "Is it safe to run the migration on a Friday afternoon?")
        .reply(
            "sv-a2",
            "Never run the schema migration on a Friday afternoon: the backup job locks the tables until midnight and the deploy then times out.",
        );
    d.send("shop", "sv1", &t, 0, "session-end").await;
    until("the session is distilled", || {
        d.state(&uri("sv1")).state == "done"
    })
    .await;

    let asked = d.script.questions();
    assert!(!asked.is_empty(), "the other passage was asked about");
    assert!(
        asked.iter().all(|q| !q.contains("pnpm")),
        "no question about what was saved: {asked:?}"
    );
    let mems = d.distilled();
    assert_eq!(mems.len(), 1, "{mems:?}");
    assert_eq!(mems[0].kind, "warning");
    let c = d.db();
    let pnpm: i64 = c
        .query_row(
            "SELECT count(*) FROM memories WHERE title LIKE '%pnpm%' OR body LIKE '%pnpm%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pnpm, 1, "only the agent's own memory ({:?})", saved.id);
    d.shutdown();
}

/// Spec §4.4, §8.3: a backfilled session is not distilled by any trigger, and `alice capture
/// distill <session>` (the route it calls) makes it due.
#[tokio::test(flavor = "multi_thread")]
async fn a_backfilled_session_is_not_distilled_unless_asked() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = one_decision("bf1");
    d.send("shop", "bf1", &t, 0, "stop").await;
    // As a backfill registers it (Task 18b).
    d.restart_with("UPDATE capture_state SET distill_state = 'skipped_backfill'")
        .await;
    // More of the file caught up by a sweep, then a session end with nothing new: neither is the
    // session going on live (review I1), so it stays backfilled.
    let off = t.len();
    t.prompt("bf1-p2", BUGFIX_PROMPT).reply("bf1-a2", BUGFIX);
    d.send("shop", "bf1", &t, off, "sweep").await;
    d.send("shop", "bf1", &t, t.len(), "session-end").await;
    settle().await;
    assert_eq!(d.state(&uri("bf1")).state, "skipped_backfill");
    assert!(d.state(&uri("bf1")).ended);
    assert_eq!(d.script.distill_calls(), 0);

    // Unknown ids are refused by name.
    let err = d
        .svc
        .capture_distill_now("no-such-session")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no-such-session"), "{err}");

    // The route `alice capture distill bf1` calls.
    alice_daemon::install_tls();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "t".repeat(64);
    let cancel = tokio_util::sync::CancellationToken::new();
    let app = alice_daemon::http::router(d.svc.clone(), token.clone(), port, cancel.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://127.0.0.1:{port}/api/v1/capture/distill");
    let http = reqwest::Client::new();
    let unauthorised = http
        .post(&url)
        .json(&json!({"session_id": "bf1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorised.status(), 401, "behind the token");
    let r = http
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({"session_id": "bf1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["due"], json!([uri("bf1")]), "{body}");
    let missing = http
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({"source_id": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    until("the asked-for session is distilled", || {
        d.state(&uri("bf1")).state == "done"
    })
    .await;
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision", "bugfix"]);
    cancel.cancel();
    d.shutdown();
}

/// Task 18b review, I1 and M8: a session `alice capture backfill` registered, with its subagent,
/// is left alone by the quiet trigger. When it goes on live (`claude --resume` appends to the
/// same file, and its hooks fire), only what is captured from then on is distilled: one run,
/// with neither the backfilled turns nor the backfilled subagent's report.
#[tokio::test(flavor = "multi_thread")]
async fn a_backfilled_session_that_goes_on_live_distills_only_the_live_turns() {
    let d = Daemon::start(
        Opts {
            idle: Some(Duration::from_millis(400)),
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let mut t = one_decision("rl1");
    let path = d.write_transcript("rl1", &t);
    let mut sub = Transcript::subagent("w1");
    sub.prompt("rl1-w1-p1", "Find how the cache service is configured.")
        .reply(
            "rl1-w1-a1",
            "The cache service listens on port 6380 and keeps its data in /var/lib/cache; the config file is deploy/cache.toml and the service restarts nightly at 03:00 UTC.",
        );
    let sub_path = d.dir.path().join("rl1/subagents/agent-w1.jsonl");
    std::fs::create_dir_all(sub_path.parent().unwrap()).unwrap();
    std::fs::write(&sub_path, &sub.bytes).unwrap();
    // Old files, as `alice capture backfill` finds them.
    age(&path, Duration::from_secs(3 * 24 * 3600));
    age(&sub_path, Duration::from_secs(3 * 24 * 3600));
    let backfill = |agent: Option<(&str, &Path)>| CaptureNotify {
        agent: "claude-code".into(),
        event: "backfill".into(),
        session_id: "rl1".into(),
        transcript_path: path.to_string_lossy().into_owned(),
        cwd: Some(d.repo("shop").to_string_lossy().into_owned()),
        agent_id: agent.map(|(id, _)| id.to_string()),
        agent_transcript_path: agent.map(|(_, p)| p.to_string_lossy().into_owned()),
        ..Default::default()
    };
    d.svc.capture_notify(backfill(None)).await.unwrap();
    until("the session is backfilled", || {
        d.has_state(&uri("rl1")) && d.state(&uri("rl1")).byte_offset == t.len() as i64
    })
    .await;
    d.svc
        .capture_notify(backfill(Some(("w1", &sub_path))))
        .await
        .unwrap();
    let sub_uri = format!("{}:agent:w1", uri("rl1"));
    until("its subagent is backfilled", || {
        d.has_state(&sub_uri) && d.state(&sub_uri).byte_offset == sub.len() as i64
    })
    .await;
    // Quiet for longer than `idle_minutes` (400 ms here), and captured just now.
    settle().await;
    settle().await;
    assert_eq!(d.state(&uri("rl1")).state, "skipped_backfill");
    assert_eq!(d.state(&sub_uri).state, "skipped_backfill");
    assert_eq!(d.script.distill_calls(), 0, "the quiet trigger leaves it");

    // The session is resumed: a new turn, then its hooks' session end.
    t.prompt("rl1-p2", BUGFIX_PROMPT).reply("rl1-a2", BUGFIX);
    std::fs::write(&path, &t.bytes).unwrap();
    d.notify("shop", "rl1", &path, "session-end").await;
    until("the live part is distilled", || {
        d.state(&uri("rl1")).state == "done"
    })
    .await;
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["bugfix"], "the live turn only");
    assert_eq!(d.script.distill_calls(), 1, "one run");
    assert!(
        !d.script.received("pnpm workspaces") && !d.script.received("port 6380"),
        "neither the backfilled turn nor the backfilled subagent was sent"
    );
    let st = d.state(&uri("rl1"));
    assert_eq!(st.distill_ord, st.next_ord);
    let sub_st = d.state(&sub_uri);
    assert_eq!(sub_st.distill_ord, sub_st.next_ord);
    d.shutdown();
}

/// Spec §6.2: a session quiet for `distill.idle_minutes` that holds undistilled turns is
/// distilled while it is still open.
#[tokio::test(flavor = "multi_thread")]
async fn the_quiet_trigger_distills_an_open_session() {
    let d = Daemon::start(
        Opts {
            idle: Some(Duration::from_millis(400)),
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let mut t = decision_and_bugfix("q1");
    t.prompt("q1-p3", "Where do we stand with the cache work right now?")
        .reply("q1-a3", HANDOFF);
    d.send("shop", "q1", &t, 0, "stop").await;
    until("the quiet session is distilled", || {
        d.state(&uri("q1")).state == "done"
    })
    .await;
    let st = d.state(&uri("q1"));
    assert!(!st.ended, "still open");
    assert_eq!(st.distill_ord, st.next_ord);
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(
        kinds,
        ["decision", "bugfix"],
        "no handoff while the session is open (it is not offered)"
    );
    // Done stays done: nothing new, nothing asked again.
    settle().await;
    assert_eq!(d.script.distill_calls(), 1);
    assert_eq!(d.state(&uri("q1")).state, "done");
    d.shutdown();
}

// ---- More of the behaviour ----

/// Spec §6.2, §8.4: with `judge.provider = jev` and no key, the source waits for the judge and
/// health and the doctor say a key is missing.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_key_waits_and_says_so() {
    let d = Daemon::start(
        Opts {
            key: false,
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let t = one_decision("nk1");
    d.send("shop", "nk1", &t, 0, "session-end").await;
    until("the source waits for the judge", || {
        d.state(&uri("nk1")).state == "waiting_judge"
    })
    .await;
    let h = d.capture_health().await;
    assert_eq!(h.distill_state, "no_key");
    assert!(
        h.distill_warning
            .as_deref()
            .unwrap_or_default()
            .contains("no Jev key"),
        "{h:?}"
    );
    let line = d.distill_check().await;
    assert_eq!(line.status, CheckStatus::Warn);
    assert!(
        line.line().contains("alice config set-secret jev_api_key"),
        "{}",
        line.line()
    );
    assert_eq!(d.script.calls(), 0);
    d.shutdown();
}

/// Spec §7: distillation is off by default; nothing is sent and the doctor says it is off.
#[tokio::test(flavor = "multi_thread")]
async fn with_distillation_off_nothing_is_sent() {
    let d = Daemon::start(
        Opts {
            config: "[judge]\nprovider = \"jev\"\n",
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let t = one_decision("off1");
    d.send("shop", "off1", &t, 0, "session-end").await;
    settle().await;
    let st = d.state(&uri("off1"));
    assert!(
        st.state == "idle" || st.state == "due",
        "never distilled: {st:?}"
    );
    assert_eq!(d.script.distill_calls(), 0);
    assert!(d.distilled().is_empty());
    let h = d.capture_health().await;
    assert_eq!(h.distill_state, "off");
    let line = d.distill_check().await;
    assert_eq!(line.status, CheckStatus::Ok);
    assert!(line.detail.contains("off"), "{line:?}");
    d.shutdown();
}

/// Spec §6.2: a run covers the session and the new chunks of its subagents. A subagent's final
/// report becomes a memory with provenance in the subagent's own chunks, and the subagent
/// source's cursor moves with the run.
#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_report_is_distilled_with_its_session() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut main = Transcript::default();
    main.prompt("sa-p1", "Look up how the cache service is configured, please.")
        .tool(
            "sa-a1",
            "tu_task",
            "Task",
            json!({"subagent_type": "Explore", "description": "find the cache config", "prompt": "Find how the cache service is configured."}),
        )
        .tool_result("sa-r1", "tu_task", "done")
        .reply("sa-a1b", "Done.");
    d.send("shop", "sa1", &main, 0, "stop").await;

    let mut sub = Transcript::subagent("ag1");
    sub.prompt("ag-p1", "Find how the cache service is configured.")
        .reply(
            "ag-a1",
            "The cache service listens on port 6380 and keeps its data in /var/lib/cache; the config file is deploy/cache.toml and the service restarts nightly at 03:00 UTC.",
        );
    let a = d
        .send_q(
            TranscriptQuery {
                agent: "claude-code".into(),
                session: "sa1".into(),
                subagent: Some("ag1".into()),
                offset: 0,
                event: "subagent-stop".into(),
                cwd: Some(d.repo("shop").to_string_lossy().into_owned()),
                ..Default::default()
            },
            &sub.bytes,
        )
        .await;
    assert!(a.turns_stored >= 1, "{a:?}");
    let sub_uri = format!("{}:agent:ag1", uri("sa1"));

    let off = main.len();
    main.prompt("sa-p2", "Thanks, that is all for today.")
        .reply("sa-a2", HANDOFF);
    d.send("shop", "sa1", &main, off, "session-end").await;
    until("the session is distilled", || {
        d.state(&uri("sa1")).state == "done"
    })
    .await;
    let mems = d.distilled();
    let fact = mems
        .iter()
        .find(|m| m.kind == "fact")
        .unwrap_or_else(|| panic!("the report became a fact: {mems:?}"));
    assert!(fact.body.contains("port 6380"));
    assert!(
        fact.body
            .ends_with("\n\nContext: Find how the cache service is configured."),
        "the subagent's first chunk opens its turn, so its prompt is known: {}",
        fact.body
    );
    assert!(
        mems.iter().any(|m| m.kind == "handoff"),
        "a closing run offers the handoff: {mems:?}"
    );
    assert_eq!(
        d.provenance(&fact.id),
        d.chunks_with(&sub_uri, "port 6380"),
        "provenance in the subagent's own chunks"
    );
    let sub_state = d.state(&sub_uri);
    assert_eq!(
        sub_state.distill_ord, sub_state.next_ord,
        "the subagent's cursor moved with the run"
    );
    d.shutdown();
}

/// Spec §6.2: the client's `no_distill` and an excluded project are never distilled (raw
/// capture of the first goes on).
#[tokio::test(flavor = "multi_thread")]
async fn no_distill_and_excluded_projects_are_skipped() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let t = decision_and_bugfix("nd1");
    let a = d
        .send_q(
            TranscriptQuery {
                agent: "claude-code".into(),
                session: "nd1".into(),
                offset: 0,
                event: "session-end".into(),
                cwd: Some(d.repo("shop").to_string_lossy().into_owned()),
                no_distill: Some(1),
                ..Default::default()
            },
            &t.bytes,
        )
        .await;
    assert_eq!(a.turns_stored, 2, "raw capture goes on");
    until("the no_distill session is skipped", || {
        d.state(&uri("nd1")).state == "skipped_excluded"
    })
    .await;

    // A project excluded after its session was captured.
    let t = decision_and_bugfix("ex1");
    d.send("secret", "ex1", &t, 0, "stop").await;
    d.write_config(
        "[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n\n[capture]\nexcluded_projects = [\"secret\"]\n",
        |s| !s.capture.excluded_projects.is_empty(),
    )
    .await;
    d.svc.capture_distill_now("ex1").await.unwrap();
    until("the excluded session is skipped", || {
        d.state(&uri("ex1")).state == "skipped_excluded"
    })
    .await;
    settle().await;
    assert_eq!(d.script.distill_calls(), 0);
    assert!(d.distilled().is_empty());
    d.shutdown();
}

/// A time `hours` ago, as the store writes times.
fn hours_ago(hours: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::hours(hours))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Task 12 supplement, rulings 4 and 5: on a server tenant (no sweep), a session with no
/// capture for 24 h is marked ended and distilled as a closed session, so its handoff is offered.
#[tokio::test(flavor = "multi_thread")]
async fn a_tenant_session_silent_for_a_day_is_ended_and_distilled() {
    let mut d = Daemon::start(
        Opts {
            tenant: true,
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let mut t = one_decision("st1");
    t.prompt("st1-p2", "Where do we stand with the cache work right now?")
        .reply("st1-a2", HANDOFF);
    d.send("shop", "st1", &t, 0, "stop").await;
    assert!(!d.state(&uri("st1")).ended);
    d.restart_with(&format!(
        "UPDATE capture_state SET last_capture_at = '{0}', created_at = '{0}'",
        hours_ago(30)
    ))
    .await;
    until("the silent session is distilled", || {
        d.state(&uri("st1")).state == "done"
    })
    .await;
    assert!(d.state(&uri("st1")).ended, "marked ended");
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision", "handoff"]);
    d.shutdown();
}

/// Fix round 1 (the Task 6b ruling): locally the capture sweep owns the stale end, with its
/// final pass that captures the trailing turn, so the distill worker never marks a local source
/// ended. The quiet session is still distilled, as an open one (no handoff offered).
#[tokio::test(flavor = "multi_thread")]
async fn a_local_session_silent_for_a_day_is_not_marked_ended() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = one_decision("sl1");
    t.prompt("sl1-p2", "Where do we stand with the cache work right now?")
        .reply("sl1-a2", HANDOFF);
    d.send("shop", "sl1", &t, 0, "stop").await;
    d.restart_with(&format!(
        "UPDATE capture_state SET last_capture_at = '{0}', created_at = '{0}'",
        hours_ago(30)
    ))
    .await;
    until("the quiet session is distilled", || {
        d.state(&uri("sl1")).state == "done"
    })
    .await;
    settle().await;
    assert!(!d.state(&uri("sl1")).ended, "not marked ended locally");
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision"], "an open session offers no handoff");
    d.shutdown();
}

/// Spec §6.2: a run takes at most 60 turns; a longer backlog becomes several runs, oldest
/// first, each with its own request.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_session_is_distilled_in_runs_oldest_first() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = Transcript::default();
    t.prompt("lg-p0", BUGFIX_PROMPT).reply("lg-a0", BUGFIX);
    for i in 1..60 {
        t.prompt(
            &format!("lg-p{i}"),
            &format!("Question number {i} about the build pipeline settings?"),
        )
        .reply(&format!("lg-a{i}"), &format!("Answer {i}."));
    }
    t.prompt("lg-p60", DECISION_PROMPT)
        .reply("lg-a60", DECISION);
    let a = d.send("shop", "lg1", &t, 0, "session-end").await;
    assert_eq!(a.turns_stored, 61);
    until("the long session is distilled", || {
        d.state(&uri("lg1")).state == "done"
    })
    .await;
    let st = d.state(&uri("lg1"));
    assert_eq!(st.distill_ord, st.next_ord);
    assert_eq!(d.script.distill_calls(), 2, "two runs");
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["bugfix", "decision"], "the oldest run first");
    let runs = d.svc.distill_runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0].state, "due", "more was left after the first run");
    assert_eq!(runs[1].state, "done");
    d.shutdown();
}

/// Task 12 supplement, ruling 11: the session's first chunk goes by the text rule, so a session
/// opened with a slash command does not have the command quoted as a prompt.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_opened_with_a_command_does_not_quote_it() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = Transcript::default();
    t.prompt("cm-p1", "/review-pr 42 carefully please")
        .reply("cm-a1", DECISION)
        .prompt("cm-p2", BUGFIX_PROMPT)
        .reply("cm-a2", BUGFIX);
    d.send("shop", "cm1", &t, 0, "session-end").await;
    until("the session is distilled", || {
        d.state(&uri("cm1")).state == "done"
    })
    .await;
    let asked = d.script.questions();
    assert!(
        asked.iter().all(|q| !q.contains("review-pr")),
        "the command is never quoted: {asked:?}"
    );
    let mems = d.distilled();
    let decision = mems
        .iter()
        .find(|m| m.kind == "decision")
        .expect("a decision");
    assert!(!decision.body.contains("Context:"), "{}", decision.body);
    let bugfix = mems.iter().find(|m| m.kind == "bugfix").expect("a bugfix");
    assert!(
        bugfix
            .body
            .ends_with(&format!("\n\nContext: {BUGFIX_PROMPT}")),
        "a later turn keeps its prompt: {}",
        bugfix.body
    );
    d.shutdown();
}

/// Spec §6.4: a preference of the user's goes to `global`, every other kind to the session's
/// project.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_preference_goes_to_global() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = one_decision("pu1");
    t.prompt("pu1-p2", "Please keep the wording consistent everywhere you write.")
        .reply(
            "pu1-a2",
            "Understood: from now on I will always answer in British English in every project, including code comments and commit messages.",
        );
    d.send("shop", "pu1", &t, 0, "session-end").await;
    until("the session is distilled", || {
        d.state(&uri("pu1")).state == "done"
    })
    .await;
    let mems = d.distilled();
    let pref = mems
        .iter()
        .find(|m| m.kind == "preference")
        .unwrap_or_else(|| panic!("a preference: {mems:?}"));
    assert_eq!(pref.project, "global");
    let decision = mems.iter().find(|m| m.kind == "decision").unwrap();
    assert_eq!(decision.project, "shop");
    d.shutdown();
}

/// The brief: a failed run backs the source off; after five attempts it is `failed`, and
/// `alice capture distill` tries it again from scratch.
#[tokio::test(flavor = "multi_thread")]
async fn five_failed_attempts_leave_a_source_failed_until_asked() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    d.script.set_outage(Outage::Unreachable);
    let t = one_decision("fa1");
    d.send("shop", "fa1", &t, 0, "session-end").await;
    until("the first attempt failed", || {
        d.state(&uri("fa1")).attempts == 1
    })
    .await;
    let st = d.state(&uri("fa1"));
    let at = chrono::DateTime::parse_from_rfc3339(st.next_try_at.as_deref().unwrap()).unwrap();
    let wait = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
    assert!(
        wait > chrono::Duration::minutes(4) && wait <= chrono::Duration::minutes(5),
        "five minutes first: {st:?}"
    );
    // Four attempts so far; the next one is the fifth (a restart also closes the breaker).
    d.restart_with("UPDATE capture_state SET distill_attempts = 4, distill_next_try_at = NULL")
        .await;
    until("the fifth attempt failed", || {
        d.state(&uri("fa1")).state == "failed"
    })
    .await;
    let st = d.state(&uri("fa1"));
    assert_eq!((st.attempts, st.next_try_at.as_deref()), (5, None));
    settle().await;
    assert_eq!(
        d.state(&uri("fa1")).state,
        "failed",
        "no trigger retries it"
    );

    d.script.set_outage(Outage::None);
    d.svc.capture_distill_now("fa1").await.unwrap();
    until("asked, it is distilled", || {
        d.state(&uri("fa1")).state == "done"
    })
    .await;
    assert_eq!(d.state(&uri("fa1")).attempts, 0);
    assert_eq!(d.distilled().len(), 1);
    d.shutdown();
}

/// A session that ends while it is being distilled is distilled to its end: the capture pass
/// finds it due already, and the run keeps it due for what it did not read.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_ends_during_its_run_is_distilled_to_the_end() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    *d.script.delay.lock() = Duration::from_millis(1_500);
    let mut t = one_decision("dr1");
    d.send("shop", "dr1", &t, 0, "stop").await;
    d.svc.capture_distill_now("dr1").await.unwrap();
    until("the first run asks Jev", || d.script.distill_calls() == 1).await;
    // While Jev answers, the session goes on and ends.
    let off = t.len();
    t.prompt("dr1-p2", BUGFIX_PROMPT).reply("dr1-a2", BUGFIX);
    d.send("shop", "dr1", &t, off, "session-end").await;
    *d.script.delay.lock() = Duration::ZERO;
    until("the whole session is distilled", || {
        let st = d.state(&uri("dr1"));
        st.state == "done" && st.distill_ord == st.next_ord
    })
    .await;
    assert_eq!(d.script.distill_calls(), 2);
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision", "bugfix"]);
    d.shutdown();
}

/// A source whose chunks are gone (raw retention) has nothing to read: its cursor moves to its
/// end without a request, so the quiet check does not find it again and again.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_whose_chunks_are_gone_moves_on() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let t = one_decision("gc1");
    d.send("shop", "gc1", &t, 0, "session-end").await;
    until("the session is distilled", || {
        d.state(&uri("gc1")).state == "done"
    })
    .await;
    // Five more ords were taken, and their chunks are gone.
    d.restart_with(
        "UPDATE capture_state SET next_ord = next_ord + 5, distill_state = 'due', distill_next_try_at = NULL",
    )
    .await;
    until("the empty run ends", || {
        let st = d.state(&uri("gc1"));
        st.state == "done" && st.distill_ord == st.next_ord
    })
    .await;
    assert_eq!(d.script.distill_calls(), 1, "nothing to ask about");
    d.shutdown();
}

// ---- Fix round 1 ----

/// Fix round 1, C1: a subagent working in another repository is gated by its own project. A
/// subagent in a cloud-denied project is never sent (its chunks wait, and its state says why);
/// one in an allowed project is sent, and its memory lands in its own project, not the session's.
#[tokio::test(flavor = "multi_thread")]
async fn a_cloud_denied_subagent_is_never_sent() {
    let config = "[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n\n[privacy]\ncloud_denied_projects = [\"work\"]\n";
    let answers: &[(&str, &str)] = &[
        ("decided to use pnpm workspaces", "decision"),
        ("The cache service listens on port", "fact"),
        ("zebra-quartz", "fact"),
    ];
    let d = Daemon::start(
        Opts {
            config,
            ..Opts::jev()
        },
        DistillScript::new(answers),
    );
    let mut main = one_decision("cs1");
    main.prompt("cs1-p2", "Ask two helpers about the payroll keys and the cache, please.")
        .tool(
            "cs1-a2",
            "tu_w",
            "Task",
            json!({"subagent_type": "Explore", "description": "payroll keys", "prompt": "Find when the payroll keys rotate."}),
        )
        .tool_result("cs1-r2", "tu_w", "done")
        .tool(
            "cs1-a3",
            "tu_t",
            "Task",
            json!({"subagent_type": "Explore", "description": "cache config", "prompt": "Find how the cache service is configured."}),
        )
        .tool_result("cs1-r3", "tu_t", "done")
        .reply("cs1-a4", "Done.");
    d.send("shop", "cs1", &main, 0, "stop").await;
    let subagent = |id: &str, repo: &str, prompt: &str, report: &str| {
        let mut t = Transcript::subagent(id);
        t.prompt(&format!("{id}-p"), prompt)
            .reply(&format!("{id}-a"), report);
        let q = TranscriptQuery {
            agent: "claude-code".into(),
            session: "cs1".into(),
            subagent: Some(id.into()),
            offset: 0,
            event: "subagent-stop".into(),
            cwd: Some(d.repo(repo).to_string_lossy().into_owned()),
            ..Default::default()
        };
        (q, t)
    };
    let (q, t) = subagent(
        "agw",
        "work",
        "Find when the payroll keys rotate.",
        "The payroll signing keys rotate every Monday at 02:00 UTC from the vault-west cluster; the rotation job is tagged zebra-quartz-41 in the scheduler.",
    );
    assert!(d.send_q(q, &t.bytes).await.turns_stored >= 1);
    let (q, t) = subagent(
        "agt",
        "tools",
        "Find how the cache service is configured.",
        "The cache service listens on port 6380 and keeps its data in /var/lib/cache; the config file is deploy/cache.toml, tracked as TCK-55321, and the service restarts nightly at 03:00 UTC.",
    );
    assert!(d.send_q(q, &t.bytes).await.turns_stored >= 1);
    // `tools` gains a rule after the capture: the request is redacted with every covered
    // project's rules, so the ticket never leaves (the daemon looks at a project file every 2 s).
    std::fs::write(
        d.repo("tools").join(".alice.toml"),
        "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\n",
    )
    .unwrap();
    let tools = alice_core::store::projects::tree_of(&d.db(), "tools").unwrap();
    until("the daemon holds the tools rule", || {
        !d.svc
            .settings_handle()
            .project_privacy(tools.as_ref())
            .custom_patterns
            .is_empty()
    })
    .await;
    let off = main.len();
    main.prompt("cs1-p3", "Thanks, that is all for today.")
        .reply("cs1-a5", "Glad to help.");
    d.send("shop", "cs1", &main, off, "session-end").await;
    until("the session is distilled", || {
        d.state(&uri("cs1")).state == "done"
    })
    .await;

    assert!(d.script.distill_calls() >= 1);
    assert!(
        !d.script.received("zebra-quartz") && !d.script.received("payroll signing keys"),
        "the denied subagent's text never left"
    );
    let work = d.state(&format!("{}:agent:agw", uri("cs1")));
    assert_eq!(work.state, "skipped_cloud", "{work:?}");
    assert_eq!(work.distill_ord, 0, "its chunks wait for the policy");
    let tools = d.state(&format!("{}:agent:agt", uri("cs1")));
    assert_eq!(tools.distill_ord, tools.next_ord);

    let mems = d.distilled();
    assert!(
        mems.iter().all(|m| !m.body.contains("zebra-quartz")),
        "{mems:?}"
    );
    let fact = mems
        .iter()
        .find(|m| m.body.contains("port 6380"))
        .unwrap_or_else(|| panic!("the allowed subagent's report: {mems:?}"));
    assert_eq!(
        fact.project, "tools",
        "a subagent's passage goes to its own project"
    );
    assert!(
        !d.script.received("TCK-55321"),
        "the request was redacted with the subagent's project rules too"
    );
    assert!(
        fact.body.contains("[REDACTED:custom:ticket]") && !fact.body.contains("TCK-55321"),
        "the memory was redacted with its project's rules: {}",
        fact.body
    );
    let in_shop: Vec<&str> = mems
        .iter()
        .filter(|m| m.project == "shop")
        .map(|m| m.kind.as_str())
        .collect();
    assert_eq!(
        in_shop,
        ["decision"],
        "nothing from a subagent lands in shop"
    );
    d.shutdown();
}

/// Fix round 1, I1, and fix round 2: while the database refuses writes, no run starts (a paid
/// answer could not be kept), and the worker sleeps between its ticks instead of spinning.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_sent_while_the_database_is_read_only() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    *d.script.delay.lock() = Duration::from_millis(1_500);
    d.send(
        "shop",
        "ro1",
        &decision_marked("ro1", "ro1"),
        0,
        "session-end",
    )
    .await;
    until("the first run asks Jev", || d.script.distill_calls() == 1).await;
    // While Jev answers, a second session ends, and then the disk fills up.
    d.send(
        "shop",
        "ro2",
        &decision_marked("ro2", "ro2"),
        0,
        "session-end",
    )
    .await;
    assert_eq!(d.state(&uri("ro2")).state, "due");
    d.svc.force_read_only("the disk is full (test)");
    until("the first answer came", || d.script.answered() == 1).await;
    settle().await;
    let before = d.svc.distill_rounds();
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let turns = d.svc.distill_rounds() - before;
    assert!(
        turns <= 30,
        "the worker sleeps while read-only (a 100 ms tick): {turns} turns in a second"
    );
    assert_eq!(
        d.script.distill_calls(),
        1,
        "the second session is not sent"
    );
    assert_eq!(d.state(&uri("ro2")).state, "due");
    assert!(
        d.distilled().is_empty(),
        "the first answer could not be kept"
    );
    d.shutdown();
}

/// Fix round 1, I2: the global capture switches leave a session idle (never skipped for good):
/// switched on again, it is distilled.
#[tokio::test(flavor = "multi_thread")]
async fn the_capture_switches_leave_a_session_idle_until_they_are_on() {
    let d = Daemon::start(
        Opts {
            idle: Some(Duration::from_millis(1_000)),
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let t = one_decision("sw1");
    d.send("shop", "sw1", &t, 0, "stop").await;
    d.write_config(
        "[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n\n[capture]\nenabled = false\n",
        |s| !s.capture.enabled,
    )
    .await;
    // Quiet now, but its capture is off: the quiet trigger leaves it alone.
    tokio::time::sleep(Duration::from_millis(1_800)).await;
    assert!(
        d.svc.distill_runs().is_empty(),
        "no run while capture is off: {:?}",
        d.svc.distill_runs()
    );
    // Asked for, the run leaves it idle (never skipped for good).
    d.svc.capture_distill_now("sw1").await.unwrap();
    until("the run leaves it idle", || {
        d.svc
            .distill_runs()
            .iter()
            .any(|r| r.uri == uri("sw1") && r.state == "idle")
    })
    .await;
    assert_eq!(d.state(&uri("sw1")).state, "idle");
    assert_eq!(d.script.distill_calls(), 0);
    // Switched on again, the quiet trigger distils it by itself.
    d.write_config(JEV, |s| s.capture.enabled).await;
    until("switched on, it is distilled", || {
        d.state(&uri("sw1")).state == "done"
    })
    .await;
    assert_eq!(d.distilled().len(), 1);
    d.shutdown();
}

/// Fix round 1, I3: a run reads only below the cursor's `next_ord`. A turn whose last slice is
/// not written yet (its chunks are there, the cursor is not) is left whole for the next run.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_reads_only_complete_turns() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let t = decision_and_bugfix("ht1");
    d.send("shop", "ht1", &t, 0, "stop").await;
    assert_eq!(d.state(&uri("ht1")).next_ord, 2);
    // Turn 2's chunk is written, its slice's cursor is not (yet).
    d.restart_with("UPDATE capture_state SET next_ord = 1, distill_state = 'due'")
        .await;
    until("the first run", || d.state(&uri("ht1")).state == "done").await;
    let st = d.state(&uri("ht1"));
    assert_eq!(st.distill_ord, 1, "only the complete turn was read");
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision"]);
    // The last slice lands.
    d.restart_with("UPDATE capture_state SET next_ord = 2, distill_state = 'due'")
        .await;
    until("the second run", || d.state(&uri("ht1")).distill_ord == 2).await;
    let mems = d.distilled();
    let bugfix = mems.iter().find(|m| m.kind == "bugfix").expect("a bugfix");
    assert!(
        bugfix
            .body
            .ends_with(&format!("\n\nContext: {BUGFIX_PROMPT}")),
        "the turn was read whole, with its prompt: {}",
        bugfix.body
    );
    d.shutdown();
}

/// One decision turn whose reply carries `marker` (so sessions differ in text).
fn decision_marked(s: &str, marker: &str) -> Transcript {
    let mut t = Transcript::default();
    t.prompt(&format!("{s}-p1"), DECISION_PROMPT).reply(
        &format!("{s}-a1"),
        &format!("{DECISION} Noted as {marker}."),
    );
    t
}

/// Fix round 1, I4: the quiet trigger marks only sessions captured within the last week, and
/// due sessions are run the most recently captured first; an older one waits to be asked.
#[tokio::test(flavor = "multi_thread")]
async fn the_backlog_is_drained_newest_first_within_a_week() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    for s in ["old1", "mid1", "new1"] {
        d.send("shop", s, &decision_marked(s, s), 0, "stop").await;
    }
    d.restart_with(&format!(
        "UPDATE capture_state SET last_capture_at = '{}' WHERE session_id = 'old1';
         UPDATE capture_state SET last_capture_at = '{}' WHERE session_id = 'mid1';
         UPDATE capture_state SET last_capture_at = '{}' WHERE session_id = 'new1';",
        hours_ago(8 * 24),
        hours_ago(3 * 24),
        hours_ago(24)
    ))
    .await;
    until("the recent sessions are distilled", || {
        d.state(&uri("mid1")).state == "done" && d.state(&uri("new1")).state == "done"
    })
    .await;
    settle().await;
    let order: Vec<String> = d.svc.distill_runs().into_iter().map(|r| r.uri).collect();
    assert_eq!(order, [uri("new1"), uri("mid1")], "newest first");
    assert_eq!(
        d.state(&uri("old1")).state,
        "idle",
        "older than a week: asked for, not quiet-triggered"
    );
    d.svc.capture_distill_now("old1").await.unwrap();
    until("asked, the old one is distilled", || {
        d.state(&uri("old1")).state == "done"
    })
    .await;
    assert_eq!(d.distilled().len(), 3);
    d.shutdown();
}

/// Fix round 1 (minor): an answer from the cache did not reach Jev, so it does not close the
/// breaker: two failures, a cached answer, and a third failure open it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cached_answer_does_not_close_the_breaker() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    d.send("shop", "x1", &one_decision("x1"), 0, "session-end")
        .await;
    until("the first session", || d.state(&uri("x1")).state == "done").await;
    assert_eq!(d.script.distill_calls(), 1);
    d.script.set_outage(Outage::Unreachable);
    let failing = |s: &str, prompt: &str, reply: &str| {
        let mut t = Transcript::default();
        t.prompt(&format!("{s}-p1"), prompt)
            .reply(&format!("{s}-a1"), reply);
        t
    };
    d.send(
        "shop",
        "y1",
        &failing("y1", BUGFIX_PROMPT, BUGFIX),
        0,
        "session-end",
    )
    .await;
    until("y1 failed", || d.state(&uri("y1")).attempts == 1).await;
    d.send(
        "shop",
        "y2",
        &failing(
            "y2",
            "Is it safe to run the migration on a Friday afternoon?",
            "Never run the schema migration on a Friday afternoon: the backup job locks the tables until midnight and the deploy then times out.",
        ),
        0,
        "session-end",
    )
    .await;
    until("y2 failed", || d.state(&uri("y2")).attempts == 1).await;
    assert_eq!(d.script.distill_calls(), 3);
    assert_ne!(d.capture_health().await.distill_state, "breaker_open");
    // The same text in another session: the same request, answered from the cache.
    d.send("shop", "x2", &one_decision("x2"), 0, "session-end")
        .await;
    until("x2 from the cache", || d.state(&uri("x2")).state == "done").await;
    assert_eq!(d.script.distill_calls(), 3, "nothing was sent for x2");
    d.send(
        "shop",
        "y3",
        &failing(
            "y3",
            "Look up how the cache service is configured, please.",
            "The cache service listens on port 6380 and keeps its data in /var/lib/cache; the config file is deploy/cache.toml.",
        ),
        0,
        "session-end",
    )
    .await;
    until("y3 failed", || d.state(&uri("y3")).attempts == 1).await;
    assert_eq!(
        d.capture_health().await.distill_state,
        "breaker_open",
        "the third real failure in a row opens it"
    );
    d.shutdown();
}

/// Fix round 1 (minor): a source whose run cannot read what it needs is put off with its
/// back-off, and the round goes on with the other sources.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_source_is_put_off_and_the_others_go_on() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    d.send("shop", "ok1", &decision_marked("ok1", "ok1"), 0, "stop")
        .await;
    d.send("shop", "bad1", &decision_marked("bad1", "bad1"), 0, "stop")
        .await;
    // bad1's project row cannot be found (it runs first: it is the newest).
    d.restart_with(&format!(
        "PRAGMA foreign_keys = OFF;
         UPDATE sources SET project_id = 'NO-SUCH-PROJECT' WHERE uri = '{}';
         UPDATE capture_state SET distill_state = 'due', last_capture_at = '{}'
           WHERE session_id = 'bad1';
         UPDATE capture_state SET distill_state = 'due', last_capture_at = '{}'
           WHERE session_id = 'ok1';",
        uri("bad1"),
        hours_ago(1),
        hours_ago(2)
    ))
    .await;
    until("the readable one is distilled", || {
        d.state(&uri("ok1")).state == "done"
    })
    .await;
    let bad = d.state(&uri("bad1"));
    assert_eq!((bad.state.as_str(), bad.attempts), ("due", 1), "{bad:?}");
    assert!(bad.next_try_at.is_some(), "with its back-off: {bad:?}");
    d.shutdown();
}

// ---- Fix round 2 ----

/// A session with a subagent in the project `repo`, whose report is about the cache: the main
/// session `s` is uploaded (`stop`), then the subagent `agent`.
async fn session_with_subagent(d: &Daemon, s: &str, agent: &str, repo: &str) -> Transcript {
    let main = one_decision(s);
    d.send("shop", s, &main, 0, "stop").await;
    let mut sub = Transcript::subagent(agent);
    sub.prompt(&format!("{agent}-p"), "Find how the cache service is configured.")
        .reply(
            &format!("{agent}-a"),
            "The cache service listens on port 6380 and keeps its data in /var/lib/cache; the config file is deploy/cache.toml and it restarts at 03:00 UTC.",
        );
    let a = d
        .send_q(
            TranscriptQuery {
                agent: "claude-code".into(),
                session: s.into(),
                subagent: Some(agent.into()),
                offset: 0,
                event: "subagent-stop".into(),
                cwd: Some(d.repo(repo).to_string_lossy().into_owned()),
                ..Default::default()
            },
            &sub.bytes,
        )
        .await;
    assert!(a.turns_stored >= 1, "{a:?}");
    main
}

/// Fix round 2: a subagent the cloud policy lets go again wakes its session only when the
/// session is at rest; a failed one stays failed until `alice capture distill` asks for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_subagent_recheck_does_not_revive_a_failed_session() {
    let config = "[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n\n[privacy]\ncloud_denied_projects = [\"work\"]\n";
    let mut d = Daemon::start(
        Opts {
            config,
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let main = session_with_subagent(&d, "rv1", "agr", "work").await;
    d.send("shop", "rv1", &main, main.len(), "session-end")
        .await;
    let sub_uri = format!("{}:agent:agr", uri("rv1"));
    until("the session ran, the subagent waits for the policy", || {
        d.state(&uri("rv1")).state == "done" && d.state(&sub_uri).state == "skipped_cloud"
    })
    .await;
    assert_eq!(d.script.distill_calls(), 1);
    d.restart_with(
        "UPDATE capture_state SET distill_state = 'failed', distill_attempts = 5
         WHERE session_id = 'rv1' AND parent_source_id IS NULL",
    )
    .await;
    // The policy lets `work` go: the subagent's recheck makes it due, which wakes no session.
    d.write_config(JEV, |s| s.privacy.cloud_denied_projects.is_empty())
        .await;
    until("the subagent was rechecked", || {
        d.state(&sub_uri).state == "idle"
    })
    .await;
    settle().await;
    assert_eq!(d.state(&uri("rv1")).state, "failed", "not revived");
    assert_eq!(d.state(&sub_uri).distill_ord, 0);
    assert_eq!(d.script.distill_calls(), 1);
    // Asked for, it goes on, with the subagent.
    d.svc.capture_distill_now("rv1").await.unwrap();
    until("asked, the session is distilled", || {
        let sub = d.state(&sub_uri);
        d.state(&uri("rv1")).state == "done" && sub.distill_ord == sub.next_ord
    })
    .await;
    assert!(d.distilled().iter().any(|m| m.kind == "fact"));
    d.shutdown();
}

/// Fix round 2: a subagent whose project rules are unknown right now (a broken `.alice.toml`)
/// is not sent; the session's own passages are, and the session is due again a few minutes
/// later for the subagent instead of done.
#[tokio::test(flavor = "multi_thread")]
async fn a_subagent_with_unknown_rules_puts_its_session_off() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let main = session_with_subagent(&d, "uk1", "agk", "lab").await;
    std::fs::write(
        d.repo("lab").join(".alice.toml"),
        "[privacy\nno_capture = tru",
    )
    .unwrap();
    let lab = alice_core::store::projects::tree_of(&d.db(), "lab").unwrap();
    until("the daemon sees the broken rules", || {
        d.svc.settings_handle().project_fail_closed(lab.as_ref())
    })
    .await;
    d.send("shop", "uk1", &main, main.len(), "session-end")
        .await;
    until("the session ran", || {
        let st = d.state(&uri("uk1"));
        st.distill_ord == st.next_ord && st.next_try_at.is_some()
    })
    .await;
    let st = d.state(&uri("uk1"));
    assert_eq!(st.state, "due", "{st:?}");
    let at = chrono::DateTime::parse_from_rfc3339(st.next_try_at.as_deref().unwrap()).unwrap();
    assert!(
        at > chrono::Utc::now() + chrono::Duration::minutes(4),
        "due again in five minutes: {st:?}"
    );
    let sub = d.state(&format!("{}:agent:agk", uri("uk1")));
    assert_eq!(sub.distill_ord, 0, "the subagent's chunks wait");
    assert!(!d.script.received("port 6380"), "and were not sent");
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision"]);
    settle().await;
    let runs = d
        .svc
        .distill_runs()
        .iter()
        .filter(|r| r.uri == uri("uk1"))
        .count();
    assert_eq!(runs, 1, "not run again every tick");
    d.shutdown();
}

// ---- The merge boundary: capture, distillation and the judge rules together ----

/// A Codex rollout, built line by line (format: `alice-core/tests/fixtures/capture/codex`).
#[derive(Default)]
struct Rollout {
    bytes: Vec<u8>,
    clock: u64,
}

impl Rollout {
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

/// Merge boundary (Tasks 12 and 17a): a Codex session on a server tenant is distilled like a
/// Claude Code one. It takes one run and one request, the memories' author is the Codex session,
/// a conclusion carries its prompt, and the provenance is the rollout's own chunks.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_session_is_distilled_like_a_claude_code_one() {
    let d = Daemon::start(
        Opts {
            tenant: true,
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let mut r = Rollout::default();
    r.line(
        "session_meta",
        json!({"id": "cxd1", "cwd": "/home/user/shop"}),
    )
    .prompt(DECISION_PROMPT)
    .reply(DECISION)
    .prompt(BUGFIX_PROMPT)
    .reply(BUGFIX);
    let a = d
        .send_q(
            TranscriptQuery {
                agent: "codex".into(),
                session: "cxd1".into(),
                offset: 0,
                event: "session-end".into(),
                project_header: Some("shop".into()),
                ..Default::default()
            },
            &r.bytes,
        )
        .await;
    assert_eq!(a.turns_stored, 2);
    let uri = "codex:session:cxd1";
    until("the Codex session is distilled", || {
        d.state(uri).state == "done"
    })
    .await;
    let st = d.state(uri);
    assert_eq!(st.distill_ord, st.next_ord, "the cursor is at the end");
    assert!(st.ended);
    assert_eq!(d.script.distill_calls(), 1, "one request for the run");
    let mems = d.distilled();
    let mut kinds: Vec<&str> = mems.iter().map(|m| m.kind.as_str()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ["bugfix", "decision"], "{mems:?}");
    for m in &mems {
        assert_eq!(m.agent.as_deref(), Some("codex"));
        assert_eq!(m.session.as_deref(), Some("cxd1"));
        assert_eq!(m.project, "shop");
    }
    let decision = mems.iter().find(|m| m.kind == "decision").unwrap();
    assert!(
        decision
            .body
            .ends_with(&format!("\n\nContext: {DECISION_PROMPT}")),
        "a Codex conclusion carries its prompt: {}",
        decision.body
    );
    assert_eq!(
        d.provenance(&decision.id),
        d.chunks_with(uri, "decided to use pnpm workspaces")
    );
    assert!(!d.provenance(&decision.id).is_empty());
    d.shutdown();
}

/// Merge boundary (Tasks 6b and 12): a session end that the local worker reads after a notify
/// makes the session due at once (the quiet trigger would wait 15 minutes here), and the ended
/// session offers its handoff.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_session_end_notify_makes_the_session_due_at_once() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = one_decision("ln1");
    t.prompt("ln1-p2", "Where do we stand with the cache work right now?")
        .reply("ln1-a2", HANDOFF);
    let path = d.write_transcript("ln1", &t);
    d.notify("shop", "ln1", &path, "session-end").await;
    until("the local session is distilled", || {
        d.has_state(&uri("ln1")) && d.state(&uri("ln1")).state == "done"
    })
    .await;
    let st = d.state(&uri("ln1"));
    assert!(st.ended, "the session end ended the source");
    assert_eq!(st.byte_offset, t.len() as i64, "the worker read the file");
    assert_eq!(st.distill_ord, st.next_ord);
    assert_eq!(d.script.distill_calls(), 1);
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision", "handoff"]);
    d.shutdown();
}

/// Merge boundary (Tasks 6b, 12 and 15): a session end sent in several bodies (a capped upload
/// says `more=1`; the local worker reads a long transcript in several passes) makes the session
/// due only with its last body. A run before it would take part of the session for its end and
/// offer a handoff from the middle of the session.
#[tokio::test(flavor = "multi_thread")]
async fn a_capped_session_end_is_due_only_after_its_last_body() {
    let d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = decision_and_bugfix("cp1");
    let capped = t.len() as usize;
    t.prompt("cp1-p3", "Where do we stand with the cache work right now?")
        .reply("cp1-a3", HANDOFF);
    let q = |offset: u64, more: Option<u8>| TranscriptQuery {
        agent: "claude-code".into(),
        session: "cp1".into(),
        offset,
        event: "session-end".into(),
        cwd: Some(d.repo("shop").to_string_lossy().into_owned()),
        more,
        ..Default::default()
    };
    // The first body: a capped one, whose last turn stays open.
    let a = d.send_q(q(0, Some(1)), &t.bytes[..capped]).await;
    assert_eq!(a.turns_stored, 1);
    settle().await;
    let st = d.state(&uri("cp1"));
    assert_eq!(st.state, "idle", "not due before the last body: {st:?}");
    assert!(!st.ended);
    assert_eq!(
        d.script.distill_calls(),
        0,
        "no run over part of the session"
    );

    // The last body: the session end, whole.
    let a2 = d
        .send_q(q(a.next_offset, None), &t.bytes[a.next_offset as usize..])
        .await;
    assert_eq!(a2.turns_stored, 2);
    until("the whole session is distilled", || {
        d.state(&uri("cp1")).state == "done"
    })
    .await;
    let st = d.state(&uri("cp1"));
    assert!(st.ended);
    assert_eq!(st.distill_ord, st.next_ord);
    assert_eq!(
        d.script.distill_calls(),
        1,
        "one run, over the whole session"
    );
    let kinds: Vec<String> = d.distilled().into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["decision", "bugfix", "handoff"]);
    d.shutdown();
}

/// Merge boundary (Tasks 6b and 12; the ruling "locally Task 6b's sweep owns the stale end"): on
/// a local daemon both workers run. A session whose last capture was 30 hours ago is not ended
/// by the distill worker while its transcript is fresh. The sweep ends it once the file has been
/// idle for a day, and its final pass captures the trailing turn. An end by the distill worker
/// would have lost that turn, because an ended source is never swept again.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_sweep_ends_a_stale_local_session() {
    let mut d = Daemon::start(Opts::jev(), DistillScript::new(ANSWERS));
    let mut t = one_decision("sw1");
    let open_at = t.len() as i64;
    // The trailing turn waits for its tool call: a `stop` leaves it open.
    t.prompt("sw1-p2", "Watch the nightly build for me.").tool(
        "sw1-a2",
        "tu_sw",
        "Bash",
        json!({"command": "sleep 999"}),
    );
    let path = d.write_transcript("sw1", &t);
    d.notify("shop", "sw1", &path, "stop").await;
    until("the complete turn is captured", || {
        d.has_state(&uri("sw1")) && d.state(&uri("sw1")).byte_offset == open_at
    })
    .await;
    d.restart_with(&format!(
        "UPDATE capture_state SET last_capture_at = '{0}', created_at = '{0}'",
        hours_ago(30)
    ))
    .await;
    // The quiet session is distilled as an open one meanwhile; it is not ended.
    until("the quiet session is distilled", || {
        d.state(&uri("sw1")).state == "done"
    })
    .await;
    settle().await;
    let st = d.state(&uri("sw1"));
    assert!(
        !st.ended,
        "the distill worker leaves a local session to the sweep"
    );
    assert_eq!(st.byte_offset, open_at);

    age(&path, Duration::from_secs(25 * 3600));
    until("the sweep ends the stale session", || {
        d.state(&uri("sw1")).ended
    })
    .await;
    let st = d.state(&uri("sw1"));
    assert_eq!(
        st.byte_offset,
        t.len() as i64,
        "the final pass read to the end"
    );
    assert_eq!(
        d.chunks_with(&uri("sw1"), "sleep 999").len(),
        1,
        "the final pass closed the trailing turn"
    );
    d.shutdown();
}

/// Merge boundary (Tasks 12 and 16, rule R11): capture retention empties a captured session but
/// keeps its source and its state. Asked to distil it, the distiller finds nothing left to read
/// and settles it as `done`, with no request and no second run.
#[tokio::test(flavor = "multi_thread")]
async fn an_emptied_capture_source_settles_done_without_a_request() {
    let config = "[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n\n[capture]\nraw_retention = \"30d\"\n";
    let mut d = Daemon::start(
        Opts {
            config,
            ..Opts::jev()
        },
        DistillScript::new(ANSWERS),
    );
    let t = decision_and_bugfix("er1");
    d.send("shop", "er1", &t, 0, "stop").await;
    // Captured 40 days ago (a state is stamped before its first chunk) and never distilled.
    d.restart_with(&format!(
        "UPDATE chunks SET created_at = '{0}'
           WHERE source_id IN (SELECT source_id FROM capture_state);
         UPDATE capture_state SET created_at = '{1}', last_capture_at = '{0}';",
        hours_ago(40 * 24),
        hours_ago(41 * 24)
    ))
    .await;
    d.svc
        .run_job(alice_daemon::maintenance::Job::CaptureRetention)
        .await
        .unwrap();
    until("retention empties the session", || {
        d.chunks_with(&uri("er1"), "").is_empty()
    })
    .await;
    let st = d.state(&uri("er1"));
    assert!(
        st.next_ord > 0,
        "R11 keeps the emptied source and its state: {st:?}"
    );
    assert_eq!((st.state.as_str(), st.distill_ord), ("idle", 0));

    d.svc.capture_distill_now("er1").await.unwrap();
    until("the empty run settles", || {
        let st = d.state(&uri("er1"));
        st.state == "done" && st.distill_ord == st.next_ord
    })
    .await;
    settle().await;
    let st = d.state(&uri("er1"));
    assert_eq!(st.state, "done", "{st:?}");
    assert_eq!((st.attempts, st.next_try_at.as_deref()), (0, None));
    assert_eq!(d.script.distill_calls(), 0, "nothing to ask about");
    let runs = d
        .svc
        .distill_runs()
        .iter()
        .filter(|r| r.uri == uri("er1"))
        .count();
    assert_eq!(runs, 1, "one run, then it rests");
    assert!(d.distilled().is_empty());
    d.shutdown();
}
