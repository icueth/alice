//! The judge stage of the write path (spec §5.1 step 5): for each memory whose embedding step is
//! over, compare it with its nearest memories in the project, set its kind or project when asked,
//! decide with the thresholds, and hand the judgement to the writer, which applies it and
//! activates the memory. Network calls (Jev) happen here, never on the writer.
//!
//! The one decision whether text of a project may be sent to a cloud judge is the configured
//! judge's own (Jev asks `DaemonJevPolicy` for every project a call involves). This module asks
//! the judge scoped questions and takes what it answers: `Denied` and `Budget` mean the
//! heuristic decides and nothing is retried; any other failure means the heuristic decides now
//! and the memory is judged again later. The one thing this module asks the policy itself is
//! which projects it may even *mention* to a model when routing (a denied project's name and
//! recent titles are never offered to one).

use crate::config::SettingsHandle;
use crate::judge_http::JudgeHub;
use crate::redaction::Redactors;
use crate::service::IndexSlot;
use crate::writer::{Judgement, WriteOp, WriterHandle};
use alice_core::embed::cosine;
use alice_core::error::Result;
use alice_core::judge::heuristic::HeuristicJudge;
use alice_core::judge::pipeline::{Thresholds, decide};
use alice_core::judge::text::title_similarity;
use alice_core::judge::{Distribution, Judge, JudgeError, JudgeMemory, ProjectOption, Relation};
use alice_core::model::{Kind, Memory, Status};
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::judge::{PendingJudgement, pending, retry_backoff};
use alice_core::store::memories;
use alice_core::store::projects;
use alice_core::store::reviews::{NewReview, ReviewType};
use alice_core::vector::KeyFilter;
use crossbeam_channel::Receiver;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the worker needs from the service's judge: the judge in force and which projects the
/// cloud policy lets a model be told about.
pub trait JudgeSource: Send + Sync {
    /// The judge in force now (the service rebuilds it when settings or the key change).
    fn current(&self) -> Arc<dyn Judge>;
    /// Whether the cloud policy allows text of `project` to be sent to a model.
    fn cloud_allowed(&self, project: &str) -> bool;
    /// Whether `current` is only a stand-in for the judge the settings ask for (the key is
    /// missing or still being read, the client could not start).
    fn stand_in(&self) -> bool;
}

impl JudgeSource for JudgeHub {
    fn current(&self) -> Arc<dyn Judge> {
        JudgeHub::current(self)
    }

    fn cloud_allowed(&self, project: &str) -> bool {
        JudgeHub::cloud_allowed(self, project)
    }

    fn stand_in(&self) -> bool {
        self.fell_back()
    }
}

/// Neighbours compared with a new memory, by vector.
const NEIGHBOURS: usize = 5;
/// Further neighbours of the same kind with a nearly identical title (the "updates" signal).
const TITLE_CANDIDATES: usize = 3;
/// Nearest memories looked at first to find `NEIGHBOURS` in the same project; a project with
/// few memories in a large store widens the search (x4 twice).
const SEARCH_DEPTH: usize = if cfg!(test) { 2 } else { 100 };
/// Times in a row a memory may fail to be judged or applied before it is activated unjudged.
const STRIKES: u32 = 3;
/// Memories judged per look at the queue.
const BATCH: usize = 8;
/// Times a memory is judged in all (the first and the retries after a failure).
const MAX_ATTEMPTS: u32 = 5;
/// Projects offered to the router, newest first.
const ROUTE_PROJECTS: usize = 50;
/// Recent titles that describe a project to the router.
const ROUTE_HINTS: usize = 3;

pub struct JudgeWorkerConfig {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    pub judge: Arc<dyn JudgeSource>,
    pub settings: Arc<SettingsHandle>,
    pub redactors: Arc<Redactors>,
    /// The embedding model whose stored vectors the judge reads (follows a model switch).
    pub model: Arc<dyn Fn() -> String + Send + Sync>,
    pub wake: Receiver<()>,
    pub stop: Arc<AtomicBool>,
    /// How often memories whose judgement failed (`retry`) are looked at again (each one waits
    /// its own backoff, see `store::judge::retry_backoff`).
    pub retry_every: Duration,
    /// The first wait of a memory that failed to be judged (it doubles with every failure).
    pub retry_base: Duration,
    /// False: decide but change nothing (see `ServiceOptions::judge_applies`).
    pub applies: bool,
}

