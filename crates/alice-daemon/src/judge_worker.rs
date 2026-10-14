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
use alice_core::store::judge::{PendingJudgement, pending};
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
}

impl JudgeSource for JudgeHub {
    fn current(&self) -> Arc<dyn Judge> {
        JudgeHub::current(self)
    }

    fn cloud_allowed(&self, project: &str) -> bool {
        JudgeHub::cloud_allowed(self, project)
    }
}

/// Neighbours compared with a new memory, by vector.
const NEIGHBOURS: usize = 5;
/// Further neighbours of the same kind with a nearly identical title (the "updates" signal).
const TITLE_CANDIDATES: usize = 3;
/// Nearest memories looked at to find `NEIGHBOURS` in the same project.
const SEARCH_DEPTH: usize = 100;
/// Memories judged per look at the queue.
const BATCH: usize = 8;
/// Times a memory is judged in all (the first and the retries after a failure).
pub(crate) const MAX_ATTEMPTS: u32 = 5;
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
    /// How often memories whose judgement failed (`retry`) are queued again.
    pub retry_every: Duration,
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
            match judge_one(&cfg, &item) {
                Ok(Some(j)) => {
                    if let Err(e) = cfg.writer.submit_blocking(WriteOp::ApplyJudgement(j)) {
                        tracing::warn!(error = %e, id = %item.id, "applying a judgement failed");
                        failed = true;
                    }
                }
                // Changed while it was read: the next look at the queue judges the new text.
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, id = %item.id, "judging failed; it is tried again");
                    failed = true;
                }
            }
        }
        if failed {
            // Never spin on a memory that cannot be judged or a writer that cannot apply.
            nap(&cfg.stop, Duration::from_secs(1));
        }
    }
}

/// An existing memory the new one is compared with, and the `content_hash` its text had when
/// it was read (the writer refuses to act on it if that has changed).
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

fn hash_of(c: &Connection, seq: i64) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT content_hash FROM memories WHERE seq = ?1",
        [seq],
        |r| r.get(0),
    )
    .optional()?)
}

/// The `content_hash` of each of `seqs` that exists.
fn hashes(c: &Connection, seqs: &[i64]) -> Result<HashMap<i64, String>> {
    let mut out = HashMap::with_capacity(seqs.len());
    for seq in seqs {
        if let Some(h) = hash_of(c, *seq)? {
            out.insert(*seq, h);
        }
    }
    Ok(out)
}

fn is_live(m: &Memory) -> bool {
    matches!(m.status, Status::Active | Status::Pending)
}

