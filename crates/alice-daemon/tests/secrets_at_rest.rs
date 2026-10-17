//! Secrets at rest (owner's decision 2026-10-17): a person's store keeps credentials as written
//! when `privacy.redact` is off (on a server tenant: when the admin pinned
//! `ALICE_PRIVACY_REDACT=false`), the shared team store always redacts, and nothing sent to a
//! cloud service carries a secret, whatever the store keeps.
//!
//! Secret-shaped strings are built with `concat!`, so no scanner takes them for real ones.

mod support;

use alice_core::api::{GetRequest, OutputFormat, RecallMode, RecallRequest, RememberRequest};
use std::path::Path;
use support::caddy::Server;

/// A GitHub-shaped token.
const TOKEN: &str = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
/// The admin's `alice-server secrets keep`, as the tenant file pins it.
const KEEP: &[(&str, &str)] = &[("ALICE_PRIVACY_REDACT", "false")];

/// Whether any file under `dir` holds `needle` (the database and its WAL included).
fn tree_contains(dir: &Path, needle: &str) -> bool {
    std::fs::read_dir(dir).unwrap().any(|e| {
        let p = e.unwrap().path();
        if p.is_dir() {
            tree_contains(&p, needle)
        } else {
            std::fs::read(&p)
                .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
                .unwrap_or(false)
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keep_tenant_stores_and_recalls_a_secret_as_written_and_the_team_store_never_holds_one() {
    let s = Server::start_with_team_and_env(&["ann"], KEEP).await;
    let ann = s.tenant("ann");
    assert!(
        !ann.svc.settings().privacy.redact,
        "the admin's pin applies"
    );

    // The person's own store: as written, and recalled as written.
    let r = ann
        .svc
        .remember(RememberRequest {
            content: format!("The deploy bot pushes with the token {TOKEN} on the ops box"),
            project_hint: Some("ops".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let id = r.id.unwrap();
    let got = ann
        .svc
        .get(GetRequest {
            ids: vec![id.clone()],
            include_provenance: false,
        })
        .await
        .unwrap();
    assert!(
        got.memories[0].memory.body.contains(TOKEN),
        "stored as written"
    );
    let found = ann
        .svc
        .recall(RecallRequest {
            query: "deploy bot token ops box".into(),
            project: Some("ops".into()),
            mode: Some(RecallMode::Fast),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        found
            .results
            .iter()
            .any(|h| h.id == id && h.body.as_deref().is_some_and(|b| b.contains(TOKEN))),
        "recalled as written: {:?}",
        found.results
    );
    assert!(tree_contains(ann.home.root(), TOKEN));

    // A shared write leaves redacted: everyone reads the team store.
    let shared = ann
        .svc
        .remember(RememberRequest {
            content: format!("Team CI pushes with {TOKEN} from the runner"),
            shared: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(shared.shared);
    let team = s.team.as_ref().unwrap();
    let held = team
        .svc
        .get(GetRequest {
            ids: vec![shared.id.unwrap().trim_start_matches("team:").to_string()],
            include_provenance: false,
        })
        .await
        .unwrap();
    let body = &held.memories[0].memory.body;
    assert!(
        body.contains("[REDACTED:github_token]") && !body.contains(TOKEN),
        "{body}"
    );
    // The team store redacts what reaches it any other way too, pin or no pin.
    team.svc
        .remember(RememberRequest {
            content: format!("Straight to the team store: {TOKEN}"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !tree_contains(team.home.root(), TOKEN),
        "the team store holds no secret"
    );

    // The agents are told: credentials may be remembered here, never shared; the team store
    // (and a tenant without the pin) says never store secrets.
    use alice_daemon::mcp::{AliceMcp, Backend, INSTRUCTIONS, INSTRUCTIONS_KEEPING_SECRETS};
    use rmcp::ServerHandler;
    let told = |svc: &std::sync::Arc<alice_daemon::service::Service>| {
        AliceMcp::new(Backend::Local(svc.clone()))
            .get_info()
            .instructions
    };
    assert_eq!(
        told(&ann.svc).as_deref(),
        Some(INSTRUCTIONS_KEEPING_SECRETS)
    );
    assert_eq!(told(&team.svc).as_deref(), Some(INSTRUCTIONS));
    let plain = Server::start(&["bob"]).await;
    assert_eq!(
        told(&plain.tenant("bob").svc).as_deref(),
        Some(INSTRUCTIONS)
    );
}

// ---------------------------------------------------------------------------------------
// Egress: a local home with `privacy.redact = false` keeps secrets, and every cloud request
// (the Jev judge, precise recall, distillation, a remote embedder) is redacted all the same.
// The tests assert on the recorded request bytes.
// ---------------------------------------------------------------------------------------

use alice_core::api::TranscriptQuery;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::judge::jev::{HttpReply, JevTransport, TransportError};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A test key, split inside its prefix so no scanner takes it for a real one.
const KEY: &str = concat!("ts_", "live_secrets_at_rest_key_01");
/// Storage keeps secrets; Jev judges and distils.
const KEEPING: &str =
    "[privacy]\nredact = false\n\n[judge]\nprovider = \"jev\"\n\n[distill]\nprovider = \"jev\"\n";
/// What the token's first characters leave when a cut falls just after them.
const FRAGMENT: &str = concat!("gh", "p_");
/// A JWT's header (`{"alg":"HS256"}`): what is left of a cut token.
const JWT_HEAD: &str = concat!("eyJhbGciOiJ", "IUzI1NiJ9");

/// Every Jev request body, answered with a neutral choice (a distillation passage is kept as a
/// decision, so a memory is written from it).
#[derive(Default)]
struct JevRecorder {
    bodies: Mutex<Vec<String>>,
}

impl JevTransport for JevRecorder {
    fn post(
        &self,
        _endpoint: &str,
        _key: &str,
        body: &[u8],
        _timeout: Duration,
    ) -> Result<HttpReply, TransportError> {
        self.bodies
            .lock()
            .push(String::from_utf8_lossy(body).into_owned());
        let req: serde_json::Value = serde_json::from_slice(body).unwrap();
        let mut answers = serde_json::Map::new();
        for (qid, q) in req["questions"].as_object().unwrap() {
            let options: Vec<String> = q["criteria"].as_object().unwrap().keys().cloned().collect();
            let pick = ["decision", "unrelated", "none", "note", "global"]
                .into_iter()
                .find(|o| options.iter().any(|x| x == o))
                .map_or_else(|| options[0].clone(), str::to_string);
            let probs: serde_json::Map<String, serde_json::Value> = options
                .iter()
                .map(|o| (o.clone(), json!(if *o == pick { 1.0 } else { 0.0 })))
                .collect();
            answers.insert(
                qid.clone(),
                json!({"type": "choice", "choice": pick, "confidence": 1.0, "probabilities": probs}),
            );
        }
        let resp = json!({"model": "jev-1.13.0", "answers": answers,
            "usage": {"input_tokens": 500, "output_tokens": 2}});
        Ok(HttpReply {
            status: 200,
            body: serde_json::to_vec(&resp).unwrap(),
            retry_after: None,
        })
    }
}

/// A remote embedding provider: every text it is given (what the request would carry).
struct RemoteRecorder {
    inner: FakeEmbedder,
    texts: Arc<Mutex<Vec<String>>>,
}

impl Embedder for RemoteRecorder {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed_passages(&self, texts: &[String]) -> alice_core::error::Result<Vec<Vec<f32>>> {
        self.texts.lock().extend(texts.iter().cloned());
        self.inner.embed_passages(texts)
    }
    fn embed_query(&self, text: &str) -> alice_core::error::Result<Vec<f32>> {
        self.texts.lock().push(text.to_string());
        self.inner.embed_query(text)
    }
    fn is_remote(&self) -> bool {
        true
    }
}

struct Local {
    dir: tempfile::TempDir,
    svc: Arc<Service>,
    jev: Arc<JevRecorder>,
    embedded: Arc<Mutex<Vec<String>>>,
}

impl Local {
    fn start() -> Local {
        let dir = tempfile::tempdir().unwrap();
        let home = AliceHome::new(dir.path().join("home"));
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.config_file(), KEEPING).unwrap();
        let jev = Arc::new(JevRecorder::default());
        let embedded = Arc::new(Mutex::new(Vec::new()));
        let texts = embedded.clone();
        let svc = Service::start(
            home,
            &move || {
                Ok(Arc::new(RemoteRecorder {
                    inner: FakeEmbedder::new(384),
                    texts: texts.clone(),
                }) as Arc<dyn Embedder>)
            },
            ServiceOptions {
                secret_env: Some(Arc::new(|v: &str| {
                    (v == "TYPESAFE_API_KEY").then(|| KEY.to_string())
                })),
                settings_env: Some(Vec::new()),
                jev_transport: Some(jev.clone()),
                distill_tick: Duration::from_millis(100),
                judge_retry_every: Duration::from_millis(200),
                capture_settle: Duration::from_millis(50),
                capture_sweep_every: Duration::from_millis(100),
                ..Default::default()
            },
        )
        .unwrap();
        Local {
            dir,
            svc,
            jev,
            embedded,
        }
    }

    fn repo(&self, slug: &str) -> String {
        let p = self.dir.path().join(slug);
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn db(&self) -> rusqlite::Connection {
        alice_core::store::open_reader(&self.svc.home().db()).unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Nothing that left for the cloud holds the token, or the start of it.
    fn assert_no_secret_left(&self) {
        for b in self.jev.bodies.lock().iter() {
            assert!(
                !b.contains(TOKEN) && !b.contains(FRAGMENT) && !b.contains(JWT_HEAD),
                "a Jev request: {b}"
            );
        }
        for t in self.embedded.lock().iter() {
            assert!(
                !t.contains(TOKEN) && !t.contains(FRAGMENT),
                "an embedder input: {t}"
            );
        }
    }
}

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let t = Instant::now();
    while !cond() {
        assert!(t.elapsed() < Duration::from_secs(60), "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `n` characters of prose, ending in a space.
fn words(n: usize) -> String {
    let mut s = "deploy notes for the ops box ".repeat(n / 29 + 1);
    s.truncate(n - 1);
    s.push(' ');
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_secret_is_recalled_as_written_and_never_reaches_jev_or_a_remote_embedder() {
    let l = Local::start();
    assert!(!l.svc.settings().privacy.redact);
    let cwd = Some(l.repo("ops"));
    // The judge cuts a body to 700 characters, the embedding passage to 2,000 (with the title
    // and a line break): each key starts just before one of the cuts.
    for (title, body) in [
        (
            "Ops box deploy token",
            format!("{}{TOKEN} rotates monthly", words(696)),
        ),
        (
            "Ops runner token",
            format!("{}{TOKEN} rotates yearly", words(1978)),
        ),
    ] {
        l.svc
            .remember(RememberRequest {
                content: body,
                title: Some(title.into()),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    until("both memories are embedded and judged", || {
        l.count("SELECT count(*) FROM memories WHERE embed_state = 'done' AND judge_state = 'done'")
            == 2
    })
    .await;
    assert_eq!(
        l.count(&format!(
            "SELECT count(*) FROM memories WHERE instr(body, '{TOKEN}') > 0"
        )),
        2,
        "stored as written"
    );
    assert!(!l.jev.bodies.lock().is_empty(), "the judge asked Jev");
    assert!(
        l.embedded
            .lock()
            .iter()
            .any(|t| t.contains("[REDACTED:github_token]")),
        "the provider got the redacted passage"
    );

    // Precise recall: the query goes to the embedder and the results to Jev.
    let r = l
        .svc
        .recall(RecallRequest {
            query: format!("which token does the ops runner use, {TOKEN}?"),
            project: Some("ops".into()),
            mode: Some(RecallMode::Precise),
            format: OutputFormat::Json,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        r.results
            .iter()
            .any(|h| h.body.as_deref().is_some_and(|b| b.contains(TOKEN))),
        "recalled as written: {:?}",
        r.results
    );
    assert!(
        l.jev
            .bodies
            .lock()
            .iter()
            .any(|b| b.contains("How well does result")),
        "precise recall asked Jev"
    );
    l.assert_no_secret_left();
    l.svc.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_captured_secret_is_stored_as_written_and_distillation_never_sends_it() {
    let l = Local::start();
    let line = |v: serde_json::Value| {
        let mut b = serde_json::to_vec(&v).unwrap();
        b.push(b'\n');
        b
    };
    // A long access token right after the reply's only early spaces: the state's 600-character
    // excerpt of the reply has no space to cut at in its last three quarters, so it is cut hard
    // inside the token, which leaves a piece the JWT detector no longer knows unless the run
    // was redacted before the cut.
    let jwt = format!(
        "{JWT_HEAD}.eyJ{}.{}",
        "c3ViIjoib3Bz-_Ym90".repeat(50),
        "U2lnbmF0dXJl-_".repeat(4)
    );
    let decision = format!(
        "We decided to deploy the ops box with the bot token {TOKEN} and the API token: {jwt} from now on."
    );
    let mut bytes = line(
        json!({"type": "user", "uuid": "p1", "timestamp": "2026-10-17T10:00:00.000Z",
        "isSidechain": false, "message": {"role": "user", "content": "How should the ops box deploy from now on?"}}),
    );
    bytes.extend(line(
        json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-10-17T10:00:01.000Z",
        "isSidechain": false, "message": {"id": "msg-a1", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": decision}], "stop_reason": "end_turn"}}),
    ));
    let answer = l
        .svc
        .capture_transcript(
            TranscriptQuery {
                agent: "claude-code".into(),
                session: "s1".into(),
                offset: 0,
                event: "session-end".into(),
                cwd: Some(l.repo("ops")),
                ..Default::default()
            },
            axum::body::Bytes::from(bytes),
        )
        .await
        .unwrap()
        .unwrap_or_else(|c| panic!("refused behind the cursor {c}"));
    assert_eq!(answer.turns_stored, 1);
    assert!(
        l.count(&format!(
            "SELECT count(*) FROM chunks WHERE instr(text, '{TOKEN}') > 0"
        )) >= 1,
        "the captured chunk keeps the token"
    );
    assert!(
        l.count(&format!(
            "SELECT count(*) FROM chunks WHERE instr(text, '{jwt}') > 0"
        )) >= 1,
        "and the access token, whole"
    );
    until("the session is distilled", || {
        l.count(
            "SELECT count(*) FROM capture_state WHERE distill_state = 'done' AND session_id = 's1'",
        ) == 1
    })
    .await;
    assert!(
        l.jev
            .bodies
            .lock()
            .iter()
            .any(|b| b.contains("deploy the ops box") && b.contains("[REDACTED:jwt]")),
        "distillation asked Jev about the passage"
    );
    until("the distilled memory is written", || {
        l.count("SELECT count(*) FROM memories WHERE origin = 'distill'") >= 1
    })
    .await;
    assert_eq!(
        l.count(&format!(
            "SELECT count(*) FROM memories WHERE origin = 'distill' AND instr(body || title, '{FRAGMENT}') > 0"
        )),
        0,
        "a distilled memory holds no secret"
    );
    l.assert_no_secret_left();
    l.svc.shutdown();
}
