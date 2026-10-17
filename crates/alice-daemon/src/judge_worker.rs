//! The judge stage of the write path (spec §5.1 step 5): for each memory whose embedding step is
//! over, compare it with its nearest memories in the project (a distilled one in `global` also
//! with those of the project it was distilled from, Task 11b), set its kind or project when asked,
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
use alice_core::judge::pipeline::{JudgeAction, Thresholds, decide};
use alice_core::judge::text::title_similarity;
use alice_core::judge::{Distribution, Judge, JudgeError, JudgeMemory, ProjectOption, Relation};
use alice_core::model::{Kind, Memory, Origin, Status};
use alice_core::settings::ProjectTree;
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::judge::{PendingJudgement, pending, retry_backoff};
use alice_core::store::memories;
use alice_core::store::projects;
use alice_core::store::reviews::{NewReview, ReviewType};
use alice_core::vector::KeyFilter;
use alice_core::vector::VectorIndex;
use crossbeam_channel::Receiver;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
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
    /// The embedding model whose stored vectors the judge reads and the index holding them,
    /// read together (follows a model switch).
    pub model: Arc<dyn Fn() -> (String, Option<Arc<VectorIndex>>) + Send + Sync>,
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
            // AssertUnwindSafe: after a panic nothing the closure borrowed is read again. `cfg`
            // is only read (its locks are parking_lot ones, which do not poison) and `item` is
            // plain data.
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
                        // Never stuck `pending`: activated without a judgement (a distilled one
                        // then keeps its session's best set at its size, as a judged one does).
                        strikes.remove(&item.seq);
                        let _ = cfg
                            .writer
                            .submit_blocking(WriteOp::ApplyJudgement(Judgement {
                                distill: Some(cfg.settings.get().distill.clone()),
                                ..plain(&item, "heuristic", false)
                            }));
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
        raw: false,
        status: None,
        origin: m.origin,
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

