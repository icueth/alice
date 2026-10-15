//! Jev vs the heuristic on the labelled set, and Jev-graded nDCG@10 of recall, with the real
//! embedding model. Network, real model, hard budget $2 for the whole run (enforced by the
//! judge's budget, which counts only this run's spend, across UTC days). The set is synthetic
//! (`fixtures/judge_eval_pairs.jsonl`).
//!
//! Estimate first (no key, no network: the requests are built and counted, never sent):
//!   JEV_EVAL_DRY=1 ALICE_MODELS_DIR=$HOME/.alice/models cargo test -p alice-daemon --release \
//!     --test jev_eval -- --ignored --nocapture
//! Then the run itself:
//!   set -a; . ./.env; set +a
//!   ALICE_MODELS_DIR=$HOME/.alice/models cargo test -p alice-daemon --release --test jev_eval \
//!     -- --ignored --nocapture
//!   unset TYPESAFE_API_KEY
//! Output: the markdown tables on stdout and in `target/m2-jev-eval.md` (or `$ALICE_EVAL_OUT`),
//! the raw (query, candidate) rows in `m2-jev-floor.json` beside it, and every answered Jev call
//! (request hash, response body, tokens, cost, latency; never a header) in `m2-jev-calls.jsonl`
//! beside it. The call log is truncated at the start of a run; with `ALICE_EVAL_REUSE=1` it is
//! read back as a cache instead, and the report says how many requests were replayed.
//!
//! Ranking comparison (Task 19, `docs/benchmarks/m2.md`): with `ALICE_EVAL_GRADES=<file>` the run
//! is search only (no relation or kind calls) and keeps a grade cache in that JSON file, keyed by
//! the query and the memory's text, so a ranking that surfaces a memory nobody graded costs one
//! request for it and not a regrade of the whole set; `ALICE_EVAL_GRID=<k>:<lambda>,...` runs the
//! search once per fusion setting (`search.rrf_k`, `search.boost_exponent`) and prints one line
//! each; `ALICE_EVAL_BUDGET_USD` lowers the judge's budget (default $2).
mod common;

use alice_core::embed::{Embedder, cosine, memory_embed_text};
use alice_core::judge::heuristic::{HeuristicJudge, kind_rules};
use alice_core::judge::jev::{
    FixedPolicy, HttpReply, JevCall, JevConfig, JevJudge, JevStore, JevTransport, MemoryJevStore,
    TransportError,
};
use alice_core::judge::metrics::*;
use alice_core::judge::{
    Distribution, Judge, JudgeError, JudgeMemory, RELEVANCE_ANSWERS, RELEVANCE_PARTIAL,
};
use alice_core::model::Kind;
use alice_core::redact::Redactor;
use alice_core::search::{
    Lists, SearchParams,
    fusion::{Fusion, Weights},
    run,
};
use alice_core::settings::SecretValue;
use alice_core::store::embeddings::VecKey;
use alice_core::store::memories::{self, InsertOutcome, NewMemoryInput};
use alice_core::store::{embeddings, open_writer, projects};
use common::*;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

type RelationSlot = Mutex<Option<(Distribution, Option<bool>)>>;
type GradeTriple = (f64, f64, f64);
type GradeMap = HashMap<(usize, String), GradeTriple>;
/// A grade cache: (query, memory text) to (grade, P(answers), P(partially)).
type GradeCache = HashMap<String, GradeTriple>;

const LABELS: [&str; 4] = ["duplicate", "updates", "contradicts", "unrelated"];
const BUDGET_USD: f64 = 2.0;
const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-1.13.0";
const WORKERS: usize = 4;
const FLOORS: [f32; 5] = [0.70, 0.75, 0.78, 0.80, 0.82];
const MODES: [(&str, Lists, f32); 4] = [
    ("hybrid", Lists::Hybrid, 0.82),
    ("lexical only", Lists::LexicalOnly, 0.82),
    ("vector only", Lists::VectorOnly, 0.82),
    ("vector only, no floor", Lists::VectorOnly, 0.0),
];

/// The judge's budget: `ALICE_EVAL_BUDGET_USD` when set (a malformed value is an error, not the
/// default), else $0.50 for a cached ranking comparison (`ALICE_EVAL_GRADES`), else the full
/// run's $2.
fn eval_budget(var: Option<&str>, cached_grades: bool) -> f64 {
    match var {
        Some(v) => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|b| b.is_finite() && *b >= 0.0)
            .unwrap_or_else(|| panic!("ALICE_EVAL_BUDGET_USD={v:?} is not a dollar amount")),
        None if cached_grades => 0.5,
        None => BUDGET_USD,
    }
}

#[test]
fn the_eval_budget_is_strict_and_defaults_to_fifty_cents_for_cached_grades() {
    assert_eq!(eval_budget(None, false), BUDGET_USD);
    assert_eq!(eval_budget(None, true), 0.5);
    assert_eq!(eval_budget(Some("0.15"), true), 0.15);
    for bad in ["", "abc", "-1", "NaN"] {
        let r = std::panic::catch_unwind(|| eval_budget(Some(bad), true));
        assert!(r.is_err(), "{bad:?} must fail loudly");
    }
}

// ---------------------------------------------------------------------------------------------
// Plumbing: the dry transport, the call log, the thread pool, small statistics.

/// Never sends: the dry run builds each request and counts its bytes.
struct DryTransport;

impl JevTransport for DryTransport {
    fn post(&self, _: &str, _: &str, _: &[u8], _: Duration) -> Result<HttpReply, TransportError> {
        Err(TransportError::Connect("dry run".into()))
    }
}

/// The in-memory store, also appending every call to a JSONL file. The file is truncated at the
/// start unless `reuse`, in which case its calls are replayed as a cache. `spent` and the
/// totals count only this run's own calls, so the budget is per run.
struct LoggedStore {
    inner: MemoryJevStore,
    file: Mutex<Option<std::fs::File>>,
    fresh: Mutex<Vec<JevCall>>,
    replayed: usize,
    replayed_days: BTreeSet<String>,
}

impl LoggedStore {
    fn new(path: Option<&Path>, reuse: bool) -> Self {
        let inner = MemoryJevStore::default();
        let (mut file, mut replayed, mut days) = (None, 0, BTreeSet::new());
        if let Some(path) = path {
            if reuse && let Ok(text) = std::fs::read_to_string(path) {
                for l in text.lines().filter(|l| !l.trim().is_empty()) {
                    if let Ok(c) = serde_json::from_str::<JevCall>(l) {
                        days.insert(c.day.clone());
                        inner.record(&c);
                        replayed += 1;
                    }
                }
            }
            file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(!reuse)
                .append(reuse)
                .open(path)
                .ok();
        }
        Self {
            inner,
            file: Mutex::new(file),
            fresh: Mutex::new(Vec::new()),
            replayed,
            replayed_days: days,
        }
    }
}

