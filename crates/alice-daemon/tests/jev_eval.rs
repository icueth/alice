//! Jev vs the heuristic on the labelled set, and Jev-graded nDCG@10 of recall, with the real
//! embedding model. Network, real model, hard budget $2 for the whole run (enforced by the
//! judge's daily budget). The set is synthetic (`fixtures/judge_eval_pairs.jsonl`).
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
//! beside it, which a second run reads back as its cache.
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
use alice_core::search::{Lists, SearchParams, fusion::Weights, run};
use alice_core::settings::SecretValue;
use alice_core::store::embeddings::VecKey;
use alice_core::store::memories::{self, InsertOutcome, NewMemoryInput};
use alice_core::store::{embeddings, open_writer, projects};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

type RelationSlot = Mutex<Option<(Distribution, Option<bool>)>>;
type GradeMap = Mutex<HashMap<(usize, String), (f64, f64, f64)>>;

const LABELS: [&str; 4] = ["duplicate", "updates", "contradicts", "unrelated"];
const BUDGET_USD: f64 = 2.0;
const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-1.13.0";
const WORKERS: usize = 4;
const FLOORS: [f32; 5] = [0.70, 0.75, 0.78, 0.80, 0.82];

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

/// Never sends: the dry run builds each request and counts its bytes.
struct DryTransport;

impl JevTransport for DryTransport {
    fn post(&self, _: &str, _: &str, _: &[u8], _: Duration) -> Result<HttpReply, TransportError> {
        Err(TransportError::Connect("dry run".into()))
    }
}

/// The in-memory store, also appending every call to a JSONL file and reading it back at start
/// (so a run that crashes late does not pay for its answered calls again).
struct LoggedStore {
    inner: MemoryJevStore,
    file: Mutex<Option<std::fs::File>>,
}

impl LoggedStore {
    fn new(path: Option<&Path>) -> Self {
        let inner = MemoryJevStore::default();
        let mut file = None;
        if let Some(path) = path {
            if let Ok(text) = std::fs::read_to_string(path) {
                for l in text.lines().filter(|l| !l.trim().is_empty()) {
                    if let Ok(c) = serde_json::from_str::<JevCall>(l) {
                        inner.record(&c);
                    }
                }
            }
            file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok();
        }
        Self {
            inner,
            file: Mutex::new(file),
        }
    }
}

