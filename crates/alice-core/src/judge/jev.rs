//! TypeSafe Jev (spec §5.2): choice questions about one state, answered with probabilities.
//!
//! Every request: the state is redacted first (with the rules of every project it covers);
//! metadata is in words (`verbal`); each question's options are shuffled with a seed derived
//! from the request itself (a repeated decision builds the same body, so the SHA-256 cache
//! answers it; different decisions see different orders); decisions that may supersede ask
//! forward and reversed questions in one request and average them. Calls are spaced to
//! `rate_per_sec`, retried only on 429, 5xx and timeouts, and refused once the UTC day's spend
//! (reserved before the call, settled after) would pass the budget.
//!
//! One policy decides whether a project's text may leave the machine: [`JevPolicy`], asked once
//! per call for every project the call covers. The daemon's implementation reads the live
//! settings and the project's `.alice.toml`; this module keeps no denylist of its own.

use super::verbal::{kind_state, memory_block};
use super::{
    Choice, Distribution, Judge, JudgeError, JudgeMemory, JudgeResult, ProjectOption, Relation,
    Rubric, Score, kind_choices, predicate_question, relation_choices, relevance_choices,
    reverse_relation_choices,
};
use crate::normalize::sha256_hex;
use crate::redact::Redactor;
use crate::settings::{JudgeSettings, SecretValue};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// USD per million input tokens (output is free).
pub const USD_PER_MTOK_INPUT: f64 = 0.042;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
/// Input tokens Jev adds to every request (its own prompt), as a safe upper bound. The recorded
/// requests show 231 bytes costing 325 tokens; with `START_TOKEN_RATIO` below, a 231-byte
/// request is reserved at 646 tokens.
pub const REQUEST_OVERHEAD_TOKENS: f64 = 300.0;
/// Input tokens per request byte before any reply has shown a higher one. The recorded
/// requests range from 0.37 to 1.41 tokens a byte.
pub const START_TOKEN_RATIO: f64 = 1.5;
/// The project that text belonging to no project in particular is attributed to.
pub const UNSCOPED: &str = "global";

#[derive(Debug, Clone)]
pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    /// Nothing reached Jev (the connection could not be made).
    Connect(String),
    /// The request may have reached Jev but its reply did not come back whole.
    Lost(String),
}

pub trait JevTransport: Send + Sync {
    /// POSTs `body` to `endpoint` with `key` as the bearer token. `key` is only for the
    /// `Authorization` header: an implementation never logs or stores it.
    fn post(
        &self,
        endpoint: &str,
        key: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpReply, TransportError>;
}

/// Decides, per call, whether text of a project may be sent to Jev and which rules redact it.
/// The daemon injects an implementation over its live settings; nothing in this module keeps
/// a copy of a denylist.
pub trait JevPolicy: Send + Sync {
    /// May text of `project` (a slug) leave the machine? Asked once per call, for every project
    /// the call covers; an empty or unknown slug must answer `false`.
    fn cloud_allowed(&self, project: &str) -> bool;
    /// The redactor for text of `project`, always switched on: judge requests are redacted,
    /// never refused.
    fn redactor(&self, project: &str) -> Arc<Redactor>;
}

/// A fixed policy: a list of denied projects and the redactors to use. For tests, the
/// evaluation and the live checks; the daemon uses its own.
pub struct FixedPolicy {
    denied: Vec<String>,
    default: Arc<Redactor>,
    per_project: BTreeMap<String, Arc<Redactor>>,
}

impl FixedPolicy {
    pub fn allow_all(redactor: Redactor) -> Self {
        Self {
            denied: Vec::new(),
            default: Arc::new(redactor),
            per_project: BTreeMap::new(),
        }
    }

    pub fn denying(mut self, projects: &[&str]) -> Self {
        self.denied = projects.iter().map(|p| p.to_string()).collect();
        self
    }

    pub fn with_redactor(mut self, project: &str, redactor: Redactor) -> Self {
        self.per_project
            .insert(project.to_string(), Arc::new(redactor));
        self
    }
}

impl JevPolicy for FixedPolicy {
    fn cloud_allowed(&self, project: &str) -> bool {
        !project.trim().is_empty() && !self.denied.iter().any(|d| d == project)
    }

