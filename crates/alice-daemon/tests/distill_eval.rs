//! The distillation evaluation (SP2 spec §11.2–§11.3, Task 13). The synthetic labelled set in
//! `fixtures/distill/`, checked by `distill_fixtures.rs`, goes through the real pipeline: capture,
//! the distill worker and the judge. The stored memories are then scored against `labels.json`.
//!
//! - `distill_eval_replays_the_recorded_answers` (CI): every Jev request, distillation's and the
//!   judge's, is answered from `fixtures/distill/recorded.jsonl` by the SHA-256 of its exact body.
//!   A request with no recorded answer fails the test, and so does a recorded answer that no
//!   request asked for. That pins the whole pipeline. The test then asserts the thresholds that
//!   §11.3 declares.
//! - `distill_eval_live` (ignored): the real Jev, through `ReqwestTransport` and the key in
//!   `TYPESAFE_API_KEY`. It writes `recorded.jsonl` with bodies only (no header, so no key),
//!   prints the metrics and the spend, and stops once the spend passes $0.10, or
//!   `ALICE_DISTILL_EVAL_STOP_USD` when that is set (Task 12b review: one re-record runs both
//!   live tests, which share $0.10 in all, so give the second what the first left):
//!   `cargo test -p alice-daemon --test distill_eval distill_eval_live -- --ignored --nocapture`.
//!   With `ALICE_DISTILL_EVAL_DRY=1` nothing is sent: a scripted Jev answers, and the pipeline runs
//!   twice and must send the same bodies both times. This is the free check before a live run.
//! - `distill_eval_long_session_replay` and `distill_eval_long_session_live` (Task 12c): the
//!   same two for the long-session set in `fixtures/distill/long/` (a session that fills its best
//!   set), with its own `recorded.jsonl` and checks.
//! - `distill_eval_flood_replay` and `distill_eval_flood_live` (Task 12b): the same two for the
//!   flood set in `fixtures/distill/flood/` (twenty reviewer verdicts next to three lessons), with
//!   its own `recorded.jsonl` and thresholds; Task 13's set and thresholds are not touched. Record
//!   it with `cargo test -p alice-daemon --test distill_eval distill_eval_flood_live -- --ignored
//!   --nocapture` (`ALICE_DISTILL_EVAL_DRY=1` for the free check first).
//!
//! How the set is run:
//! - **Isolation:** each session gets its own daemon, so what the judge compares never depends on
//!   scheduling.
//! - **Seeding:** an `already` session's `remember` calls are stored first, as the agent's own
//!   memories in the session's project, and are judged. Then the transcript is uploaded as a
//!   session end, and everything is distilled and judged before any result is read.
//! - **Matching:** a distilled memory counts while it is active; one the judge trashed (a
//!   duplicate) or superseded (by a newer distilled memory) does not, and any other status fails
//!   the test. A memory matches a labelled item of its own session when its body, without the
//!   `Context:` line, holds the item's phrase and its kind is one of the item's kinds. Each memory
//!   is matched on its own; s29's summary is cut into two chunks, and memories are never joined
//!   across that cut.
//!
//! **Clocks.** The distillation request holds no clock: `alice_core::distill` is pure. The judge's
//! comparison gives each memory's age (`age(created_at, now)`). The writer's clock and the judge's
//! are both the real one, and every memory is written and judged within the run, so the age always
//! reads "today". Both tests check that every request says so.

use alice_core::api::*;
use alice_core::capture::{AgentKind, ReadOptions, read};
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::judge::jev::{HttpReply, JevTransport, TransportError, USD_PER_MTOK_INPUT};
use alice_core::model::{Author, Kind};
use alice_core::normalize::sha256_hex;
use alice_daemon::judge_http::ReqwestTransport;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use axum::body::Bytes;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The key of the replay and the dry run, split inside its prefix so no scanner takes it for a
/// real one.
const FAKE_KEY: &str = concat!("ts_", "replay_distill_eval_key_0001");
/// The test home: Jev judges and distils, the whole $1 day is this run's (spec §11.3 asks for less
/// than $0.10), and a request may take a minute, so a 40-question request is not cut short.
const CONFIG: &str = "[judge]\nprovider = \"jev\"\ndaily_budget_usd = 1.0\ntimeout_ms = 60000\n\n[distill]\nprovider = \"jev\"\nbudget_share = 1.0\n";
/// A live run stops once its spend passes this, unless [`STOP_ENV`] says otherwise; it is also
/// §11.3's spend threshold.
const SPEND_STOP_USD: f64 = 0.10;
/// The live runs' spend stop in USD, so the two live runs of one re-record can share $0.10.
const STOP_ENV: &str = "ALICE_DISTILL_EVAL_STOP_USD";

/// The live runs' spend stop: `value` (of [`STOP_ENV`]) when it is a positive number, else
/// [`SPEND_STOP_USD`].
fn spend_stop(value: Option<&str>) -> f64 {
    value
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(SPEND_STOP_USD)
}
/// Set to run the dry check instead of the live run.
const DRY_ENV: &str = "ALICE_DISTILL_EVAL_DRY";
const REMEMBER: &str = "mcp__alice__remember";
const POLL: Duration = Duration::from_millis(50);

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/distill")
}

fn recorded_path() -> PathBuf {
    dir().join("recorded.jsonl")
}

/// The flood set (Task 12b): one session of reviewer verdicts next to three lessons.
fn flood_dir() -> PathBuf {
    dir().join("flood")
}

/// The long-session set (Task 12c, review M4): one session long enough to fill its best set.
fn long_dir() -> PathBuf {
    dir().join("long")
}

// ---- The set ----

#[derive(Debug, Clone)]
struct Item {
    phrase: String,
    kinds: Vec<String>,
    /// The long-session set (Task 12c): late in the session, after its best set first filled.
    late: bool,
}

/// One `remember` call of the transcript.
struct Remembered {
    title: Option<String>,
    content: String,
    kind: Option<Kind>,
}

struct Session {
    file: String,
    /// `gold`, `nothing` or `already`.
    kind: String,
    items: Vec<Item>,
    bytes: Vec<u8>,
    /// The transcript's session id.
    id: String,
    /// The last component of the transcript's working directory: the project's folder.
    repo: String,
    /// Written in Thai, or in Thai and English.
    thai: bool,
    remembered: Vec<Remembered>,
    /// The flood set: the opening words of each verdict or status report, which should end
    /// `ephemeral`.
    verdicts: Vec<String>,
}

impl Session {
    fn language(&self) -> &'static str {
        if self.thai {
            "Thai or mixed"
        } else {
            "English"
        }
    }
}

fn load_set() -> Vec<Session> {
    load_set_in(&dir())
}