/// Up to `NEIGHBOURS` nearest memories of project `project_id` (the memory's own, or the source
/// project of a distilled one in `global`, see [`source_projects`]) by vector, plus up to
/// `TITLE_CANDIDATES` more of the same kind whose titles are nearly the same. Only OLDER
/// memories that are `active` count (so "the older one survives" holds, a pair judged in one
/// batch meets once, and a retry compares as the first try did). Similarities are the exact
/// cosine of the stored vectors. Hashes are read before the text.
fn neighbours(
    cfg: &JudgeWorkerConfig,
    c: &Connection,
    item: &PendingJudgement,
    me: &JudgeMemory,
    project_id: &str,
) -> Result<Vec<Neighbour>> {
    let (model, live_index) = (cfg.model)();
    let my_vec = embeddings::get_many(c, &model, &[VecKey::Memory(item.seq)])?
        .pop()
        .map(|(_, v)| v);
    // seq -> hash, for the memories chosen, in order.
    let mut chosen: Vec<(i64, String)> = Vec::new();
    if let (Some(v), Some(index)) = (&my_vec, live_index) {
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
            let mut live = active_in_project(c, project_id, &older)?;
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
            rusqlite::params![project_id, me.kind.as_str(), item.seq],
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

/// `(id, slug)` of the projects a distilled memory in `global` came from (Task 11b): those of
/// the chunks in its provenance, or, once raw retention removed them, the project of its author
/// session's capture source. A preference of the user's is stored in `global` (SP2 spec §6.4),
/// so without them the judge would never meet the agent's own copy in the session's project.
fn source_projects(c: &Connection, m: &Memory) -> Result<Vec<(String, String)>> {
    let mut stmt = c.prepare_cached(
        "SELECT DISTINCT p.id, p.slug FROM provenance v
           JOIN chunks ch ON ch.id = v.chunk_id JOIN projects p ON p.id = ch.project_id
         WHERE v.memory_id = ?1 AND p.slug <> 'global' ORDER BY p.slug",
    )?;
    let found: Vec<(String, String)> = stmt
        .query_map([&m.id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let (Some(agent), Some(session), true) = (&m.author.agent, &m.author.session, found.is_empty())
    else {
        return Ok(found);
    };
    let mut stmt = c.prepare_cached(
        "SELECT DISTINCT p.id, p.slug FROM capture_state cs
           JOIN sources s ON s.id = cs.source_id JOIN projects p ON p.id = s.project_id
         WHERE cs.agent = ?1 AND cs.session_id = ?2 AND cs.parent_source_id IS NULL
           AND p.slug <> 'global' ORDER BY p.slug",
    )?;
    Ok(stmt
        .query_map([agent, session], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?)
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

/// Whether moving the memory into the project in the working tree `tree` would change its text
/// under that project's privacy rules (a stricter project): such a move is left to a person,
/// whose answer goes through the same screening as any other move.
fn rules_would_change(cfg: &JudgeWorkerConfig, tree: Option<&ProjectTree>, m: &Memory) -> bool {
    let r = cfg.redactors.for_root(tree);
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

/// `None` when the memory is not the text (and time) the queue entry was made from. A distilled
/// memory in `global` also meets the memories of its source projects (Task 11b), as many and as
/// chosen as its own project's; a project the cloud policy denies is left out, so none of its
/// memories is sent to a model (the heuristic, which compares within one project only, could
/// decide nothing about them either).
fn read_item(cfg: &JudgeWorkerConfig, item: &PendingJudgement) -> Result<Option<Read>> {
    let read = cfg.readers.with(|c| {
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
        let found = neighbours(cfg, c, item, &me, &item.project_id)?;
        let sources = if me.origin == Origin::Distill && item.project_slug == "global" {
            source_projects(c, &memory)?
        } else {
            Vec::new()
        };
        Ok(Some((Read { memory, me, found }, sources)))
    })?;
    let Some((mut read, sources)) = read else {
        return Ok(None);
    };
    // The policy is asked outside the reader: it may need one of its own.
    for (project_id, slug) in sources {
        if !cfg.judge.cloud_allowed(&slug) {
            tracing::debug!(id = %item.id, project = slug, "a distilled memory's source project is kept from the cloud; it is not compared with its memories");
            continue;
        }
        let more = cfg
            .readers
            .with(|c| neighbours(cfg, c, item, &read.me, &project_id))?;
        for n in more {
            if !read.found.iter().any(|f| f.memory.id == n.memory.id) {
                read.found.push(n);
            }
        }
    }
    Ok(Some(read))
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
    // A memory that fell back to `global` is routed; one that was moved (by anyone) or given a
    // project is where its writer wanted it. The local router looks first: if it points at a
    // project the cloud policy keeps from a model, nothing about this memory (its comparison,
    // its kind, its routing) is sent to one.
    let options = if item.project_defaulted && item.project_slug == "global" {
        cfg.readers.with(route_candidates)?
    } else {
        Vec::new()
    };
    let local_route = (!options.is_empty())
        .then(|| heuristic.route_project(&me, &options).ok())
        .flatten();
    let keep_local = local_route
        .as_ref()
        .and_then(|d| d.best())
        .is_some_and(|(s, p)| s != "global" && p >= t.review && !cfg.judge.cloud_allowed(&s));
    let model = model && !keep_local;
    let mut outcome = Outcome {
        // A heuristic standing in for the judge that was asked for is a fallback like an outage.
        retry: stand_in,
        ..Default::default()
    };

    let (mut relations, provider) = relate(
        &judge,
        &heuristic,
        model,
        &me,
        &existing,
        &mut outcome,
        &item.id,
    );
    // A judge does not say how the memories it compared came to exist, nor what they hold (for
    // the "holds more" guard, Task 12c); the neighbours do.
    for r in &mut relations {
        if let Some(e) = existing.iter().find(|e| e.id == r.existing_id) {
            r.existing_origin = e.origin;
            r.existing_body.clone_from(&e.body);
        }
    }
    // A heuristic that answers because the judge could not is only trusted to suggest.
    let review_only = provider == "heuristic" && outcome.retry;
    let mut decided = decide(&me, &relations, &t, &provider, review_only);
    // Task 11b review (I1): a match in another project (a distilled memory in `global` meets its
    // source project's) only ever settles as a duplicate or a link. An update never supersedes
    // across projects: a user-wide memory never retires a project's own, and which came first
    // would decide it (a project's newcomer never meets `global`).
    for a in &mut decided.actions {
        if let JudgeAction::Supersedes { old, p } = a
            && existing
                .iter()
                .any(|e| e.id == *old && e.project != me.project)
        {
            *a = JudgeAction::RelatesTo {
                other: std::mem::take(old),
                p: *p,
            };
        }
    }

    let kind = (!item.kind_explicit && model && !outcome.model_down)
        .then(|| classify(&judge, &me, &t, &mut outcome, &item.id))
        .flatten();
    let kind_provider = kind.map(|_| judge.provider().to_string());

    let mut project_to = None;
    let mut project_provider = None;
    let routed = route_memory(
        cfg,
        &judge,
        &memory,
        &me,
        &options,
        local_route,
        &t,
        model,
        &mut outcome,
        &item.id,
    )?;
    if let Some((slug, p, by, review)) = routed {
        if review {
            // A distilled memory never asks a person (SP2 spec §6.6): it stays where it is.
            if me.origin != Origin::Distill {
                decided.reviews.push(project_review(&me.id, &slug, p, &by));
            }
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
        distill: Some(settings.distill.clone()),
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
/// whose rules would change the text). `options` are the candidate projects (empty: not a
/// memory to route) and `local` the heuristic's answer over them.
#[allow(clippy::too_many_arguments)]
fn route_memory(
    cfg: &JudgeWorkerConfig,
    judge: &Arc<dyn Judge>,
    memory: &Memory,
    me: &JudgeMemory,
    options: &[ProjectOption],
    local: Option<Distribution>,
    t: &Thresholds,
    model: bool,
    outcome: &mut Outcome,
    id: &str,
) -> Result<Option<(String, f64, String, bool)>> {
    if options.is_empty() {
        return Ok(None);
    }
    let (routed, by) = route(cfg, judge, me, options, local, model, outcome, id);
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
        Ok(Some(projects::tree_of(c, &p.id)?))
    })?;
    let auto_ok = p >= t.auto
        && by != "heuristic"
        && dest_root
            .as_ref()
            .is_some_and(|tree| !rules_would_change(cfg, tree.as_ref(), memory));
    if auto_ok {
        Ok(Some((slug, p, by, false)))
    } else if p >= t.review {
        Ok(Some((slug, p, by, true)))
    } else {
        Ok(None)
    }
}

/// Which project a memory that fell back to `global` belongs to, and who decided. A model
/// (`model`: one is in force, not down, and the memory may be sent to it) is offered only the
/// projects the cloud policy lets it be told about (a denied project's name and recent titles
/// are never sent); otherwise `local`, the heuristic's choice among all of them, stands.
#[allow(clippy::too_many_arguments)]
fn route(
    cfg: &JudgeWorkerConfig,
    judge: &Arc<dyn Judge>,
    me: &JudgeMemory,
    options: &[ProjectOption],
    local: Option<Distribution>,
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
            model: Arc::new(|| ("m".to_string(), None)),
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

    /// A judge source with a fixed judge; `stand_in`: it stands in for the one the settings ask
    /// for (the key is missing).
    struct Fixed(Arc<dyn Judge>, bool);

    impl JudgeSource for Fixed {
        fn current(&self) -> Arc<dyn Judge> {
            self.0.clone()
        }
        fn cloud_allowed(&self, _: &str) -> bool {
            true
        }
        fn stand_in(&self) -> bool {
            self.1
        }
    }

    /// SP2 spec §6.6: a heuristic that decides because Jev cannot (its transport fails) or
    /// because it stands in for Jev (no key yet) decides nothing about a distilled memory: no
    /// action, no review item (not even where it belongs), and the memory waits (`retry`) for
    /// the judge. The same memory written by an agent gets the usual suggestions.
    #[test]
    fn the_stand_in_heuristic_leaves_distilled_memories_waiting() {
        use alice_core::project::ProjectRef;
        let dir = tempfile::tempdir().unwrap();
        let home = AliceHome::new(dir.path().to_path_buf());
        home.ensure_dirs().unwrap();
        let conn = alice_core::store::open_writer(&home.db()).unwrap();
        // A project the router may point a `global` memory at.
        projects::upsert_project(
            &conn,
            &ProjectRef {
                key: "k/billing".into(),
                slug: "billing".into(),
                name: "billing".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        let (wake_tx, _wake_rx) = crossbeam_channel::bounded(1);
        let (writer, _t) = spawn_writer(conn, "dev".into(), wake_tx);
        let settings = SettingsHandle::load_with_env(&home, Vec::new());
        let readers = Arc::new(ReaderPool::new(home.db(), 2));
        let remember = |text: &str, origin: Origin, project: ProjectChoice| match writer
            .submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: text.into(),
                    kind: Some(Kind::Note),
                    origin,
                    ..Default::default()
                },
                project,
            })
            .unwrap()
        {
            WriteOutcome::Remembered(InsertOutcome::New { id, seq }, _) => (id, seq),
            other => panic!("{other:?}"),
        };
        let global = || ProjectChoice::Slug("global".into());
        let embed_all = || {
            let items = readers
                .with(|c| alice_core::store::embeddings::pending(c, 10))
                .unwrap();
            writer
                .submit_blocking(WriteOp::StoreEmbeddings {
                    model: "m".into(),
                    items: items
                        .into_iter()
                        .map(|i| (i.key, i.stamp, vec![1.0; 16]))
                        .collect(),
                })
                .unwrap();
        };
        let set_origin = |id: &str, origin: Origin| {
            alice_core::store::open_writer(&home.db())
                .unwrap()
                .execute(
                    "UPDATE memories SET origin = ?1 WHERE id = ?2",
                    [origin.as_str(), id],
                )
                .unwrap();
        };
        let (old, old_seq) = remember("deploy notes for staging", Origin::Agent, global());
        embed_all();
        let (stamp, updated_at) = readers
            .with(|c| {
                Ok(c.query_row(
                    "SELECT content_hash, updated_at FROM memories WHERE seq = ?1",
                    [old_seq],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        writer
            .submit_blocking(WriteOp::ApplyJudgement(Judgement {
                seq: old_seq,
                id: old,
                stamp,
                updated_at,
                provider: "heuristic".into(),
                ..Default::default()
            }))
            .unwrap();
        // A near twin of the older memory (same vector, other words): the heuristic would ask
        // to merge them. And a memory in `global` that names a project: it would ask to move it.
        let (twin, _) = remember(
            "deploy notes for staging servers",
            Origin::Distill,
            global(),
        );
        let (stray, _) = remember(
            "billing invoices go out on the first of the month",
            Origin::Distill,
            ProjectChoice::Fallback,
        );
        embed_all();
        let items = readers.with(|c| pending(c, BATCH)).unwrap();
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            [twin.as_str(), stray.as_str()]
        );

        let transport = crate::judge_http::ScriptedJev::failing(503);
        let jev = alice_core::judge::jev::JevJudge::new(
            alice_core::judge::jev::JevConfig {
                endpoint: "https://api.typesafe.ai/v1/systemone".into(),
                model: "jev-1.13.0".into(),
                api_key: alice_core::settings::SecretValue::new(concat!(
                    "ts_",
                    "live_worker_test_key_01"
                )),
                rate_per_sec: 1_000.0,
                timeout: Duration::from_secs(5),
                daily_budget_usd: 1.0,
                max_retries: 0,
            },
            transport.clone(),
            Arc::new(alice_core::judge::jev::MemoryJevStore::default()),
            Arc::new(crate::judge_http::DaemonJevPolicy::new(
                settings.clone(),
                readers.clone(),
                Redactors::new(settings.clone()),
            )),
        )
        .with_sleeper(Arc::new(|_| {}));
        let cfg = |judge: Arc<dyn JudgeSource>| JudgeWorkerConfig {
            readers: readers.clone(),
            writer: writer.clone(),
            index: Arc::new(parking_lot::RwLock::new(Some(Arc::new(
                VectorIndex::new(16).unwrap(),
            )))),
            judge,
            settings: settings.clone(),
            redactors: Redactors::fixed(Default::default()),
            model: Arc::new(|| ("m".to_string(), None)),
            wake: crossbeam_channel::never(),
            stop: Arc::new(AtomicBool::new(false)),
            retry_every: Duration::from_secs(3600),
            retry_base: Duration::from_secs(3600),
            applies: true,
        };
        let failing = cfg(Arc::new(Fixed(Arc::new(jev), false)));
        let stand_in = cfg(Arc::new(Fixed(Arc::new(HeuristicJudge::default()), true)));
        for (name, cfg) in [("jev down", &failing), ("stand-in", &stand_in)] {
            for item in &items {
                let j = judge_one(cfg, item).unwrap().unwrap();
                assert!(j.retry, "{name}: judged again later");
                assert!(
                    j.actions.is_empty() && j.reviews.is_empty() && j.project_to.is_none(),
                    "{name}: {j:?}"
                );
                // The control: the same memory, written by an agent, is asked about.
                set_origin(&item.id, Origin::Agent);
                let agent = judge_one(cfg, item).unwrap().unwrap();
                set_origin(&item.id, Origin::Distill);
                let asked: Vec<ReviewType> = agent.reviews.iter().map(|r| r.kind).collect();
                let want = if item.id == twin {
                    ReviewType::Merge
                } else {
                    ReviewType::Project
                };
                assert_eq!(asked, [want], "{name}: {agent:?}");
            }
        }
        assert!(transport.sent() > 0, "the Jev transport was tried");
        for item in &items {
            let j = judge_one(&failing, item).unwrap().unwrap();
            writer.submit_blocking(WriteOp::ApplyJudgement(j)).unwrap();
        }
        let (states, reviews): (Vec<(String, String)>, i64) = readers
            .with(|c| {
                let mut st = c.prepare(
                    "SELECT judge_state, status FROM memories WHERE origin = 'distill' ORDER BY seq",
                )?;
                let states = st
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                Ok((
                    states,
                    c.query_row("SELECT count(*) FROM review_items", [], |r| r.get(0))?,
                ))
            })
            .unwrap();
        let waiting = ("retry".to_string(), "active".to_string());
        assert_eq!(states, [waiting.clone(), waiting]);
        assert_eq!(reviews, 0);
        writer.send_nowait(WriteOp::Shutdown);
    }

    /// A judge source whose cloud policy denies the projects in its list.
    struct Denying(Arc<dyn Judge>, Vec<&'static str>);

    impl JudgeSource for Denying {
        fn current(&self) -> Arc<dyn Judge> {
            self.0.clone()
        }
        fn cloud_allowed(&self, project: &str) -> bool {
            !self.1.contains(&project)
        }
        fn stand_in(&self) -> bool {
            false
        }
    }

    /// Task 11b: a store with a project `shop` and a captured session `S1` of it holding one
    /// chunk, and a live vector index in which every memory has the same vector.
    struct ShopFixture {
        _dir: tempfile::TempDir,
        home: AliceHome,
        writer: WriterHandle,
        settings: Arc<SettingsHandle>,
        readers: Arc<ReaderPool>,
        index: Arc<VectorIndex>,
    }

    impl ShopFixture {
        fn new() -> Self {
            use alice_core::project::ProjectRef;
            let dir = tempfile::tempdir().unwrap();
            let home = AliceHome::new(dir.path().to_path_buf());
            home.ensure_dirs().unwrap();
            let conn = alice_core::store::open_writer(&home.db()).unwrap();
            let shop = projects::upsert_project(
                &conn,
                &ProjectRef {
                    key: "k/shop".into(),
                    slug: "shop".into(),
                    name: "shop".into(),
                    root: None,
                    remote: None,
                },
            )
            .unwrap();
            let now = now_rfc3339();
            conn.execute_batch(&format!(
                "INSERT INTO sources (id, project_id, type, agent, uri, session_id, created_at)
                   VALUES ('SRC1', '{p}', 'session', 'claude-code', 'claude-code:session:S1', 'S1', '{now}');
                 INSERT INTO capture_state (source_id, agent, session_id, created_at)
                   VALUES ('SRC1', 'claude-code', 'S1', '{now}');
                 INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at, embed_state)
                   VALUES ('CH1', 'SRC1', '{p}', 0, '[assistant] a preference', 'h1', '{now}', 'done');",
                p = shop.id
            ))
            .unwrap();
            let (wake_tx, _wake_rx) = crossbeam_channel::bounded(1);
            let (writer, _t) = spawn_writer(conn, "dev".into(), wake_tx);
            ShopFixture {
                settings: SettingsHandle::load_with_env(&home, Vec::new()),
                readers: Arc::new(ReaderPool::new(home.db(), 2)),
                index: Arc::new(VectorIndex::new(16).unwrap()),
                writer,
                home,
                _dir: dir,
            }
        }

        /// A preference in `project`, written by session `S1`'s agent; a distilled one has the
        /// session's chunk as its provenance.
        fn remember(&self, text: &str, project: &str, origin: Origin) -> (String, i64) {
            let distilled = origin == Origin::Distill;
            match self
                .writer
                .submit_blocking(WriteOp::Remember {
                    input: NewMemoryInput {
                        content: text.into(),
                        kind: Some(Kind::Preference),
                        origin,
                        author: alice_core::model::Author {
                            agent: Some("claude-code".into()),
                            session: Some("S1".into()),
                            model: None,
                        },
                        provenance: if distilled {
                            vec!["CH1".into()]
                        } else {
                            vec![]
                        },
                        ..Default::default()
                    },
                    project: ProjectChoice::Slug(project.into()),
                })
                .unwrap()
            {
                WriteOutcome::Remembered(InsertOutcome::New { id, seq }, _) => (id, seq),
                other => panic!("{other:?}"),
            }
        }

        /// Every memory waiting gets the same vector, in the store and in the live index.
        fn embed_all(&self) {
            let items = self
                .readers
                .with(|c| alice_core::store::embeddings::pending(c, 10))
                .unwrap();
            for i in &items {
                self.index.upsert(i.key, &[1.0; 16]).unwrap();
            }
            self.writer
                .submit_blocking(WriteOp::StoreEmbeddings {
                    model: "m".into(),
                    items: items
                        .into_iter()
                        .map(|i| (i.key, i.stamp, vec![1.0; 16]))
                        .collect(),
                })
                .unwrap();
        }

        /// The memory becomes live, as a judgement that decides nothing leaves it.
        fn activate(&self, id: &str, seq: i64) {
            let (stamp, updated_at) = self
                .readers
                .with(|c| {
                    Ok(c.query_row(
                        "SELECT content_hash, updated_at FROM memories WHERE seq = ?1",
                        [seq],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?)
                })
                .unwrap();
            self.writer
                .submit_blocking(WriteOp::ApplyJudgement(Judgement {
                    seq,
                    id: id.into(),
                    stamp,
                    updated_at,
                    provider: "heuristic".into(),
                    ..Default::default()
                }))
                .unwrap();
        }

        /// The judge queue's entry for memory `seq`.
        fn item(&self, seq: i64) -> PendingJudgement {
            self.readers
                .with(|c| pending(c, BATCH))
                .unwrap()
                .into_iter()
                .find(|i| i.seq == seq)
                .unwrap()
        }

        /// The worker with Jev answering `relation` to every comparison, behind a cloud policy
        /// that denies the projects in `denied`; and Jev's transport.
        fn cfg(
            &self,
            relation: (&'static str, f64),
            denied: Vec<&'static str>,
        ) -> (JudgeWorkerConfig, Arc<crate::judge_http::ScriptedJev>) {
            let transport = crate::judge_http::ScriptedJev::new(relation);
            let jev = alice_core::judge::jev::JevJudge::new(
                alice_core::judge::jev::JevConfig {
                    endpoint: "https://api.typesafe.ai/v1/systemone".into(),
                    model: "jev-1.13.0".into(),
                    api_key: alice_core::settings::SecretValue::new(concat!(
                        "ts_",
                        "live_worker_test_key_02"
                    )),
                    rate_per_sec: 1_000.0,
                    timeout: Duration::from_secs(5),
                    daily_budget_usd: 1.0,
                    max_retries: 0,
                },
                transport.clone(),
                Arc::new(alice_core::judge::jev::MemoryJevStore::default()),
                Arc::new(
                    alice_core::judge::jev::FixedPolicy::allow_all(
                        alice_core::redact::Redactor::builtin(),
                    )
                    .denying(&denied),
                ),
            )
            .with_sleeper(Arc::new(|_| {}));
            let index = self.index.clone();
            let cfg = JudgeWorkerConfig {
                readers: self.readers.clone(),
                writer: self.writer.clone(),
                index: Arc::new(parking_lot::RwLock::new(Some(index.clone()))),
                judge: Arc::new(Denying(Arc::new(jev), denied)),
                settings: self.settings.clone(),
                redactors: Redactors::fixed(Default::default()),
                model: Arc::new(move || ("m".to_string(), Some(index.clone()))),
                wake: crossbeam_channel::never(),
                stop: Arc::new(AtomicBool::new(false)),
                retry_every: Duration::from_secs(3600),
                retry_base: Duration::from_secs(3600),
                applies: true,
            };
            (cfg, transport)
        }
    }

    impl Drop for ShopFixture {
        fn drop(&mut self) {
            self.writer.send_nowait(WriteOp::Shutdown);
        }
    }

    /// Task 11b (a): a distilled memory routed to `global` (a preference of the user's) is also
    /// compared with the older live memories of its source project, the project of its
    /// provenance chunks, or once raw retention removed them, of its author session's capture
    /// source. A source project the cloud policy denies is left out: none of its memories is
    /// read into the comparison or sent, and the comparison with the `global` ones still goes to
    /// Jev. An agent's memory in `global` meets no other project.
    #[test]
    fn a_global_distilled_memory_meets_its_source_project_unless_denied() {
        let f = ShopFixture::new();
        let older = [
            f.remember(
                "Dates are written as YYYY-MM-DD everywhere.",
                "global",
                Origin::Agent,
            ),
            f.remember(
                "Always answer in British English (the agent's own copy).",
                "shop",
                Origin::Agent,
            ),
        ];
        f.embed_all();
        for (id, seq) in &older {
            f.activate(id, *seq);
        }
        let (in_global, in_shop) = (older[0].0.clone(), older[1].0.clone());
        let (_, new_seq) = f.remember(
            "From now on I will always answer in British English in every project.",
            "global",
            Origin::Distill,
        );
        let (_, agent_seq) = f.remember(
            "Commit messages are written in the imperative mood.",
            "global",
            Origin::Agent,
        );
        f.embed_all();
        let unrelated = ("unrelated", 1.0);
        let seen = |j: &Judgement| -> Vec<String> {
            let mut ids: Vec<String> = j.seen.iter().map(|(id, _)| id.clone()).collect();
            ids.sort();
            ids
        };
        let both = {
            let mut v = vec![in_global.clone(), in_shop.clone()];
            v.sort();
            v
        };

        // Allowed: the source project's memory is compared too, and sent.
        let (allowed, sent) = f.cfg(unrelated, vec![]);
        let j = judge_one(&allowed, &f.item(new_seq)).unwrap().unwrap();
        assert_eq!(seen(&j), both, "{j:?}");
        assert!(sent.mentioned("the agent's own copy") && sent.mentioned("project shop"));

        // Denied: nothing of `shop` is read into the comparison or sent; `global` still is.
        let (denied, sent) = f.cfg(unrelated, vec!["shop"]);
        let j = judge_one(&denied, &f.item(new_seq)).unwrap().unwrap();
        assert_eq!(seen(&j), std::slice::from_ref(&in_global), "{j:?}");
        assert!(!j.retry, "nothing failed: {j:?}");
        assert_eq!(sent.sent(), 1, "the comparison with `global` went to Jev");
        assert!(!sent.mentioned("the agent's own copy") && !sent.mentioned("project shop"));

        // An agent's memory in `global` stays in `global`.
        let (allowed, _) = f.cfg(unrelated, vec![]);
        let j = judge_one(&allowed, &f.item(agent_seq)).unwrap().unwrap();
        assert_eq!(
            seen(&j),
            std::slice::from_ref(&in_global),
            "an agent's memory meets its own project only: {j:?}"
        );

        // Raw retention removed the chunks (and their provenance): the author session's capture
        // source still names the project.
        alice_core::store::open_writer(&f.home.db())
            .unwrap()
            .execute_batch("DELETE FROM provenance; DELETE FROM chunks;")
            .unwrap();
        let (allowed, sent) = f.cfg(unrelated, vec![]);
        let j = judge_one(&allowed, &f.item(new_seq)).unwrap().unwrap();
        assert_eq!(seen(&j), both, "{j:?}");
        assert!(sent.mentioned("the agent's own copy"));
    }

    /// Task 11b review (I1): across projects a match only ever settles as a duplicate or a link.
    /// A distilled memory in `global` that updates an older distilled memory of its source
    /// project at the auto threshold does not supersede it: both stay, linked `relates_to`, and
    /// nothing is asked. The same update of an older distilled memory in `global` still
    /// supersedes it (Task 11).
    #[test]
    fn a_global_distilled_memory_never_supersedes_across_projects() {
        let f = ShopFixture::new();
        let (in_shop, shop_seq) = f.remember(
            "Use npm in this repository: CI caches its store (the project's own rule).",
            "shop",
            Origin::Distill,
        );
        let (in_global, global_seq) = f.remember(
            "The owner prefers npm for new projects.",
            "global",
            Origin::Distill,
        );
        f.embed_all();
        f.activate(&in_shop, shop_seq);
        f.activate(&in_global, global_seq);
        let (new_id, new_seq) = f.remember(
            "I prefer pnpm everywhere, in every project.",
            "global",
            Origin::Distill,
        );
        f.embed_all();
        let (cfg, _) = f.cfg(("updates", 0.95), vec![]);
        let j = judge_one(&cfg, &f.item(new_seq)).unwrap().unwrap();
        let actions: Vec<(&str, &str)> = j
            .actions
            .iter()
            .map(|a| match a {
                JudgeAction::Supersedes { old, .. } => ("supersedes", old.as_str()),
                JudgeAction::RelatesTo { other, .. } => ("relates_to", other.as_str()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            actions,
            [
                ("supersedes", in_global.as_str()),
                ("relates_to", in_shop.as_str())
            ],
            "{j:?}"
        );
        assert!(j.reviews.is_empty(), "{j:?}");

        f.writer
            .submit_blocking(WriteOp::ApplyJudgement(j))
            .unwrap();
        // `(id, status)` of every memory, `(to, type)` of the newcomer's links, review items.
        type Pairs = Vec<(String, String)>;
        let (statuses, links, reviews): (Pairs, Pairs, i64) = f
            .readers
            .with(|c| {
                let mut st = c.prepare("SELECT id, status FROM memories ORDER BY seq")?;
                let statuses = st
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                let mut st =
                    c.prepare("SELECT to_id, type FROM links WHERE from_id = ?1 ORDER BY type")?;
                let links = st
                    .query_map([&new_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                let reviews = c.query_row("SELECT count(*) FROM review_items", [], |r| r.get(0))?;
                Ok((statuses, links, reviews))
            })
            .unwrap();
        let status = |id: &str| -> &str {
            statuses
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, s)| s.as_str())
                .unwrap()
        };
        assert_eq!(status(&in_shop), "active", "the project's own rule stays");
        assert_eq!(status(&new_id), "active");
        assert_eq!(status(&in_global), "superseded");
        assert_eq!(
            links,
            [
                (in_shop.clone(), "relates_to".to_string()),
                (in_global.clone(), "supersedes".to_string()),
            ]
        );
        assert_eq!(reviews, 0);
    }
}