    fn redactor(&self, project: &str) -> Arc<Redactor> {
        self.per_project
            .get(project)
            .unwrap_or(&self.default)
            .clone()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevCall {
    pub day: String,
    pub hash: String,
    pub model: String,
    pub response_json: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
    pub latency_ms: u64,
    /// `false` for a call that was sent but has no usable answer: it is billed in the ledger and
    /// its response is not put in the cache.
    pub cacheable: bool,
}

pub trait JevStore: Send + Sync {
    fn cached(&self, hash: &str) -> Option<String>;
    /// USD spent on `day`; `Err` when it cannot be read (the judge then fails closed).
    fn spent(&self, day: &str) -> Result<f64, String>;
    fn record(&self, call: &JevCall);
}

/// In-memory store for tests and the evaluation.
#[derive(Default)]
pub struct MemoryJevStore {
    cache: Mutex<BTreeMap<String, String>>,
    calls: Mutex<Vec<JevCall>>,
}

impl MemoryJevStore {
    pub fn calls(&self) -> Vec<JevCall> {
        self.calls.lock().clone()
    }
}

impl JevStore for MemoryJevStore {
    fn cached(&self, hash: &str) -> Option<String> {
        self.cache.lock().get(hash).cloned()
    }
    fn spent(&self, day: &str) -> Result<f64, String> {
        Ok(self
            .calls
            .lock()
            .iter()
            .filter(|c| c.day == day)
            .map(|c| c.usd)
            .sum())
    }
    fn record(&self, call: &JevCall) {
        if call.cacheable {
            self.cache
                .lock()
                .insert(call.hash.clone(), call.response_json.clone());
        }
        self.calls.lock().push(call.clone());
    }
}

/// Everything a `JevJudge` needs to talk to Jev. `Debug` never shows the key.
#[derive(Debug, Clone)]
pub struct JevConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: SecretValue,
    pub rate_per_sec: f64,
    pub timeout: Duration,
    pub daily_budget_usd: f64,
    pub max_retries: u32,
}

impl JevConfig {
    pub fn from_settings(j: &JudgeSettings, api_key: SecretValue) -> Self {
        Self {
            endpoint: j.jev.endpoint.clone(),
            model: j.jev.model.clone(),
            api_key,
            rate_per_sec: j.jev.rate_per_sec,
            timeout: Duration::from_millis(j.timeout_ms),
            daily_budget_usd: j.daily_budget_usd,
            max_retries: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub instructions: String,
    pub options: Vec<Choice>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct JevStats {
    pub calls: u64,
    pub cache_hits: u64,
    pub retries: u64,
    pub failures: u64,
    pub budget_stops: u64,
    /// Calls that were not sent because a project they cover may not go to the cloud.
    pub denied: u64,
    pub last_error: Option<String>,
    /// Latency of the last (at most 1,000) successful calls, in ms.
    pub latencies_ms: Vec<u64>,
}

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
pub type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

struct Budget {
    day: String,
    /// The day's recorded spend plus what calls in flight reserved.
    spent: f64,
}

/// What every `JevJudge` of a daemon shares, however often the judge is rebuilt: the day's
/// reservations, the learned tokens-per-byte ratio and the request rate slot. A rebuilt judge
/// with fresh copies of these could overshoot the budget or the rate while the old one still
/// has calls in flight.
pub struct JevLimits {
    next_slot: Mutex<Option<Instant>>,
    budget: Mutex<Option<Budget>>,
    /// Input tokens per request byte (raised if Jev ever counts more than it has so far).
    token_ratio: Mutex<f64>,
    /// Why today's spend could not be read (calls are refused meanwhile); `Judge::warning`.
    spend_error: Mutex<Option<String>>,
    /// The UTC day the budget stopped calls on, and why; `Judge::warning` while it is that day.
    budget_stop: Mutex<Option<(String, String)>>,
}

impl Default for JevLimits {
    fn default() -> Self {
        Self {
            next_slot: Mutex::new(None),
            budget: Mutex::new(None),
            token_ratio: Mutex::new(START_TOKEN_RATIO),
            spend_error: Mutex::new(None),
            budget_stop: Mutex::new(None),
        }
    }
}

pub struct JevJudge {
    cfg: JevConfig,
    transport: Arc<dyn JevTransport>,
    store: Arc<dyn JevStore>,
    policy: Arc<dyn JevPolicy>,
    clock: Clock,
    sleep: Sleeper,
    limits: Arc<JevLimits>,
    last_request: Mutex<Option<Vec<u8>>>,
    stats: Mutex<JevStats>,
}

impl std::fmt::Debug for JevJudge {
    /// The configuration without the key, and the counters.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevJudge")
            .field("cfg", &self.cfg)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// `criteria` keeps the (shuffled) order of its options when serialised.
struct Criteria(Vec<(String, String)>);

impl Serialize for Criteria {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

#[derive(Serialize)]
struct QuestionBody<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'a str,
    criteria: Criteria,
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    state: &'a str,
    questions: BTreeMap<&'a str, QuestionBody<'a>>,
}

#[derive(Deserialize)]
struct ResponseBody {
    answers: BTreeMap<String, AnswerBody>,
    #[serde(default)]
    usage: UsageBody,
}

#[derive(Deserialize)]
struct AnswerBody {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    probabilities: BTreeMap<String, f64>,
}

#[derive(Deserialize, Default)]
struct UsageBody {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

fn seed(parts: &[&str]) -> u64 {
    let h = sha256_hex(parts.join("\u{1f}").as_bytes());
    u64::from_str_radix(&h[..16], 16).unwrap_or(0x9e37_79b9_7f4a_7c15) | 1
}

/// Fisher–Yates with xorshift64, seeded per question.
fn shuffled(options: &[Choice], mut s: u64) -> Vec<Choice> {
    let mut v = options.to_vec();
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        v.swap(i, (s % (i as u64 + 1)) as usize);
    }
    v
}

/// The cache key: the provider, the model and the exact bytes sent. The body holds the
/// redacted state and every question (ids, instructions, option ids and descriptions) in their
/// shuffled order, so two requests that could get different answers never share an entry.
fn request_hash(model: &str, body: &[u8]) -> String {
    let mut input = format!("jev\u{1f}{model}\u{1f}").into_bytes();
    input.extend_from_slice(body);
    sha256_hex(&input)
}

fn parse(
    body: &[u8],
    questions: &[Question],
) -> JudgeResult<(BTreeMap<String, Distribution>, UsageBody)> {
    let r: ResponseBody = serde_json::from_slice(body).map_err(|_| {
        // A fixed reason: the parser's message may quote what the server sent.
        JudgeError::Failed("Jev answered with a body Alice could not read".into())
    })?;
    let mut out = BTreeMap::new();
    for q in questions {
        let a = r
            .answers
            .get(&q.id)
            .ok_or_else(|| JudgeError::Failed(format!("Jev answered without question {}", q.id)))?;
        let known = |k: &str| q.options.iter().any(|o| o.id == k);
        let mut d = Distribution::from_pairs(
            a.probabilities
                .iter()
                .filter(|(k, _)| known(k))
                .map(|(k, v)| (k.clone(), *v)),
        );
        if d.0.is_empty()
            && let Some(c) = a.choice.as_deref().filter(|c| known(c))
        {
            d = Distribution::certain(c);
        }
        if d.0.is_empty() {
            return Err(JudgeError::Failed(format!(
                "Jev's answer to {} names none of the options",
                q.id
            )));
        }
        out.insert(q.id.clone(), d);
    }
    Ok((out, r.usage))
}

/// `(input, output)` tokens from a reply that may not otherwise be usable; `None` when it
/// reports none.
fn loose_usage(body: &[u8]) -> Option<(u64, u64)> {
    #[derive(Deserialize)]
    struct Loose {
        usage: Option<UsageBody>,
    }
    let u = serde_json::from_slice::<Loose>(body).ok()?.usage?;
    (u.input_tokens > 0).then_some((u.input_tokens, u.output_tokens))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.min(5)))
}

/// Requests per second, kept in 0.1..=10 (a value that is not a number counts as 1).
fn rate(r: f64) -> f64 {
    if r.is_finite() && r > 0.0 {
        r.clamp(0.1, 10.0)
    } else {
        1.0
    }
}

/// Distinct project slugs, in order of first appearance.
fn distinct<'a>(it: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut v: Vec<&str> = Vec::new();
    for p in it {
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

impl JevJudge {
    pub fn new(
        cfg: JevConfig,
        transport: Arc<dyn JevTransport>,
        store: Arc<dyn JevStore>,
        policy: Arc<dyn JevPolicy>,
    ) -> Self {
        Self {
            cfg,
            transport,
            store,
            policy,
            clock: Arc::new(Utc::now),
            sleep: Arc::new(std::thread::sleep),
            limits: Arc::new(JevLimits::default()),
            last_request: Mutex::new(None),
            stats: Mutex::new(JevStats::default()),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_sleeper(mut self, sleep: Sleeper) -> Self {
        self.sleep = sleep;
        self
    }

    /// Share `limits` with other judges (the daemon passes the same one to every rebuild).
    pub fn with_limits(mut self, limits: Arc<JevLimits>) -> Self {
        self.limits = limits;
        self
    }

    /// The input tokens reserved for a request of `body_len` bytes: an upper bound on what Jev
    /// bills, because of the fixed overhead and a ratio that only ever rises.
    pub fn estimate_tokens(&self, body_len: usize) -> f64 {
        body_len as f64 * *self.limits.token_ratio.lock() + REQUEST_OVERHEAD_TOKENS
    }

    /// The USD reserved for a request of `body_len` bytes.
    pub fn estimate_usd(&self, body_len: usize) -> f64 {
        self.estimate_tokens(body_len) * USD_PER_MTOK_INPUT / 1e6
    }

    pub fn stats(&self) -> JevStats {
        self.stats.lock().clone()
    }

    /// The body of the most recent request built (sent or answered from the cache).
    pub fn last_request(&self) -> Option<Vec<u8>> {
        self.last_request.lock().clone()
    }

    /// The one cloud gate of a call: every project it covers must be allowed by the policy.
    /// Nothing is built or sent for a call that fails it.
    fn gate(&self, projects: &[&str]) -> JudgeResult<()> {
        let allowed = !projects.is_empty() && projects.iter().all(|p| self.policy.cloud_allowed(p));
        if allowed {
            Ok(())
        } else {
            self.stats.lock().denied += 1;
            Err(JudgeError::Denied)
        }
    }

    /// The exact bytes `ask` would send for this (unredacted) state, covering `projects`: the
    /// state, the instructions and the option descriptions are redacted with the rules of
    /// every one of those projects first.
    pub fn request_body(&self, projects: &[&str], state: &str, questions: &[Question]) -> Vec<u8> {
        let redactors: Vec<Arc<Redactor>> = distinct(projects.iter().copied())
            .into_iter()
            .map(|p| self.policy.redactor(p))
            .collect();
        let redact = |text: &str| -> String {
            let mut out = text.to_string();
            for r in &redactors {
                out = r.apply(&out).text;
            }
            out
        };
        let state = redact(state);
        let questions: Vec<Question> = questions
            .iter()
            .map(|q| Question {
                id: q.id.clone(),
                instructions: redact(&q.instructions),
                options: q
                    .options
                    .iter()
                    .map(|o| Choice {
                        id: o.id.clone(),
                        description: redact(&o.description),
                    })
                    .collect(),
            })
            .collect();
        let mut parts: Vec<&str> = vec![&self.cfg.model, &state];
        for q in &questions {
            parts.push(&q.id);
            parts.push(&q.instructions);
            for o in &q.options {
                parts.push(&o.id);
                parts.push(&o.description);
            }
        }
        let base = seed(&parts).to_string();
        let shuffled_options: Vec<Vec<Choice>> = questions
            .iter()
            .map(|q| shuffled(&q.options, seed(&[&base, &q.id])))
            .collect();
        let body = RequestBody {
            model: &self.cfg.model,
            state: &state,
            questions: questions
                .iter()
                .zip(&shuffled_options)
                .map(|(q, opts)| {
                    (
                        q.id.as_str(),
                        QuestionBody {
                            kind: "choice",
                            instructions: &q.instructions,
                            criteria: Criteria(
                                opts.iter()
                                    .map(|o| (o.id.clone(), o.description.clone()))
                                    .collect(),
                            ),
                        },
                    )
                })
                .collect(),
        };
        serde_json::to_vec(&body).expect("request serialises")
    }

    fn wait_for_slot(&self) {
        let gap = Duration::from_secs_f64(1.0 / rate(self.cfg.rate_per_sec));
        let wait = {
            let mut next = self.limits.next_slot.lock();
            let now = Instant::now();
            let slot = next.map_or(now, |n| n.max(now));
            *next = Some(slot + gap);
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            (self.sleep)(wait);
        }
    }

    fn today(&self) -> String {
        self.now().format("%Y-%m-%d").to_string()
    }

    /// Reserve `estimate` USD of today's budget, or refuse. Failing closed: a spend that cannot
    /// be read, a budget that is not a number and a reservation that does not fit all refuse.
    fn reserve(&self, day: &str, estimate: f64) -> JudgeResult<()> {
        let mut b = self.limits.budget.lock();
        if b.as_ref().is_none_or(|b| b.day != day) {
            // A spend that cannot be read counts as an exhausted budget (it is read again on
            // the next call, and the warning shows in health until then).
            match self.store.spent(day) {
                Ok(spent) if spent.is_finite() => {
                    *self.limits.spend_error.lock() = None;
                    *b = Some(Budget {
                        day: day.into(),
                        spent,
                    });
                }
                other => {
                    let why = match other {
                        Err(e) => e,
                        Ok(_) => "the stored amount is not a number".into(),
                    };
                    let msg = format!(
                        "cannot read today's Jev spend ({why}); Jev calls are paused until it can be read"
                    );
                    *b = None;
                    *self.limits.spend_error.lock() = Some(msg.clone());
                    self.stats.lock().budget_stops += 1;
                    return Err(JudgeError::Budget(msg));
                }
            }
        }
        let b = b.as_mut().expect("set above");
        // `NaN` on either side fails the comparison, so it refuses.
        let fits = b.spent + estimate <= self.cfg.daily_budget_usd;
        if !fits {
            let msg = format!(
                "${:.4} of ${:.2} spent on {day}; raise judge.daily_budget_usd or wait for the next UTC day",
                b.spent, self.cfg.daily_budget_usd
            );
            self.stats.lock().budget_stops += 1;
            *self.limits.budget_stop.lock() =
                Some((day.into(), format!("Jev is paused for today: {msg}")));
            return Err(JudgeError::Budget(msg));
        }
        *self.limits.budget_stop.lock() = None;
        b.spent += estimate;
        Ok(())
    }

    /// Replace the reservation of `estimate` with what the call really cost.
    fn settle(&self, day: &str, estimate: f64, actual: f64) {
        if let Some(b) = self.limits.budget.lock().as_mut()
            && b.day == day
        {
            b.spent += actual - estimate;
        }
    }

    /// Ask `questions` about `state`, which covers text of `projects` (every one of them must
    /// be allowed to the cloud). Answers come from the cache when the same request was
    /// answered before.
    pub fn ask(
        &self,
        projects: &[&str],
        state: &str,
        questions: &[Question],
    ) -> JudgeResult<BTreeMap<String, Distribution>> {
        self.gate(projects)?;
        let body = self.request_body(projects, state, questions);
        *self.last_request.lock() = Some(body.clone());
        let hash = request_hash(&self.cfg.model, &body);
        if let Some(cached) = self.store.cached(&hash)
            && let Ok((d, _)) = parse(cached.as_bytes(), questions)
        {
            self.stats.lock().cache_hits += 1;
            return Ok(d);
        }
        let day = self.today();
        // An upper bound on the cost: the request's bytes at the highest tokens-per-byte seen
        // so far (it starts above every recorded request and only rises) plus the fixed
        // overhead Jev adds to every request. The reservation is shared by all judges of the
        // daemon, so the day's spend never passes the budget, rebuilds included.
        let estimate = self.estimate_usd(body.len());
        self.reserve(&day, estimate)?;
        // `reserved` is held for the attempt in flight. A timed-out attempt may have been billed:
        // its estimate turns into `charged` (it stays in the day's spend) and a fresh estimate is
        // reserved for the next attempt, so the spend never under-counts.
        let mut reserved = estimate;
        let mut charged = 0.0;
        let mut last = String::new();
        for attempt in 0..=self.cfg.max_retries {
            if attempt > 0 {
                // A retry can come up to two minutes after the first try: the policy is asked
                // again, so a denial that lands meanwhile stops it.
                if let Err(e) = self.gate(projects) {
                    self.settle_unanswered(&day, &hash, reserved, charged, false);
                    return Err(e);
                }
            }
            self.wait_for_slot();
            let started = Instant::now();
            let reply = self.transport.post(
                &self.cfg.endpoint,
                self.cfg.api_key.expose(),
                &body,
                self.cfg.timeout,
            );
            let delay = match reply {
                Ok(r) if r.status == 200 => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    let (d, usage) = match parse(&r.body, questions) {
                        Ok(x) => x,
                        Err(e) => {
                            // Delivered and answered, but not in a form we can use. Whatever the
                            // usage says (or the reservation, when it says nothing) is spent.
                            let tokens = loose_usage(&r.body);
                            let usd =
                                tokens.map_or(reserved, |t| t.0 as f64 * USD_PER_MTOK_INPUT / 1e6);
                            self.settle(&day, reserved, usd);
                            self.store.record(&JevCall {
                                day: day.clone(),
                                hash: hash.clone(),
                                model: self.cfg.model.clone(),
                                response_json: String::new(),
                                input_tokens: tokens.map_or(0, |t| t.0),
                                output_tokens: tokens.map_or(0, |t| t.1),
                                usd: usd + charged,
                                latency_ms,
                                cacheable: false,
                            });
                            self.fail(&e);
                            return Err(e);
                        }
                    };
                    // No usage reported: the real cost is unknown, so the reservation is it.
                    let usd = if usage.input_tokens > 0 {
                        usage.input_tokens as f64 * USD_PER_MTOK_INPUT / 1e6
                    } else {
                        reserved
                    };
                    self.settle(&day, reserved, usd);
                    let ratio = usage.input_tokens as f64 / body.len().max(1) as f64;
                    {
                        let mut r = self.limits.token_ratio.lock();
                        if ratio > *r {
                            tracing::warn!(
                                ratio,
                                "Jev counted more input tokens per request byte than before; budget estimates scale up"
                            );
                            *r = ratio;
                        }
                    }
                    self.store.record(&JevCall {
                        day: day.clone(),
                        hash: hash.clone(),
                        model: self.cfg.model.clone(),
                        response_json: String::from_utf8_lossy(&r.body).into_owned(),
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        usd: usd + charged,
                        latency_ms,
                        cacheable: true,
                    });
                    let mut s = self.stats.lock();
                    s.calls += 1;
                    if s.latencies_ms.len() == 1_000 {
                        s.latencies_ms.remove(0);
                    }
                    s.latencies_ms.push(latency_ms);
                    return Ok(d);
                }
                Ok(r) if r.status == 401 || r.status == 403 => {
                    self.settle_unanswered(&day, &hash, reserved, charged, false);
                    let e = JudgeError::Unavailable(format!(
                        "TypeSafe refused the API key ({}); set a valid one with `alice config set-secret jev_api_key`",
                        r.status
                    ));
                    self.fail(&e);
                    return Err(e);
                }
                Ok(r) if r.status == 429 || r.status >= 500 => {
                    last = format!("Jev answered {}", r.status);
                    r.retry_after.unwrap_or_else(|| backoff(attempt))
                }
                Ok(r) => {
                    self.settle_unanswered(&day, &hash, reserved, charged, false);
                    // Only the status: a server's explanation may quote the request.
                    let e = JudgeError::Failed(format!(
                        "Jev refused the request ({}); check judge.jev.model and judge.jev.endpoint",
                        r.status
                    ));
                    self.fail(&e);
                    return Err(e);
                }
                Err(TransportError::Timeout) => {
                    charged += reserved;
                    reserved = 0.0;
                    last = "Jev did not answer in time".into();
                    if attempt < self.cfg.max_retries {
                        // Room for the next attempt as well, or the retries stop here.
                        if let Err(e) = self.reserve(&day, estimate) {
                            self.settle_unanswered(&day, &hash, 0.0, charged, false);
                            self.fail(&e);
                            return Err(e);
                        }
                        reserved = estimate;
                    }
                    backoff(attempt)
                }
                Err(TransportError::Connect(e)) => {
                    // Spec §5.2: retries only on 429, 5xx and timeouts. Nothing reached Jev: the
                    // heuristic decides now and the judge stage asks again later.
                    self.settle_unanswered(&day, &hash, reserved, charged, false);
                    let e = JudgeError::Unavailable(format!(
                        "cannot reach Jev ({e}); will retry later"
                    ));
                    self.fail(&e);
                    return Err(e);
                }
                Err(TransportError::Lost(e)) => {
                    // Sent, reply lost: not retried (it may have been billed already).
                    self.settle_unanswered(&day, &hash, reserved, charged, true);
                    let e = JudgeError::Unavailable(format!(
                        "Jev's reply did not arrive whole ({e}); will retry later"
                    ));
                    self.fail(&e);
                    return Err(e);
                }
            };
            if attempt < self.cfg.max_retries {
                self.stats.lock().retries += 1;
                (self.sleep)(delay.min(MAX_RETRY_WAIT));
            }
        }
        self.settle_unanswered(&day, &hash, reserved, charged, false);
        let e = JudgeError::Unavailable(format!("{last}; will retry later"));
        self.fail(&e);
        Err(e)
    }

    /// The end of a call that got no usable answer. `charged` (timed-out attempts) and, when
    /// `bill_reserved`, the attempt in flight (`reserved`) stay as the cost and go in the ledger
    /// (never in the cache); the rest of the reservation is released.
    fn settle_unanswered(
        &self,
        day: &str,
        hash: &str,
        reserved: f64,
        charged: f64,
        bill_reserved: bool,
    ) {
        let mut cost = charged;
        if bill_reserved {
            cost += reserved;
        } else {
            self.settle(day, reserved, 0.0);
        }
        if cost > 0.0 {
            self.store.record(&JevCall {
                day: day.into(),
                hash: hash.into(),
                model: self.cfg.model.clone(),
                response_json: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                usd: cost,
                latency_ms: 0,
                cacheable: false,
            });
        }
    }

    fn fail(&self, e: &JudgeError) {
        let mut s = self.stats.lock();
        s.failures += 1;
        s.last_error = Some(e.to_string());
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// A classification of text that belongs to `projects`; every one must be allowed to the
    /// cloud. (The trait's `classify` is refused: it names no project.)
    pub fn classify_in(
        &self,
        projects: &[&str],
        state: &str,
        options: &[Choice],
    ) -> JudgeResult<Distribution> {
        let q = Question {
            id: "q".into(),
            instructions: "Choose the option that fits best.".into(),
            options: options.to_vec(),
        };
        self.ask(projects, state, std::slice::from_ref(&q))?
            .remove("q")
            .ok_or_else(|| JudgeError::Failed("no answer".into()))
    }
}

impl Judge for JevJudge {
    fn provider(&self) -> &str {
        "jev"
    }

    fn warning(&self) -> Option<String> {
        if let Some(w) = self.limits.spend_error.lock().clone() {
            return Some(w);
        }
        let today = self.today();
        self.limits
            .budget_stop
            .lock()
            .as_ref()
            .filter(|(day, _)| *day == today)
            .map(|(_, msg)| msg.clone())
    }

    /// Refused: free text has no project, and "no project" would be taken for `global`, which
    /// is open to the cloud. See [`Judge::classify_constant`] and [`JevJudge::classify_in`].
    fn classify(&self, _state: &str, _options: &[Choice]) -> JudgeResult<Distribution> {
        self.stats.lock().denied += 1;
        Err(JudgeError::Denied)
    }

    fn classify_constant(
        &self,
        state: &'static str,
        options: &'static [(&'static str, &'static str)],
    ) -> JudgeResult<Distribution> {
        let options: Vec<Choice> = options.iter().map(|(id, d)| Choice::new(id, d)).collect();
        self.classify_in(&[UNSCOPED], state, &options)
    }

    /// Refused for the same reason as `classify`.
    fn score(&self, _state: &str, _rubric: &Rubric) -> JudgeResult<Score> {
        self.stats.lock().denied += 1;
        Err(JudgeError::Denied)
    }

    /// Refused for the same reason as `classify`; see [`Judge::match_predicate_in`].
    fn match_predicate(&self, _new: &str, _existing: &[String]) -> JudgeResult<Distribution> {
        self.stats.lock().denied += 1;
        Err(JudgeError::Denied)
    }

    /// Predicate names are the knowledge graph's vocabulary, which belongs to no project, so
    /// the call covers `global` and the `projects` given (the project of a memory a fact
    /// cites): every one must be allowed to the cloud.
    fn match_predicate_in(
        &self,
        projects: &[&str],
        new: &str,
        existing: &[String],
    ) -> JudgeResult<Distribution> {
        let covered = distinct(projects.iter().copied().chain(std::iter::once(UNSCOPED)));
        let (state, options) = predicate_question(new, existing);
        self.classify_in(&covered, &state, &options)
    }

    fn compare(&self, new: &JudgeMemory, existing: &[JudgeMemory]) -> JudgeResult<Vec<Relation>> {
        if existing.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.now();
        let mut state = memory_block("New memory", new, now);
        let mut questions = Vec::new();
        for (n, e) in existing.iter().enumerate() {
            let k = n + 1;
            state.push('\n');
            state.push_str(&memory_block(&format!("Existing memory {k}"), e, now));
            questions.push(Question {
                id: format!("fwd_{k}"),
                instructions: format!("How does the new memory relate to existing memory {k}?"),
                options: relation_choices(),
            });
            questions.push(Question {
                id: format!("rev_{k}"),
                instructions: format!("How does existing memory {k} relate to the new memory?"),
                options: reverse_relation_choices(),
            });
        }
        let projects = distinct(
            std::iter::once(new.project.as_str())
                .chain(existing.iter().map(|e| e.project.as_str())),
        );
        let answers = self.ask(&projects, &state, &questions)?;
        Ok(existing
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let k = i + 1;
                let fwd = answers
                    .get(&format!("fwd_{k}"))
                    .cloned()
                    .unwrap_or_default();
                let rev = answers
                    .get(&format!("rev_{k}"))
                    .cloned()
                    .unwrap_or_default();
                let rev = Distribution::from_pairs(rev.0.into_iter().map(|(id, p)| {
                    (
                        if id == "outdated_by" {
                            "updates".to_string()
                        } else {
                            id
                        },
                        p,
                    )
                }));
                let agreement = match (fwd.best(), rev.best()) {
                    (Some(a), Some(b)) => Some(a.0 == b.0),
                    _ => None,
                };
                Relation {
                    existing_id: e.id.clone(),
                    probs: Distribution::average(&fwd, &rev),
                    agreement,
                }
            })
            .collect())
    }

    fn classify_kind(&self, m: &JudgeMemory) -> JudgeResult<Distribution> {
        self.classify_in(
            &[m.project.as_str()],
            &kind_state(m, self.now()),
            &kind_choices(),
        )
    }

    fn route_project(
        &self,
        m: &JudgeMemory,
        projects: &[ProjectOption],
    ) -> JudgeResult<Distribution> {
        // The options name every candidate project (and its recent titles), so all of them
        // are involved in the call, besides the memory's own.
        let covered = distinct(
            std::iter::once(m.project.as_str())
                .chain(projects.iter().map(|p| p.slug.as_str()))
                .chain(std::iter::once(UNSCOPED)),
        );
        let options: Vec<Choice> = projects
            .iter()
            .map(|p| Choice::new(&p.slug, &format!("{}: {}", p.name, p.hint)))
            .chain([Choice::new(UNSCOPED, "not about any one project")])
            .collect();
        self.classify_in(
            &covered,
            &format!(
                "Which project does this memory belong to?\n\n{}",
                memory_block("Memory", m, self.now())
            ),
            &options,
        )
    }

    fn grade_relevance(
        &self,
        query_project: &str,
        query: &str,
        results: &[JudgeMemory],
    ) -> JudgeResult<Vec<Distribution>> {
        if results.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.now();
        let mut state = format!("Query: {query}\n");
        let mut questions = Vec::new();
        for (i, r) in results.iter().enumerate() {
            let k = i + 1;
            state.push('\n');
            state.push_str(&memory_block(&format!("Result {k}"), r, now));
            questions.push(Question {
                id: format!("r_{k}"),
                instructions: format!("How well does result {k} answer the query?"),
                options: relevance_choices(),
            });
        }
        let projects = distinct(
            std::iter::once(query_project).chain(results.iter().map(|r| r.project.as_str())),
        );
        let answers = self.ask(&projects, &state, &questions)?;
        Ok((0..results.len())
            .map(|i| {
                answers
                    .get(&format!("r_{}", i + 1))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect())
    }
}

/// Fixed inputs shared by the unit tests and the fixture recorder (`jev_live.rs`), so the
/// recorded requests match what the code builds.
pub mod scenarios {
    use crate::judge::{Choice, JudgeMemory};
    use crate::model::Kind;
    use chrono::{DateTime, TimeZone, Utc};

    pub fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
    }

    fn m(id: &str, kind: Kind, title: &str, body: &str, days_ago: i64) -> JudgeMemory {
        JudgeMemory {
            id: id.into(),
            project: "alice".into(),
            kind,
            title: title.into(),
            body: body.into(),
            importance: 3,
            created_at: (fixed_now() - chrono::Duration::days(days_ago)).to_rfc3339(),
            similarity: None,
        }
    }

    /// A new decision against an older version of it and an unrelated fact.
    pub fn compare_case() -> (JudgeMemory, Vec<JudgeMemory>) {
        (
            m(
                "NEW",
                Kind::Decision,
                "Embeddings run on ONNX Runtime directly",
                "We dropped fastembed: the daemon runs the int8 e5 model on ONNX Runtime with a compact tokenizer, which saves about 250 MB.",
                0,
            ),
            vec![
                m(
                    "OLD",
                    Kind::Decision,
                    "Embeddings run through fastembed",
                    "The daemon embeds with fastembed 7.1 and the Hugging Face tokenizer.",
                    14,
                ),
                m(
                    "FACT",
                    Kind::Fact,
                    "Thai search uses trigram FTS5",
                    "Thai has no spaces between words, so lexical search uses the FTS5 trigram tokenizer.",
                    30,
                ),
            ],
        )
    }

    /// What the recorded compare fixture chose for each existing memory.
    pub const COMPARE_EXPECTED: [&str; 2] = ["updates", "unrelated"];

    pub fn kind_case() -> JudgeMemory {
        m(
            "K",
            Kind::Note,
            "Writer lease held during mining",
            "Symptom: diary writes timed out after 120 s. Cause: the hub held the writer lease while mining. Fix: mining now runs off the writer.",
            1,
        )
    }

    pub const KIND_EXPECTED: &str = "bugfix";

    pub fn relevance_case() -> (String, Vec<JudgeMemory>) {
        (
            "why did diary writes time out?".into(),
            vec![
                kind_case(),
                m(
                    "R2",
                    Kind::Fact,
                    "Spool directory",
                    "Writes go to ~/.alice/spool when the daemon is down.",
                    3,
                ),
                m(
                    "R3",
                    Kind::Preference,
                    "Answer in Thai",
                    "The user wants answers in Thai.",
                    9,
                ),
            ],
        )
    }

    pub const CLASSIFY_STATE: &str = "This is a connection test from Alice. Answer yes.";

    /// The options of the connection test, as (id, description) constants.
    pub const CLASSIFY_OPTIONS: &[(&str, &str)] =
        &[("yes", "this is a test"), ("no", "this is not a test")];

    pub fn classify_choices() -> Vec<Choice> {
        CLASSIFY_OPTIONS
            .iter()
            .map(|(id, d)| Choice::new(id, d))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Judge, JudgeError, RelationKind};
    use crate::settings::{CustomPattern, PrivacySettings};
    use parking_lot::Mutex as PMutex;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Not secret-shaped as one literal (push protection); this is only a stand-in key.
    const KEY: &str = concat!("ts_", "live_abcdef1234567890");

    /// Replays fixtures by request; anything else is a test failure that names the request.
    struct Fixtures(Vec<(serde_json::Value, serde_json::Value)>);

    impl Fixtures {
        fn load() -> Self {
            let mut v = Vec::new();
            for raw in [
                include_str!("../../tests/fixtures/jev/compare.json"),
                include_str!("../../tests/fixtures/jev/kind.json"),
                include_str!("../../tests/fixtures/jev/relevance.json"),
                include_str!("../../tests/fixtures/jev/classify.json"),
            ] {
                let f: serde_json::Value = serde_json::from_str(raw).unwrap();
                v.push((f["request"].clone(), f["response"].clone()));
            }
            Self(v)
        }
    }

    impl JevTransport for Fixtures {
        fn post(
            &self,
            _e: &str,
            key: &str,
            body: &[u8],
            _t: Duration,
        ) -> Result<HttpReply, TransportError> {
            assert_eq!(key, KEY);
            let req: serde_json::Value = serde_json::from_slice(body).unwrap();
            let (_, resp) = self.0.iter().find(|(r, _)| *r == req).unwrap_or_else(|| {
                panic!("no fixture for this request; re-record with jev_live::record_fixtures:\n{req:#}")
            });
            Ok(HttpReply {
                status: 200,
                body: serde_json::to_vec(resp).unwrap(),
                retry_after: None,
            })
        }
    }

    /// Answers from a script: status codes or transport errors, then 200 with `ok`.
    struct Scripted {
        script: PMutex<VecDeque<Result<u16, TransportError>>>,
        ok: serde_json::Value,
        calls: PMutex<Vec<serde_json::Value>>,
        /// What a scripted status reply says in `Retry-After`.
        retry_after: PMutex<Option<Duration>>,
        /// What a 200 says instead of `ok`, as raw bytes, when set.
        raw_ok: PMutex<Option<Vec<u8>>>,
    }

    impl Scripted {
        fn new(script: Vec<Result<u16, TransportError>>, ok: serde_json::Value) -> Arc<Self> {
            Arc::new(Self {
                script: PMutex::new(script.into()),
                ok,
                calls: PMutex::new(Vec::new()),
                retry_after: PMutex::new(Some(Duration::ZERO)),
                raw_ok: PMutex::new(None),
            })
        }
        fn ok() -> Arc<Self> {
            Self::new(Vec::new(), classify_ok())
        }
    }

    impl JevTransport for Scripted {
        fn post(
            &self,
            _e: &str,
            _k: &str,
            body: &[u8],
            _t: Duration,
        ) -> Result<HttpReply, TransportError> {
            self.calls
                .lock()
                .push(serde_json::from_slice(body).unwrap());
            match self.script.lock().pop_front() {
                Some(Ok(status)) => Ok(HttpReply {
                    status,
                    body: b"{\"detail\":\"x\"}".to_vec(),
                    retry_after: *self.retry_after.lock(),
                }),
                Some(Err(e)) => Err(e),
                None => Ok(HttpReply {
                    status: 200,
                    body: self
                        .raw_ok
                        .lock()
                        .clone()
                        .unwrap_or_else(|| serde_json::to_vec(&self.ok).unwrap()),
                    retry_after: None,
                }),
            }
        }
    }

    fn cfg() -> JevConfig {
        JevConfig {
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            model: "jev-1.13.0".into(),
            api_key: SecretValue::new(KEY),
            rate_per_sec: 10.0,
            timeout: Duration::from_secs(10),
            daily_budget_usd: 1.0,
            max_retries: 3,
        }
    }

    fn policy() -> Arc<FixedPolicy> {
        Arc::new(FixedPolicy::allow_all(Redactor::builtin()).denying(&["secret-proj"]))
    }

    fn judge_with(
        t: Arc<dyn JevTransport>,
        store: Arc<dyn JevStore>,
        cfg: JevConfig,
        policy: Arc<dyn JevPolicy>,
    ) -> JevJudge {
        JevJudge::new(cfg, t, store, policy)
            .with_clock(Arc::new(scenarios::fixed_now))
            .with_sleeper(Arc::new(|_| {}))
    }

    fn judge(t: Arc<dyn JevTransport>, store: Arc<MemoryJevStore>, cfg: JevConfig) -> JevJudge {
        judge_with(t, store, cfg, policy())
    }

    fn classify_ok() -> serde_json::Value {
        classify_ok_tokens(100)
    }

    /// A yes/no answer whose usage reports `input_tokens`.
    fn classify_ok_tokens(input_tokens: u64) -> serde_json::Value {
        serde_json::json!({"model": "jev-1.13.0", "answers": {"q": {"type": "choice", "choice": "yes", "confidence": 0.9,
            "probabilities": {"yes": 0.9, "no": 0.1}}}, "usage": {"input_tokens": input_tokens, "output_tokens": 3}})
    }

    fn yes_no() -> Vec<Choice> {
        vec![Choice::new("yes", "it is"), Choice::new("no", "it is not")]
    }

    fn q(instructions: &str) -> Question {
        Question {
            id: "q".into(),
            instructions: instructions.into(),
            options: yes_no(),
        }
    }

    #[test]
    fn the_request_body_has_the_documented_shape_and_no_key() {
        let j = judge(
            Arc::new(Fixtures::load()),
            Arc::new(MemoryJevStore::default()),
            cfg(),
        );
        let body = j.request_body(&["alice"], "state text", &[q("Pick one.")]);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "jev-1.13.0");
        assert_eq!(v["state"], "state text");
        assert_eq!(v["questions"]["q"]["type"], "choice");
        assert!(
            v["questions"]["q"]["criteria"].is_object(),
            "criteria must be an object"
        );
        assert!(!String::from_utf8_lossy(&body).contains("abcdef1234567890"));
        assert_eq!(
            body,
            j.request_body(&["alice"], "state text", &[q("Pick one.")]),
            "deterministic"
        );
    }

    #[test]
    fn options_are_shuffled_but_the_same_question_always_gets_the_same_order() {
        let j = judge(
            Arc::new(Fixtures::load()),
            Arc::new(MemoryJevStore::default()),
            cfg(),
        );
        let opts: Vec<Choice> = (0..6).map(|i| Choice::new(&format!("o{i}"), "x")).collect();
        let mut orders = std::collections::HashSet::new();
        for i in 0..20 {
            let question = Question {
                id: "q".into(),
                instructions: "Pick.".into(),
                options: opts.clone(),
            };
            let body = j.request_body(&["alice"], &format!("state {i}"), &[question]);
            let text = String::from_utf8(body).unwrap();
            let pos: Vec<usize> = (0..6)
                .map(|k| text.find(&format!("\"o{k}\"")).unwrap())
                .collect();
            let mut order: Vec<usize> = (0..6).collect();
            order.sort_by_key(|&k| pos[k]);
            orders.insert(order);
        }
        assert!(
            orders.len() > 3,
            "options are shuffled across requests: {} orders",
            orders.len()
        );
    }

    #[test]
    fn compare_averages_forward_and_reversed_from_the_recorded_fixture() {
        let store = Arc::new(MemoryJevStore::default());
        let j = judge(Arc::new(Fixtures::load()), store.clone(), cfg());
        let (new, existing) = scenarios::compare_case();
        let rel = j.compare(&new, &existing).unwrap();
        assert_eq!(rel.len(), existing.len());
        for r in &rel {
            let total: f64 = RelationKind::ALL
                .iter()
                .map(|k| r.probs.p(k.as_str()))
                .sum();
            assert!((total - 1.0).abs() < 1e-6, "{r:?}");
            assert!(r.agreement.is_some());
        }
        for (r, want) in rel.iter().zip(scenarios::COMPARE_EXPECTED) {
            assert_eq!(r.probs.best().unwrap().0, want, "{r:?}");
        }
        assert_eq!(
            store.calls().len(),
            1,
            "one request for all pairs, both directions"
        );
        assert!(store.calls()[0].usd > 0.0);
        // The same decision again: answered from the cache, no request, no cost.
        let again = j.compare(&new, &existing).unwrap();
        assert_eq!(again, rel);
        assert_eq!(store.calls().len(), 1);
        assert_eq!(j.stats().cache_hits, 1);
    }

    #[test]
    fn kind_and_relevance_fixtures_parse() {
        let j = judge(
            Arc::new(Fixtures::load()),
            Arc::new(MemoryJevStore::default()),
            cfg(),
        );
        let m = scenarios::kind_case();
        assert_eq!(
            j.classify_kind(&m).unwrap().best().unwrap().0,
            scenarios::KIND_EXPECTED
        );
        let (query, results) = scenarios::relevance_case();
        let grades = j.grade_relevance("alice", &query, &results).unwrap();
        assert_eq!(grades.len(), results.len());
        assert!(
            grades[0].p(crate::judge::RELEVANCE_ANSWERS)
                > grades[2].p(crate::judge::RELEVANCE_ANSWERS)
        );
        assert_eq!(
            j.classify_constant(scenarios::CLASSIFY_STATE, scenarios::CLASSIFY_OPTIONS)
                .unwrap()
                .best()
                .unwrap()
                .0,
            "yes"
        );
    }

    #[test]
    fn retries_only_on_429_5xx_and_timeouts() {
        let s = Scripted::new(
            vec![Ok(429), Ok(503), Err(TransportError::Timeout)],
            classify_ok(),
        );
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        assert_eq!(
            j.classify_in(&[UNSCOPED], "is this a test?", &yes_no())
                .unwrap()
                .best()
                .unwrap()
                .0,
            "yes"
        );
        assert_eq!((s.calls.lock().len(), j.stats().retries), (4, 3));
        for (status, want) in [
            (400u16, "400"),
            (401, "set-secret jev_api_key"),
            (422, "422"),
        ] {
            let s = Scripted::new(vec![Ok(status)], classify_ok());
            let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
            let e = j
                .classify_in(&[UNSCOPED], &format!("state {status}"), &yes_no())
                .unwrap_err();
            assert!(e.to_string().contains(want), "{status}: {e}");
            assert_eq!(s.calls.lock().len(), 1, "{status} is not retried");
        }
        let s = Scripted::new(vec![Err(TransportError::Timeout); 4], classify_ok());
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "t", &yes_no()),
            Err(JudgeError::Unavailable(_))
        ));
        assert_eq!(s.calls.lock().len(), 4, "one try and three retries");
        // Spec §5.2: a connection error is not retried; the heuristic decides and the memory's
        // judge_state becomes 'retry' (Task 12).
        let s = Scripted::new(
            vec![Err(TransportError::Connect("connection refused".into()))],
            classify_ok(),
        );
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        let e = j.classify_in(&[UNSCOPED], "c", &yes_no()).unwrap_err();
        assert!(
            matches!(e, JudgeError::Unavailable(_)) && e.to_string().contains("cannot reach Jev"),
            "{e}"
        );
        assert_eq!(
            (s.calls.lock().len(), j.stats().retries),
            (1, 0),
            "connection errors are not retried"
        );
    }