fn load_set_in(dir: &Path) -> Vec<Session> {
    let text = std::fs::read_to_string(dir.join("labels.json")).unwrap();
    let labels: Value = serde_json::from_str(&text).unwrap();
    let opts = ReadOptions {
        at_file_start: true,
        close_trailing: true,
        ..Default::default()
    };
    labels["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let file = s["file"].as_str().unwrap().to_owned();
            let bytes = std::fs::read(dir.join("sessions").join(&file)).unwrap();
            let (mut id, mut cwd, mut remembered) = (None, None, Vec::new());
            for line in std::str::from_utf8(&bytes).unwrap().lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                if let Some(x) = v["sessionId"].as_str() {
                    id.get_or_insert_with(|| x.to_owned());
                }
                if let Some(x) = v["cwd"].as_str() {
                    cwd.get_or_insert_with(|| x.to_owned());
                }
                if v["type"] != "assistant" {
                    continue;
                }
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_use" && b["name"] == REMEMBER {
                        let input = &b["input"];
                        remembered.push(Remembered {
                            title: input["title"].as_str().map(str::to_owned),
                            content: input["content"].as_str().unwrap().to_owned(),
                            kind: input["kind"].as_str().map(|k| Kind::parse(k).unwrap()),
                        });
                    }
                }
            }
            let cwd = cwd.unwrap_or_else(|| panic!("{file}: no working directory"));
            let rendered: Vec<String> = read(AgentKind::ClaudeCode, &bytes, &opts)
                .turns
                .iter()
                .flat_map(|t| t.chunks.iter().map(|c| c.text.clone()))
                .collect();
            let items = s["gold"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| Item {
                    late: i["late"].as_bool().unwrap_or(false),
                    phrase: i["phrase"].as_str().unwrap().to_owned(),
                    kinds: i["kinds"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|k| k.as_str().unwrap().to_owned())
                        .collect(),
                })
                .collect();
            Session {
                kind: s["kind"].as_str().unwrap().to_owned(),
                items,
                id: id.unwrap_or_else(|| panic!("{file}: no session id")),
                repo: Path::new(&cwd)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                thai: rendered
                    .iter()
                    .any(|t| t.chars().any(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c))),
                remembered,
                verdicts: s["verdicts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect(),
                bytes,
                file,
            }
        })
        .collect()
}

/// `recorded.jsonl`: the response body of each request body, by the request body's SHA-256.
fn load_recorded() -> HashMap<String, Vec<u8>> {
    load_recorded_from(&recorded_path())
}

fn load_recorded_from(path: &Path) -> HashMap<String, Vec<u8>> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "no recorded answers ({e}); record them once with the live run (see the module docs)"
        )
    });
    let mut out = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            v.as_object().map(|o| o.len()),
            Some(2),
            "line {}: bodies only",
            n + 1
        );
        let request = v["request_body"].as_str().unwrap();
        let response = v["response_body"].as_str().unwrap();
        let old = out.insert(sha256_hex(request.as_bytes()), response.as_bytes().to_vec());
        assert!(old.is_none(), "line {}: a request recorded twice", n + 1);
    }
    out
}

// ---- The transport ----

enum Mode {
    /// Answers from `recorded.jsonl` by the request body's SHA-256.
    Replay(HashMap<String, Vec<u8>>),
    /// The real Jev.
    Live(ReqwestTransport),
    /// A scripted Jev that sends nothing (see [`dry_answer`]).
    Dry,
}

struct EvalTransport {
    mode: Mode,
    /// The labelled items of the session being run: the dry script keeps their passages.
    items: Mutex<Vec<Item>>,
    /// Every request answered with a 200, in order: `(request body, response body)`.
    answered: Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
    /// Replay: the hashes of the requests that have no recorded answer.
    misses: Mutex<Vec<String>>,
    /// Requests that give a memory an age other than "today": the body's SHA-256 and those ages.
    ages: Mutex<Vec<String>>,
    /// What Jev's answers say was spent (input tokens at Jev's price).
    spent: Mutex<f64>,
    /// Nothing more is sent once the spend passes this: [`spend_stop`] in a live run,
    /// [`SPEND_STOP_USD`] otherwise.
    stop_usd: f64,
    /// The spend passed `stop_usd`: nothing more is sent.
    stopped: AtomicBool,
    /// Live: how long each request took.
    latencies_ms: Mutex<Vec<u64>>,
}

impl EvalTransport {
    fn new(mode: Mode) -> Arc<Self> {
        let stop_usd = match mode {
            Mode::Live(_) => spend_stop(std::env::var(STOP_ENV).ok().as_deref()),
            _ => SPEND_STOP_USD,
        };
        Arc::new(Self {
            stop_usd,
            mode,
            items: Mutex::new(Vec::new()),
            answered: Mutex::new(Vec::new()),
            misses: Mutex::new(Vec::new()),
            ages: Mutex::new(Vec::new()),
            spent: Mutex::new(0.0),
            stopped: AtomicBool::new(false),
            latencies_ms: Mutex::new(Vec::new()),
        })
    }

    /// Fails the test (naming `what`) once a request had no answer, an age was not "today", or the
    /// spend passed the stop.
    fn check(&self, what: &str) {
        let misses = self.misses.lock().clone();
        assert!(
            misses.is_empty(),
            "{what}: {} request(s) have no recorded answer, so the pipeline no longer sends what \
             recorded.jsonl holds (record it again with the live run): {misses:?}",
            misses.len()
        );
        let ages = self.ages.lock().clone();
        assert!(
            ages.is_empty(),
            "{what}: a request gives a memory an age other than \"today\", so a clock the replay cannot \
             pin reached its body (body SHA-256: ages): {ages:?}"
        );
        assert!(
            !self.stopped.load(Ordering::SeqCst),
            "{what}: the spend passed ${}; the run stops",
            self.stop_usd
        );
    }

    fn hashes(&self) -> Vec<String> {
        self.answered
            .lock()
            .iter()
            .map(|(b, _)| sha256_hex(b))
            .collect()
    }
}