/// Up to `NEIGHBOURS` nearest live memories of the same project by vector, plus up to
/// `TITLE_CANDIDATES` more of the same kind whose titles are nearly the same. Similarities are
/// the exact cosine of the stored vectors.
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
    // A neighbour's hash is read before its text: a change in between shows up as a
    // difference when the writer compares, never as a text newer than its hash being trusted.
    let mut seen_hashes: HashMap<i64, String> = HashMap::new();
    let mut found: Vec<(i64, Memory)> = Vec::new();
    if let (Some(v), Some(index)) = (&my_vec, cfg.index.read().clone()) {
        let hits = index.search(v, SEARCH_DEPTH, KeyFilter::Memories)?;
        let seqs: Vec<i64> = hits
            .iter()
            .filter_map(|(k, _)| match k {
                VecKey::Memory(s) if *s != item.seq => Some(*s),
                _ => None,
            })
            .collect();
        seen_hashes.extend(hashes(c, &seqs)?);
        for (seq, m) in memories::get_by_seqs(c, &seqs)? {
            if found.len() == NEIGHBOURS {
                break;
            }
            if m.project == item.project_slug && is_live(&m) {
                found.push((seq, m));
            }
        }
    }
    let mut stmt = c.prepare_cached(
        "SELECT seq FROM memories WHERE project_id = ?1 AND kind = ?2 AND status IN ('active', 'pending') AND seq <> ?3
         ORDER BY updated_at DESC LIMIT 200",
    )?;
    let same_kind: Vec<i64> = stmt
        .query_map(
            rusqlite::params![item.project_id, me.kind.as_str(), item.seq],
            |r| r.get(0),
        )?
        .collect::<std::result::Result<_, _>>()?;
    let mut added = 0;
    for (seq, h) in hashes(c, &same_kind)? {
        seen_hashes.entry(seq).or_insert(h);
    }
    for (seq, m) in memories::get_by_seqs(c, &same_kind)? {
        if added == TITLE_CANDIDATES {
            break;
        }
        if found.iter().any(|(_, o)| o.id == m.id) || title_similarity(&me.title, &m.title) < 0.9 {
            continue;
        }
        found.push((seq, m));
        added += 1;
    }
    let keys: Vec<VecKey> = found.iter().map(|(s, _)| VecKey::Memory(*s)).collect();
    let vectors: HashMap<VecKey, Vec<f32>> = embeddings::get_many(c, &model, &keys)?
        .into_iter()
        .collect();
    let mut out = Vec::with_capacity(found.len());
    for (seq, m) in &found {
        let similarity = match (&my_vec, vectors.get(&VecKey::Memory(*seq))) {
            (Some(a), Some(b)) => Some(cosine(a, b)),
            _ => None,
        };
        let Some(hash) = seen_hashes.remove(seq) else {
            continue;
        };
        out.push(Neighbour {
            memory: judge_memory(m, similarity),
            hash,
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

/// What a model judge's failure means for the rest of the judgement.
#[derive(Default)]
struct Outcome {
    /// Some call failed for a reason that may pass: judge this memory again later.
    retry: bool,
    /// A call failed that way: the model is not asked again for this memory in this round (the
    /// retry asks), so an outage costs one failed call, not one per question.
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

/// Judge one memory: the configured judge, the heuristic where it may not or cannot answer.
/// `None` when the memory changed while it was read.
pub fn judge_one(cfg: &JudgeWorkerConfig, item: &PendingJudgement) -> Result<Option<Judgement>> {
    let settings = cfg.settings.get();
    let judge = cfg.judge.current();
    let model = judge.provider() != "heuristic";
    let heuristic = HeuristicJudge::default();
    let t = Thresholds {
        auto: settings.judge.auto_apply_threshold,
        review: settings.judge.review_threshold,
    };

    let read = cfg.readers.with(|c| {
        let Some((_, memory)) = memories::get_by_seqs(c, &[item.seq])?.pop() else {
            return Ok(None);
        };
        // The text must still be the text the queue entry was made from.
        if hash_of(c, item.seq)?.as_deref() != Some(item.stamp.as_str()) {
            return Ok(None);
        }
        let me = judge_memory(&memory, None);
        let found = neighbours(cfg, c, item, &me)?;
        Ok(Some((memory, me, found)))
    })?;
    let Some((memory, me, found)) = read else {
        return Ok(None);
    };
    let existing: Vec<JudgeMemory> = found.iter().map(|n| n.memory.clone()).collect();
    let mut outcome = Outcome::default();

    // Relations to the neighbours.
    let (relations, provider): (Vec<Relation>, String) = if existing.is_empty() {
        (Vec::new(), judge.provider().to_string())
    } else {
        let model_answer = if model {
            match judge.compare(&me, &existing) {
                Ok(r) => Some(r),
                Err(e) => {
                    outcome.note(&item.id, "compare", &e);
                    None
                }
            }
        } else {
            None
        };
        match model_answer {
            Some(r) => (r, judge.provider().to_string()),
            None => (
                heuristic.compare(&me, &existing).unwrap_or_default(),
                "heuristic".to_string(),
            ),
        }
    };
    let mut decided = decide(&me, &relations, &t, &provider);

    // Kind: only a model judge may change it (the heuristic's rules are never sure enough). A
    // failed classification marks the memory for a later retry, like a failed comparison.
    let mut kind = None;
    if !item.kind_explicit && model && !outcome.model_down {
        match judge.classify_kind(&me) {
            Ok(d) => {
                kind = d
                    .best()
                    .filter(|(_, p)| *p >= t.auto)
                    .and_then(|(k, _)| Kind::parse(&k))
                    .filter(|k| *k != me.kind);
            }
            Err(e) => outcome.note(&item.id, "kind", &e),
        }
    }

    // Project routing for a memory that fell back to `global`.
    let mut project_to = None;
    let mut project_provider = None;
    if item.project_defaulted {
        let options = cfg.readers.with(route_candidates)?;
        if !options.is_empty() {
            let (routed, by) = route(
                cfg,
                &judge,
                &heuristic,
                &me,
                &options,
                model,
                &mut outcome,
                &item.id,
            );
            if let Some((slug, p)) = routed
                .and_then(|d| d.best())
                .filter(|(s, _)| s != "global" && options.iter().any(|o| o.slug == *s))
            {
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
                        .is_some_and(|root| !rules_would_change(cfg, root.as_deref(), &memory));
                if auto_ok {
                    project_to = Some(slug);
                    project_provider = Some(by);
                } else if p >= t.review {
                    decided.reviews.push(NewReview {
                        kind: ReviewType::Project,
                        payload: serde_json::json!({"memory_id": me.id, "from": "global", "to": slug, "p": p, "provider": by}),
                        confidence: Some(p),
                        dedupe_key: Some(format!("project:{}", me.id)),
                    });
                }
            }
        }
    }

    let kind_provider = kind.map(|_| judge.provider().to_string());
    Ok(Some(Judgement {
        seq: item.seq,
        id: item.id.clone(),
        stamp: item.stamp.clone(),
        provider,
        actions: decided.actions,
        reviews: decided.reviews,
        kind,
        kind_provider,
        project_to,
        project_provider,
        retry: outcome.retry,
        seen: found.into_iter().map(|n| (n.memory.id, n.hash)).collect(),
    }))
}

/// Which project a memory that fell back to `global` belongs to, and who decided. A model is
/// offered only the projects the cloud policy lets it be told about (a denied project's name
/// and recent titles are never sent), and only if a model judge is in force and not down; the
/// heuristic, which runs here, chooses among all of them.
#[allow(clippy::too_many_arguments)]
fn route(
    cfg: &JudgeWorkerConfig,
    judge: &Arc<dyn Judge>,
    heuristic: &HeuristicJudge,
    me: &JudgeMemory,
    options: &[ProjectOption],
    model: bool,
    outcome: &mut Outcome,
    id: &str,
) -> (Option<Distribution>, String) {
    if model && !outcome.model_down {
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
    (
        heuristic.route_project(me, options).ok(),
        "heuristic".to_string(),
    )
}