    /// A judge sharing `s` and `store`, with a movable clock and the given daily budget.
    fn budget_judge(
        s: &Arc<Scripted>,
        store: &Arc<MemoryJevStore>,
        day: &Arc<PMutex<DateTime<Utc>>>,
        budget: f64,
    ) -> JevJudge {
        let d = day.clone();
        JevJudge::new(
            JevConfig {
                daily_budget_usd: budget,
                ..cfg()
            },
            s.clone(),
            store.clone(),
            policy(),
        )
        .with_clock(Arc::new(move || *d.lock()))
        .with_sleeper(Arc::new(|_| {}))
    }

    /// What `classify_in(state, yes_no())` reserves.
    fn estimate(j: &JevJudge, state: &str) -> f64 {
        let question = q("Choose the option that fits best.");
        let len = j
            .request_body(&[UNSCOPED], state, std::slice::from_ref(&question))
            .len();
        j.estimate_usd(len)
    }

    #[test]
    fn the_daily_budget_is_never_exceeded_and_resets_the_next_utc_day() {
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::ok();
        let day = Arc::new(PMutex::new(scenarios::fixed_now()));
        // Budget: exactly the first request's estimate. The reply reports 100 input tokens,
        // fewer than the body's bytes, so the first call fits and settles below its estimate.
        let budget = estimate(&budget_judge(&s, &store, &day, 1.0), "first");
        let j = budget_judge(&s, &store, &day, budget);
        j.classify_in(&[UNSCOPED], "first", &yes_no()).unwrap();
        let e = j.classify_in(&[UNSCOPED], "second", &yes_no()).unwrap_err();
        assert!(matches!(e, JudgeError::Budget(_)), "{e}");
        assert_eq!(
            s.calls.lock().len(),
            1,
            "no request once the next estimate does not fit"
        );
        assert!(
            store.spent("2026-10-13").unwrap() <= budget,
            "the day's spend never exceeds the budget"
        );
        let w = j.warning().expect("a used-up budget shows in health");
        assert!(w.contains("2026-10-13") && w.contains("paused"), "{w}");
        *day.lock() = scenarios::fixed_now() + chrono::Duration::days(1);
        assert_eq!(j.warning(), None, "the new day's budget is a clean slate");
        j.classify_in(&[UNSCOPED], "third", &yes_no()).unwrap();
        assert_eq!(s.calls.lock().len(), 2);
    }