impl JevTransport for EvalTransport {
    fn post(
        &self,
        endpoint: &str,
        key: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpReply, TransportError> {
        let hash = sha256_hex(body);
        let text = std::str::from_utf8(body).expect("a request body is UTF-8 JSON");
        let ages = ages_not_today(text);
        if !ages.is_empty() {
            self.ages
                .lock()
                .push(format!("{hash}: {}", ages.join("; ")));
        }
        let reply = match &self.mode {
            Mode::Replay(answers) => match answers.get(&hash) {
                Some(r) => HttpReply {
                    status: 200,
                    body: r.clone(),
                    retry_after: None,
                },
                None => {
                    self.misses.lock().push(hash);
                    // A refusal, never retried: the source or memory waits, and the test fails on
                    // the miss.
                    return Ok(HttpReply {
                        status: 400,
                        body: b"{}".to_vec(),
                        retry_after: None,
                    });
                }
            },
            Mode::Live(http) => {
                if self.stopped.load(Ordering::SeqCst) {
                    return Err(TransportError::Connect(
                        "the evaluation's spend passed its stop".into(),
                    ));
                }
                let started = Instant::now();
                let r = http.post(endpoint, key, body, timeout)?;
                self.latencies_ms
                    .lock()
                    .push(started.elapsed().as_millis() as u64);
                r
            }
            Mode::Dry => HttpReply {
                status: 200,
                body: dry_answer(text, &self.items.lock()),
                retry_after: None,
            },
        };
        if reply.status == 200 {
            let mut spent = self.spent.lock();
            *spent += usage_usd(&reply.body);
            if *spent > self.stop_usd {
                self.stopped.store(true, Ordering::SeqCst);
            }
            self.answered
                .lock()
                .push((body.to_vec(), reply.body.clone()));
        }
        Ok(reply)
    }
}

/// The memory blocks in `body` (`(written <age>, kind …`) whose age is not "today", each as
/// `written <age>`. The age is the only clock that reaches a request (see the module docs).
///
/// Neither clock can be fixed from a test: the writer stamps `created_at` and the judge reads its
/// `now`, both from the real clock. So both stay real, and the age is still always "today": every
/// memory is written and then judged within one run, so `now - created_at` is under 24 h (it would
/// take a day to say "yesterday") and above -60 s (`age` reads skew below a minute as "today", not
/// "in the future"). A body that says anything else has a clock in it the replay cannot pin.
fn ages_not_today(body: &str) -> Vec<String> {
    body.match_indices("(written ")
        .map(|(i, _)| &body[i + 1..])
        .filter(|rest| !rest.starts_with("written today,"))
        .map(|rest| rest.split(',').next().unwrap_or(rest).to_string())
        .collect()
}

/// What a Jev answer says it cost: its input tokens at Jev's price (output is free).
fn usage_usd(body: &[u8]) -> f64 {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["usage"]["input_tokens"].as_u64())
        .map_or(0.0, |t| t as f64 * USD_PER_MTOK_INPUT / 1e6)
}

fn is_distill_question(id: &str) -> bool {
    id.len() > 1 && id.starts_with('c') && id[1..].chars().all(|c| c.is_ascii_digit())
}

/// The passage a distillation question quotes: `Passage <n> from this session[ (the user had
/// asked: «…»)]: «<passage>». What lasting knowledge …`.
fn quoted_passage(instructions: &str) -> &str {
    let start = ["»): «", "from this session: «"]
        .iter()
        .find_map(|m| instructions.find(m).map(|i| i + m.len()))
        .unwrap_or(0);
    let end = instructions
        .rfind("». What lasting knowledge")
        .unwrap_or(instructions.len());
    instructions.get(start..end).unwrap_or(instructions)
}

/// The dry run's Jev. A distillation passage that holds a labelled phrase of the session is kept
/// with the item's first kind, and any other passage is `ephemeral`. The judge's questions get a
/// neutral answer. The usage is an upper bound: 1.41 tokens a byte, the most any recorded request
/// showed, plus 300 for Jev's own prompt.
fn dry_answer(body: &str, items: &[Item]) -> Vec<u8> {
    let req: Value = serde_json::from_str(body).unwrap();
    let mut answers = serde_json::Map::new();
    for (qid, q) in req["questions"].as_object().unwrap() {
        let options: Vec<&str> = q["criteria"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let offered = |o: &str| options.contains(&o);
        let choice: String = if is_distill_question(qid) {
            let passage = quoted_passage(q["instructions"].as_str().unwrap_or_default());
            items
                .iter()
                .find(|i| passage.contains(&i.phrase))
                .map(|i| match i.kinds[0].as_str() {
                    "preference" => "preference_project".to_string(),
                    k => k.to_string(),
                })
                .filter(|o| offered(o))
                .unwrap_or_else(|| "ephemeral".into())
        } else {
            ["unrelated", "none", "note", "global"]
                .into_iter()
                .find(|o| offered(o))
                .unwrap_or(options[0])
                .to_string()
        };
        let probs: serde_json::Map<String, Value> = options
            .iter()
            .map(|o| (o.to_string(), json!(if *o == choice { 1.0 } else { 0.0 })))
            .collect();
        answers.insert(
            qid.clone(),
            json!({"type": "choice", "choice": choice, "confidence": 1.0, "probabilities": probs}),
        );
    }
    let tokens = (body.len() as f64 * 1.41).ceil() as u64 + 300;
    let n = answers.len();
    serde_json::to_vec(&json!({"model": req["model"], "answers": answers,
        "usage": {"input_tokens": tokens, "output_tokens": n}}))
    .unwrap()
}

/// Writes what the live run was answered to `recorded.jsonl` when dropped, so a run that stops
/// early (the spend, a failure) still keeps what it paid for. Request and response bodies only,
/// each request once, in the order they were sent.
struct Recorder {
    t: Arc<EvalTransport>,
    key: String,
    /// Where it writes: `recorded.jsonl` of the set it ran.
    path: PathBuf,
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let mut seen = HashSet::new();
        let mut text = String::new();
        for (request, response) in self.t.answered.lock().iter() {
            if !seen.insert(sha256_hex(request)) {
                continue;
            }
            let (Ok(request), Ok(response)) =
                (std::str::from_utf8(request), std::str::from_utf8(response))
            else {
                eprintln!("recorded.jsonl is not written: a body is not UTF-8");
                return;
            };
            let line = json!({"request_body": request, "response_body": response});
            text.push_str(&line.to_string());
            text.push('\n');
        }
        if text.contains(&self.key) || text.contains("Authorization") {
            eprintln!("recorded.jsonl is not written: a body holds the key or a header");
            return;
        }
        match std::fs::write(&self.path, &text) {
            Ok(()) => println!(
                "recorded.jsonl: {} requests, {} bytes",
                seen.len(),
                text.len()
            ),
            Err(e) => eprintln!("recorded.jsonl could not be written: {e}"),
        }
    }
}

// ---- The pipeline ----

/// A distilled memory, as it was left after the judge.
#[derive(Debug, Clone)]
struct Mem {
    kind: String,
    title: String,
    body: String,
    status: String,
}

impl Mem {
    /// Stored and counted: `active`. A judged distilled memory is that, `trashed` (a duplicate of
    /// an older memory) or `superseded` (a newer distilled memory updates it, Task 11); recall
    /// hides the last two. `run_session` fails on any other status, and the report counts each.
    fn live(&self) -> bool {
        self.status == "active"
    }

    /// The body without the `Context:` line that distillation adds (spec §6.5).
    fn passage(&self) -> &str {
        self.body
            .rfind("\n\nContext: ")
            .map_or(self.body.as_str(), |i| &self.body[..i])
    }

    fn matches(&self, item: &Item) -> bool {
        self.passage().contains(&item.phrase) && item.kinds.contains(&self.kind)
    }
}