impl JudgeWorkerConfig {
    /// How often memories whose judgement failed are queued again, by default.
    pub const RETRY_EVERY: Duration = Duration::from_secs(300);
}

pub fn spawn_judge_worker(cfg: JudgeWorkerConfig) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("alice-judge".into())
        .spawn(move || run(cfg))
        .expect("spawn judge thread")
}

/// Sleep for `d`, waking early when `stop` is set.
fn nap(stop: &AtomicBool, d: Duration) {
    let until = Instant::now() + d;
    while !stop.load(Ordering::Relaxed) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn run(cfg: JudgeWorkerConfig) {
    let mut last_requeue: Option<Instant> = None;
    // Consecutive failures to judge or apply, per memory (by seq).
    let mut strikes: HashMap<i64, u32> = HashMap::new();
    while !cfg.stop.load(Ordering::Relaxed) {
        if cfg.writer.read_only.reason().is_some() {
            nap(&cfg.stop, Duration::from_secs(1));
            continue;
        }
        // Neighbours come from the vector index: wait while it is being rebuilt.
        if cfg.index.read().is_none() {
            nap(&cfg.stop, Duration::from_millis(100));
            continue;
        }
        if last_requeue.is_none_or(|t| t.elapsed() >= cfg.retry_every) {
            last_requeue = Some(Instant::now());
            if let Err(e) = cfg.writer.submit_blocking(WriteOp::RequeueJudgeRetries {
                max_attempts: MAX_ATTEMPTS,
                now: now_rfc3339(),
            }) {
                tracing::warn!(error = %e, "queueing failed judgements again failed");
            }
        }
        let items = match cfg.readers.with(|c| pending(c, BATCH)) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "reading the judge queue failed");
                nap(&cfg.stop, Duration::from_secs(1));
                continue;
            }
        };
        if items.is_empty() {
            let _ = cfg.wake.recv_timeout(Duration::from_millis(250));
            continue;
        }
        let mut failed = false;
        for item in items {
            if cfg.stop.load(Ordering::Relaxed) {
                break;
            }
            let judged =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| judge_one(&cfg, &item)));
            let outcome = match judged {
                Ok(Ok(Some(j))) => cfg
                    .writer
                    .submit_blocking(WriteOp::ApplyJudgement(j))
                    .map(|_| ()),
                // Changed while it was read: the next look at the queue judges the new text.
                Ok(Ok(None)) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(_) => {
                    // Not the judge's fault to be retried at once: the memory is activated
                    // as it is and judged again later.
                    tracing::error!(id = %item.id, "judging panicked; it is tried again later");
                    cfg.writer
                        .submit_blocking(WriteOp::ApplyJudgement(plain(&item, "heuristic", true)))
                        .map(|_| ())
                }
            };
            match outcome {
                Ok(()) => {
                    strikes.remove(&item.seq);
                }
                Err(e) => {
                    failed = true;
                    let n = strikes.entry(item.seq).or_insert(0);
                    *n += 1;
                    tracing::warn!(error = %e, id = %item.id, strikes = *n, "judging failed; it is tried again");
                    if *n >= STRIKES {
                        // Never stuck `pending`: activated without a judgement.
                        strikes.remove(&item.seq);
                        let _ = cfg.writer.submit_blocking(WriteOp::ApplyJudgement(plain(
                            &item,
                            "heuristic",
                            false,
                        )));
                    }
                }
            }
        }
        if failed {
            // Never spin on a memory that cannot be judged or a writer that cannot apply.
            nap(&cfg.stop, Duration::from_secs(1));
        }
    }
}

fn now_rfc3339() -> String {
    alice_core::ids::now_rfc3339()
}

/// A judgement that decides nothing: the memory is activated as it is (`retry`: and judged
/// again later).
fn plain(item: &PendingJudgement, provider: &str, retry: bool) -> Judgement {
    Judgement {
        seq: item.seq,
        id: item.id.clone(),
        stamp: item.stamp.clone(),
        updated_at: item.updated_at.clone(),
        provider: provider.into(),
        retry,
        ..Default::default()
    }
}