    #[test]
    fn crossing_utc_midnight_starts_a_fresh_budget_and_keeps_the_old_days_spend() {
        use chrono::TimeZone;
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::ok();
        let before = Utc.with_ymd_and_hms(2026, 10, 13, 23, 59, 58).unwrap();
        let day = Arc::new(PMutex::new(before));
        let budget = estimate(&budget_judge(&s, &store, &day, 1.0), "call a");
        let j = budget_judge(&s, &store, &day, budget);
        j.classify_in(&[UNSCOPED], "call a", &yes_no()).unwrap();
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "call b", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
        let spent_old = store.spent("2026-10-13").unwrap();
        assert!(spent_old > 0.0);
        *day.lock() = Utc.with_ymd_and_hms(2026, 10, 14, 0, 0, 1).unwrap();
        j.classify_in(&[UNSCOPED], "call b", &yes_no()).unwrap();
        assert_eq!(
            store.spent("2026-10-13").unwrap(),
            spent_old,
            "the old day's ledger is untouched"
        );
        let new_day = store.spent("2026-10-14").unwrap();
        assert!(new_day > 0.0 && new_day <= budget);
        let days: Vec<String> = store.calls().into_iter().map(|c| c.day).collect();
        assert_eq!(days, ["2026-10-13", "2026-10-14"]);
        // A clock that goes back reads the old day's spend again instead of trusting memory.
        *day.lock() = before;
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "call c", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
    }