/// What one session left.
struct Outcome {
    /// Every memory distillation wrote, in order, whatever the judge did with it.
    memories: Vec<Mem>,
    /// Questions the distill worker asked.
    questions: usize,
    /// The cost ledger of the session's daemon: USD and calls (the judge's and distillation's).
    spend_usd: f64,
    calls: i64,
    /// Distilled memories a better one replaced in the session's best set (trashed by
    /// `distill:cap`, Task 12c).
    cap_trashed: usize,
}

impl Outcome {
    /// The distilled memories the judge left with `status`.
    fn count(&self, status: &str) -> usize {
        self.memories.iter().filter(|m| m.status == status).count()
    }
}

fn start(home: &Path, t: &Arc<EvalTransport>, key: &str) -> Arc<Service> {
    let key = key.to_owned();
    Service::start(
        AliceHome::new(home.to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions {
            secret_env: Some(Arc::new(move |v: &str| {
                (v == "TYPESAFE_API_KEY").then(|| key.clone())
            })),
            settings_env: Some(Vec::new()),
            settings_poll: Duration::from_millis(50),
            jev_transport: Some(t.clone()),
            distill_tick: Duration::from_millis(100),
            judge_retry_every: Duration::from_millis(200),
            judge_retry_base: Duration::from_millis(200),
            ..Default::default()
        },
    )
    .unwrap()
}

fn db(svc: &Service) -> rusqlite::Connection {
    alice_core::store::open_reader(&svc.home().db()).unwrap()
}

/// Memories the judge has not settled yet. A trashed one is never judged (a distilled memory a
/// better one replaced in its session's best set, Task 12c, may be trashed before its turn).
fn unjudged(svc: &Service) -> i64 {
    db(svc)
        .query_row(
            "SELECT count(*) FROM memories WHERE judge_state <> 'done' AND status <> 'trashed'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// `(distill_state, distill_attempts)` of the source `uri`; none before it is captured.
fn source_state(svc: &Service, uri: &str) -> Option<(String, i64)> {
    db(svc)
        .query_row(
            "SELECT c.distill_state, c.distill_attempts
             FROM capture_state c JOIN sources s ON s.id = c.source_id WHERE s.uri = ?1",
            [uri],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

/// Waits until `cond` holds, failing at once on what [`EvalTransport::check`] fails on.
async fn until(t: &EvalTransport, what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let started = Instant::now();
    while !cond() {
        t.check(what);
        assert!(started.elapsed() < limit, "timed out: {what}");
        tokio::time::sleep(POLL).await;
    }
    t.check(what);
}

/// One session through its own daemon: the agent's own memories first (judged), then the
/// transcript as a session end, distilled and judged.
async fn run_session(t: &Arc<EvalTransport>, s: &Session, key: &str, limit: Duration) -> Outcome {
    *t.items.lock() = s.items.clone();
    let answered_before = t.answered.lock().len();
    let what = |step: &str| format!("{}: {step}", s.file);
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(AliceHome::new(home.clone()).config_file(), CONFIG).unwrap();
    let repo = dir.path().join(&s.repo);
    std::fs::create_dir_all(&repo).unwrap();
    let cwd = repo.to_string_lossy().into_owned();
    let svc = start(&home, t, key);

    // Jev is in force before anything is written, so no memory is judged by a stand-in first.
    let started = Instant::now();
    while svc.distill_status().await.unwrap().state != "ready" {
        assert!(
            started.elapsed() < limit,
            "{}",
            what("distillation never got ready")
        );
        tokio::time::sleep(POLL).await;
    }

    let mut project = None;
    for r in &s.remembered {
        let out = svc
            .remember(RememberRequest {
                content: r.content.clone(),
                title: r.title.clone(),
                kind: r.kind,
                cwd: Some(cwd.clone()),
                author: Author {
                    agent: Some("claude-code".into()),
                    model: None,
                    session: Some(s.id.clone()),
                },
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.status, RememberStatus::New, "{}", what("remember"));
        project = Some(out.project);
    }
    until(t, &what("the agent's memories are judged"), limit, || {
        unjudged(&svc) == 0
    })
    .await;

    let a = svc
        .capture_transcript(
            TranscriptQuery {
                agent: "claude-code".into(),
                session: s.id.clone(),
                offset: 0,
                event: "session-end".into(),
                cwd: Some(cwd),
                ..Default::default()
            },
            Bytes::from(s.bytes.clone()),
        )
        .await
        .unwrap()
        .unwrap_or_else(|c| panic!("{}: refused as behind the cursor {c}", s.file));
    assert_eq!(
        a.next_offset,
        s.bytes.len() as u64,
        "{}",
        what("the whole transcript is captured")
    );
    assert!(a.turns_stored >= 4, "{}: {a:?}", s.file);

    let uri = format!("claude-code:session:{}", s.id);
    let started = Instant::now();
    let mut asked_again = 0;
    loop {
        t.check(&what("distilled and judged"));
        let state = source_state(&svc, &uri);
        if state.as_ref().is_some_and(|(st, _)| st == "done") && unjudged(&svc) == 0 {
            break;
        }
        // A run Jev failed (four tries in a row) waits five minutes: ask for it again now.
        if let Some((st, attempts)) = &state
            && st == "due"
            && *attempts > 0
            && asked_again < 2
        {
            asked_again += 1;
            println!("{}: a run failed ({attempts}); asking again", s.file);
            svc.capture_distill_now(&s.id).await.unwrap();
        }
        assert!(
            started.elapsed() < limit,
            "{}: {state:?}",
            what("timed out waiting for the distill worker and the judge")
        );
        tokio::time::sleep(POLL).await;
    }

    // The ledger rows go to the writer apart from the judgements: wait for every answered call.
    let answered = (t.answered.lock().len() - answered_before) as i64;
    let ledger = |svc: &Service| -> (f64, i64) {
        db(svc)
            .query_row(
                "SELECT coalesce(sum(usd), 0.0), coalesce(sum(calls), 0) FROM cost_ledger",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    until(t, &what("the ledger holds every call"), limit, || {
        ledger(&svc).1 >= answered
    })
    .await;
    let (spend_usd, calls) = ledger(&svc);

    let c = db(&svc);
    let mut st = c
        .prepare(
            "SELECT m.kind, m.title, m.body, m.status, p.slug
             FROM memories m JOIN projects p ON p.id = m.project_id
             WHERE m.origin = 'distill' ORDER BY m.seq",
        )
        .unwrap();
    let rows: Vec<(Mem, String)> = st
        .query_map([], |r| {
            Ok((
                Mem {
                    kind: r.get(0)?,
                    title: r.get(1)?,
                    body: r.get(2)?,
                    status: r.get(3)?,
                },
                r.get(4)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let source_project: String = c
        .query_row(
            "SELECT p.slug FROM sources s JOIN projects p ON p.id = s.project_id WHERE s.uri = ?1",
            [&uri],
            |r| r.get(0),
        )
        .unwrap();
    if let Some(p) = project {
        assert_eq!(
            p, source_project,
            "{}: the agent's memories are in the session's project",
            s.file
        );
    }
    for (m, _) in &rows {
        assert!(
            ["active", "trashed", "superseded"].contains(&m.status.as_str()),
            "{}: the distilled {} «{}» is {}; the evaluation scores only active, trashed and superseded memories",
            s.file,
            m.kind,
            m.title,
            m.status
        );
    }
    let questions = svc
        .distill_runs()
        .iter()
        .filter(|r| r.uri == uri)
        .map(|r| r.questions)
        .sum();
    let cap_trashed: i64 = c
        .query_row(
            "SELECT count(*) FROM events WHERE action = 'memory.forget' AND actor = 'distill:cap'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(st);
    drop(c);
    let stop = svc.clone();
    tokio::task::spawn_blocking(move || stop.shutdown())
        .await
        .unwrap();
    Outcome {
        memories: rows.into_iter().map(|(m, _)| m).collect(),
        questions,
        spend_usd,
        calls,
        cap_trashed: usize::try_from(cap_trashed).unwrap_or(0),
    }
}

/// The sessions one after another (never two at once).
async fn run_all(
    t: &Arc<EvalTransport>,
    set: &[Session],
    key: &str,
    limit: Duration,
) -> Vec<Outcome> {
    let mut out = Vec::new();
    for s in set {
        let started = Instant::now();
        let o = run_session(t, s, key, limit).await;
        println!(
            "{} ({}): {} questions, {} distilled, {} live, {} trashed, {} superseded, {} Jev calls, ${:.5}, {:.1} s",
            s.file,
            s.kind,
            o.questions,
            o.memories.len(),
            o.memories.iter().filter(|m| m.live()).count(),
            o.count("trashed"),
            o.count("superseded"),
            o.calls,
            o.spend_usd,
            started.elapsed().as_secs_f64()
        );
        out.push(o);
    }
    out
}

// ---- The metrics (spec §11.3) ----

/// Counts for one kind or one language.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    /// Live distilled memories.
    stored: usize,
    /// Of them, those that match an item of their session.
    matched: usize,
    /// Gold items (of the gold sessions).
    items: usize,
    /// Of them, those a live memory matches.
    recalled: usize,
}

#[derive(Debug, Default)]
#[allow(dead_code)] // every field is shown in the assertions' messages
struct Metrics {
    /// matched / stored, over every session.
    precision: f64,
    /// recalled / gold_items.
    recall: f64,
    /// Items of the `already` sessions that a live distilled memory matches.
    doubles: usize,
    already_items: usize,
    /// Live distilled memories of the `nothing` sessions.
    flood: usize,
    /// The cost ledgers of every session's daemon.
    spend_usd: f64,
    jev_calls: i64,
    stored: usize,
    matched: usize,
    /// matched / stored over the gold sessions only.
    precision_gold: f64,
    gold_stored: usize,
    gold_matched: usize,
    gold_items: usize,
    recalled: usize,
    /// Distilled memories the judge trashed (duplicates of an older memory).
    trashed: usize,
    /// Distilled memories a newer distilled memory superseded.
    superseded: usize,
    /// Precision by the memory's kind; recall by the item's first kind.
    by_kind: BTreeMap<String, Tally>,
    by_language: BTreeMap<&'static str, Tally>,
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

fn metrics(set: &[Session], outcomes: &[Outcome]) -> Metrics {
    let mut m = Metrics::default();
    for (s, o) in set.iter().zip(outcomes) {
        m.spend_usd += o.spend_usd;
        m.jev_calls += o.calls;
        m.trashed += o.count("trashed");
        m.superseded += o.count("superseded");
        let live: Vec<&Mem> = o.memories.iter().filter(|x| x.live()).collect();
        for mem in &live {
            let hit = s.items.iter().any(|i| mem.matches(i));
            m.stored += 1;
            m.matched += usize::from(hit);
            if s.kind == "gold" {
                m.gold_stored += 1;
                m.gold_matched += usize::from(hit);
            }
            if s.kind == "nothing" {
                m.flood += 1;
            }
            for tally in [
                m.by_kind.entry(mem.kind.clone()).or_default(),
                m.by_language.entry(s.language()).or_default(),
            ] {
                tally.stored += 1;
                tally.matched += usize::from(hit);
            }
        }
        for item in &s.items {
            let found = live.iter().any(|mem| mem.matches(item));
            match s.kind.as_str() {
                "gold" => {
                    m.gold_items += 1;
                    m.recalled += usize::from(found);
                    for tally in [
                        m.by_kind.entry(item.kinds[0].clone()).or_default(),
                        m.by_language.entry(s.language()).or_default(),
                    ] {
                        tally.items += 1;
                        tally.recalled += usize::from(found);
                    }
                }
                "already" => {
                    m.already_items += 1;
                    m.doubles += usize::from(found);
                }
                other => panic!("{}: a {other} session has items", s.file),
            }
        }
    }
    m.precision = ratio(m.matched, m.stored);
    m.precision_gold = ratio(m.gold_matched, m.gold_stored);
    m.recall = ratio(m.recalled, m.gold_items);
    m
}

/// The metrics as Markdown tables, then what was missed and what matched nothing.
fn report(set: &[Session], outcomes: &[Outcome], m: &Metrics) -> String {
    let frac = |a: usize, b: usize| {
        if b == 0 {
            "–".to_string()
        } else {
            format!("{:.2} ({a}/{b})", ratio(a, b))
        }
    };
    let mut r = String::new();
    let w = &mut r;
    writeln!(w, "| Metric | Result | Threshold |\n|---|---|---|").unwrap();
    writeln!(
        w,
        "| Precision, all 30 sessions | {} | ≥ 0.80 |",
        frac(m.matched, m.stored)
    )
    .unwrap();
    writeln!(
        w,
        "| Precision, the 20 gold sessions | {} | — |",
        frac(m.gold_matched, m.gold_stored)
    )
    .unwrap();
    writeln!(
        w,
        "| Recall | {} | ≥ 0.60 |",
        frac(m.recalled, m.gold_items)
    )
    .unwrap();
    writeln!(
        w,
        "| Doubles | {} of {} items | ≤ {} |",
        m.doubles,
        m.already_items,
        m.already_items / 10
    )
    .unwrap();
    writeln!(w, "| Flood | {} | ≤ 2 |", m.flood).unwrap();
    writeln!(
        w,
        "| Spend | ${:.4} ({} Jev calls) | < $0.10 |",
        m.spend_usd, m.jev_calls
    )
    .unwrap();
    writeln!(
        w,
        "\nDistilled memories the judge trashed as duplicates: {}; superseded by a newer distilled memory: {}.",
        m.trashed,
        m.superseded
    )
    .unwrap();

    writeln!(
        w,
        "\n| Kind | Stored | Precision | Gold items | Recall |\n|---|---|---|---|---|"
    )
    .unwrap();
    for (k, t) in &m.by_kind {
        writeln!(
            w,
            "| {k} | {} | {} | {} | {} |",
            t.stored,
            frac(t.matched, t.stored),
            t.items,
            frac(t.recalled, t.items)
        )
        .unwrap();
    }
    writeln!(
        w,
        "\n| Language | Sessions | Stored | Precision | Gold items | Recall |\n|---|---|---|---|---|---|"
    )
    .unwrap();
    for (lang, t) in &m.by_language {
        let sessions = set.iter().filter(|s| s.language() == *lang).count();
        writeln!(
            w,
            "| {lang} | {sessions} | {} | {} | {} | {} |",
            t.stored,
            frac(t.matched, t.stored),
            t.items,
            frac(t.recalled, t.items)
        )
        .unwrap();
    }

    writeln!(
        w,
        "\n| Session | Set | Language | Items | Questions | Live | Trashed | Superseded | Matched | Items found |\n|---|---|---|---|---|---|---|---|---|---|"
    )
    .unwrap();
    let mut missed = Vec::new();
    let mut unmatched = Vec::new();
    for (s, o) in set.iter().zip(outcomes) {
        let live: Vec<&Mem> = o.memories.iter().filter(|x| x.live()).collect();
        let matched = live
            .iter()
            .filter(|mem| s.items.iter().any(|i| mem.matches(i)))
            .count();
        let found = s
            .items
            .iter()
            .filter(|i| live.iter().any(|mem| mem.matches(i)))
            .count();
        writeln!(
            w,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            s.file,
            s.kind,
            s.language(),
            s.items.len(),
            o.questions,
            live.len(),
            o.count("trashed"),
            o.count("superseded"),
            matched,
            found
        )
        .unwrap();
        for i in &s.items {
            if !live.iter().any(|mem| mem.matches(i)) {
                let near: Vec<&str> = live
                    .iter()
                    .filter(|mem| mem.passage().contains(&i.phrase))
                    .map(|mem| mem.kind.as_str())
                    .collect();
                let how = if near.is_empty() {
                    String::new()
                } else {
                    format!(" (stored as {})", near.join(", "))
                };
                missed.push(format!(
                    "- {} ({}) {}: «{}»{how}",
                    s.file,
                    s.kind,
                    i.kinds.join("|"),
                    i.phrase
                ));
            }
        }
        for mem in &live {
            if !s.items.iter().any(|i| mem.matches(i)) {
                unmatched.push(format!(
                    "- {} ({}) {}: {}",
                    s.file, s.kind, mem.kind, mem.title
                ));
            }
        }
    }
    writeln!(
        w,
        "\nItems found: gold items a live memory matches (in an already session, the doubles)."
    )
    .unwrap();
    writeln!(w, "\nItems no live memory matches ({}):", missed.len()).unwrap();
    for line in &missed {
        writeln!(w, "{line}").unwrap();
    }
    writeln!(
        w,
        "\nLive memories that match no item of their session ({}):",
        unmatched.len()
    )
    .unwrap();
    for line in &unmatched {
        writeln!(w, "{line}").unwrap();
    }
    r
}

fn assert_thresholds(m: &Metrics) {
    assert!(m.precision >= 0.80, "{m:?}");
    assert!(m.recall >= 0.60, "{m:?}");
    assert!(m.doubles <= m.already_items / 10, "{m:?}");
    assert!(m.flood <= 2, "{m:?}");
    assert!(m.spend_usd < SPEND_STOP_USD, "{m:?}");
}

// ---- The tests ----

/// The recorded answers replayed: the pipeline sends exactly the recorded requests (no more, no
/// fewer), and the results meet the thresholds of spec §11.3.
#[tokio::test(flavor = "multi_thread")]
async fn distill_eval_replays_the_recorded_answers() {
    let set = load_set();
    let recorded = load_recorded();
    let keys: HashSet<String> = recorded.keys().cloned().collect();
    let t = EvalTransport::new(Mode::Replay(recorded));
    let outcomes = run_all(&t, &set, FAKE_KEY, Duration::from_secs(60)).await;
    // A request sent while the last session was read or shut down is checked too.
    t.check("after the last session");
    let asked: HashSet<String> = t.hashes().into_iter().collect();
    let mut unused: Vec<&String> = keys.difference(&asked).collect();
    unused.sort();
    assert!(
        unused.is_empty(),
        "recorded answers no request asked for, so the pipeline no longer sends what \
         recorded.jsonl holds (record it again with the live run): {unused:?}"
    );
    let m = metrics(&set, &outcomes);
    println!("{}", report(&set, &outcomes, &m));
    assert_thresholds(&m);
}

/// The live evaluation (spec §11.3): the real Jev, once. With `ALICE_DISTILL_EVAL_DRY=1`, the
/// free dry check instead.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "network: the real Jev, with TYPESAFE_API_KEY; rewrites fixtures/distill/recorded.jsonl"]
async fn distill_eval_live() {
    let set = load_set();
    if std::env::var_os(DRY_ENV).is_some() {
        dry_run(&set).await;
        return;
    }
    // Never printed: an unset or non-UTF-8 value names only the variable.
    let key = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| panic!("set TYPESAFE_API_KEY (the key is never printed)"));
    let t = EvalTransport::new(Mode::Live(ReqwestTransport::new().unwrap()));
    let started = Instant::now();
    let outcomes = {
        let _recorder = Recorder {
            t: t.clone(),
            key: key.clone(),
            path: recorded_path(),
        };
        run_all(&t, &set, &key, Duration::from_secs(300)).await
    };
    t.check("after the last session");
    let m = metrics(&set, &outcomes);
    println!("{}", report(&set, &outcomes, &m));
    let mut lat = t.latencies_ms.lock().clone();
    lat.sort_unstable();
    println!(
        "Jev requests sent: {}; latency median {} ms, max {} ms; spend by Jev's usage ${:.4}, by the ledgers ${:.4}; {:.0} s in all",
        lat.len(),
        lat.get(lat.len() / 2).copied().unwrap_or(0),
        lat.last().copied().unwrap_or(0),
        *t.spent.lock(),
        m.spend_usd,
        started.elapsed().as_secs_f64()
    );
    assert_thresholds(&m);
}

/// The free check before a live run: a scripted Jev, the whole pipeline twice, and the same
/// request bodies both times. It prints the metrics the script gets (the passages it keeps are the
/// labelled ones, so its recall is what candidate extraction allows) and an upper bound on the
/// spend. Returns the second run.
async fn dry_run(set: &[Session]) -> (Arc<EvalTransport>, Vec<Outcome>) {
    let mut runs = Vec::new();
    let mut last = None;
    for n in 1..=2 {
        let t = EvalTransport::new(Mode::Dry);
        let outcomes = run_all(&t, set, FAKE_KEY, Duration::from_secs(60)).await;
        t.check("after the last session");
        let m = metrics(set, &outcomes);
        let hashes = t.hashes();
        let bytes: usize = t
            .answered
            .lock()
            .iter()
            .map(|(b, r)| b.len() + r.len())
            .sum();
        let distinct: HashSet<&String> = hashes.iter().collect();
        println!("{}", report(set, &outcomes, &m));
        println!(
            "dry run {n}: {} requests ({} distinct), {bytes} body bytes, at most ${:.4}",
            hashes.len(),
            distinct.len(),
            m.spend_usd
        );
        runs.push(hashes);
        last = Some((t, outcomes));
    }
    let same_order = runs[0] == runs[1];
    for r in &mut runs {
        r.sort();
    }
    assert_eq!(
        runs[0], runs[1],
        "the two dry runs sent different request bodies"
    );
    println!(
        "dry runs: the same {} request bodies both times (same order: {same_order})",
        runs[0].len()
    );
    last.expect("two runs")
}

/// Task 12b review, M9: the live runs' spend stop is `ALICE_DISTILL_EVAL_STOP_USD` when it is a
/// positive number, else $0.10.
#[test]
fn the_live_spend_stop_reads_its_variable() {
    assert_eq!(spend_stop(None), 0.10);
    assert_eq!(spend_stop(Some("0.06")), 0.06);
    assert_eq!(spend_stop(Some(" 0.035 ")), 0.035);
    for bad in ["", "abc", "0", "-1", "NaN", "inf"] {
        assert_eq!(spend_stop(Some(bad)), 0.10, "{bad}");
    }
}

// ---- The flood set (Task 12b) ----

/// What Jev answered about each verdict of the flood set that it was asked about: the verdict's
/// opening words and the option it found most probable.
fn verdict_answers(t: &EvalTransport, s: &Session) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (request, response) in t.answered.lock().iter() {
        let (Ok(req), Ok(resp)) = (
            serde_json::from_slice::<Value>(request),
            serde_json::from_slice::<Value>(response),
        ) else {
            continue;
        };
        for (qid, q) in req["questions"].as_object().into_iter().flatten() {
            if !is_distill_question(qid) {
                continue;
            }
            let passage = quoted_passage(q["instructions"].as_str().unwrap_or_default());
            let Some(v) = s.verdicts.iter().find(|v| passage.contains(v.as_str())) else {
                continue;
            };
            let best = resp["answers"][qid]["probabilities"]
                .as_object()
                .and_then(|p| {
                    p.iter()
                        .max_by(|a, b| {
                            let p = |x: &Value| x.as_f64().unwrap_or(0.0);
                            p(a.1).total_cmp(&p(b.1))
                        })
                        .map(|(o, _)| o.clone())
                })
                .unwrap_or_default();
            out.push((v.clone(), best));
        }
    }
    out
}

/// The flood set's thresholds (Task 12b, declared before any run): nearly all of the twenty
/// verdicts reach Jev; most of them end `ephemeral`; at most five are kept, as in the real-session
/// check; and the three lessons are kept, the one stated inside a reviewer's verdict too.
fn assert_flood(set: &[Session], outcomes: &[Outcome], t: &EvalTransport) {
    let (s, o) = (&set[0], &outcomes[0]);
    let answers = verdict_answers(t, s);
    let asked: HashSet<&str> = answers.iter().map(|(v, _)| v.as_str()).collect();
    let ephemeral = answers.iter().filter(|(_, a)| a == "ephemeral").count();
    let live: Vec<&Mem> = o.memories.iter().filter(|m| m.live()).collect();
    let kept_verdicts: Vec<&&Mem> = live
        .iter()
        .filter(|m| s.verdicts.iter().any(|v| m.passage().contains(v.as_str())))
        .collect();
    println!(
        "flood: {} of {} verdicts asked, {ephemeral} answered ephemeral, {} kept; {} live memories, {} questions",
        asked.len(),
        s.verdicts.len(),
        kept_verdicts.len(),
        live.len(),
        o.questions
    );
    for (v, a) in &answers {
        println!("- {a}: {v}");
    }
    for m in &live {
        println!("+ {} «{}»", m.kind, m.title);
    }
    assert!(
        asked.len() + 2 >= s.verdicts.len(),
        "the verdicts reach Jev: {answers:?}"
    );
    assert!(
        ephemeral * 2 > answers.len(),
        "most verdicts end ephemeral: {answers:?}"
    );
    assert!(kept_verdicts.len() <= 5, "{kept_verdicts:#?}");
    for item in &s.items {
        assert!(
            live.iter().any(|m| m.matches(item)),
            "the lesson «{}» ({}) is kept: {live:#?}",
            item.phrase,
            item.kinds.join("|")
        );
    }
}

/// The flood set replayed (Task 12b): one session whose plan ran through twenty reviewer
/// subagents, each answering with a one-task verdict or status report, next to three lessons.
/// Every request is answered from `fixtures/distill/flood/recorded.jsonl`, as in
/// [`distill_eval_replays_the_recorded_answers`], and the flood's thresholds are asserted. Task
/// 13's set and thresholds are not touched. Until the live run records the file, every request
/// misses and the test fails on that alone (ruling F8).
#[tokio::test(flavor = "multi_thread")]
async fn distill_eval_flood_replay() {
    let set = load_set_in(&flood_dir());
    let recorded = load_recorded_from(&flood_dir().join("recorded.jsonl"));
    let keys: HashSet<String> = recorded.keys().cloned().collect();
    let t = EvalTransport::new(Mode::Replay(recorded));
    let outcomes = run_all(&t, &set, FAKE_KEY, Duration::from_secs(60)).await;
    t.check("after the flood session");
    let asked: HashSet<String> = t.hashes().into_iter().collect();
    let mut unused: Vec<&String> = keys.difference(&asked).collect();
    unused.sort();
    assert!(
        unused.is_empty(),
        "recorded answers no request asked for, so the pipeline no longer sends what \
         flood/recorded.jsonl holds (record it again with the live run): {unused:?}"
    );
    assert_flood(&set, &outcomes, &t);
}

/// The flood set with the real Jev, once (Task 12b): it writes `fixtures/distill/flood/recorded.jsonl`
/// and stops once its spend passes $0.10, or `ALICE_DISTILL_EVAL_STOP_USD`:
/// `cargo test -p alice-daemon --test distill_eval distill_eval_flood_live -- --ignored --nocapture`.
/// With `ALICE_DISTILL_EVAL_DRY=1`, the free dry check instead.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "network: the real Jev, with TYPESAFE_API_KEY; rewrites fixtures/distill/flood/recorded.jsonl"]
async fn distill_eval_flood_live() {
    let set = load_set_in(&flood_dir());
    if std::env::var_os(DRY_ENV).is_some() {
        let (t, outcomes) = dry_run(&set).await;
        assert_flood(&set, &outcomes, &t);
        return;
    }
    // Never printed: an unset or non-UTF-8 value names only the variable.
    let key = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| panic!("set TYPESAFE_API_KEY (the key is never printed)"));
    let t = EvalTransport::new(Mode::Live(ReqwestTransport::new().unwrap()));
    let outcomes = {
        let _recorder = Recorder {
            t: t.clone(),
            key: key.clone(),
            path: flood_dir().join("recorded.jsonl"),
        };
        run_all(&t, &set, &key, Duration::from_secs(300)).await
    };
    t.check("after the flood session");
    println!(
        "Jev requests sent: {}; spend by Jev's usage ${:.4}",
        t.latencies_ms.lock().len(),
        *t.spent.lock()
    );
    assert_flood(&set, &outcomes, &t);
}

// ---- The long-session set (Task 12c) ----

/// The distillation questions of every answered request: one list of quoted passages per
/// request.
fn asked_passages(t: &EvalTransport) -> Vec<Vec<String>> {
    t.answered
        .lock()
        .iter()
        .filter_map(|(request, _)| serde_json::from_slice::<Value>(request).ok())
        .map(|req| {
            req["questions"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(qid, _)| is_distill_question(qid))
                .map(|(_, q)| {
                    quoted_passage(q["instructions"].as_str().unwrap_or_default()).to_owned()
                })
                .collect::<Vec<_>>()
        })
        .filter(|q| !q.is_empty())
        .collect()
}

/// The long-session set's checks (Task 12c, declared before any run; §11.3's set and thresholds
/// are not touched): the session is asked about in every run (three); its best set never holds
/// more than `distill.max_per_session` (16); the late compaction summary's sections are asked
/// about. Its own figures are printed: kept, the late items kept, replaced (`distill:cap`), and
/// the verdict or status lines kept.
fn assert_long(set: &[Session], outcomes: &[Outcome], t: &EvalTransport) {
    let (s, o) = (&set[0], &outcomes[0]);
    let live: Vec<&Mem> = o.memories.iter().filter(|m| m.live()).collect();
    let late: Vec<&Item> = s.items.iter().filter(|i| i.late).collect();
    let late_kept = late
        .iter()
        .filter(|i| live.iter().any(|m| m.matches(i)))
        .count();
    let early_kept = s
        .items
        .iter()
        .filter(|i| !i.late && live.iter().any(|m| m.matches(i)))
        .count();
    let verdicts_kept = live
        .iter()
        .filter(|m| s.verdicts.iter().any(|v| m.passage().contains(v.as_str())))
        .count();
    let requests = asked_passages(t);
    println!(
        "long session: {} requests, {} questions; kept {} (of {} distilled); late items kept {late_kept} of {}, early {early_kept} of {}; replaced (distill:cap) {}; verdicts kept {verdicts_kept} of {}",
        requests.len(),
        o.questions,
        live.len(),
        o.memories.len(),
        late.len(),
        s.items.len() - late.len(),
        o.cap_trashed,
        s.verdicts.len()
    );
    for m in &live {
        println!("+ {} «{}»", m.kind, m.title);
    }
    assert_eq!(
        requests.len(),
        3,
        "one request per run, a full session's too"
    );
    assert!(live.len() <= 16, "the best set's size: {}", live.len());
    for phrase in [
        "sends a comment line every 15 seconds so the proxy flushes",
        "Weather alerts use the met office's warnings API, not scraping",
        "writes a draft first and commits only on save",
        "report their status over MQTT on topic cranes/<id>/status",
    ] {
        assert!(
            requests.iter().flatten().any(|q| q.contains(phrase)),
            "the late summary's section is asked about: {phrase}"
        );
    }
}

/// The long-session set replayed (Task 12c, review M4): a session long enough to fill its best
/// set (three runs), with lasting knowledge early, in the middle and late (a late compaction
/// summary), and status lines in between. Every request is answered from
/// `fixtures/distill/long/recorded.jsonl`, and the set's checks run. Until the live run records
/// the file, every request misses and the test fails on that alone.
#[tokio::test(flavor = "multi_thread")]
async fn distill_eval_long_session_replay() {
    let set = load_set_in(&long_dir());
    let recorded = load_recorded_from(&long_dir().join("recorded.jsonl"));
    let keys: HashSet<String> = recorded.keys().cloned().collect();
    let t = EvalTransport::new(Mode::Replay(recorded));
    let outcomes = run_all(&t, &set, FAKE_KEY, Duration::from_secs(60)).await;
    t.check("after the long session");
    let asked: HashSet<String> = t.hashes().into_iter().collect();
    let mut unused: Vec<&String> = keys.difference(&asked).collect();
    unused.sort();
    assert!(
        unused.is_empty(),
        "recorded answers no request asked for, so the pipeline no longer sends what \
         long/recorded.jsonl holds (record it again with the live run): {unused:?}"
    );
    assert_long(&set, &outcomes, &t);
}

/// The long-session set with the real Jev, once (Task 12c): it writes
/// `fixtures/distill/long/recorded.jsonl` and stops once its spend passes $0.10, or
/// `ALICE_DISTILL_EVAL_STOP_USD`:
/// `cargo test -p alice-daemon --test distill_eval distill_eval_long_session_live -- --ignored --nocapture`.
/// With `ALICE_DISTILL_EVAL_DRY=1`, the free dry check instead.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "network: the real Jev, with TYPESAFE_API_KEY; rewrites fixtures/distill/long/recorded.jsonl"]
async fn distill_eval_long_session_live() {
    let set = load_set_in(&long_dir());
    if std::env::var_os(DRY_ENV).is_some() {
        let (t, outcomes) = dry_run(&set).await;
        assert_long(&set, &outcomes, &t);
        return;
    }
    // Never printed: an unset or non-UTF-8 value names only the variable.
    let key = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| panic!("set TYPESAFE_API_KEY (the key is never printed)"));
    let t = EvalTransport::new(Mode::Live(ReqwestTransport::new().unwrap()));
    let outcomes = {
        let _recorder = Recorder {
            t: t.clone(),
            key: key.clone(),
            path: long_dir().join("recorded.jsonl"),
        };
        run_all(&t, &set, &key, Duration::from_secs(300)).await
    };
    t.check("after the long session");
    println!(
        "Jev requests sent: {}; spend by Jev's usage ${:.4}",
        t.latencies_ms.lock().len(),
        *t.spent.lock()
    );
    assert_long(&set, &outcomes, &t);
}