impl JevStore for LoggedStore {
    fn cached(&self, hash: &str) -> Option<String> {
        self.inner.cached(hash)
    }
    /// The whole run's fresh spend, whatever the UTC day: the $2 cap is per run, so a run that
    /// crosses midnight does not get a second budget.
    fn spent(&self, _day: &str) -> Result<f64, String> {
        Ok(self.fresh.lock().iter().map(|c| c.usd).sum())
    }
    fn record(&self, call: &JevCall) {
        self.inner.record(call);
        self.fresh.lock().push(call.clone());
        if let Some(f) = self.file.lock().as_mut() {
            let _ = writeln!(f, "{}", serde_json::to_string(call).unwrap());
        }
    }
}

/// Wilson 95% interval of a proportion.
fn wilson(k: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let (k, n, z) = (k as f64, n as f64, 1.96f64);
    let p = k / n;
    let d = 1.0 + z * z / n;
    let c = p + z * z / (2.0 * n);
    let m = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt();
    ((c - m) / d, (c + m) / d)
}

/// "62% (29-80%, 8/13)": a share with its Wilson interval and counts.
fn share(k: usize, n: usize) -> String {
    if n == 0 {
        return "–".into();
    }
    let (lo, hi) = wilson(k, n);
    format!(
        "{:.0}% ({:.0}–{:.0}%, {k}/{n})",
        100.0 * k as f64 / n as f64,
        100.0 * lo,
        100.0 * hi
    )
}

/// Run `n` tasks on `workers` threads; `f(i)` is task `i`. Stops handing out tasks once `stop`
/// is set.
fn parallel<F: Fn(usize) + Sync>(n: usize, workers: usize, stop: &AtomicBool, f: F) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n || stop.load(Ordering::SeqCst) {
                        break;
                    }
                    f(i);
                }
            });
        }
    });
}

/// A deterministic xorshift generator (bootstrap resampling, batch shuffling).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i + 1));
        }
    }
}