    #[test]
    fn a_reply_counting_more_tokens_than_bytes_raises_later_estimates() {
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::new(Vec::new(), classify_ok_tokens(10_000));
        let day = Arc::new(PMutex::new(scenarios::fixed_now()));
        let probe = budget_judge(&s, &store, &day, 1.0);
        // Enough for the first call as Jev counts it, not for a second one of the same size.
        let budget = estimate(&probe, "first") + 10_000.0 * USD_PER_MTOK_INPUT / 1e6;
        let j = budget_judge(&s, &store, &day, budget);
        j.classify_in(&[UNSCOPED], "first", &yes_no()).unwrap();
        // "2nd" is shorter than "first": by bytes alone it would fit and overspend.
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "2nd", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
        assert!(store.spent("2026-10-13").unwrap() <= budget);
    }

    /// Holds every request until `open` is called.
    struct Gate {
        open: PMutex<bool>,
        cv: parking_lot::Condvar,
        sent: AtomicUsize,
        ok: serde_json::Value,
    }

    impl JevTransport for Gate {
        fn post(
            &self,
            _e: &str,
            _k: &str,
            _b: &[u8],
            _t: Duration,
        ) -> Result<HttpReply, TransportError> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            let mut open = self.open.lock();
            while !*open {
                self.cv.wait(&mut open);
            }
            Ok(HttpReply {
                status: 200,
                body: serde_json::to_vec(&self.ok).unwrap(),
                retry_after: None,
            })
        }
    }