/// An existing memory the new one is compared with, and the `content_hash` its text had
/// before it was read (the writer refuses to act on it if that has changed).
struct Neighbour {
    memory: JudgeMemory,
    hash: String,
}

fn judge_memory(m: &Memory, similarity: Option<f32>) -> JudgeMemory {
    JudgeMemory {
        id: m.id.clone(),
        project: m.project.clone(),
        kind: m.kind,
        title: m.title.clone(),
        body: m.body.clone(),
        importance: m.importance,
        created_at: m.created_at.clone(),
        similarity,
    }
}

/// `(seq, content_hash)` of the live (`active`) memories among `seqs` that are in the project,
/// in one query.
fn active_in_project(
    c: &Connection,
    project_id: &str,
    seqs: &[i64],
) -> Result<HashMap<i64, String>> {
    if seqs.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT seq, content_hash FROM memories WHERE project_id = ?1 AND status = 'active' AND seq IN ({})",
        vec!["?"; seqs.len()].join(",")
    );
    let mut stmt = c.prepare(&sql)?;
    let params = std::iter::once(rusqlite::types::Value::Text(project_id.to_string()))
        .chain(seqs.iter().map(|s| rusqlite::types::Value::Integer(*s)));
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Up to `NEIGHBOURS` nearest memories of the same project by vector, plus up to
/// `TITLE_CANDIDATES` more of the same kind whose titles are nearly the same. Only OLDER
/// memories that are `active` count (so "the older one survives" holds, a pair judged in one
/// batch meets once, and a retry compares as the first try did). Similarities are the exact
/// cosine of the stored vectors. Hashes are read before the text.
fn neighbours(
    cfg: &JudgeWorkerConfig,
    c: &Connection,
    item: &PendingJudgement,
    me: &JudgeMemory,
) -> Result<Vec<Neighbour>> {
    let model = (cfg.model)();
    let my_vec = embeddings::get_many(c, &model, &[VecKey::Memory(item.seq)])?
        .pop()
        .map(|(_, v)| v);
    // seq -> hash, for the memories chosen, in order.
    let mut chosen: Vec<(i64, String)> = Vec::new();
    if let (Some(v), Some(index)) = (&my_vec, cfg.index.read().clone()) {
        let mut depth = SEARCH_DEPTH;
        for _ in 0..3 {
            let hits = index.search(v, depth, KeyFilter::Memories)?;
            let older: Vec<i64> = hits
                .iter()
                .filter_map(|(k, _)| match k {
                    VecKey::Memory(s) if *s < item.seq => Some(*s),
                    _ => None,
                })
                .collect();
            let mut live = active_in_project(c, &item.project_id, &older)?;
            chosen = older
                .into_iter()
                .filter_map(|s| live.remove(&s).map(|h| (s, h)))
                .take(NEIGHBOURS)
                .collect();
            if chosen.len() == NEIGHBOURS || hits.len() < depth {
                break;
            }
            depth *= 4;
        }
    }
    let mut stmt = c.prepare_cached(
        "SELECT seq, title, content_hash FROM memories WHERE project_id = ?1 AND kind = ?2
           AND status = 'active' AND seq < ?3 ORDER BY updated_at DESC LIMIT 200",
    )?;
    let same_kind: Vec<(i64, String, String)> = stmt
        .query_map(
            rusqlite::params![item.project_id, me.kind.as_str(), item.seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .collect::<std::result::Result<_, _>>()?;
    let mut added = 0;
    for (seq, title, hash) in same_kind {
        if added == TITLE_CANDIDATES {
            break;
        }
        if chosen.iter().any(|(s, _)| *s == seq) || title_similarity(&me.title, &title) < 0.9 {
            continue;
        }
        chosen.push((seq, hash));
        added += 1;
    }
    let seqs: Vec<i64> = chosen.iter().map(|(s, _)| *s).collect();
    let keys: Vec<VecKey> = seqs.iter().map(|s| VecKey::Memory(*s)).collect();
    let vectors: HashMap<VecKey, Vec<f32>> = embeddings::get_many(c, &model, &keys)?
        .into_iter()
        .collect();
    let hashes: HashMap<i64, String> = chosen.into_iter().collect();
    let mut out = Vec::with_capacity(seqs.len());
    for (seq, m) in memories::get_by_seqs(c, &seqs)? {
        let (Some(hash), true) = (hashes.get(&seq), m.status == Status::Active) else {
            continue;
        };
        let similarity = match (&my_vec, vectors.get(&VecKey::Memory(seq))) {
            (Some(a), Some(b)) => Some(cosine(a, b)),
            _ => None,
        };
        out.push(Neighbour {
            memory: judge_memory(&m, similarity),
            hash: hash.clone(),
        });
    }
    Ok(out)
}

/// The projects a router may choose from (all but `global`), with a few recent titles each.
fn route_candidates(c: &Connection) -> Result<Vec<ProjectOption>> {
    let mut stmt = c.prepare(
        "SELECT slug, name FROM projects WHERE slug <> 'global' ORDER BY created_at DESC, slug LIMIT ?1",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([ROUTE_PROJECTS as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut titles = c.prepare_cached(
        "SELECT m.title FROM memories m JOIN projects p ON p.id = m.project_id
         WHERE p.slug = ?1 AND m.status = 'active' ORDER BY m.updated_at DESC LIMIT ?2",
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for (slug, name) in rows {
        let hint: Vec<String> = titles
            .query_map(rusqlite::params![slug, ROUTE_HINTS as i64], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        out.push(ProjectOption {
            slug,
            name,
            hint: hint.join("; "),
        });
    }
    Ok(out)
}

/// What the failures of model calls mean for the rest of the judgement.
#[derive(Default)]
struct Outcome {
    /// Some call failed for a reason that may pass: judge this memory again later.
    retry: bool,
    /// ... and one of those failures was not just the judge being unreachable (it counts
    /// toward giving up).
    counted: bool,
    /// A call failed that way: the model is not asked again for this memory in this round (the
    /// retry asks), so an outage costs one failed call chain, not one per question.
    model_down: bool,
}

impl Outcome {
    /// Sorts the failure of one model call: a denial or a used-up budget is a decision (no
    /// retry), anything else is a failure to retry.
    fn note(&mut self, id: &str, what: &str, e: &JudgeError) {
        match e {
            JudgeError::Denied | JudgeError::Budget(_) => {
                tracing::debug!(
                    id,
                    what,
                    "the model judge may not answer; the heuristic decides"
                );
            }
            e => {
                tracing::info!(error = %e, id, what, "the judge failed; the heuristic decides for now");
                self.retry = true;
                self.model_down = true;
                self.counted |= !matches!(e, JudgeError::Unavailable(_));
            }
        }
    }
}

/// Whether moving the memory into the project rooted at `root` would change its text under
/// that project's privacy rules (a stricter project): such a move is left to a person, whose
/// answer goes through the same screening as any other move.
fn rules_would_change(cfg: &JudgeWorkerConfig, root: Option<&str>, m: &Memory) -> bool {
    let r = cfg.redactors.for_root(root.map(Path::new));
    [&m.title, &m.body]
        .into_iter()
        .chain(m.topics.iter())
        .any(|t| !r.apply(t).found.is_empty())
}

/// What the judge reads about a memory: the memory, its neighbours.
struct Read {
    memory: Memory,
    me: JudgeMemory,
    found: Vec<Neighbour>,
}

/// `None` when the memory is not the text (and time) the queue entry was made from.
fn read_item(cfg: &JudgeWorkerConfig, item: &PendingJudgement) -> Result<Option<Read>> {
    cfg.readers.with(|c| {
        // The stamp before the text: an edit in between makes the writer refuse the result.
        let stamp: Option<(String, String)> = c
            .query_row(
                "SELECT content_hash, updated_at FROM memories WHERE seq = ?1",
                [item.seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if stamp.as_ref().map(|(h, u)| (h.as_str(), u.as_str()))
            != Some((item.stamp.as_str(), item.updated_at.as_str()))
        {
            return Ok(None);
        }
        let Some((_, memory)) = memories::get_by_seqs(c, &[item.seq])?.pop() else {
            return Ok(None);
        };
        let me = judge_memory(&memory, None);
        let found = neighbours(cfg, c, item, &me)?;
        Ok(Some(Read { memory, me, found }))
    })
}

/// Judge one memory: the configured judge, the heuristic where it may not or cannot answer.
/// `None` when the memory changed while it was read.
pub fn judge_one(cfg: &JudgeWorkerConfig, item: &PendingJudgement) -> Result<Option<Judgement>> {
    let settings = cfg.settings.get();
    let judge = cfg.judge.current();
    let stand_in = cfg.judge.stand_in();
    let model = judge.provider() != "heuristic";
    let heuristic = HeuristicJudge::default();
    let t = Thresholds {
        auto: settings.judge.auto_apply_threshold,
        review: settings.judge.review_threshold,
    };
    let Some(Read { memory, me, found }) = read_item(cfg, item)? else {
        return Ok(None);
    };
    let existing: Vec<JudgeMemory> = found.iter().map(|n| n.memory.clone()).collect();
    let mut outcome = Outcome {
        // A heuristic standing in for the judge that was asked for is a fallback like an outage.
        retry: stand_in,
        ..Default::default()
    };

    let (relations, provider) = relate(
        &judge,
        &heuristic,
        model,
        &me,
        &existing,
        &mut outcome,
        &item.id,
    );
    // A heuristic that answers because the judge could not is only trusted to suggest.
    let review_only = provider == "heuristic" && outcome.retry;
    let mut decided = decide(&me, &relations, &t, &provider, review_only);

    let kind = (!item.kind_explicit && model && !outcome.model_down)
        .then(|| classify(&judge, &me, &t, &mut outcome, &item.id))
        .flatten();
    let kind_provider = kind.map(|_| judge.provider().to_string());

    let mut project_to = None;
    let mut project_provider = None;
    // Only a memory that fell back to `global` is routed; one that was moved (by anyone) or
    // given a project is where its writer wanted it.
    let routed = if item.project_defaulted && item.project_slug == "global" {
        route_memory(
            cfg,
            &judge,
            &heuristic,
            &memory,
            &me,
            &t,
            model,
            &mut outcome,
            &item.id,
        )?
    } else {
        None
    };
    if let Some((slug, p, by, review)) = routed {
        if review {
            decided.reviews.push(project_review(&me.id, &slug, p, &by));
        } else {
            project_to = Some(slug);
            project_provider = Some(by);
        }
    }

    let mut j = Judgement {
        seq: item.seq,
        id: item.id.clone(),
        stamp: item.stamp.clone(),
        updated_at: item.updated_at.clone(),
        provider,
        actions: decided.actions,
        reviews: decided.reviews,
        kind,
        kind_provider,
        project_to,
        project_provider,
        retry: outcome.retry,
        unavailable: outcome.retry && !outcome.counted,
        retry_wait: retry_backoff(cfg.retry_base, item.attempts + item.unavailable + 1),
        seen: found.into_iter().map(|n| (n.memory.id, n.hash)).collect(),
    };
    if !cfg.applies {
        j.actions.clear();
        j.reviews.clear();
        j.kind = None;
        j.project_to = None;
    }
    Ok(Some(j))
}

/// Relations to the neighbours, and who decided them.
fn relate(
    judge: &Arc<dyn Judge>,
    heuristic: &HeuristicJudge,
    model: bool,
    me: &JudgeMemory,
    existing: &[JudgeMemory],
    outcome: &mut Outcome,
    id: &str,
) -> (Vec<Relation>, String) {
    if existing.is_empty() {
        return (Vec::new(), judge.provider().to_string());
    }
    if model {
        match judge.compare(me, existing) {
            Ok(r) => return (r, judge.provider().to_string()),
            Err(e) => outcome.note(id, "compare", &e),
        }
    }
    (
        heuristic.compare(me, existing).unwrap_or_default(),
        "heuristic".to_string(),
    )
}

/// The kind a model is sure of (and the memory does not have already). A failed
/// classification marks the memory for a later retry, like a failed comparison.
fn classify(
    judge: &Arc<dyn Judge>,
    me: &JudgeMemory,
    t: &Thresholds,
    outcome: &mut Outcome,
    id: &str,
) -> Option<Kind> {
    match judge.classify_kind(me) {
        Ok(d) => d
            .best()
            .filter(|(_, p)| *p >= t.auto)
            .and_then(|(k, _)| Kind::parse(&k))
            .filter(|k| *k != me.kind),
        Err(e) => {
            outcome.note(id, "kind", &e);
            None
        }
    }
}

fn project_review(memory_id: &str, to: &str, p: f64, provider: &str) -> NewReview {
    NewReview {
        kind: ReviewType::Project,
        payload: serde_json::json!({"memory_id": memory_id, "from": "global", "to": to, "p": p, "provider": provider}),
        confidence: Some(p),
        dedupe_key: Some(format!("project:{memory_id}")),
    }
}

/// Where a memory that fell back to `global` belongs: `(project, p, who decided, ask)`. `ask`
/// is true when a person should say (not sure enough, a heuristic's guess, or a destination
/// whose rules would change the text).
#[allow(clippy::too_many_arguments)]
fn route_memory(
    cfg: &JudgeWorkerConfig,
    judge: &Arc<dyn Judge>,
    heuristic: &HeuristicJudge,
    memory: &Memory,
    me: &JudgeMemory,
    t: &Thresholds,
    model: bool,
    outcome: &mut Outcome,
    id: &str,
) -> Result<Option<(String, f64, String, bool)>> {
    let options = cfg.readers.with(route_candidates)?;
    if options.is_empty() {
        return Ok(None);
    }
    let (routed, by) = route(cfg, judge, heuristic, me, &options, t, model, outcome, id);
    let Some((slug, p)) = routed
        .and_then(|d| d.best())
        .filter(|(s, _)| s != "global" && options.iter().any(|o| o.slug == *s))
    else {
        return Ok(None);
    };
    let dest_root = cfg.readers.with(|c| {
        let Some(p) = projects::resolve_slug(c, &slug)? else {
            return Ok(None);
        };
        Ok(Some(projects::root_of(c, &p.id)?))
    })?;
    let auto_ok = p >= t.auto
        && by != "heuristic"
        && dest_root
            .as_ref()
            .is_some_and(|root| !rules_would_change(cfg, root.as_deref(), memory));
    if auto_ok {
        Ok(Some((slug, p, by, false)))
    } else if p >= t.review {
        Ok(Some((slug, p, by, true)))
    } else {
        Ok(None)
    }
}

/// Which project a memory that fell back to `global` belongs to, and who decided. The local
/// heuristic looks first: if it points at a project the cloud policy keeps from a model, the
/// memory stays on this machine (a model asked "which project?" would be told what the text
/// is about, and which projects exist). Otherwise a model is offered only the projects the
/// policy lets it be told about (a denied project's name and recent titles are never sent)
/// when one is in force and not down; the heuristic chooses among all of them.
#[allow(clippy::too_many_arguments)]
fn route(
    cfg: &JudgeWorkerConfig,
    judge: &Arc<dyn Judge>,
    heuristic: &HeuristicJudge,
    me: &JudgeMemory,
    options: &[ProjectOption],
    t: &Thresholds,
    model: bool,
    outcome: &mut Outcome,
    id: &str,
) -> (Option<Distribution>, String) {
    let local = heuristic.route_project(me, options).ok();
    let points_at_denied = local
        .as_ref()
        .and_then(|d| d.best())
        .is_some_and(|(s, p)| s != "global" && p >= t.review && !cfg.judge.cloud_allowed(&s));
    if model && !outcome.model_down && !points_at_denied {
        let offered: Vec<ProjectOption> = options
            .iter()
            .filter(|o| cfg.judge.cloud_allowed(&o.slug))
            .cloned()
            .collect();
        if !offered.is_empty() {
            match judge.route_project(me, &offered) {
                Ok(d) => return (Some(d), judge.provider().to_string()),
                Err(e) => outcome.note(id, "route", &e),
            }
        }
    }
    (local, "heuristic".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AliceHome;
    use crate::writer::{ProjectChoice, WriteOutcome, spawn_writer};
    use alice_core::judge::{Choice, JudgeResult, Rubric, Score};
    use alice_core::store::memories::{InsertOutcome, NewMemoryInput};
    use alice_core::vector::VectorIndex;

    /// A model judge whose comparison panics.
    struct Panicky;

    impl Judge for Panicky {
        fn provider(&self) -> &str {
            "panicky"
        }
        fn classify(&self, _: &str, _: &[Choice]) -> JudgeResult<Distribution> {
            Err(JudgeError::Denied)
        }
        fn score(&self, _: &str, _: &Rubric) -> JudgeResult<Score> {
            Err(JudgeError::Denied)
        }
        fn compare(&self, _: &JudgeMemory, _: &[JudgeMemory]) -> JudgeResult<Vec<Relation>> {
            panic!("the judge blew up");
        }
    }

    struct Source(Arc<dyn Judge>);

    impl JudgeSource for Source {
        fn current(&self) -> Arc<dyn Judge> {
            self.0.clone()
        }
        fn cloud_allowed(&self, _: &str) -> bool {
            true
        }
        fn stand_in(&self) -> bool {
            false
        }
    }

    /// M1: a panic while judging neither kills the worker nor leaves the memory `pending`: it
    /// is activated as it is and marked for another try.
    #[test]
    fn a_panicking_judge_keeps_the_worker_alive_and_the_memory_retried() {
        let dir = tempfile::tempdir().unwrap();
        let home = AliceHome::new(dir.path().to_path_buf());
        home.ensure_dirs().unwrap();
        let conn = alice_core::store::open_writer(&home.db()).unwrap();
        let (wake_tx, _wake_rx) = crossbeam_channel::bounded(1);
        let (writer, _t) = spawn_writer(conn, "dev".into(), wake_tx);
        let settings = SettingsHandle::load_with_env(&home, Vec::new());
        let readers = Arc::new(ReaderPool::new(home.db(), 2));
        let remember = |text: &str| match writer
            .submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: text.into(),
                    kind: Some(Kind::Note),
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .unwrap()
        {
            WriteOutcome::Remembered(InsertOutcome::New { id, seq }, _) => (id, seq),
            other => panic!("{other:?}"),
        };
        let row = |seq: i64| -> (String, String) {
            readers
                .with(|c| {
                    Ok(c.query_row(
                        "SELECT content_hash, updated_at FROM memories WHERE seq = ?1",
                        [seq],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?)
                })
                .unwrap()
        };
        let (a, a_seq) = remember("deploy notes for staging");
        let (stamp, updated_at) = row(a_seq);
        writer
            .submit_blocking(WriteOp::ApplyJudgement(Judgement {
                seq: a_seq,
                id: a,
                stamp,
                updated_at,
                provider: "heuristic".into(),
                ..Default::default()
            }))
            .unwrap();
        let (b, b_seq) = remember("deploy notes for staging servers");
        let pending = readers
            .with(|c| alice_core::store::embeddings::pending(c, 10))
            .unwrap();
        let item = pending
            .into_iter()
            .find(|i| i.key == VecKey::Memory(b_seq))
            .unwrap();
        writer
            .submit_blocking(WriteOp::StoreEmbeddings {
                model: "m".into(),
                items: vec![(item.key, item.stamp, vec![1.0; 16])],
            })
            .unwrap();
        let (_wake_tx, wake) = crossbeam_channel::bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_judge_worker(JudgeWorkerConfig {
            readers: readers.clone(),
            writer: writer.clone(),
            index: Arc::new(parking_lot::RwLock::new(Some(Arc::new(
                VectorIndex::new(16).unwrap(),
            )))),
            judge: Arc::new(Source(Arc::new(Panicky))),
            settings,
            redactors: Redactors::fixed(Default::default()),
            model: Arc::new(|| "m".to_string()),
            wake,
            stop: stop.clone(),
            retry_every: Duration::from_secs(3600),
            retry_base: Duration::from_secs(3600),
            applies: true,
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        let state = loop {
            let s: (String, String) = readers
                .with(|c| {
                    Ok(c.query_row(
                        "SELECT judge_state, status FROM memories WHERE id = ?1",
                        [&b],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?)
                })
                .unwrap();
            if s.0 != "pending" || Instant::now() > deadline {
                break s;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(state, ("retry".to_string(), "active".to_string()));
        assert!(!handle.is_finished(), "the worker survived the panic");
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        writer.send_nowait(WriteOp::Shutdown);
    }
}