impl JevStore for LoggedStore {
    fn cached(&self, hash: &str) -> Option<String> {
        self.inner.cached(hash)
    }
    fn spent(&self, day: &str) -> Result<f64, String> {
        self.inner.spent(day)
    }
    fn record(&self, call: &JevCall) {
        self.inner.record(call);
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
    /// Auto-apply at `t` for non-`unrelated` predictions: (applied, coverage, precision).
    fn auto_at(&self, t: f64) -> (usize, f64, f64) {
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
            applied.len() as f64 / actionable as f64,
            if applied.is_empty() {
                0.0
            } else {
                right as f64 / applied.len() as f64
            },
        )
    }
    fn row(&self, name: &str) -> String {
        let (_, cov, prec) = self.auto_at(0.85);
        let right = self
            .gold
            .iter()
            .zip(&self.pred)
            .filter(|(g, p)| g == p)
            .count();
        let (lo, hi) = wilson(right, self.gold.len());
        format!(
            "| {name} | {} | {:.3} ({:.3}–{:.3}) | {:.3} | {:.3} | {:.3} | {:.0}% | {:.0}% |",
            self.gold.len(),
            accuracy(&self.gold, &self.pred),
            lo,
            hi,
            macro_f1(&self.gold, &self.pred, &LABELS),
            brier(&self.gold, &self.probs, &LABELS),
            ece(&self.gold, &self.probs, 10),
            cov * 100.0,
            prec * 100.0
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
    /// Found by the lexical lists (the lexical-only ranking's top 10).
    lexical: bool,
    /// In the top 10 of at least one of the four recall modes.
    in_modes: bool,
}

/// Cosine distribution and the effect of each floor, for pairs Jev calls relevant (`is_rel`)
/// and the rest.
fn floor_tables(rows: &[Row], is_rel: &dyn Fn(&Row) -> bool, what: &str) -> String {
    let mut out = format!(
        "\n#### Relevant = {what}\n\nCosine distribution (min / p10 / median / p90) of the real model's exact cosine between the query and the memory.\n\n| Query kind | Relevant pairs | Relevant cosine | Other graded pairs | Other cosine |\n|---|---|---|---|---|\n"
    );
    let kinds = ["same", "cross", "para", "all"];
    let sel =
        |k: &str| -> Vec<&Row> { rows.iter().filter(|r| k == "all" || r.qkind == k).collect() };
    for k in kinds {
        let rs = sel(k);
        let mut rel: Vec<f64> = rs.iter().filter(|r| is_rel(r)).map(|r| r.cosine).collect();
        let mut non: Vec<f64> = rs.iter().filter(|r| !is_rel(r)).map(|r| r.cosine).collect();
        out.push_str(&format!(
            "| {k} | {} | {} | {} | {} |\n",
            rel.len(),
            stats4(&mut rel),
            non.len(),
            stats4(&mut non)
        ));
    }
    for k in kinds {
        let rs = sel(k);
        let rel: Vec<&&Row> = rs.iter().filter(|r| is_rel(r)).collect();
        let non: Vec<&&Row> = rs.iter().filter(|r| !is_rel(r)).collect();
        out.push_str(&format!(
            "\nFloor sweep, query kind **{k}** ({} relevant, {} other graded pairs). \"cosine\" is what a vector-only match needs; \"cosine or lexical\" is the shipped rule (a lexical match passes at any similarity).\n\n| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |\n|---|---|---|---|---|\n",
            rel.len(),
            non.len()
        ));
        let share = |v: &[&&Row], f: &dyn Fn(&Row) -> bool| -> String {
            if v.is_empty() {
                "–".into()
            } else {
                format!(
                    "{:.0}% ({}/{})",
                    100.0 * v.iter().filter(|r| f(r)).count() as f64 / v.len() as f64,
                    v.iter().filter(|r| f(r)).count(),
                    v.len()
                )
            }
        };
        for fl in FLOORS {
            let by_cos = |r: &Row| r.cosine >= f64::from(fl);
            let by_any = |r: &Row| r.cosine >= f64::from(fl) || r.lexical;
            out.push_str(&format!(
                "| {fl:.2} | {} | {} | {} | {} |\n",
                share(&rel, &by_cos),
                share(&rel, &by_any),
                share(&non, &by_cos),
                share(&non, &by_any)
            ));
        }
    }
    out
}

#[test]
#[ignore = "network and real model: TYPESAFE_API_KEY and ALICE_MODELS_DIR (JEV_EVAL_DRY=1 estimates without either key or network)"]
fn jev_versus_heuristic_and_graded_search() {
    let dry = std::env::var("JEV_EVAL_DRY").is_ok_and(|v| v == "1");
    let models = PathBuf::from(std::env::var("ALICE_MODELS_DIR").expect("ALICE_MODELS_DIR"));
    let out_path = PathBuf::from(std::env::var("ALICE_EVAL_OUT").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/m2-jev-eval.md").into()
    }));
    let out_dir = out_path.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&out_dir).unwrap();
    let e = alice_embed::OnnxEmbedder::new(&models, alice_embed::OnnxOptions::default()).unwrap();
    let now = chrono::Utc::now();
    let calls_path = out_dir.join("m2-jev-calls.jsonl");
    let store = Arc::new(LoggedStore::new(if dry {
        None
    } else {
        Some(calls_path.as_path())
    }));
    let (transport, key): (Arc<dyn JevTransport>, SecretValue) = if dry {
        (Arc::new(DryTransport), SecretValue::new("dry-run"))
    } else {
        (
            Arc::new(alice_daemon::judge_http::ReqwestTransport::new().unwrap()),
            SecretValue::new(std::env::var("TYPESAFE_API_KEY").expect("set -a; . ./.env; set +a")),
        )
    };
    let jev = JevJudge::new(
        JevConfig {
            endpoint: ENDPOINT.into(),
            model: MODEL.into(),
            api_key: key,
            rate_per_sec: if dry { 1000.0 } else { 8.0 },
            timeout: Duration::from_secs(30),
            daily_budget_usd: BUDGET_USD,
            max_retries: 3,
        },
        transport,
        store.clone(),
        // Allow-all test policy with the built-in redactor: the set is synthetic.
        Arc::new(FixedPolicy::allow_all(Redactor::builtin())),
    );
    let workers = if dry { 1 } else { WORKERS };
    let heuristic = HeuristicJudge::default();
    let pairs: Vec<serde_json::Value> = include_str!("fixtures/judge_eval_pairs.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let queries: Vec<serde_json::Value> = include_str!("fixtures/judge_eval_queries.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

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

    // Relation pairs. The old memory carries the cosine of the real model, as in the daemon.
    let n = pairs.len();
    let news: Vec<JudgeMemory> = pairs
        .iter()
        .map(|p| mem("N", &p["new"], now, None))
        .collect();
    let olds: Vec<JudgeMemory> = pairs
        .iter()
        .enumerate()
        .map(|(i, p)| {
            mem(
                "O",
                &p["old"],
                now,
                Some(cosine(&vecs[2 * i], &vecs[2 * i + 1])),
            )
        })
        .collect();
    let stop = AtomicBool::new(false);
    let stopped: Mutex<Option<String>> = Mutex::new(None);
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let note_err = |what: &str, err: JudgeError| {
        if let JudgeError::Budget(m) = &err {
            *stopped.lock() = Some(m.clone());
            stop.store(true, Ordering::SeqCst);
        }
        errors.lock().push(format!("{what}: {err}"));
    };
    let dry_bytes = Mutex::new(0usize);
    let dry_requests = Mutex::new(0usize);
    let dry_note = |jev: &JevJudge| {
        if dry {
            // One worker in the dry run, so `last_request` is this call's request.
            *dry_bytes.lock() += jev.last_request().map_or(0, |b| b.len());
            *dry_requests.lock() += 1;
        }
    };
    let dry_usd = Mutex::new(0.0f64);
    let dry_add = |jev: &JevJudge| {
        if dry {
            let len = jev.last_request().map_or(0, |b| b.len());
            *dry_usd.lock() += jev.estimate_usd(len);
        }
    };

    let relation: Vec<RelationSlot> = (0..n).map(|_| Mutex::new(None)).collect();
    let kind_jev: Vec<Mutex<Option<Distribution>>> = (0..n).map(|_| Mutex::new(None)).collect();
    parallel(n, workers, &stop, |i| {
        match jev.compare(&news[i], std::slice::from_ref(&olds[i])) {
            Ok(mut r) => {
                let r = r.remove(0);
                *relation[i].lock() = Some((r.probs, r.agreement));
            }
            Err(err) => note_err(pairs[i]["id"].as_str().unwrap(), err),
        }
        dry_note(&jev);
        dry_add(&jev);
        // The kind is asked of a memory whose kind is still the default (`note`): the request
        // states the memory's kind, and the true one must not be in it.
        let blank = JudgeMemory {
            kind: Kind::Note,
            ..news[i].clone()
        };
        match jev.classify_kind(&blank) {
            Ok(d) => *kind_jev[i].lock() = Some(d),
            Err(err) => note_err(pairs[i]["id"].as_str().unwrap(), err),
        }
        dry_note(&jev);
        dry_add(&jev);
    });

    // Graded search: corpus = every memory of the set, in a throwaway database.
    let d = tempfile::tempdir().unwrap();
    let c = open_writer(&d.path().join("a.db")).unwrap();
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
    let mut by_id: HashMap<String, JudgeMemory> = HashMap::new();
    let mut vec_of: HashMap<String, Vec<f32>> = HashMap::new();
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
                vec_of.insert(id, v);
            }
        }
    }
    let modes = [
        ("hybrid", Lists::Hybrid, 0.82f32),
        ("lexical only", Lists::LexicalOnly, 0.82),
        ("vector only", Lists::VectorOnly, 0.82),
        ("vector only, no floor", Lists::VectorOnly, 0.0),
    ];
    let search = |q: &str, lists: Lists, floor: f32, limit: usize| -> Vec<String> {
        let params = SearchParams {
            query: q.into(),
            current_project_id: Some(proj.id.clone()),
            limit,
            relevance_floor: floor,
            raw_min_results: 0,
            lists,
            ..Default::default()
        };
        run(&c, Some(&idx), &e, &Weights::default(), &params, now)
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.id)
            .collect()
    };
    struct Q {
        text: String,
        kind: String,
        lang: String,
        answerable: bool,
        rankings: Vec<Vec<String>>,
        /// The four modes' candidates, and those plus the vector top 20 (for the floor evidence).
        pool: Vec<String>,
        floor_pool: Vec<String>,
        qvec: Vec<f32>,
    }
    let qs: Vec<Q> = queries
        .iter()
        .map(|q| {
            let text = q["q"].as_str().unwrap().to_string();
            let rankings: Vec<Vec<String>> = modes
                .iter()
                .map(|(_, lists, floor)| search(&text, *lists, *floor, 10))
                .collect();
            let mut pool: Vec<String> = rankings.concat();
            pool.sort();
            pool.dedup();
            let mut floor_pool = pool.clone();
            floor_pool.extend(search(&text, Lists::VectorOnly, 0.0, 20));
            floor_pool.sort();
            floor_pool.dedup();
            Q {
                qvec: e.embed_query(&text).unwrap(),
                kind: q["kind"].as_str().unwrap().into(),
                lang: q["lang"].as_str().unwrap().into(),
                answerable: q.get("answerable").and_then(|a| a.as_bool()) != Some(false),
                text,
                rankings,
                pool,
                floor_pool,
            }
        })
        .collect();

    // Grade every pooled (query, candidate) in batches of ten: 2 × P(answers) + P(partially).
    let tasks: Vec<(usize, Vec<String>)> = qs
        .iter()
        .enumerate()
        .flat_map(|(qi, q)| {
            q.floor_pool
                .chunks(10)
                .map(move |b| (qi, b.to_vec()))
                .collect::<Vec<_>>()
        })
        .collect();
    let grades: GradeMap = Mutex::new(HashMap::new());
    parallel(tasks.len(), workers, &stop, |t| {
        let (qi, ids) = &tasks[t];
        let ms: Vec<JudgeMemory> = ids.iter().map(|id| by_id[id].clone()).collect();
        match jev.grade_relevance("eval", &qs[*qi].text, &ms) {
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
            Err(err) => note_err(&qs[*qi].text, err),
        }
        dry_note(&jev);
        dry_add(&jev);
    });

    if dry {
        let usd = *dry_usd.lock();
        println!(
            "DRY RUN: {} requests, {} request bytes, upper-bound cost ${usd:.4} (reservation rule: {} tokens per byte + {} per request); {} (query, candidate) pairs to grade; embedding {} passages took {embed_secs:.1} s",
            *dry_requests.lock(),
            *dry_bytes.lock(),
            alice_core::judge::jev::START_TOKEN_RATIO,
            alice_core::judge::jev::REQUEST_OVERHEAD_TOKENS,
            qs.iter().map(|q| q.floor_pool.len()).sum::<usize>(),
            texts.len()
        );
        println!("dry-run errors: {}", errors.lock().len());
        return;
    }

    let grades = grades.into_inner();
    let stopped = stopped.into_inner();
    let errors = errors.into_inner();

    // Relation and kind tables.
    let (mut h, mut j) = (Run::new(), Run::new());
    let mut per_lang: BTreeMap<String, (Run, Run)> = BTreeMap::new();
    let (mut agree, mut asked, mut agree_right, mut disagree_right) =
        (0usize, 0usize, 0usize, 0usize);
    let (mut kind_gold, mut kind_h, mut kind_j) = (vec![], vec![], vec![]);
    for (i, p) in pairs.iter().enumerate() {
        let label = p["label"].as_str().unwrap();
        let lang = p["lang"].as_str().unwrap().to_string();
        let hd = heuristic
            .compare(&news[i], std::slice::from_ref(&olds[i]))
            .unwrap()
            .remove(0)
            .probs;
        if let Some((jd, agreement)) = relation[i].lock().clone() {
            asked += 1;
            let right = jd.best().is_some_and(|b| b.0 == label);
            if agreement == Some(true) {
                agree += 1;
                agree_right += usize::from(right);
            } else {
                disagree_right += usize::from(right);
            }
            h.push(label, hd.clone());
            j.push(label, jd.clone());
            let e = per_lang
                .entry(lang)
                .or_insert_with(|| (Run::new(), Run::new()));
            e.0.push(label, hd);
            e.1.push(label, jd);
        }
        if let Some(kd) = kind_jev[i].lock().clone() {
            kind_gold.push(p["new_kind"].as_str().unwrap().to_string());
            kind_h.push(kind_rules(&news[i].title, &news[i].body).best().unwrap().0);
            kind_j.push(kd.best().map(|b| b.0).unwrap_or_else(|| "note".into()));
        }
    }
    let kind_not_note: Vec<usize> = (0..kind_gold.len())
        .filter(|&i| kind_gold[i] != "note")
        .collect();
    let pick =
        |v: &[String]| -> Vec<String> { kind_not_note.iter().map(|&i| v[i].clone()).collect() };

    // Search tables: nDCG over the queries whose whole pool Jev graded.
    let complete = |q: &Q, qi: usize| {
        q.floor_pool
            .iter()
            .all(|id| grades.contains_key(&(qi, id.clone())))
    };
    let mut ndcg: Vec<Vec<f64>> = vec![vec![]; modes.len()];
    let mut ndcg_cross_para: Vec<Vec<f64>> = vec![vec![]; modes.len()];
    let mut ndcg_answerable: Vec<Vec<f64>> = vec![vec![]; modes.len()];
    let mut incomplete = 0;
    let mut unanswerable_rows = String::new();
    for (qi, q) in qs.iter().enumerate() {
        if !complete(q, qi) {
            incomplete += 1;
            continue;
        }
        let g = |id: &String| grades[&(qi, id.clone())].0;
        let pool_grades: Vec<f64> = q.pool.iter().map(g).collect();
        for (m, ranking) in q.rankings.iter().enumerate() {
            let gr: Vec<f64> = ranking.iter().map(g).collect();
            let v = ndcg_at(&gr, &pool_grades, 10);
            ndcg[m].push(v);
            if q.kind != "same" {
                ndcg_cross_para[m].push(v);
            }
            if q.answerable {
                ndcg_answerable[m].push(v);
            }
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
                q.pool.iter().map(g).fold(0.0f64, f64::max),
                q.pool.len()
            ));
        }
    }

    // Floor evidence rows.
    let mut rows: Vec<Row> = Vec::new();
    for (qi, q) in qs.iter().enumerate() {
        let lexical: HashSet<&String> = q.rankings[1].iter().collect();
        let in_modes: HashSet<&String> = q.pool.iter().collect();
        for id in &q.floor_pool {
            let Some(&(grade, pa, pp)) = grades.get(&(qi, id.clone())) else {
                continue;
            };
            rows.push(Row {
                query: q.text.clone(),
                qkind: q.kind.clone(),
                lang: q.lang.clone(),
                id: id.clone(),
                cosine: f64::from(cosine(&q.qvec, &vec_of[id])),
                grade,
                p_answers: pa,
                p_partially: pp,
                lexical: lexical.contains(id),
                in_modes: in_modes.contains(id),
            });
        }
    }
    let floor_json: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "query": r.query, "query_kind": r.qkind, "lang": r.lang, "memory": r.id,
                "cosine": r.cosine, "grade": r.grade, "p_answers": r.p_answers,
                "p_partially": r.p_partially, "lexical_top10": r.lexical, "in_recall_modes": r.in_modes,
            })
        })
        .collect();
    std::fs::write(
        out_dir.join("m2-jev-floor.json"),
        serde_json::to_string_pretty(&floor_json).unwrap(),
    )
    .unwrap();

    let calls = store.inner.calls();
    let usd: f64 = calls.iter().map(|c| c.usd).sum();
    let tokens: u64 = calls.iter().map(|c| c.input_tokens).sum();
    let mut lat: Vec<f64> = calls
        .iter()
        .filter(|c| c.cacheable)
        .map(|c| c.latency_ms as f64)
        .collect();
    let decisions = j.gold.len() + kind_j.len() + grades.len();
    let st = jev.stats();

    let mut report = String::new();
    report.push_str(&format!(
        "Single run, {} UTC. Jev model {MODEL}, embedder {} ({} passages embedded in {embed_secs:.1} s).\n\n",
        now.format("%Y-%m-%d %H:%M"),
        e.model_id(),
        texts.len()
    ));
    report.push_str("### Relation\n\n| Judge | Pairs | Accuracy (95% Wilson) | Macro-F1 | Brier | ECE | Auto-apply coverage at 0.85 | Auto-apply precision at 0.85 |\n|---|---|---|---|---|---|---|---|\n");
    report.push_str(&format!("{}\n{}\n", h.row("heuristic"), j.row(MODEL)));
    report.push_str(&h.confusion("Heuristic"));
    report.push_str(&j.confusion("Jev"));
    report.push_str(
        "\n| Language | Pairs | Heuristic accuracy | Jev accuracy |\n|---|---|---|---|\n",
    );
    for (lang, (hh, jj)) in &per_lang {
        report.push_str(&format!(
            "| {lang} | {} | {:.3} | {:.3} |\n",
            hh.gold.len(),
            accuracy(&hh.gold, &hh.pred),
            accuracy(&jj.gold, &jj.pred)
        ));
    }
    report.push_str("\nAuto-apply sweep (non-`unrelated` predictions with P at or above the threshold; coverage is the share of the actionable pairs, precision the share right):\n\n| Threshold | Heuristic applied / coverage / precision | Jev applied / coverage / precision |\n|---|---|---|\n");
    for t in [0.5, 0.7, 0.85, 0.9, 0.95, 0.99] {
        let (ha, hc, hp) = h.auto_at(t);
        let (ja, jc, jp) = j.auto_at(t);
        report.push_str(&format!(
            "| {t:.2} | {ha} / {:.0}% / {:.0}% | {ja} / {:.0}% / {:.0}% |\n",
            hc * 100.0,
            hp * 100.0,
            jc * 100.0,
            jp * 100.0
        ));
    }
    report.push_str(&format!(
        "\nForward/reversed agreement (Jev): {:.1}% of {asked} pairs; accuracy when they agree {:.1}% ({agree_right}/{agree}), when they disagree {:.1}% ({disagree_right}/{})\n",
        100.0 * agree as f64 / asked.max(1) as f64,
        100.0 * agree_right as f64 / agree.max(1) as f64,
        100.0 * disagree_right as f64 / (asked - agree).max(1) as f64,
        asked - agree
    ));
    report.push_str(&format!(
        "\n### Kind classification\n\nThe memory is sent with the default kind (`note`) so the request does not state the answer.\n\nKind accuracy: heuristic {:.3}, Jev {:.3} ({} memories); without the {} gold `note` memories (the default the request states): heuristic {:.3}, Jev {:.3}\n",
        accuracy(&kind_gold, &kind_h),
        accuracy(&kind_gold, &kind_j),
        kind_gold.len(),
        kind_gold.len() - kind_not_note.len(),
        accuracy(&pick(&kind_gold), &pick(&kind_h)),
        accuracy(&pick(&kind_gold), &pick(&kind_j)),
    ));
    let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len().max(1) as f64;
    report.push_str("\n### Search, Jev-graded\n\nGrade = 2 × P(answers) + P(partially); the ideal ranking is the best of every candidate any of the four modes returned (pooling).\n\n| Recall mode | Queries | nDCG@10 (all) | nDCG@10 (answerable) | nDCG@10 (cross-lingual and paraphrase) |\n|---|---|---|---|---|\n");
    for (m, (name, _, floor)) in modes.iter().enumerate() {
        report.push_str(&format!(
            "| {name}{} | {} | {:.3} | {:.3} | {:.3} |\n",
            if matches!(m, 0 | 2) {
                format!(" (floor {floor:.2})")
            } else {
                String::new()
            },
            ndcg[m].len(),
            mean(&ndcg[m]),
            mean(&ndcg_answerable[m]),
            mean(&ndcg_cross_para[m]),
        ));
    }
    if incomplete > 0 {
        report.push_str(&format!("\n{incomplete} queries were left out because Jev did not grade all of their candidates.\n"));
    }
    report.push_str("\nQueries no memory answers: best Jev grade among the results of hybrid / vector only (floor 0.82) / all pooled candidates (result count in brackets); a grade near 0 is right.\n\n| Query | Language | Hybrid | Vector only | Pool |\n|---|---|---|---|---|\n");
    report.push_str(&unanswerable_rows);
    report.push_str("\n### Relevance floor evidence for Task 13\n");
    report.push_str(&format!(
        "\n{} graded (query, candidate) pairs: the candidates of the four recall modes plus the vector top 20 without a floor. {} of them were returned by a recall mode. The cosine is the model's exact cosine; the raw rows are in `m2-jev-floor.json`. \"Lexical\" means the memory is in the lexical-only top 10 (a lexical match passes the floor at any similarity).\n",
        rows.len(),
        rows.iter().filter(|r| r.in_modes).count()
    ));
    report.push_str(&floor_tables(
        &rows,
        &|r| r.p_answers >= 0.5,
        "Jev says the memory answers the query (P(answers) ≥ 0.5)",
    ));
    report.push_str(&floor_tables(
        &rows,
        &|r| r.p_answers + r.p_partially >= 0.5,
        "Jev says the memory answers or is about the query's subject (P(answers) + P(partially) ≥ 0.5)",
    ));
    report.push_str(&format!(
        "\n### Cost and latency\n\nJev requests {} (answered {}, cache hits {}, retries {}, failures {}); input tokens {tokens}; latency p50 {:.0} ms, p95 {:.0} ms; ledger total ${usd:.4}; ${:.4} per 1,000 decisions ({decisions} decisions: {} relation pairs, {} kinds, {} graded pairs).\n",
        calls.len(),
        calls.iter().filter(|c| c.cacheable).count(),
        st.cache_hits,
        st.retries,
        st.failures,
        percentile(&mut lat.clone(), 0.5),
        percentile(&mut lat, 0.95),
        usd * 1000.0 / decisions.max(1) as f64,
        j.gold.len(),
        kind_j.len(),
        grades.len()
    ));
    if !errors.is_empty() {
        report.push_str(&format!(
            "\n**{} calls failed** (first: {}). Results above cover what was answered.\n",
            errors.len(),
            errors[0]
        ));
    }
    if let Some(m) = &stopped {
        report.push_str(&format!(
            "\n**BUDGET STOP** at ${BUDGET_USD}: {m}. Results above are partial.\n"
        ));
    }
    println!("{report}");
    std::fs::write(&out_path, &report).unwrap();
    assert!(usd <= BUDGET_USD + 0.01, "the hard budget held");
}