    #[test]
    fn parallel_calls_near_the_cap_cannot_overshoot_the_budget() {
        let store = Arc::new(MemoryJevStore::default());
        let gate = Arc::new(Gate {
            open: PMutex::new(false),
            cv: parking_lot::Condvar::new(),
            sent: AtomicUsize::new(0),
            ok: classify_ok(),
        });
        let probe = judge(Scripted::ok(), store.clone(), cfg());
        // Room for two requests of this size, in flight at the same time, but not for three.
        let budget = estimate(&probe, "call-0") * 2.5;
        let j = Arc::new(judge(
            gate.clone(),
            store.clone(),
            JevConfig {
                daily_budget_usd: budget,
                ..cfg()
            },
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let (j, tx) = (j.clone(), tx.clone());
                std::thread::spawn(move || {
                    let r = j.classify_in(&[UNSCOPED], &format!("call-{i}"), &yes_no());
                    tx.send(r.is_ok()).unwrap();
                })
            })
            .collect();
        // The two admitted calls are held at the gate, so the other six are refused at once.
        for _ in 0..6 {
            let ok = rx
                .recv_timeout(Duration::from_secs(20))
                .expect("a refusal arrives");
            assert!(!ok, "no call finishes while the gate is shut");
        }
        *gate.open.lock() = true;
        gate.cv.notify_all();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(
            (0..2).filter(|_| rx.recv().unwrap()).count(),
            2,
            "the two reserved calls complete"
        );
        assert_eq!(
            gate.sent.load(Ordering::SeqCst),
            2,
            "only two requests left"
        );
        assert!(store.spent("2026-10-13").unwrap() <= budget);
        assert_eq!(j.stats().budget_stops, 6);
    }

    #[test]
    fn an_unreadable_spend_counts_as_an_exhausted_budget_until_it_can_be_read() {
        struct FlakySpend(AtomicBool);
        impl JevStore for FlakySpend {
            fn cached(&self, _: &str) -> Option<String> {
                None
            }
            fn spent(&self, _: &str) -> Result<f64, String> {
                if self.0.load(Ordering::SeqCst) {
                    Err("disk I/O error".into())
                } else {
                    Ok(0.0)
                }
            }
            fn record(&self, _: &JevCall) {}
        }
        let broken = Arc::new(FlakySpend(AtomicBool::new(true)));
        let s = Scripted::ok();
        let j = judge_with(s.clone(), broken.clone(), cfg(), policy());
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "x", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
        assert!(s.calls.lock().is_empty(), "fail closed: nothing is sent");
        assert!(
            j.warning()
                .unwrap()
                .contains("cannot read today's Jev spend"),
            "{:?}",
            j.warning()
        );
        broken.0.store(false, Ordering::SeqCst);
        j.classify_in(&[UNSCOPED], "x", &yes_no()).unwrap();
        assert_eq!(
            j.warning(),
            None,
            "reading the spend again clears the warning"
        );
        // A spend that is not a number is as unreadable as an error.
        struct NotANumber;
        impl JevStore for NotANumber {
            fn cached(&self, _: &str) -> Option<String> {
                None
            }
            fn spent(&self, _: &str) -> Result<f64, String> {
                Ok(f64::NAN)
            }
            fn record(&self, _: &JevCall) {}
        }
        let s = Scripted::ok();
        let j = judge_with(s.clone(), Arc::new(NotANumber), cfg(), policy());
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "y", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
        assert!(s.calls.lock().is_empty());
        // A budget that is not a number never lets a call through.
        let s = Scripted::ok();
        let j = judge(
            s.clone(),
            Arc::new(MemoryJevStore::default()),
            JevConfig {
                daily_budget_usd: f64::NAN,
                ..cfg()
            },
        );
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "z", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
        assert!(s.calls.lock().is_empty());
    }

    #[test]
    fn denied_projects_never_leave_and_text_is_redacted_first() {
        let s = Scripted::ok();
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        let (mut new, existing) = scenarios::compare_case();
        new.project = "secret-proj".into();
        assert!(matches!(
            j.compare(&new, &existing),
            Err(JudgeError::Denied)
        ));
        let mut m = scenarios::kind_case();
        m.project = "secret-proj".into();
        assert!(matches!(j.classify_kind(&m), Err(JudgeError::Denied)));
        assert!(s.calls.lock().is_empty());
        assert_eq!(j.stats().denied, 2);
        assert_eq!(
            j.last_request(),
            None,
            "no body was built for a denied call"
        );
        j.classify_in(
            &[UNSCOPED],
            concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here"),
            &yes_no(),
        )
        .unwrap();
        let sent = s.calls.lock()[0].to_string();
        assert!(
            sent.contains("[REDACTED:github_token]") && !sent.contains("ghp_"),
            "{sent}"
        );
    }

    #[test]
    fn a_call_covering_several_projects_needs_every_one_of_them_allowed() {
        let s = Scripted::ok();
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        // The new memory and one existing memory are fine; the other existing one is denied.
        let (new, mut existing) = scenarios::compare_case();
        existing[1].project = "secret-proj".into();
        assert!(matches!(
            j.compare(&new, &existing),
            Err(JudgeError::Denied)
        ));
        // Results of several projects.
        let (query, mut results) = scenarios::relevance_case();
        results[2].project = "secret-proj".into();
        assert!(matches!(
            j.grade_relevance("alice", &query, &results),
            Err(JudgeError::Denied)
        ));
        // A routing question names every candidate project in its options.
        let routed = ProjectOption {
            slug: "secret-proj".into(),
            name: "Secret".into(),
            hint: "client work".into(),
        };
        assert!(matches!(
            j.route_project(&scenarios::kind_case(), &[routed]),
            Err(JudgeError::Denied)
        ));
        assert!(s.calls.lock().is_empty());
        assert_eq!(j.stats().denied, 3);
    }

    #[test]
    fn an_empty_scope_or_an_empty_slug_is_never_sent() {
        let s = Scripted::ok();
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        let question = q("Pick.");
        assert!(matches!(
            j.ask(&[], "state", std::slice::from_ref(&question)),
            Err(JudgeError::Denied)
        ));
        assert!(matches!(
            j.classify_in(&[""], "state", &yes_no()),
            Err(JudgeError::Denied)
        ));
        assert!(matches!(
            j.classify_in(&["  "], "state", &yes_no()),
            Err(JudgeError::Denied)
        ));
        assert!(s.calls.lock().is_empty());
    }

    /// A policy that can be flipped between calls.
    struct Switch {
        allowed: AtomicBool,
        asked: PMutex<Vec<String>>,
    }

    impl JevPolicy for Switch {
        fn cloud_allowed(&self, project: &str) -> bool {
            self.asked.lock().push(project.to_string());
            self.allowed.load(Ordering::SeqCst)
        }
        fn redactor(&self, _: &str) -> Arc<Redactor> {
            Arc::new(Redactor::builtin())
        }
    }