/// Mean paired difference `a - b` with a 95% bootstrap interval over the queries.
fn paired_bootstrap(a: &[f64], b: &[f64]) -> (f64, f64, f64) {
    let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    if d.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut means: Vec<f64> = (0..10_000)
        .map(|_| {
            mean(
                &(0..d.len())
                    .map(|_| d[rng.below(d.len())])
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    (
        mean(&d),
        percentile(&mut means, 0.025),
        percentile(&mut means, 0.975),
    )
}

fn stats4(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "–".into();
    }
    format!(
        "{:.3} / {:.3} / {:.3} / {:.3}",
        percentile(v, 0.0),
        percentile(v, 0.1),
        percentile(v, 0.5),
        percentile(v, 0.9)
    )
}

fn mem(
    id: &str,
    v: &serde_json::Value,
    now: chrono::DateTime<chrono::Utc>,
    sim: Option<f32>,
) -> JudgeMemory {
    JudgeMemory {
        id: id.into(),
        project: "eval".into(),
        kind: Kind::parse(v["kind"].as_str().unwrap()).unwrap(),
        title: v["title"].as_str().unwrap().into(),
        body: v["body"].as_str().unwrap().into(),
        importance: v["importance"].as_u64().unwrap_or(3) as u8,
        created_at: (now - chrono::Duration::days(v["days_ago"].as_i64().unwrap())).to_rfc3339(),
        similarity: sim,
    }
}

// ---------------------------------------------------------------------------------------------
// The judge and what a run needs around it.

struct Env {
    dry: bool,
    workers: usize,
    now: chrono::DateTime<chrono::Utc>,
    out_path: PathBuf,
    out_dir: PathBuf,
    store: Arc<LoggedStore>,
    jev: JevJudge,
    stop: AtomicBool,
    stopped: Mutex<Option<String>>,
    errors: Mutex<Vec<String>>,
    /// Dry run only: (phase, requests, estimated USD).
    dry_phases: Mutex<BTreeMap<&'static str, (usize, f64, usize)>>,
}

impl Env {
    fn new() -> (Self, alice_embed::OnnxEmbedder) {
        let dry = std::env::var("JEV_EVAL_DRY").is_ok_and(|v| v == "1");
        let reuse = std::env::var("ALICE_EVAL_REUSE").is_ok_and(|v| v == "1");
        let models = PathBuf::from(std::env::var("ALICE_MODELS_DIR").expect("ALICE_MODELS_DIR"));
        let out_path = PathBuf::from(std::env::var("ALICE_EVAL_OUT").unwrap_or_else(|_| {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/m2-jev-eval.md").into()
        }));
        let out_dir = out_path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&out_dir).unwrap();
        let e =
            alice_embed::OnnxEmbedder::new(&models, alice_embed::OnnxOptions::default()).unwrap();
        let calls_path = out_dir.join("m2-jev-calls.jsonl");
        let store = Arc::new(LoggedStore::new(
            (!dry).then_some(calls_path.as_path()),
            reuse,
        ));
        let (transport, key): (Arc<dyn JevTransport>, SecretValue) = if dry {
            (Arc::new(DryTransport), SecretValue::new("dry-run"))
        } else {
            (
                Arc::new(alice_daemon::judge_http::ReqwestTransport::new().unwrap()),
                SecretValue::new(
                    std::env::var("TYPESAFE_API_KEY").expect("set -a; . ./.env; set +a"),
                ),
            )
        };
        let jev = JevJudge::new(
            JevConfig {
                endpoint: ENDPOINT.into(),
                model: MODEL.into(),
                api_key: key,
                rate_per_sec: if dry { 1000.0 } else { 8.0 },
                timeout: Duration::from_secs(30),
                daily_budget_usd: eval_budget(
                    std::env::var("ALICE_EVAL_BUDGET_USD").ok().as_deref(),
                    std::env::var("ALICE_EVAL_GRADES").is_ok(),
                ),
                max_retries: 3,
            },
            transport,
            store.clone(),
            // Allow-all test policy with the built-in redactor: the set is synthetic.
            Arc::new(FixedPolicy::allow_all(Redactor::builtin())),
        );
        (
            Self {
                dry,
                workers: if dry { 1 } else { WORKERS },
                now: chrono::Utc::now(),
                out_path,
                out_dir,
                store,
                jev,
                stop: AtomicBool::new(false),
                stopped: Mutex::new(None),
                errors: Mutex::new(Vec::new()),
                dry_phases: Mutex::new(BTreeMap::new()),
            },
            e,
        )
    }

    fn note_err(&self, what: &str, err: JudgeError) {
        if let JudgeError::Budget(m) = &err {
            *self.stopped.lock() = Some(m.clone());
            self.stop.store(true, Ordering::SeqCst);
        }
        self.errors.lock().push(format!("{what}: {err}"));
    }

    /// Dry run: one worker, so `last_request` is the request of the call that just ended.
    fn dry_note(&self, phase: &'static str) {
        if self.dry {
            let len = self.jev.last_request().map_or(0, |b| b.len());
            let mut p = self.dry_phases.lock();
            let e = p.entry(phase).or_default();
            e.0 += 1;
            e.1 += self.jev.estimate_usd(len);
            e.2 += len;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Relation and kind.

struct Relations {
    relation: Vec<RelationSlot>,
    kind: Vec<Mutex<Option<Distribution>>>,
}

fn run_relations(
    env: &Env,
    pairs: &[serde_json::Value],
    news: &[JudgeMemory],
    olds: &[JudgeMemory],
) -> Relations {
    let n = pairs.len();
    let r = Relations {
        relation: (0..n).map(|_| Mutex::new(None)).collect(),
        kind: (0..n).map(|_| Mutex::new(None)).collect(),
    };
    parallel(n, env.workers, &env.stop, |i| {
        let id = pairs[i]["id"].as_str().unwrap();
        match env.jev.compare(&news[i], std::slice::from_ref(&olds[i])) {
            Ok(mut rel) => {
                let rel = rel.remove(0);
                *r.relation[i].lock() = Some((rel.probs, rel.agreement));
            }
            Err(err) => env.note_err(id, err),
        }
        env.dry_note("relation");
        // The kind is asked of a memory whose kind is still the default (`note`): the request
        // states the memory's kind, and the true one must not be in it.
        let blank = JudgeMemory {
            kind: Kind::Note,
            ..news[i].clone()
        };
        match env.jev.classify_kind(&blank) {
            Ok(d) => *r.kind[i].lock() = Some(d),
            Err(err) => env.note_err(id, err),
        }
        env.dry_note("kind");
    });
    r
}

struct Run {
    gold: Vec<String>,
    pred: Vec<String>,
    probs: Vec<Distribution>,
}

impl Run {
    fn new() -> Self {
        Self {
            gold: vec![],
            pred: vec![],
            probs: vec![],
        }
    }
    fn push(&mut self, gold: &str, d: Distribution) {
        self.gold.push(gold.into());
        self.pred
            .push(d.best().map(|b| b.0).unwrap_or_else(|| "unrelated".into()));
        self.probs.push(d);
    }
    /// Auto-apply at `t` for non-`unrelated` predictions: (applied, right, coverage).
    fn auto_at(&self, t: f64) -> (usize, usize, f64) {
        let applied: Vec<usize> = (0..self.gold.len())
            .filter(|&i| self.pred[i] != "unrelated" && self.probs[i].p(&self.pred[i]) >= t)
            .collect();
        let right = applied
            .iter()
            .filter(|&&i| self.pred[i] == self.gold[i])
            .count();
        let actionable = self
            .gold
            .iter()
            .filter(|g| *g != "unrelated")
            .count()
            .max(1);
        (
            applied.len(),
            right,
            applied.len() as f64 / actionable as f64,
        )
    }
    fn correct(&self) -> usize {
        self.gold
            .iter()
            .zip(&self.pred)
            .filter(|(g, p)| g == p)
            .count()
    }
    /// A judge that gives probabilities: all columns.
    fn row(&self, name: &str) -> String {
        let (applied, right, cov) = self.auto_at(0.85);
        let (lo, hi) = wilson(self.correct(), self.gold.len());
        format!(
            "| {name} | {} | {:.3} ({:.3}–{:.3}) | {:.3} | {:.3} | {:.3} | {:.0}% | {} |",
            self.gold.len(),
            accuracy(&self.gold, &self.pred),
            lo,
            hi,
            macro_f1(&self.gold, &self.pred, &LABELS),
            brier(&self.gold, &self.probs, &LABELS),
            ece(&self.gold, &self.probs, 10),
            cov * 100.0,
            share(right, applied)
        )
    }
    /// A baseline that names a label without a probability.
    fn row_labels_only(&self, name: &str) -> String {
        let (lo, hi) = wilson(self.correct(), self.gold.len());
        format!(
            "| {name} | {} | {:.3} ({:.3}–{:.3}) | {:.3} | – | – | – | – |",
            self.gold.len(),
            accuracy(&self.gold, &self.pred),
            lo,
            hi,
            macro_f1(&self.gold, &self.pred, &LABELS)
        )
    }
    fn confusion(&self, name: &str) -> String {
        let mut s = format!(
            "\n{name}: rows are the gold label, columns the prediction\n\n| gold \\ predicted | duplicate | updates | contradicts | unrelated | recall |\n|---|---|---|---|---|---|\n"
        );
        for g in LABELS {
            let row: Vec<usize> = LABELS
                .iter()
                .map(|p| {
                    self.gold
                        .iter()
                        .zip(&self.pred)
                        .filter(|(gg, pp)| gg.as_str() == g && pp.as_str() == *p)
                        .count()
                })
                .collect();
            let total: usize = row.iter().sum();
            s.push_str(&format!(
                "| {g} | {} | {} | {} | {} | {:.0}% |\n",
                row[0],
                row[1],
                row[2],
                row[3],
                100.0 * row[LABELS.iter().position(|l| *l == g).unwrap()] as f64
                    / total.max(1) as f64
            ));
        }
        s
    }
}

fn report_relation(
    pairs: &[serde_json::Value],
    news: &[JudgeMemory],
    olds: &[JudgeMemory],
    r: &Relations,
) -> (String, usize, usize) {
    let heuristic = HeuristicJudge::default();
    let fitted = metadata_fitted(pairs);
    let (mut h, mut j, mut meta_rule, mut meta_fit) =
        (Run::new(), Run::new(), Run::new(), Run::new());
    let mut per_lang: BTreeMap<String, (Run, Run)> = BTreeMap::new();
    let (mut asked, mut agree, mut agree_right, mut disagree_right) = (0usize, 0, 0, 0);
    for (i, p) in pairs.iter().enumerate() {
        let label = p["label"].as_str().unwrap();
        let Some((jd, agreement)) = r.relation[i].lock().clone() else {
            continue;
        };
        let hd = heuristic
            .compare(&news[i], std::slice::from_ref(&olds[i]))
            .unwrap()
            .remove(0)
            .probs;
        asked += 1;
        let right = jd.best().is_some_and(|b| b.0 == label);
        if agreement == Some(true) {
            agree += 1;
            agree_right += usize::from(right);
        } else {
            disagree_right += usize::from(right);
        }
        meta_rule.push(label, Distribution::certain(metadata_rule(p)));
        meta_fit.push(label, Distribution::certain(&fitted[i]));
        h.push(label, hd.clone());
        j.push(label, jd.clone());
        let e = per_lang
            .entry(p["lang"].as_str().unwrap().into())
            .or_insert_with(|| (Run::new(), Run::new()));
        e.0.push(label, hd);
        e.1.push(label, jd);
    }
    let mut s = String::from(
        "### Relation\n\n| Judge | Pairs | Accuracy (95% Wilson) | Macro-F1 | Brier | ECE | Auto-apply coverage at 0.85 | Auto-apply precision at 0.85 (95% Wilson) |\n|---|---|---|---|---|---|---|---|\n",
    );
    s.push_str(&format!(
        "{}\n{}\n{}\n{}\n",
        meta_rule.row_labels_only("metadata only: fixed rule (title, age order, kind; no text)"),
        meta_fit.row_labels_only("metadata only: best table fitted on this set (upper bound)"),
        h.row("heuristic"),
        j.row(MODEL)
    ));
    s.push_str(&h.confusion("Heuristic"));
    s.push_str(&j.confusion("Jev"));
    s.push_str("\n| Language | Pairs | Heuristic accuracy | Jev accuracy |\n|---|---|---|---|\n");
    for (lang, (hh, jj)) in &per_lang {
        s.push_str(&format!(
            "| {lang} | {} | {:.3} | {:.3} |\n",
            hh.gold.len(),
            accuracy(&hh.gold, &hh.pred),
            accuracy(&jj.gold, &jj.pred)
        ));
    }
    s.push_str("\nAuto-apply sweep (non-`unrelated` predictions with P at or above the threshold; coverage is the share of the actionable pairs; precision is the share right, with its 95% Wilson interval and counts). **The set is balanced (25% unrelated pairs); real traffic is mostly unrelated pairs, and no unrelated pair is ever applied here, so precision on real traffic can be lower.**\n\n| Threshold | Heuristic coverage / precision | Jev coverage / precision |\n|---|---|---|\n");
    for t in [0.5, 0.7, 0.85, 0.9, 0.95, 0.99] {
        let (ha, hr, hc) = h.auto_at(t);
        let (ja, jr, jc) = j.auto_at(t);
        s.push_str(&format!(
            "| {t:.2} | {:.0}% / {} | {:.0}% / {} |\n",
            hc * 100.0,
            share(hr, ha),
            jc * 100.0,
            share(jr, ja)
        ));
    }
    s.push_str(&format!(
        "\nForward/reversed agreement (Jev): {:.1}% of {asked} pairs; accuracy when they agree {} , when they disagree {}\n",
        100.0 * agree as f64 / asked.max(1) as f64,
        share(agree_right, agree),
        share(disagree_right, asked - agree)
    ));
    (s, asked, j.gold.len())
}

fn report_kind(
    pairs: &[serde_json::Value],
    news: &[JudgeMemory],
    r: &Relations,
) -> (String, usize) {
    let (mut gold, mut kh, mut kj) = (vec![], vec![], vec![]);
    for (i, p) in pairs.iter().enumerate() {
        if let Some(kd) = r.kind[i].lock().clone() {
            gold.push(p["new_kind"].as_str().unwrap().to_string());
            kh.push(kind_rules(&news[i].title, &news[i].body).best().unwrap().0);
            kj.push(kd.best().map(|b| b.0).unwrap_or_else(|| "note".into()));
        }
    }
    let keep: Vec<usize> = (0..gold.len()).filter(|&i| gold[i] != "note").collect();
    let pick = |v: &[String]| -> Vec<String> { keep.iter().map(|&i| v[i].clone()).collect() };
    let s = format!(
        "\n### Kind classification\n\nThe memory is sent with the default kind (`note`) so the request does not state the answer.\n\nKind accuracy: heuristic {:.3}, Jev {:.3} ({} memories); without the {} gold `note` memories (the default the request states): heuristic {:.3}, Jev {:.3}\n",
        accuracy(&gold, &kh),
        accuracy(&gold, &kj),
        gold.len(),
        gold.len() - keep.len(),
        accuracy(&pick(&gold), &pick(&kh)),
        accuracy(&pick(&gold), &pick(&kj)),
    );
    (s, kj.len())
}

// ---------------------------------------------------------------------------------------------
// Search: the corpus, the queries, the grading.

struct Corpus {
    c: Connection,
    idx: alice_core::vector::VectorIndex,
    project_id: String,
    by_id: HashMap<String, JudgeMemory>,
    vec_of: HashMap<String, Vec<f32>>,
    /// Every memory id, in insertion order.
    ids: Vec<String>,
    _dir: tempfile::TempDir,
}

fn build_corpus(
    e: &alice_embed::OnnxEmbedder,
    pairs: &[serde_json::Value],
    vecs: &[Vec<f32>],
    now: chrono::DateTime<chrono::Utc>,
) -> Corpus {
    let dir = tempfile::tempdir().unwrap();
    let c = open_writer(&dir.path().join("a.db")).unwrap();
    let proj = projects::upsert_project(
        &c,
        &alice_core::project::ProjectRef {
            key: "eval".into(),
            slug: "eval".into(),
            name: "eval".into(),
            root: None,
            remote: None,
        },
    )
    .unwrap();
    let idx = alice_core::vector::VectorIndex::for_model(e.model_id(), e.dim()).unwrap();
    let (mut by_id, mut vec_of, mut ids) = (HashMap::new(), HashMap::new(), Vec::new());
    for (i, p) in pairs.iter().enumerate() {
        for (s, side) in ["new", "old"].into_iter().enumerate() {
            let m = mem("", &p[side], now, None);
            let prepared = memories::prepare(
                NewMemoryInput {
                    content: m.body.clone(),
                    title: Some(m.title.clone()),
                    kind: Some(m.kind),
                    created_at: Some(m.created_at.clone()),
                    ..Default::default()
                },
                &proj,
                format!("{i:013}-0000-e"),
            )
            .unwrap();
            if let InsertOutcome::New { id, seq } = memories::insert(&c, &prepared).unwrap() {
                let v = vecs[2 * i + s].clone();
                embeddings::put(&c, VecKey::Memory(seq), e.model_id(), &v).unwrap();
                idx.upsert(VecKey::Memory(seq), &v).unwrap();
                by_id.insert(
                    id.clone(),
                    JudgeMemory {
                        id: id.clone(),
                        ..m
                    },
                );
                vec_of.insert(id.clone(), v);
                ids.push(id);
            }
        }
    }
    Corpus {
        c,
        idx,
        project_id: proj.id,
        by_id,
        vec_of,
        ids,
        _dir: dir,
    }
}

struct Q {
    text: String,
    kind: String,
    lang: String,
    answerable: bool,
    rankings: Vec<Vec<String>>,
    /// The four modes' candidates (the pool of the nDCG ideal).
    pool: Vec<String>,
    /// What Jev grades: for cross-lingual and paraphrase queries the whole corpus, for the
    /// others the pool plus the vector top 20 without a floor.
    graded: Vec<String>,
    full: bool,
    /// In the lexical lists' top 50 (a lexical match passes the floor at any similarity).
    lexical: HashSet<String>,
    qvec: Vec<f32>,
}

fn run_queries(
    env: &Env,
    e: &alice_embed::OnnxEmbedder,
    corpus: &Corpus,
    queries: &[serde_json::Value],
    fusion: Fusion,
) -> Vec<Q> {
    let search = |q: &str, lists: Lists, floor: f32, limit: usize| -> Vec<String> {
        let params = SearchParams {
            query: q.into(),
            current_project_id: Some(corpus.project_id.clone()),
            limit,
            relevance_floor: floor,
            raw_min_results: 0,
            lists,
            fusion,
            ..Default::default()
        };
        run(
            &corpus.c,
            Some(&corpus.idx),
            e,
            &Weights::default(),
            &params,
            env.now,
        )
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.id)
        .collect()
    };
    queries
        .iter()
        .enumerate()
        .map(|(qi, q)| {
            let text = q["q"].as_str().unwrap().to_string();
            let kind: String = q["kind"].as_str().unwrap().into();
            let rankings: Vec<Vec<String>> = MODES
                .iter()
                .map(|(_, lists, floor)| search(&text, *lists, *floor, 10))
                .collect();
            let mut pool: Vec<String> = rankings.concat();
            pool.sort();
            pool.dedup();
            let full = kind != "same";
            let mut graded = if full {
                corpus.ids.clone()
            } else {
                let mut g = pool.clone();
                g.extend(search(&text, Lists::VectorOnly, 0.0, 20));
                g.sort();
                g.dedup();
                g
            };
            // Batches of ten in a fixed shuffled order, so a pair's two memories do not always
            // share a batch.
            Rng(0xC0FF_EE00 + qi as u64 + 1).shuffle(&mut graded);
            Q {
                qvec: e.embed_query(&text).unwrap(),
                lexical: search(&text, Lists::LexicalOnly, 0.82, 50)
                    .into_iter()
                    .collect(),
                lang: q["lang"].as_str().unwrap().into(),
                answerable: q.get("answerable").and_then(|a| a.as_bool()) != Some(false),
                text,
                kind,
                rankings,
                pool,
                graded,
                full,
            }
        })
        .collect()
}

/// Grade every graded (query, candidate) in batches of ten: 2 × P(answers) + P(partially).
fn grade_all(env: &Env, by_id: &HashMap<String, JudgeMemory>, qs: &[Q]) -> GradeMap {
    let tasks: Vec<(usize, Vec<String>)> = qs
        .iter()
        .enumerate()
        .flat_map(|(qi, q)| {
            q.graded
                .chunks(10)
                .map(move |b| (qi, b.to_vec()))
                .collect::<Vec<_>>()
        })
        .collect();
    let grades: Mutex<GradeMap> = Mutex::new(HashMap::new());
    parallel(tasks.len(), env.workers, &env.stop, |t| {
        let (qi, ids) = &tasks[t];
        let ms: Vec<JudgeMemory> = ids.iter().map(|id| by_id[id].clone()).collect();
        match env.jev.grade_relevance("eval", &qs[*qi].text, &ms) {
            Ok(ds) => {
                let mut g = grades.lock();
                for (id, d) in ids.iter().zip(ds) {
                    g.insert(
                        (*qi, id.clone()),
                        (
                            2.0 * d.p(RELEVANCE_ANSWERS) + d.p(RELEVANCE_PARTIAL),
                            d.p(RELEVANCE_ANSWERS),
                            d.p(RELEVANCE_PARTIAL),
                        ),
                    );
                }
            }
            Err(err) => env.note_err(&qs[*qi].text, err),
        }
        env.dry_note(if qs[*qi].full {
            "grading (full corpus)"
        } else {
            "grading (pool)"
        });
    });
    grades.into_inner()
}

fn report_search(qs: &[Q], grades: &GradeMap) -> String {
    let complete = |q: &Q, qi: usize| {
        q.graded
            .iter()
            .all(|id| grades.contains_key(&(qi, id.clone())))
    };
    let mut per_mode: Vec<Vec<(usize, f64)>> = vec![vec![]; MODES.len()];
    let (mut incomplete, mut unanswerable_rows) = (0, String::new());
    let mut no_relevant = Vec::new();
    for (qi, q) in qs.iter().enumerate() {
        if !complete(q, qi) {
            incomplete += 1;
            continue;
        }
        let g = |id: &String| grades[&(qi, id.clone())].0;
        let pool_grades: Vec<f64> = q.pool.iter().map(g).collect();
        for (m, ranking) in q.rankings.iter().enumerate() {
            let gr: Vec<f64> = ranking.iter().map(g).collect();
            per_mode[m].push((qi, ndcg_at(&gr, &pool_grades, 10)));
        }
        if !q.answerable {
            let best = |m: usize| q.rankings[m].iter().map(g).fold(0.0f64, f64::max);
            unanswerable_rows.push_str(&format!(
                "| {} | {} | {:.2} ({}) | {:.2} ({}) | {:.2} ({}) |\n",
                q.text,
                q.lang,
                best(0),
                q.rankings[0].len(),
                best(2),
                q.rankings[2].len(),
                q.graded.iter().map(g).fold(0.0f64, f64::max),
                q.graded.len()
            ));
        } else if !q.graded.iter().any(|id| grades[&(qi, id.clone())].1 >= 0.5) {
            no_relevant.push(q.text.clone());
        }
    }
    let sel = |m: usize, f: &dyn Fn(&Q) -> bool| -> Vec<f64> {
        per_mode[m]
            .iter()
            .filter(|(qi, _)| f(&qs[*qi]))
            .map(|(_, v)| *v)
            .collect()
    };
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let answerable = |q: &Q| q.answerable;
    let hard = |q: &Q| q.kind != "same";
    let all = |_: &Q| true;
    let mut s = String::from(
        "\n### Search, Jev-graded\n\nGrade = 2 × P(answers) + P(partially), used as the gain `2^g - 1`; the ideal ranking is the best of every candidate any of the four modes returned (pooling). The answerable-queries column leads: for a query nothing answers, the ideal is near zero and an empty result scores 0, so the all-queries column rewards returning junk.\n\n| Recall mode | nDCG@10, answerable queries | nDCG@10, cross-lingual and paraphrase | nDCG@10, all queries |\n|---|---|---|---|\n",
    );
    for (m, (name, _, floor)) in MODES.iter().enumerate() {
        s.push_str(&format!(
            "| {name}{} | {:.3} | {:.3} | {:.3} |\n",
            if matches!(m, 0 | 2) {
                format!(" (floor {floor:.2})")
            } else {
                String::new()
            },
            mean(&sel(m, &answerable)),
            mean(&sel(m, &hard)),
            mean(&sel(m, &all))
        ));
    }
    s.push_str(&format!(
        "\nQueries: {} answerable, {} cross-lingual or paraphrase (all answerable), {} in all.\n\nPaired bootstrap (10,000 resamples over queries) of the nDCG@10 difference, 95% interval; a difference whose interval includes 0 is not established:\n\n| Difference | Answerable queries | Cross-lingual and paraphrase |\n|---|---|---|\n",
        sel(0, &answerable).len(),
        sel(0, &hard).len(),
        sel(0, &all).len()
    ));
    for (label, a, b) in [
        ("hybrid − vector only, no floor", 0, 3),
        ("hybrid − vector only (floor 0.82)", 0, 2),
        ("hybrid − lexical only", 0, 1),
    ] {
        let f = |filter: &dyn Fn(&Q) -> bool| {
            let (d, lo, hi) = paired_bootstrap(&sel(a, filter), &sel(b, filter));
            format!("{d:+.3} ({lo:+.3} to {hi:+.3})")
        };
        s.push_str(&format!(
            "| {label} | {} | {} |\n",
            f(&answerable),
            f(&hard)
        ));
    }
    if incomplete > 0 {
        s.push_str(&format!(
            "\n{incomplete} queries were left out because Jev did not grade all of their candidates.\n"
        ));
    }
    if !no_relevant.is_empty() {
        s.push_str(&format!(
            "\nAnswerable queries for which Jev found no memory that answers them (P(answers) < 0.5 for the whole graded set), so their nDCG compares near-zero grades: {}\n",
            no_relevant.join("; ")
        ));
    }
    s.push_str("\nQueries no memory answers: best Jev grade among the results of hybrid / vector only (floor 0.82) / all graded candidates (result count in brackets); a grade near 0 is right.\n\n| Query | Language | Hybrid | Vector only | Graded |\n|---|---|---|---|---|\n");
    s.push_str(&unanswerable_rows);
    s
}

// ---------------------------------------------------------------------------------------------
// The floor evidence for Task 13.

#[derive(Clone)]
struct Row {
    query: String,
    qkind: String,
    lang: String,
    id: String,
    cosine: f64,
    grade: f64,
    p_answers: f64,
    p_partially: f64,
    lexical: bool,
    in_modes: bool,
    full: bool,
}

fn floor_rows(corpus: &Corpus, qs: &[Q], grades: &GradeMap) -> Vec<Row> {
    let mut rows = Vec::new();
    for (qi, q) in qs.iter().enumerate() {
        let in_modes: HashSet<&String> = q.pool.iter().collect();
        for id in &q.graded {
            let Some(&(grade, pa, pp)) = grades.get(&(qi, id.clone())) else {
                continue;
            };
            rows.push(Row {
                query: q.text.clone(),
                qkind: q.kind.clone(),
                lang: q.lang.clone(),
                id: id.clone(),
                // The model's exact cosine; the index's own similarity (int8) can differ a little.
                cosine: f64::from(cosine(&q.qvec, &corpus.vec_of[id])),
                grade,
                p_answers: pa,
                p_partially: pp,
                lexical: q.lexical.contains(id),
                in_modes: in_modes.contains(id),
                full: q.full,
            });
        }
    }
    rows
}

/// Cosine distribution and the effect of each floor, for pairs Jev calls relevant (`is_rel`)
/// and the rest.
fn floor_table(rows: &[&Row], is_rel: &dyn Fn(&Row) -> bool, title: &str, other: &str) -> String {
    let rel: Vec<&&Row> = rows.iter().filter(|r| is_rel(r)).collect();
    let non: Vec<&&Row> = rows.iter().filter(|r| !is_rel(r)).collect();
    let mut rc: Vec<f64> = rel.iter().map(|r| r.cosine).collect();
    let mut nc: Vec<f64> = non.iter().map(|r| r.cosine).collect();
    let mut s = format!(
        "\n**{title}**: {} relevant, {} {other}. Cosine min / p10 / median / p90: relevant {}; {other} {}.\n\n| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | {other} admitted (cosine) | {other} admitted (cosine or lexical) |\n|---|---|---|---|---|\n",
        rel.len(),
        non.len(),
        stats4(&mut rc),
        stats4(&mut nc)
    );
    for fl in FLOORS {
        let by_cos = |r: &Row| r.cosine >= f64::from(fl);
        let by_any = |r: &Row| r.cosine >= f64::from(fl) || r.lexical;
        let count = |v: &[&&Row], f: &dyn Fn(&Row) -> bool| v.iter().filter(|r| f(r)).count();
        s.push_str(&format!(
            "| {fl:.2} | {} | {} | {} | {} |\n",
            share(count(&rel, &by_cos), rel.len()),
            share(count(&rel, &by_any), rel.len()),
            share(count(&non, &by_cos), non.len()),
            share(count(&non, &by_any), non.len())
        ));
    }
    s
}

fn report_floor(rows: &[Row]) -> String {
    let strict = |r: &Row| r.p_answers >= 0.5;
    let loose = |r: &Row| r.p_answers + r.p_partially >= 0.5;
    let queries = |full: bool| -> HashSet<&str> {
        rows.iter()
            .filter(|r| r.full == full)
            .map(|r| r.query.as_str())
            .collect()
    };
    let mut s = String::from("\n### Relevance floor evidence for Task 13\n");
    s.push_str(&format!(
        "\nThe cosine is the real model's exact cosine between the query and the memory. \"Lexical\" means the memory is in the top 50 of the lexical-only search (a lexical match passes the floor at any similarity). Shares come with 95% Wilson intervals and counts.\n\n#### A. Whole corpus: every memory graded for the {} cross-lingual and paraphrase queries\n\nJev graded all 432 memories against each of these queries, so recall at a floor is measured against every memory that answers the query, retrieved or not. \"Other\" is every other memory of the corpus, so the admission columns are the real admission rate of a floor over this corpus.\n",
        queries(true).len()
    ));
    for (name, kinds) in [
        ("cross-lingual and paraphrase", vec!["cross", "para"]),
        ("cross-lingual", vec!["cross"]),
        ("paraphrase", vec!["para"]),
    ] {
        let sel: Vec<&Row> = rows
            .iter()
            .filter(|r| r.full && kinds.contains(&r.qkind.as_str()))
            .collect();
        s.push_str(&floor_table(
            &sel,
            &strict,
            &format!("{name}, relevant = Jev says the memory answers the query (P(answers) ≥ 0.5)"),
            "other memories",
        ));
    }
    let sel: Vec<&Row> = rows.iter().filter(|r| r.full).collect();
    s.push_str(&floor_table(
        &sel,
        &loose,
        "cross-lingual and paraphrase, relevant = answers or is about the subject (P(answers) + P(partially) ≥ 0.5)",
        "other memories",
    ));
    let same: Vec<&Row> = rows.iter().filter(|r| !r.full).collect();
    s.push_str(&format!(
        "\n#### B. Among retrieved results: the {} same-language queries\n\nOnly the candidates the four recall modes returned plus the vector top 20 without a floor were graded ({} pairs), so every row has a cosine of at least {:.2} and the 0.70 and 0.75 rows are 100% by construction. Recall is conditional on being retrieved; the admission columns describe retrieved candidates (a harder set than the whole corpus).\n",
        queries(false).len(),
        same.len(),
        same.iter().map(|r| r.cosine).fold(f64::MAX, f64::min)
    ));
    s.push_str(&floor_table(
        &same,
        &strict,
        "same-language queries, among retrieved results, relevant = answers (P(answers) ≥ 0.5)",
        "other graded",
    ));
    s.push_str(&floor_table(
        &same,
        &loose,
        "same-language queries, among retrieved results, relevant = answers or about the subject",
        "other graded",
    ));
    s
}

// ---------------------------------------------------------------------------------------------
// Ranking comparison with a grade cache (Task 19).

/// `ALICE_EVAL_GRID=60:1.0,20:0.5`: the fusion settings to run, or just the defaults.
fn grid_rows() -> Vec<Fusion> {
    match std::env::var("ALICE_EVAL_GRID") {
        Err(_) => vec![Fusion::default()],
        Ok(v) => v
            .split(',')
            .map(|r| {
                let (k, l) = r.split_once(':').expect("ALICE_EVAL_GRID=<k>:<lambda>,...");
                Fusion {
                    rrf_k: k.trim().parse().unwrap(),
                    boost_exponent: l.trim().parse().unwrap(),
                }
            })
            .collect(),
    }
}

/// What a grade is stored under: the query and the text of the memory, not its random id.
fn grade_key(query: &str, m: &JudgeMemory) -> String {
    format!("{query}\u{1f}{:?}\u{1f}{}\u{1f}{}", m.kind, m.title, m.body)
}

/// [`grade_all`] for the candidates the cache does not hold (in batches of ten, per query),
/// then everything from the cache.
fn grade_missing(
    env: &Env,
    by_id: &HashMap<String, JudgeMemory>,
    qs: &[Q],
    cache: &mut GradeCache,
) -> (GradeMap, usize) {
    let tasks: Vec<(usize, Vec<String>)> = qs
        .iter()
        .enumerate()
        .flat_map(|(qi, q)| {
            let missing: Vec<String> = q
                .graded
                .iter()
                .filter(|id| !cache.contains_key(&grade_key(&q.text, &by_id[*id])))
                .cloned()
                .collect();
            missing
                .chunks(10)
                .map(|b| (qi, b.to_vec()))
                .collect::<Vec<_>>()
        })
        .collect();
    let fresh: Mutex<Vec<(String, GradeTriple)>> = Mutex::new(Vec::new());
    parallel(tasks.len(), env.workers, &env.stop, |t| {
        let (qi, ids) = &tasks[t];
        let ms: Vec<JudgeMemory> = ids.iter().map(|id| by_id[id].clone()).collect();
        match env.jev.grade_relevance("eval", &qs[*qi].text, &ms) {
            Ok(ds) => {
                let mut f = fresh.lock();
                for (m, d) in ms.iter().zip(ds) {
                    f.push((
                        grade_key(&qs[*qi].text, m),
                        (
                            2.0 * d.p(RELEVANCE_ANSWERS) + d.p(RELEVANCE_PARTIAL),
                            d.p(RELEVANCE_ANSWERS),
                            d.p(RELEVANCE_PARTIAL),
                        ),
                    ));
                }
            }
            Err(err) => env.note_err(&qs[*qi].text, err),
        }
    });
    let requested: usize = tasks.iter().map(|t| t.1.len()).sum();
    cache.extend(fresh.into_inner());
    let mut grades = GradeMap::new();
    for (qi, q) in qs.iter().enumerate() {
        for id in &q.graded {
            if let Some(g) = cache.get(&grade_key(&q.text, &by_id[id])) {
                grades.insert((qi, id.clone()), *g);
            }
        }
    }
    (grades, requested)
}

/// Mean hybrid nDCG@10 over the answerable queries and over the cross-lingual and paraphrase
/// queries (the two numbers of `report_search`), the counts, and the queries left out because a
/// candidate has no grade.
fn hybrid_ndcg(qs: &[Q], grades: &GradeMap) -> ((f64, usize), (f64, usize), usize) {
    let (mut ans, mut hard, mut incomplete) = (vec![], vec![], 0);
    for (qi, q) in qs.iter().enumerate() {
        if !q
            .graded
            .iter()
            .all(|id| grades.contains_key(&(qi, id.clone())))
        {
            incomplete += 1;
            continue;
        }
        let g = |id: &String| grades[&(qi, id.clone())].0;
        let pool: Vec<f64> = q.pool.iter().map(g).collect();
        let gr: Vec<f64> = q.rankings[0].iter().map(g).collect();
        let n = ndcg_at(&gr, &pool, 10);
        if q.answerable {
            ans.push(n);
        }
        if q.kind != "same" {
            hard.push(n);
        }
    }
    let mean = |v: &[f64]| (v.iter().sum::<f64>() / v.len().max(1) as f64, v.len());
    (mean(&ans), mean(&hard), incomplete)
}

fn cached_grid(
    env: &Env,
    e: &alice_embed::OnnxEmbedder,
    corpus: &Corpus,
    queries: &[serde_json::Value],
    cache_path: &Path,
) {
    let mut cache: GradeCache = std::fs::read_to_string(cache_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    println!("grade cache: {} entries", cache.len());
    for f in grid_rows() {
        let qs = run_queries(env, e, corpus, queries, f);
        let (grades, requested) = grade_missing(env, &corpus.by_id, &qs, &mut cache);
        let (ans, hard, incomplete) = hybrid_ndcg(&qs, &grades);
        println!(
            "GRID rrf_k={} boost_exponent={} answerable={:.4} (n={}) cross_lingual_paraphrase={:.4} (n={}) incomplete={incomplete} newly_graded_pairs={requested}",
            f.rrf_k, f.boost_exponent, ans.0, ans.1, hard.0, hard.1
        );
        std::fs::write(cache_path, serde_json::to_string(&cache).unwrap()).unwrap();
    }
    let fresh = env.store.fresh.lock();
    println!(
        "spend this run: {} requests, ${:.4}; errors {:?}",
        fresh.len(),
        fresh.iter().map(|c| c.usd).sum::<f64>(),
        env.errors.lock()
    );
}

// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "network and real model: TYPESAFE_API_KEY and ALICE_MODELS_DIR (JEV_EVAL_DRY=1 estimates without either key or network)"]
fn jev_versus_heuristic_and_graded_search() {
    let (env, e) = Env::new();
    let pairs = lines(include_str!("fixtures/judge_eval_pairs.jsonl"));
    let queries = lines(include_str!("fixtures/judge_eval_queries.jsonl"));

    // Corpus: every memory of the set, embedded once with the real model.
    let t0 = std::time::Instant::now();
    let texts: Vec<String> = pairs
        .iter()
        .flat_map(|p| {
            ["new", "old"].map(|s| {
                memory_embed_text(
                    p[s]["title"].as_str().unwrap(),
                    p[s]["body"].as_str().unwrap(),
                )
            })
        })
        .collect();
    let mut vecs: Vec<Vec<f32>> = Vec::new();
    for chunk in texts.chunks(32) {
        vecs.extend(e.embed_passages(chunk).unwrap());
    }
    let embed_secs = t0.elapsed().as_secs_f64();

    // The old memory carries the cosine of the real model, as in the daemon.
    let news: Vec<JudgeMemory> = pairs
        .iter()
        .map(|p| mem("N", &p["new"], env.now, None))
        .collect();
    let olds: Vec<JudgeMemory> = pairs
        .iter()
        .enumerate()
        .map(|(i, p)| {
            mem(
                "O",
                &p["old"],
                env.now,
                Some(cosine(&vecs[2 * i], &vecs[2 * i + 1])),
            )
        })
        .collect();
    if let Ok(cache) = std::env::var("ALICE_EVAL_GRADES") {
        let corpus = build_corpus(&e, &pairs, &vecs, env.now);
        cached_grid(&env, &e, &corpus, &queries, Path::new(&cache));
        return;
    }
    let relations = run_relations(&env, &pairs, &news, &olds);
    let corpus = build_corpus(&e, &pairs, &vecs, env.now);
    let qs = run_queries(&env, &e, &corpus, &queries, Fusion::default());
    let grades = grade_all(&env, &corpus.by_id, &qs);

    if env.dry {
        let phases = env.dry_phases.lock();
        let usd: f64 = phases.values().map(|p| p.1).sum();
        println!(
            "DRY RUN: upper-bound cost ${usd:.4} (reservation rule: {} tokens per byte + {} per request); {} (query, candidate) pairs to grade; embedding {} passages took {embed_secs:.1} s",
            alice_core::judge::jev::START_TOKEN_RATIO,
            alice_core::judge::jev::REQUEST_OVERHEAD_TOKENS,
            qs.iter().map(|q| q.graded.len()).sum::<usize>(),
            texts.len()
        );
        for (phase, (n, usd, bytes)) in phases.iter() {
            println!("  {phase}: {n} requests, {bytes} bytes, ${usd:.4}");
        }
        // The first run measured 0.43 input tokens per request byte (overhead included); 0.6
        // plus the 300-token overhead is a calibrated estimate with room.
        let (reqs, bytes) = phases.values().fold((0, 0), |a, p| (a.0 + p.0, a.1 + p.2));
        println!(
            "  calibrated estimate (0.6 tokens/byte + 300/request): ${:.4}",
            (bytes as f64 * 0.6 + reqs as f64 * 300.0) * alice_core::judge::jev::USD_PER_MTOK_INPUT
                / 1e6
        );
        println!("dry-run errors: {}", env.errors.lock().len());
        return;
    }

    let (relation_md, asked, relation_n) = report_relation(&pairs, &news, &olds, &relations);
    let (kind_md, kind_n) = report_kind(&pairs, &news, &relations);
    let search_md = report_search(&qs, &grades);
    let rows = floor_rows(&corpus, &qs, &grades);
    let floor_json: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "query": r.query, "query_kind": r.qkind, "lang": r.lang, "memory": r.id,
                "cosine": r.cosine, "grade": r.grade, "p_answers": r.p_answers,
                "p_partially": r.p_partially, "lexical_top50": r.lexical,
                "in_recall_modes": r.in_modes, "whole_corpus_query": r.full,
            })
        })
        .collect();
    std::fs::write(
        env.out_dir.join("m2-jev-floor.json"),
        serde_json::to_string_pretty(&floor_json).unwrap(),
    )
    .unwrap();
    let floor_md = report_floor(&rows);

    // Cost and latency of this run's own calls only.
    let st = env.jev.stats();
    let fresh = env.store.fresh.lock().clone();
    let usd: f64 = fresh.iter().map(|c| c.usd).sum();
    let tokens: u64 = fresh.iter().map(|c| c.input_tokens).sum();
    let mut lat: Vec<f64> = fresh
        .iter()
        .filter(|c| c.cacheable)
        .map(|c| c.latency_ms as f64)
        .collect();
    let decisions = relation_n + kind_n + grades.len();
    let answered = fresh.iter().filter(|c| c.cacheable).count();

    let mut report = format!(
        "Fresh run, {} UTC. Jev model {MODEL}, embedder {} ({} passages embedded in {embed_secs:.1} s). ",
        env.now.format("%Y-%m-%d %H:%M"),
        e.model_id(),
        texts.len()
    );
    if env.store.replayed > 0 {
        report.push_str(&format!(
            "**{} of {} requests were replayed** from a call log written on {} (cost and latency below count this run's own calls only).\n\n",
            st.cache_hits,
            st.cache_hits as usize + answered,
            env.store.replayed_days.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    } else {
        report.push_str(&format!(
            "No call was replayed (the call log is truncated at the start; {} requests were answered from this run's own cache).\n\n",
            st.cache_hits
        ));
    }
    report.push_str(&relation_md);
    report.push_str(&kind_md);
    report.push_str(&search_md);
    report.push_str(&floor_md);
    report.push_str(&format!(
        "\n### Cost and latency\n\nJev requests {} (answered {answered}, cache hits {}, retries {}, failures {}); input tokens {tokens}; latency p50 {:.0} ms, p95 {:.0} ms; ledger total ${usd:.4}; ${:.4} per 1,000 decisions ({decisions} decisions: {asked} relation pairs, {kind_n} kinds, {} graded pairs).\n",
        fresh.len(),
        st.cache_hits,
        st.retries,
        st.failures,
        percentile(&mut lat.clone(), 0.5),
        percentile(&mut lat, 0.95),
        usd * 1000.0 / decisions.max(1) as f64,
        grades.len()
    ));
    let errors = env.errors.lock();
    if !errors.is_empty() {
        report.push_str(&format!(
            "\n**{} calls failed** (first: {}). Results above cover what was answered.\n",
            errors.len(),
            errors[0]
        ));
    }
    if let Some(m) = env.stopped.lock().as_ref() {
        report.push_str(&format!(
            "\n**BUDGET STOP** at ${BUDGET_USD}: {m}. Results above are partial.\n"
        ));
    }
    println!("{report}");
    std::fs::write(&env.out_path, &report).unwrap();
    assert!(usd <= BUDGET_USD + 0.01, "the hard budget held");
}