    #[test]
    fn the_policy_is_asked_on_every_call_even_when_the_answer_is_cached() {
        let sw = Arc::new(Switch {
            allowed: AtomicBool::new(true),
            asked: PMutex::new(Vec::new()),
        });
        let s = Scripted::ok();
        let j = judge_with(
            s.clone(),
            Arc::new(MemoryJevStore::default()),
            cfg(),
            sw.clone(),
        );
        let m = scenarios::kind_case();
        let (new, mut existing) = scenarios::compare_case();
        existing[1].project = "other".into();
        j.classify_in(&[UNSCOPED], "same", &yes_no()).unwrap();
        j.classify_in(&[UNSCOPED], "same", &yes_no()).unwrap();
        assert_eq!(j.stats().cache_hits, 1);
        assert_eq!(*sw.asked.lock(), ["global", "global"], "once per call");
        sw.asked.lock().clear();
        // One question per distinct project, whatever the number of memories.
        let _ = j.compare(&new, &existing);
        assert_eq!(*sw.asked.lock(), ["alice", "other"]);
        // The decision is made now, not when the judge was built.
        sw.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "same", &yes_no()),
            Err(JudgeError::Denied)
        ));
        assert!(matches!(j.classify_kind(&m), Err(JudgeError::Denied)));
        assert_eq!(
            j.stats().cache_hits,
            1,
            "a cached answer is not given to a denied call"
        );
    }

    #[test]
    fn each_project_redacts_with_its_own_rules() {
        let custom = Redactor::new(&PrivacySettings {
            custom_patterns: vec![CustomPattern {
                name: "ticket".into(),
                regex: "TCK-[0-9]{4,}".into(),
            }],
            ..Default::default()
        })
        .unwrap();
        let p =
            Arc::new(FixedPolicy::allow_all(Redactor::builtin()).with_redactor("alice", custom));
        let s = Scripted::ok();
        let j = judge_with(s.clone(), Arc::new(MemoryJevStore::default()), cfg(), p);
        j.classify_in(&["alice"], "see TCK-12345 for details", &yes_no())
            .unwrap();
        j.classify_in(&["other"], "see TCK-12345 for details", &yes_no())
            .unwrap();
        let calls = s.calls.lock();
        let (a, b) = (calls[0].to_string(), calls[1].to_string());
        assert!(
            a.contains("[REDACTED:custom:ticket]") && !a.contains("TCK-12345"),
            "{a}"
        );
        assert!(
            b.contains("TCK-12345"),
            "the other project has no such rule: {b}"
        );
        drop(calls);
        // Descriptions and instructions are redacted too (a routing hint holds recent titles).
        let tok = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let body = j.request_body(
            &["alice"],
            "state",
            &[Question {
                id: "q".into(),
                instructions: format!("note {tok}"),
                options: vec![
                    Choice::new("a", &format!("hint {tok}")),
                    Choice::new("b", "b"),
                ],
            }],
        );
        let text = String::from_utf8(body).unwrap();
        assert!(
            !text.contains("ghp_") && text.contains("[REDACTED:github_token]"),
            "{text}"
        );
    }

    #[test]
    fn the_cache_key_covers_the_model_and_every_question() {
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::ok();
        let j = judge(s.clone(), store.clone(), cfg());
        let ask = |j: &JevJudge, state: &str, question: Question| {
            j.ask(&[UNSCOPED], state, &[question]).unwrap();
        };
        ask(&j, "s", q("Pick one."));
        ask(&j, "s", q("Pick one."));
        assert_eq!(
            s.calls.lock().len(),
            1,
            "the same request is answered from the cache"
        );
        ask(&j, "s", q("Pick another."));
        let mut other_option = q("Pick one.");
        other_option.options[0].description = "it certainly is".into();
        ask(&j, "s", other_option);
        let mut other_id = q("Pick one.");
        other_id.options[1].id = "nope".into();
        other_id.options[1].description = "it is not".into();
        ask(&j, "s", other_id);
        ask(&j, "s2", q("Pick one."));
        assert_eq!(
            s.calls.lock().len(),
            5,
            "any difference in what is asked is a new request"
        );
        // Another model never reads this model's answers.
        let j2 = judge(
            s.clone(),
            store.clone(),
            JevConfig {
                model: "jev-9.9.9".into(),
                ..cfg()
            },
        );
        ask(&j2, "s", q("Pick one."));
        assert_eq!(s.calls.lock().len(), 6);
        // Two texts that redact to the same request share an entry: the secret is not in it.
        let a = concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 end");
        let b = concat!("token gh", "p_ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210 end");
        ask(&j, a, q("Pick one."));
        ask(&j, b, q("Pick one."));
        assert_eq!(s.calls.lock().len(), 7);
    }

    #[test]
    fn debug_output_never_shows_the_key() {
        let j = judge(Scripted::ok(), Arc::new(MemoryJevStore::default()), cfg());
        let shown = format!("{:?} {:?} {:?}", cfg(), j, j.stats());
        assert!(
            !shown.contains("abcdef1234567890") && !shown.contains(KEY),
            "{shown}"
        );
        assert!(shown.contains("redacted"));
        let s = Scripted::new(vec![Ok(500); 4], classify_ok());
        let j = judge(s, Arc::new(MemoryJevStore::default()), cfg());
        let e = j
            .classify_in(&[UNSCOPED], "x", &yes_no())
            .unwrap_err()
            .to_string();
        let st = serde_json::to_string(&j.stats()).unwrap();
        assert!(!e.contains("abcdef1234567890") && !st.contains("abcdef1234567890"));
        let store = Arc::new(MemoryJevStore::default());
        let j = judge(Scripted::ok(), store.clone(), cfg());
        j.classify_in(&[UNSCOPED], "x", &yes_no()).unwrap();
        let recorded = serde_json::to_string(&store.calls()).unwrap();
        assert!(!recorded.contains("abcdef1234567890"));
        let sent = String::from_utf8(j.last_request().unwrap()).unwrap();
        assert!(!sent.contains("abcdef1234567890"));
    }

    #[test]
    fn requests_are_spaced_to_the_rate_limit() {
        let waited = Arc::new(PMutex::new(Duration::ZERO));
        let w = waited.clone();
        let s = Scripted::ok();
        let j = JevJudge::new(cfg(), s, Arc::new(MemoryJevStore::default()), policy())
            .with_clock(Arc::new(scenarios::fixed_now))
            .with_sleeper(Arc::new(move |d| {
                let mut m = w.lock();
                *m = (*m).max(d);
            }));
        for i in 0..20 {
            j.classify_in(&[UNSCOPED], &format!("distinct state {i}"), &yes_no())
                .unwrap();
        }
        // The sleeper does not really sleep, so the 20th call's slot is ~1.9 s after the first.
        let longest = *waited.lock();
        assert!(
            longest >= Duration::from_millis(1_700) && longest <= Duration::from_millis(1_950),
            "{longest:?}"
        );
        // A rate that is not a number or not positive never panics; it counts as one a second.
        assert_eq!(
            (rate(f64::NAN), rate(0.0), rate(-3.0), rate(50.0)),
            (1.0, 1.0, 1.0, 10.0)
        );
    }

    #[test]
    fn malformed_or_incomplete_answers_fail_without_retry() {
        for body in [
            serde_json::json!("not an object"),
            serde_json::json!({"answers": {}, "usage": {"input_tokens": 1}}),
            serde_json::json!({"answers": {"q": {"choice": "maybe", "probabilities": {"maybe": 1.0}}}}),
        ] {
            let s = Scripted::new(Vec::new(), body);
            let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
            assert!(matches!(
                j.classify_in(&[UNSCOPED], "x", &yes_no()),
                Err(JudgeError::Failed(_))
            ));
            assert_eq!(s.calls.lock().len(), 1);
        }
    }

    #[test]
    fn generic_entry_points_are_denied_unless_the_text_is_marked_as_constant() {
        let s = Scripted::ok();
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        assert_eq!(j.classify("free text", &yes_no()), Err(JudgeError::Denied));
        let rubric = Rubric {
            question: "How good?".into(),
            levels: yes_no(),
        };
        assert_eq!(j.score("free text", &rubric), Err(JudgeError::Denied));
        assert_eq!(
            j.score_many(&[("free text".into(), rubric)]),
            Err(JudgeError::Denied)
        );
        assert_eq!(
            j.match_predicate("works_on", &["worked_on".to_string()]),
            Err(JudgeError::Denied)
        );
        assert!(s.calls.lock().is_empty() && j.last_request().is_none());
        assert_eq!(j.stats().denied, 4);
        // A constant of the program is fine.
        assert_eq!(
            j.classify_constant(scenarios::CLASSIFY_STATE, scenarios::CLASSIFY_OPTIONS)
                .unwrap()
                .best()
                .unwrap()
                .0,
            "yes"
        );
        assert_eq!(s.calls.lock().len(), 1);
        // The query's own project counts besides the results' projects.
        let (query, results) = scenarios::relevance_case();
        assert_eq!(
            j.grade_relevance("secret-proj", &query, &results),
            Err(JudgeError::Denied)
        );
        assert_eq!(s.calls.lock().len(), 1);
    }

    fn sum_spent(store: &MemoryJevStore) -> f64 {
        store.spent("2026-10-13").unwrap()
    }

    #[test]
    fn a_sent_call_with_unknown_usage_costs_its_reservation() {
        let est = |j: &JevJudge, state: &str| estimate(j, state);
        // A 200 that is not JSON at all.
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::ok();
        *s.raw_ok.lock() = Some(b"<html>gateway</html>".to_vec());
        let j = judge(s.clone(), store.clone(), cfg());
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "one", &yes_no()),
            Err(JudgeError::Failed(_))
        ));
        assert!((sum_spent(&store) - est(&j, "one")).abs() < 1e-12);
        // Not cached: the same request is sent again (and billed again).
        let _ = j.classify_in(&[UNSCOPED], "one", &yes_no());
        assert_eq!(s.calls.lock().len(), 2);
        assert!((sum_spent(&store) - 2.0 * est(&j, "one")).abs() < 1e-12);
        // A 200 with answers but no usage.
        let store = Arc::new(MemoryJevStore::default());
        let no_usage =
            serde_json::json!({"answers": {"q": {"choice": "yes", "probabilities": {"yes": 1.0}}}});
        let s = Scripted::new(Vec::new(), no_usage);
        let j = judge(s, store.clone(), cfg());
        j.classify_in(&[UNSCOPED], "two", &yes_no()).unwrap();
        assert!((sum_spent(&store) - est(&j, "two")).abs() < 1e-12);
        // A 200 that cannot be used but reports its usage: that usage is charged.
        let store = Arc::new(MemoryJevStore::default());
        let odd =
            serde_json::json!({"answers": {}, "usage": {"input_tokens": 500, "output_tokens": 1}});
        let j = judge(Scripted::new(Vec::new(), odd), store.clone(), cfg());
        assert!(j.classify_in(&[UNSCOPED], "three", &yes_no()).is_err());
        assert!((sum_spent(&store) - 500.0 * USD_PER_MTOK_INPUT / 1e6).abs() < 1e-12);
        // Timeouts after sending: the reservation is the cost, once.
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::new(vec![Err(TransportError::Timeout); 4], classify_ok());
        let j = judge(s, store.clone(), cfg());
        assert!(j.classify_in(&[UNSCOPED], "four", &yes_no()).is_err());
        // Each of the four attempts may have been billed.
        assert!((sum_spent(&store) - 4.0 * est(&j, "four")).abs() < 1e-12);
        assert!(store.cached(&store.calls()[0].hash).is_none());
        // A reply that was lost on the way back.
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::new(
            vec![Err(TransportError::Lost("reset".into()))],
            classify_ok(),
        );
        let j = judge(s.clone(), store.clone(), cfg());
        let e = j.classify_in(&[UNSCOPED], "five", &yes_no()).unwrap_err();
        assert!(matches!(e, JudgeError::Unavailable(_)), "{e}");
        assert_eq!(s.calls.lock().len(), 1, "not retried");
        assert!((sum_spent(&store) - est(&j, "five")).abs() < 1e-12);
        // Answers that were refused or never reached Jev cost nothing.
        for script in [
            vec![Ok(400)],
            vec![Ok(401)],
            vec![Err(TransportError::Connect("x".into()))],
            vec![Ok(503); 4],
        ] {
            let store = Arc::new(MemoryJevStore::default());
            let j = judge(Scripted::new(script, classify_ok()), store.clone(), cfg());
            assert!(j.classify_in(&[UNSCOPED], "six", &yes_no()).is_err());
            assert_eq!(sum_spent(&store), 0.0);
        }
    }

    #[test]
    fn a_reserved_unanswered_call_still_counts_against_the_budget() {
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::new(vec![Err(TransportError::Timeout); 4], classify_ok());
        let day = Arc::new(PMutex::new(scenarios::fixed_now()));
        let budget = estimate(&budget_judge(&s, &store, &day, 1.0), "first") * 1.5;
        let j = budget_judge(&s, &store, &day, budget);
        assert!(j.classify_in(&[UNSCOPED], "first", &yes_no()).is_err());
        // The timed-out call may have been billed: there is no room for a second one.
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "other", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
    }

    #[test]
    fn error_texts_carry_the_status_and_never_the_servers_words() {
        let s = Scripted::new(vec![Ok(400)], classify_ok());
        let j = judge(s, Arc::new(MemoryJevStore::default()), cfg());
        let e = j
            .classify_in(&[UNSCOPED], "x", &yes_no())
            .unwrap_err()
            .to_string();
        assert!(e.contains("400"), "{e}");
        let shown = format!("{e} {:?}", j.stats());
        // The scripted body is {"detail":"x"}: nothing of it comes back.
        assert!(!shown.contains("detail"), "{shown}");
    }

    /// Flips a policy off after its first request, answering 503.
    struct DenyAfterFirst {
        policy: Arc<Switch>,
        calls: AtomicUsize,
    }

    impl JevTransport for DenyAfterFirst {
        fn post(
            &self,
            _e: &str,
            _k: &str,
            _b: &[u8],
            _t: Duration,
        ) -> Result<HttpReply, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.policy.allowed.store(false, Ordering::SeqCst);
            Ok(HttpReply {
                status: 503,
                body: Vec::new(),
                retry_after: Some(Duration::ZERO),
            })
        }
    }

    #[test]
    fn a_denial_that_lands_during_the_retries_stops_them() {
        let sw = Arc::new(Switch {
            allowed: AtomicBool::new(true),
            asked: PMutex::new(Vec::new()),
        });
        let t = Arc::new(DenyAfterFirst {
            policy: sw.clone(),
            calls: AtomicUsize::new(0),
        });
        let store = Arc::new(MemoryJevStore::default());
        let j = judge_with(t.clone(), store.clone(), cfg(), sw);
        assert_eq!(
            j.classify_in(&[UNSCOPED], "x", &yes_no()),
            Err(JudgeError::Denied)
        );
        assert_eq!(t.calls.load(Ordering::SeqCst), 1, "no second try");
        assert_eq!(sum_spent(&store), 0.0, "a 503 was not billed");
    }

    #[test]
    fn retry_after_is_honoured_up_to_thirty_seconds_and_backoff_grows() {
        let slept = Arc::new(PMutex::new(Vec::<Duration>::new()));
        let rec = slept.clone();
        let mk = |script: Vec<Result<u16, TransportError>>, after: Option<Duration>| {
            let s = Scripted::new(script, classify_ok());
            *s.retry_after.lock() = after;
            let rec = rec.clone();
            JevJudge::new(cfg(), s, Arc::new(MemoryJevStore::default()), policy())
                .with_clock(Arc::new(scenarios::fixed_now))
                .with_sleeper(Arc::new(move |d| rec.lock().push(d)))
        };
        let longest = |v: &Vec<Duration>| v.iter().copied().max().unwrap_or_default();
        // The server asks for 7 s.
        mk(vec![Ok(429)], Some(Duration::from_secs(7)))
            .classify_in(&[UNSCOPED], "a", &yes_no())
            .unwrap();
        assert!(
            slept.lock().contains(&Duration::from_secs(7)),
            "{:?}",
            slept.lock()
        );
        slept.lock().clear();
        // An hour is capped.
        mk(vec![Ok(503)], Some(Duration::from_secs(3_600)))
            .classify_in(&[UNSCOPED], "b", &yes_no())
            .unwrap();
        assert_eq!(longest(&slept.lock()), Duration::from_secs(30));
        slept.lock().clear();
        // No header: 0.5 s, 1 s, 2 s.
        mk(vec![Ok(429), Ok(429), Ok(429)], None)
            .classify_in(&[UNSCOPED], "c", &yes_no())
            .unwrap();
        let waits: Vec<Duration> = slept
            .lock()
            .iter()
            .copied()
            .filter(|d| *d >= Duration::from_millis(500))
            .collect();
        assert_eq!(
            waits,
            [
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2)
            ]
        );
    }

    /// Records when each request is sent.
    struct Stamps(PMutex<Vec<Instant>>, serde_json::Value);

    impl JevTransport for Stamps {
        fn post(
            &self,
            _e: &str,
            _k: &str,
            _b: &[u8],
            _t: Duration,
        ) -> Result<HttpReply, TransportError> {
            self.0.lock().push(Instant::now());
            Ok(HttpReply {
                status: 200,
                body: serde_json::to_vec(&self.1).unwrap(),
                retry_after: None,
            })
        }
    }

    #[test]
    fn many_threads_never_send_more_than_ten_requests_in_a_second() {
        let t = Arc::new(Stamps(PMutex::new(Vec::new()), classify_ok()));
        // Really sleeps, at the real clock.
        let j = Arc::new(JevJudge::new(
            cfg(),
            t.clone(),
            Arc::new(MemoryJevStore::default()),
            policy(),
        ));
        let threads: Vec<_> = (0..6)
            .map(|n| {
                let j = j.clone();
                std::thread::spawn(move || {
                    for i in 0..4 {
                        j.classify_in(&[UNSCOPED], &format!("t{n} call {i}"), &yes_no())
                            .unwrap();
                    }
                })
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        let mut at = t.0.lock().clone();
        at.sort();
        assert_eq!(at.len(), 24);
        for w in at.windows(11) {
            assert!(
                w[10].duration_since(w[0]) >= Duration::from_millis(850),
                "11 requests inside {:?}",
                w[10].duration_since(w[0])
            );
        }
    }

    #[test]
    fn a_rebuilt_judge_shares_the_budget_the_rate_and_the_ratio_of_the_old_one() {
        let store = Arc::new(MemoryJevStore::default());
        let gate = Arc::new(Gate {
            open: PMutex::new(false),
            cv: parking_lot::Condvar::new(),
            sent: AtomicUsize::new(0),
            ok: classify_ok(),
        });
        let limits = Arc::new(JevLimits::default());
        let probe = judge(Scripted::ok(), store.clone(), cfg());
        let budget = estimate(&probe, "call-0") * 1.5;
        let build = |gate: Arc<Gate>| {
            Arc::new(
                judge(
                    gate,
                    store.clone(),
                    JevConfig {
                        daily_budget_usd: budget,
                        ..cfg()
                    },
                )
                .with_limits(limits.clone()),
            )
        };
        let old = build(gate.clone());
        let t = {
            let old = old.clone();
            std::thread::spawn(move || old.classify_in(&[UNSCOPED], "call-0", &yes_no()))
        };
        while gate.sent.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The settings changed and the judge was rebuilt while that call is in flight.
        let new = build(gate.clone());
        assert!(matches!(
            new.classify_in(&[UNSCOPED], "call-1", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
        *gate.open.lock() = true;
        gate.cv.notify_all();
        t.join().unwrap().unwrap();
        assert_eq!(gate.sent.load(Ordering::SeqCst), 1);
        assert!(sum_spent(&store) <= budget);
        // What one judge learned about token counts, the other uses.
        let loud = Scripted::new(Vec::new(), classify_ok_tokens(50_000));
        let a = JevJudge::new(cfg(), loud, store.clone(), policy())
            .with_clock(Arc::new(scenarios::fixed_now))
            .with_sleeper(Arc::new(|_| {}))
            .with_limits(limits.clone());
        let before = new.estimate_tokens(300);
        a.classify_in(&[UNSCOPED], "call-2", &yes_no()).ok();
        assert!(new.estimate_tokens(300) > before * 10.0);
    }

    #[test]
    fn the_reservation_covers_every_recorded_request() {
        let j = judge(Scripted::ok(), Arc::new(MemoryJevStore::default()), cfg());
        for raw in [
            include_str!("../../tests/fixtures/jev/compare.json"),
            include_str!("../../tests/fixtures/jev/kind.json"),
            include_str!("../../tests/fixtures/jev/relevance.json"),
            include_str!("../../tests/fixtures/jev/classify.json"),
        ] {
            let f: serde_json::Value = serde_json::from_str(raw).unwrap();
            let bytes = serde_json::to_vec(&f["request"]).unwrap().len();
            let tokens = f["response"]["usage"]["input_tokens"].as_u64().unwrap() as f64;
            assert!(
                j.estimate_tokens(bytes) >= tokens,
                "{bytes} bytes reserve {} tokens but cost {tokens}",
                j.estimate_tokens(bytes)
            );
        }
    }

    #[test]
    fn the_compare_fixture_averages_to_the_expected_numbers() {
        let j = judge(
            Arc::new(Fixtures::load()),
            Arc::new(MemoryJevStore::default()),
            cfg(),
        );
        let (new, existing) = scenarios::compare_case();
        let rel = j.compare(&new, &existing).unwrap();
        // forward 0.99 updates / 0.01 contradicts, reversed 1.0 outdated_by (= updates).
        assert!(
            (rel[0].probs.p("updates") - 0.995).abs() < 1e-9,
            "{:?}",
            rel[0]
        );
        assert!((rel[0].probs.p("contradicts") - 0.005).abs() < 1e-9);
        assert_eq!(rel[0].agreement, Some(true));
        assert!((rel[1].probs.p("unrelated") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_timed_out_attempt_followed_by_a_successful_retry_is_charged_too() {
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::new(vec![Err(TransportError::Timeout)], classify_ok());
        let j = judge(s.clone(), store.clone(), cfg());
        j.classify_in(&[UNSCOPED], "retry me", &yes_no()).unwrap();
        assert_eq!(s.calls.lock().len(), 2);
        let want = estimate(&j, "retry me") + 100.0 * USD_PER_MTOK_INPUT / 1e6;
        assert!(
            (sum_spent(&store) - want).abs() < 1e-12,
            "{} vs {want}",
            sum_spent(&store)
        );
        // The answer is cached as usual.
        j.classify_in(&[UNSCOPED], "retry me", &yes_no()).unwrap();
        assert_eq!(j.stats().cache_hits, 1);
        // With room for only one attempt the retry is not made.
        let day = Arc::new(PMutex::new(scenarios::fixed_now()));
        let store = Arc::new(MemoryJevStore::default());
        let s = Scripted::new(vec![Err(TransportError::Timeout)], classify_ok());
        let budget = estimate(&budget_judge(&s, &store, &day, 1.0), "tight") * 1.5;
        let j = budget_judge(&s, &store, &day, budget);
        assert!(matches!(
            j.classify_in(&[UNSCOPED], "tight", &yes_no()),
            Err(JudgeError::Budget(_))
        ));
        assert_eq!(s.calls.lock().len(), 1);
        assert!(sum_spent(&store) <= budget);
    }

    #[test]
    fn an_unparseable_200_gives_a_fixed_reason_not_the_parsers_words() {
        let s = Scripted::ok();
        *s.raw_ok.lock() = Some(b"{\"answers\": SERVER-SAID-THIS".to_vec());
        let j = judge(s, Arc::new(MemoryJevStore::default()), cfg());
        let e = j.classify_in(&[UNSCOPED], "x", &yes_no()).unwrap_err();
        let shown = format!("{e} {:?}", j.stats());
        assert!(shown.contains("could not read"), "{shown}");
        assert!(
            !shown.contains("SERVER-SAID-THIS") && !shown.contains("expected"),
            "{shown}"
        );
    }
}
