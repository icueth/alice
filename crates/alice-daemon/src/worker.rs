//! Embedding worker. Reads pending items, embeds them off the writer thread, hands the vectors
//! to the writer (which stores them only if the text did not change meanwhile), then adds the
//! stored ones to the in-memory HNSW index.
//!
//! The embedding model is read from the [`ActiveSlot`] at the top of every round, so a model
//! switch (see `reembed`) takes effect between two batches. When the active embedder is a remote
//! one, no text leaves the machine unless the cloud policy allows its project: a denied row is
//! marked `skipped_cloud` (searchable by words) and is queued again when the policy or the
//! provider changes.

use crate::service::{ActiveSlot, IndexSlot, snapshot};
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};
use alice_core::embed::Embedder;
use alice_core::error::AliceError;
use alice_core::judge::jev::JevPolicy;
use alice_core::redact::Redactor;
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, PendingItem, VecKey};
use alice_core::vector::VectorIndex;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Embedding attempts per text before it is given up (it stays searchable by words).
const MAX_ATTEMPTS: u32 = 5;
/// How often a changed index is saved to its files, busy or idle: a crash then loses at most
/// this much work, which the next start catches up by content.
pub const SAVE_EVERY: Duration = Duration::from_secs(30);
const FIRST_BACKOFF: Duration = if cfg!(test) {
    Duration::from_millis(5)
} else {
    Duration::from_millis(500)
};
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long an idle worker waits for a wake-up before it looks at the queue again.
const IDLE_WAIT: Duration = Duration::from_secs(1);
const REBUILD_WAIT: Duration = Duration::from_millis(200);
/// How often, when idle, rows held back by the cloud policy are looked at again (a project file
/// may have changed; a settings change asks at once through `recheck_skipped`).
const RECHECK_SKIPPED_EVERY: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(300)
};
/// Rows queued again per writer job.
const REQUEUE_SLICE: usize = 500;

pub struct WorkerConfig {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    /// The embedder, model and index files in use; read at the top of every round.
    pub active: Arc<ActiveSlot>,
    pub wake: Receiver<()>,
    pub stop: Arc<AtomicBool>,
    pub batch: usize,
    /// Told after each batch that reached the writer: memories whose embedding step is over wait
    /// for the judge (`bounded(1)`, so wake-ups coalesce).
    pub judge_wake: Option<Sender<()>>,
    /// How often a changed index is saved while the worker runs (`SAVE_EVERY`).
    pub save_every: Duration,
    /// Set by a re-embed for its final pass: the worker embeds nothing (it finishes the batch
    /// in hand first) and sets `parked` once it has stopped.
    pub paused: Arc<AtomicBool>,
    pub parked: Arc<AtomicBool>,
    /// The cloud policy, asked per project before text goes to a remote embedder. `None`: no
    /// remote text is sent at all.
    pub policy: Option<Arc<dyn JevPolicy>>,
    /// Set when the policy or the provider may have changed: rows held back are looked at
    /// again now rather than at the next periodic look.
    pub recheck_skipped: Arc<AtomicBool>,
    /// Health's `embedding_error`: the worker says there that the provider is unavailable (and
    /// takes it back when a batch goes through), so health names the cause of a backlog.
    pub error: Arc<parking_lot::RwLock<Option<String>>>,
}

/// What the worker puts in `embedding_error` while the provider is unavailable.
pub const UNAVAILABLE_NOTE: &str =
    "the embedding provider is unavailable, texts wait in the queue: ";

/// Why one text was not embedded, and whether that says anything about the text.
struct Failure {
    message: String,
    /// The provider could not be reached or would not serve: the text stays queued.
    unavailable: bool,
}

pub fn spawn_worker(cfg: WorkerConfig) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("alice-embed".into())
        .spawn(move || run(cfg))
        .expect("spawn worker thread")
}

/// Tell the judge worker that memories may be ready for it.
fn wake_judge(cfg: &WorkerConfig) {
    if let Some(tx) = &cfg.judge_wake {
        let _ = tx.try_send(());
    }
}

/// Save the index when it changed and `save_every` has passed since the last save.
fn save_if_due(
    cfg: &WorkerConfig,
    index: &VectorIndex,
    path: &std::path::Path,
    last_save: &mut Instant,
) {
    if index.is_dirty() && last_save.elapsed() >= cfg.save_every {
        if let Err(e) = index.save(path) {
            tracing::warn!(error = %e, "saving the vector index failed");
        }
        *last_save = Instant::now();
    }
}

/// Sleep for `d`, waking early when `stop` is set.
pub(crate) fn nap(stop: &AtomicBool, d: Duration) {
    let until = Instant::now() + d;
    while !stop.load(Ordering::Relaxed) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// Like [`nap`], but also ends when the model in force is replaced (a provider that was down
/// came back as a new embedder): then there is no reason to wait out the backoff. True when it
/// ended for that reason.
fn nap_unless_replaced(
    stop: &AtomicBool,
    d: Duration,
    slot: &ActiveSlot,
    seen: &Arc<crate::service::Active>,
) -> bool {
    let until = Instant::now() + d;
    while !stop.load(Ordering::Relaxed) {
        if !Arc::ptr_eq(&*slot.read(), seen) {
            return true;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
    false
}

/// `embed_passages`, with a panic inside the embedder (tokenizer or runtime on an odd text)
/// turned into an error instead of killing the worker thread.
pub(crate) fn embed_guarded(
    embedder: &dyn Embedder,
    texts: &[String],
) -> alice_core::error::Result<Vec<Vec<f32>>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        embedder.embed_passages(texts)
    }))
    .unwrap_or_else(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        tracing::error!(panic = %msg, texts = texts.len(), "the embedder panicked");
        Err(AliceError::Other(format!("the embedder panicked: {msg}")))
    })
}

/// One vector (or error) per item. A failing (or panicking) batch is retried text by text, so
/// one text the model cannot handle does not fail the others, unless `isolate` is false (a
/// remote provider: the next round asks for a smaller batch instead) or the provider said it
/// is unavailable (asking again, text by text, would only repeat the failure).
fn embed_each(
    embedder: &dyn Embedder,
    texts: Vec<String>,
    isolate: bool,
) -> Vec<Result<Vec<f32>, Failure>> {
    let whole = embed_guarded(embedder, &texts).and_then(|v| {
        if v.len() == texts.len() {
            Ok(v)
        } else {
            Err(AliceError::Other(format!(
                "the embedder returned {} vectors for {} texts",
                v.len(),
                texts.len()
            )))
        }
    });
    match whole {
        Ok(vectors) => vectors.into_iter().map(Ok).collect(),
        Err(e) if texts.len() == 1 || !isolate || matches!(e, AliceError::Unavailable(_)) => {
            let f = Failure {
                message: e.to_string(),
                unavailable: matches!(e, AliceError::Unavailable(_)),
            };
            texts
                .iter()
                .map(|_| {
                    Err(Failure {
                        message: f.message.clone(),
                        unavailable: f.unavailable,
                    })
                })
                .collect()
        }
        Err(_) => texts
            .iter()
            .map(|t| match embed_guarded(embedder, std::slice::from_ref(t)) {
                Ok(mut v) if v.len() == 1 => Ok(v.remove(0)),
                Ok(v) => Err(Failure {
                    message: format!("the embedder returned {} vectors for 1 text", v.len()),
                    unavailable: false,
                }),
                Err(e) => Err(Failure {
                    unavailable: matches!(e, AliceError::Unavailable(_)),
                    message: e.to_string(),
                }),
            })
            .collect(),
    }
}

/// Failed attempts per item, for the text (stamp) that failed; an edited text starts over.
#[derive(Default)]
struct Attempts(HashMap<VecKey, (String, u32)>);

impl Attempts {
    /// Count a failure; true when the item should be given up.
    fn fail(&mut self, item: &PendingItem) -> bool {
        let entry = self
            .0
            .entry(item.key)
            .or_insert_with(|| (item.stamp.clone(), 0));
        if entry.0 != item.stamp {
            *entry = (item.stamp.clone(), 0);
        }
        entry.1 += 1;
        if entry.1 >= MAX_ATTEMPTS {
            self.0.remove(&item.key);
            true
        } else {
            false
        }
    }

    fn clear(&mut self, key: &VecKey) {
        self.0.remove(key);
    }
}

/// The slug of the project with id `project_id` (`None`: unknown or unreadable).
fn slug_of(readers: &ReaderPool, project_id: &str) -> Option<String> {
    readers
        .with(|c| {
            Ok(rusqlite::OptionalExtension::optional(c.query_row(
                "SELECT slug FROM projects WHERE id = ?1",
                [project_id],
                |r| r.get::<_, String>(0),
            ))?)
        })
        .ok()
        .flatten()
}

/// Whether the cloud policy lets text of the project with id `project_id` leave the machine.
/// Fails closed: no policy, an unknown project, an unreadable store all say no.
pub(crate) fn project_allowed(
    readers: &ReaderPool,
    policy: Option<&Arc<dyn JevPolicy>>,
    project_id: &str,
    cache: &mut HashMap<String, bool>,
) -> bool {
    if let Some(known) = cache.get(project_id) {
        return *known;
    }
    let allowed = policy
        .is_some_and(|p| slug_of(readers, project_id).is_some_and(|slug| p.cloud_allowed(&slug)));
    cache.insert(project_id.to_string(), allowed);
    allowed
}

/// The texts of `items` as the embedder may see them. A remote one gets each text redacted with
/// its project's always-on rules (the ones a Jev request gets: today's patterns, whatever
/// `privacy.redact` stored); a local one gets the stored text. Items the policy did not allow
/// never get here (see [`split_by_policy`]).
pub(crate) fn texts_for(
    readers: &ReaderPool,
    policy: Option<&Arc<dyn JevPolicy>>,
    remote: bool,
    items: &[PendingItem],
) -> Vec<String> {
    if !remote {
        return items.iter().map(|i| i.text.clone()).collect();
    }
    let mut rules: HashMap<&str, Arc<Redactor>> = HashMap::new();
    items
        .iter()
        .map(|i| {
            let r = rules.entry(i.project_id.as_str()).or_insert_with(|| {
                match (policy, slug_of(readers, &i.project_id)) {
                    (Some(p), Some(slug)) => p.redactor(&slug),
                    // Never sent (no policy, an unknown project); redacted all the same.
                    _ => Arc::new(Redactor::builtin()),
                }
            });
            r.apply(&i.text).text
        })
        .collect()
}

/// `items` split into those that may be sent to the embedder and those the cloud policy keeps
/// at home. A local embedder sends nothing anywhere: everything is allowed. The policy is asked
/// once per project per call.
pub(crate) fn split_by_policy(
    readers: &ReaderPool,
    policy: Option<&Arc<dyn JevPolicy>>,
    remote: bool,
    items: Vec<PendingItem>,
) -> (Vec<PendingItem>, Vec<PendingItem>) {
    if !remote {
        return (items, Vec::new());
    }
    let mut cache = HashMap::new();
    items
        .into_iter()
        .partition(|i| project_allowed(readers, policy, &i.project_id, &mut cache))
}

/// Queue rows held back by the cloud policy again: all of them when the active embedder is a
/// local one, else those of projects the policy allows now. Cheap when nothing is held back.
fn requeue_skipped(cfg: &WorkerConfig, remote: bool) {
    let projects = match cfg.readers.with(embeddings::skipped_projects) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(error = %e, "looking for rows held back from the embedder failed");
            return;
        }
    };
    let allowed: Option<Vec<String>> = if remote {
        let mut cache = HashMap::new();
        Some(
            projects
                .into_iter()
                .filter(|p| project_allowed(&cfg.readers, cfg.policy.as_ref(), p, &mut cache))
                .collect(),
        )
    } else {
        None
    };
    if allowed.as_ref().is_some_and(|a| a.is_empty()) {
        return;
    }
    loop {
        match cfg.writer.submit_blocking(WriteOp::RequeueSkippedCloud {
            projects: allowed.clone(),
            limit: REQUEUE_SLICE,
        }) {
            Ok(WriteOutcome::Count(0)) => break,
            Ok(WriteOutcome::Count(n)) => {
                tracing::info!(rows = n, "queued rows held back from the embedder again");
            }
            Ok(_) | Err(_) => break,
        }
        if cfg.stop.load(Ordering::Relaxed) {
            break;
        }
    }
}

fn run(cfg: WorkerConfig) {
    let batch = cfg.batch.max(1);
    // The batch asked of a remote provider shrinks after a failure (so a text it refuses is
    // found) and grows back after a success.
    let mut cur_batch = batch;
    // The most a remote provider took at once lately, and how many batches in a row went well.
    let mut ceiling = batch;
    let mut good_run = 0u32;
    let mut attempts = Attempts::default();
    let mut backoff = FIRST_BACKOFF;
    let mut last_save = Instant::now();
    let mut last_recheck: Option<Instant> = None;
    while !cfg.stop.load(Ordering::Relaxed) {
        if cfg.paused.load(Ordering::SeqCst) {
            // A re-embed is switching models: nothing is embedded until it is done.
            cfg.parked.store(true, Ordering::SeqCst);
            nap(&cfg.stop, Duration::from_millis(20));
            continue;
        }
        cfg.parked.store(false, Ordering::SeqCst);
        if cfg.writer.read_only.reason().is_some() {
            // Nothing could be stored; embedding now would only burn CPU.
            nap(&cfg.stop, IDLE_WAIT);
            continue;
        }
        let (active, index) = snapshot(&cfg.active, &cfg.index);
        let Some(index) = index.filter(|i| i.model() == active.model) else {
            // The index is being rebuilt from stored vectors (or is the other model's, a
            // moment before a switch finishes); new vectors would be lost meanwhile.
            nap(&cfg.stop, REBUILD_WAIT);
            continue;
        };
        let model = active.model.clone();
        let remote = active.embedder.is_remote();
        let items = match cfg
            .readers
            .with(|c| embeddings::pending(c, cur_batch.min(active.embedder.max_batch()).max(1)))
        {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "reading the embedding queue failed");
                nap(&cfg.stop, IDLE_WAIT);
                continue;
            }
        };
        if items.is_empty() {
            save_if_due(&cfg, &index, &active.vector_path, &mut last_save);
            let asked = cfg.recheck_skipped.swap(false, Ordering::SeqCst);
            if asked || last_recheck.is_none_or(|t| t.elapsed() >= RECHECK_SKIPPED_EVERY) {
                last_recheck = Some(Instant::now());
                requeue_skipped(&cfg, remote);
            }
            let _ = cfg.wake.recv_timeout(IDLE_WAIT);
            continue;
        }

        // A remote embedder gets only what the cloud policy lets leave the machine.
        let (items, held_back) = split_by_policy(&cfg.readers, cfg.policy.as_ref(), remote, items);
        if !held_back.is_empty() {
            tracing::info!(
                rows = held_back.len(),
                "not sending text of projects the cloud policy keeps at home to the embedding provider"
            );
            let keys = held_back.into_iter().map(|i| (i.key, i.stamp)).collect();
            if let Err(e) = cfg.writer.submit_blocking(WriteOp::EmbedSkipped { keys }) {
                tracing::warn!(error = %e, "recording rows held back from the embedder failed");
                nap(&cfg.stop, IDLE_WAIT);
            }
            wake_judge(&cfg);
        }
        if items.is_empty() {
            continue;
        }

        let n_items = items.len();
        let texts = texts_for(&cfg.readers, cfg.policy.as_ref(), remote, &items);
        let results = embed_each(active.embedder.as_ref(), texts, !remote);
        let mut done = Vec::new();
        let (mut retry, mut give_up) = (Vec::new(), Vec::new());
        let mut first_error = None;
        let mut saw_unavailable = false;
        for (item, r) in items.into_iter().zip(results) {
            match r {
                Ok(v) => done.push((item.key, item.stamp, v)),
                Err(e) => {
                    // A provider that is down says nothing about the text: it stays queued.
                    let unavailable = e.unavailable;
                    saw_unavailable |= unavailable;
                    first_error.get_or_insert(e.message);
                    // A remote batch that failed says nothing about one text in it: the next
                    // round asks for less. Only a text that failed on its own counts.
                    let blame = !remote || n_items == 1;
                    if unavailable || !blame || !attempts.fail(&item) {
                        retry.push((item.key, item.stamp));
                    } else {
                        give_up.push((item.key, item.stamp));
                    }
                }
            }
        }

        let mut healthy = first_error.is_none();
        if saw_unavailable {
            if let Some(e) = &first_error {
                *cfg.error.write() = Some(format!("{UNAVAILABLE_NOTE}{e}"));
            }
        } else if !done.is_empty() {
            let mut note = cfg.error.write();
            if note
                .as_deref()
                .is_some_and(|n| n.starts_with(UNAVAILABLE_NOTE))
            {
                *note = None;
            }
        }
        if !done.is_empty() {
            let keys: Vec<VecKey> = done.iter().map(|(k, _, _)| *k).collect();
            match cfg.writer.submit_blocking(WriteOp::StoreEmbeddings {
                model: model.clone(),
                items: done,
            }) {
                Ok(WriteOutcome::Embedded(applied)) => {
                    keys.iter().for_each(|k| attempts.clear(k));
                    // Items left out changed while they were embedded; they are pending again.
                    if let Err(e) = index.upsert_batch(&applied) {
                        tracing::warn!(error = %e, "vector index upsert failed; the next start rebuilds it");
                    }
                    // After the index has them: the judge looks for neighbours there.
                    wake_judge(&cfg);
                }
                Ok(other) => {
                    tracing::error!(
                        outcome = other.name(),
                        "unexpected writer outcome for stored embeddings"
                    );
                    healthy = false;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "storing embeddings failed");
                    healthy = false;
                }
            }
        }
        if let Some(error) = first_error {
            tracing::warn!(
                %error,
                retrying = retry.len(),
                giving_up = give_up.len(),
                "embedding failed; backing off"
            );
            for (keys, retry) in [(retry, true), (give_up, false)] {
                if keys.is_empty() {
                    continue;
                }
                match cfg
                    .writer
                    .submit_blocking(WriteOp::EmbedFailed { keys, retry })
                {
                    // Given up on: the memory is searchable by words and the judge may have it.
                    Ok(_) if !retry => wake_judge(&cfg),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "recording embedding failures failed"),
                }
            }
        }
        // Also while there is more to do: a long run of embedding must not be one unsaved stretch.
        save_if_due(&cfg, &index, &active.vector_path, &mut last_save);
        if healthy {
            backoff = FIRST_BACKOFF;
            good_run += 1;
            // Try the full batch again after a long run of good ones.
            if good_run >= 50 {
                ceiling = batch;
                good_run = 0;
            }
            cur_batch = (cur_batch * 2).min(ceiling);
        } else {
            good_run = 0;
            if remote {
                ceiling = (n_items / 2).max(1);
                cur_batch = ceiling;
            }
            if nap_unless_replaced(&cfg.stop, backoff, &cfg.active, &active) {
                backoff = FIRST_BACKOFF;
            } else {
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
    let (active, index) = snapshot(&cfg.active, &cfg.index);
    if let Some(index) = index
        && index.is_dirty()
        && index.model() == active.model
        && let Err(e) = index.save(&active.vector_path)
    {
        tracing::warn!(error = %e, "saving the vector index at shutdown failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Active;
    use crate::writer::{ProjectChoice, WriteOp, WriteOutcome, spawn_writer};
    use alice_core::embed::FakeEmbedder;
    use alice_core::error::{AliceError, Result};
    use alice_core::judge::jev::FixedPolicy;
    use alice_core::redact::Redactor;
    use alice_core::store::embeddings::VecKey;
    use alice_core::store::memories::{InsertOutcome, MemoryUpdate, NewMemoryInput};
    use alice_core::store::sources::{ChunkInput, SourceInput};
    use alice_core::vector::{KeyFilter, VectorIndex};
    use parking_lot::{Mutex, RwLock};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    type Hook = Box<dyn FnOnce() + Send>;

    /// FakeEmbedder that can fail the first calls, fail texts containing a marker, and run a
    /// hook inside its first call (to simulate an edit while a batch is being embedded).
    struct Scripted {
        inner: FakeEmbedder,
        calls: AtomicUsize,
        fail_first: usize,
        fail_if: Option<&'static str>,
        panic_if: Option<&'static str>,
        on_first_call: Mutex<Option<Hook>>,
        /// A call whose texts contain the marker waits until the sender is used or dropped.
        gate: Mutex<Option<(&'static str, std::sync::mpsc::Receiver<()>)>>,
        /// The first calls fail as "the provider is unavailable".
        unavailable_first: usize,
        /// A call with more texts than this fails (a provider that refuses big batches).
        max_batch: usize,
        /// Says it is a remote embedder (the cloud policy applies).
        remote: bool,
        /// Every text a call was given.
        seen: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new() -> Self {
            Self {
                inner: FakeEmbedder::new(16),
                calls: AtomicUsize::new(0),
                fail_first: 0,
                fail_if: None,
                panic_if: None,
                on_first_call: Mutex::new(None),
                gate: Mutex::new(None),
                unavailable_first: 0,
                max_batch: usize::MAX,
                remote: false,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Embedder for Scripted {
        fn model_id(&self) -> &str {
            "fake"
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().extend(texts.iter().cloned());
            if n < self.unavailable_first {
                return Err(AliceError::Unavailable("scripted outage".into()));
            }
            if texts.len() > self.max_batch {
                return Err(AliceError::Other("scripted refusal of a big batch".into()));
            }
            let hook = self.on_first_call.lock().take();
            if let Some(hook) = hook {
                hook();
            }
            if let Some((marker, rx)) = self.gate.lock().as_ref()
                && texts.iter().any(|t| t.contains(marker))
            {
                let _ = rx.recv();
            }
            let poisoned = self
                .fail_if
                .is_some_and(|m| texts.iter().any(|t| t.contains(m)));
            if let Some(m) = self.panic_if
                && texts.iter().any(|t| t.contains(m))
            {
                panic!("scripted embedder panic on {m:?}");
            }
            if n < self.fail_first || poisoned {
                return Err(AliceError::Other("scripted embedding failure".into()));
            }
            self.inner.embed_passages(texts)
        }
        fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
            self.inner.embed_query(text)
        }
        fn is_remote(&self) -> bool {
            self.remote
        }
    }

    struct Rig {
        dir: tempfile::TempDir,
        writer: WriterHandle,
        index: Arc<IndexSlot>,
        embedder: Arc<Scripted>,
        stop: Arc<AtomicBool>,
        wake: crossbeam_channel::Sender<()>,
        /// What the worker tells the judge worker.
        judge_woken: Receiver<()>,
        worker: Option<JoinHandle<()>>,
        active: Arc<ActiveSlot>,
        paused: Arc<AtomicBool>,
        parked: Arc<AtomicBool>,
        recheck: Arc<AtomicBool>,
        error: Arc<RwLock<Option<String>>>,
    }

    impl Rig {
        fn db(&self) -> PathBuf {
            self.dir.path().join("a.db")
        }

        fn vector_path(&self) -> PathBuf {
            self.dir.path().join("vectors").join("fake.usearch")
        }

        fn read<T: rusqlite::types::FromSql>(&self, sql: &str, p: impl rusqlite::Params) -> T {
            let c = alice_core::store::open_reader(&self.db()).unwrap();
            c.query_row(sql, p, |r| r.get(0)).unwrap()
        }

        fn remember(&self, text: &str) -> (String, i64) {
            match self
                .writer
                .submit_blocking(WriteOp::Remember {
                    input: NewMemoryInput {
                        content: text.into(),
                        ..Default::default()
                    },
                    project: ProjectChoice::Slug("global".into()),
                })
                .unwrap()
            {
                WriteOutcome::Remembered(InsertOutcome::New { id, seq }, _) => (id, seq),
                other => panic!("{other:?}"),
            }
        }

        fn index_len(&self) -> usize {
            self.index.read().as_ref().map_or(0, |i| i.len())
        }

        fn stop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = self.wake.try_send(());
            if let Some(w) = self.worker.take() {
                // Not unwrap: this also runs while a failed test unwinds.
                let _ = w.join();
            }
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop();
            self.writer.send_nowait(WriteOp::Shutdown);
        }
    }

    fn rig(
        index: Option<VectorIndex>,
        script: impl FnOnce(&mut Scripted, &WriterHandle, PathBuf),
    ) -> Rig {
        rig_with(index, SAVE_EVERY, 32, script)
    }

    fn rig_with(
        index: Option<VectorIndex>,
        save_every: Duration,
        batch: usize,
        script: impl FnOnce(&mut Scripted, &WriterHandle, PathBuf),
    ) -> Rig {
        rig_full(index, save_every, batch, None, script)
    }

    fn rig_full(
        index: Option<VectorIndex>,
        save_every: Duration,
        batch: usize,
        policy: Option<Arc<dyn JevPolicy>>,
        script: impl FnOnce(&mut Scripted, &WriterHandle, PathBuf),
    ) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("a.db");
        let conn = alice_core::store::open_writer(&db).unwrap();
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let (judge_wake_tx, judge_wake_rx) = crossbeam_channel::bounded(1);
        let (writer, _writer_thread) = spawn_writer(conn, "dev".into(), wake_tx.clone());
        let mut embedder = Scripted::new();
        script(&mut embedder, &writer, db.clone());
        let embedder = Arc::new(embedder);
        let index: Arc<IndexSlot> = Arc::new(RwLock::new(index.map(Arc::new)));
        let stop = Arc::new(AtomicBool::new(false));
        let active: Arc<ActiveSlot> = Arc::new(RwLock::new(Arc::new(Active {
            embedder: embedder.clone(),
            model: "fake".into(),
            vector_path: dir.path().join("vectors").join("fake.usearch"),
        })));
        let (paused, parked, recheck) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let error: Arc<RwLock<Option<String>>> = Arc::default();
        let worker = spawn_worker(WorkerConfig {
            readers: Arc::new(ReaderPool::new(db, 2)),
            writer: writer.clone(),
            index: index.clone(),
            active: active.clone(),
            wake: wake_rx,
            stop: stop.clone(),
            batch,
            judge_wake: Some(judge_wake_tx),
            save_every,
            paused: paused.clone(),
            parked: parked.clone(),
            policy,
            recheck_skipped: recheck.clone(),
            error: error.clone(),
        });
        Rig {
            error,
            active,
            paused,
            parked,
            recheck,
            dir,
            writer,
            index,
            embedder,
            stop,
            wake: wake_tx,
            judge_woken: judge_wake_rx,
            worker: Some(worker),
        }
    }

    fn fresh_index() -> Option<VectorIndex> {
        Some(VectorIndex::for_model("fake", 16).unwrap())
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn backlog(r: &Rig) -> usize {
        let c = alice_core::store::open_reader(&r.db()).unwrap();
        embeddings::backlog(&c).unwrap()
    }

    /// B4: a long run of embedding is saved as it goes, not only when the queue is empty: the
    /// first batch is on disk while the second is still being embedded.
    #[test]
    fn the_index_is_saved_while_the_worker_is_still_busy() {
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let r = rig_with(fresh_index(), Duration::ZERO, 1, |e, _, _| {
            *e.gate.get_mut() = Some(("stuck", gate));
        });
        r.remember("first memory is embedded at once");
        r.remember("stuck memory waits in the embedder");
        // Both parts are written one after the other: wait until the files open.
        wait_until("a save during the run", || {
            VectorIndex::open(&r.vector_path(), "fake", 16).is_ok()
        });
        assert_eq!(
            backlog(&r),
            1,
            "the worker is still busy with the second memory"
        );
        let saved = VectorIndex::open(&r.vector_path(), "fake", 16).unwrap();
        assert_eq!(saved.len(), 1, "the file holds the first batch");
        drop(release);
        wait_until("the queue to empty", || backlog(&r) == 0);
    }

    #[test]
    fn pending_memories_and_chunks_are_embedded_and_indexed() {
        let r = rig(fresh_index(), |_, _, _| {});
        let (_, a) = r.remember("sqlite writer lease timeout");
        let (_, b) = r.remember("thai trigram search");
        r.writer
            .submit_blocking(WriteOp::Ingest {
                project: ProjectChoice::Slug("global".into()),
                source: SourceInput {
                    uri: "s".into(),
                    ..Default::default()
                },
                chunks: (0..3)
                    .map(|i| ChunkInput {
                        ord: i,
                        text: format!("chunk number {i}"),
                        ..Default::default()
                    })
                    .collect(),
            })
            .unwrap();
        wait_until("the backlog to drain", || backlog(&r) == 0);
        wait_until("the index to hold every vector", || r.index_len() == 5);
        // Activation moved to the judge, which does not run in this rig.
        for seq in [a, b] {
            let row: (String, String, String) = {
                let c = alice_core::store::open_reader(&r.db()).unwrap();
                c.query_row(
                    "SELECT status, judge_state, embed_state FROM memories WHERE seq = ?1",
                    [seq],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap()
            };
            assert_eq!(
                row,
                (
                    "pending".to_string(),
                    "pending".to_string(),
                    "done".to_string()
                )
            );
        }
        let stored: i64 = r.read("SELECT count(*) FROM embeddings WHERE model = 'fake'", []);
        assert_eq!(stored, 5);
        let q = r
            .embedder
            .embed_query("sqlite writer lease timeout")
            .unwrap();
        let hits = r
            .index
            .read()
            .clone()
            .unwrap()
            .search(&q, 1, KeyFilter::Memories)
            .unwrap();
        assert_eq!(hits[0].0, VecKey::Memory(a));
    }

    #[test]
    fn a_failing_embedder_is_retried_with_backoff_then_given_up() {
        let r = rig(fresh_index(), |e, _, _| e.fail_first = usize::MAX);
        let (_, seq) = r.remember("never embeddable");
        wait_until("the item to be given up", || {
            r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [seq]) == "failed"
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            r.embedder.calls.load(Ordering::SeqCst),
            MAX_ATTEMPTS as usize
        );
        let status: String = r.read("SELECT status FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(
            status, "pending",
            "still live (pending memories are searched by words); the judge activates it"
        );
        assert_eq!(r.index_len(), 0);
    }

    /// The judge waits for the embedding step: it is told after vectors are stored (and indexed)
    /// and after an embedding is given up, but not after a failure that will be retried.
    #[test]
    fn the_judge_is_woken_after_vectors_are_stored_and_after_a_give_up() {
        let r = rig(fresh_index(), |_, _, _| {});
        r.remember("woken after storing");
        r.judge_woken
            .recv_timeout(Duration::from_secs(20))
            .expect("woken after the vector was stored");
        assert_eq!(
            r.index_len(),
            1,
            "told only once the index holds the vector"
        );

        let r = rig(fresh_index(), |e, _, _| e.fail_first = usize::MAX);
        r.remember("never embeddable");
        r.judge_woken
            .recv_timeout(Duration::from_secs(20))
            .expect("woken after the give-up");
        assert_eq!(
            r.embedder.calls.load(Ordering::SeqCst),
            MAX_ATTEMPTS as usize,
            "not woken by the retries before it"
        );
    }

    #[test]
    fn a_transient_failure_is_retried_until_it_succeeds() {
        let r = rig(fresh_index(), |e, _, _| e.fail_first = 2);
        let (_, seq) = r.remember("flaky at first");
        wait_until("the item to be embedded", || {
            r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [seq]) == "done"
        });
        assert_eq!(r.embedder.calls.load(Ordering::SeqCst), 3);
        wait_until("the index to hold it", || r.index_len() == 1);
    }

    /// One bad text must not fail the rest of its batch.
    #[test]
    fn a_failing_text_does_not_fail_its_batch() {
        // No index yet: the worker waits, so both texts end up in the same batch.
        let r = rig(None, |e, _, _| e.fail_if = Some("poison"));
        let (_, good) = r.remember("perfectly fine text");
        let (_, bad) = r.remember("poison pill");
        *r.index.write() = fresh_index().map(Arc::new);
        wait_until("the bad item to be given up", || {
            r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [bad]) == "failed"
        });
        let state: String = r.read("SELECT embed_state FROM memories WHERE seq = ?1", [good]);
        assert_eq!(state, "done");
        wait_until("the index to hold the good item", || r.index_len() == 1);
    }

    /// A panic in the embedder (tokenizer or runtime on one odd text) must not kill the worker:
    /// it counts as a failure of that text, the rest of the queue is embedded, and the text is
    /// given up after MAX_ATTEMPTS instead of blocking the queue on every restart.
    #[test]
    fn an_embedder_panic_fails_only_its_text_and_the_worker_keeps_going() {
        // No index yet: the worker waits, so both texts end up in the same batch.
        let r = rig(None, |e, _, _| e.panic_if = Some("explosive"));
        let (_, good) = r.remember("perfectly fine text");
        let (_, bad) = r.remember("explosive text");
        *r.index.write() = fresh_index().map(Arc::new);
        wait_until("the panicking text to be given up", || {
            r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [bad]) == "failed"
        });
        let state: String = r.read("SELECT embed_state FROM memories WHERE seq = ?1", [good]);
        assert_eq!(state, "done");
        let (_, later) = r.remember("written after the panics");
        wait_until("the worker to embed a later text", || {
            r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [later]) == "done"
        });
        wait_until("the index to hold both good texts", || r.index_len() == 2);
        assert!(
            !r.worker.as_ref().unwrap().is_finished(),
            "the worker is alive"
        );
    }

    /// Ruling: an edit that lands while a batch is embedded keeps its stale vector out of the
    /// database and out of the index.
    #[test]
    fn a_vector_for_text_edited_during_embedding_is_not_indexed() {
        let r = rig(fresh_index(), |e, writer, db| {
            // The edited text cannot be embedded, so the only vector ever produced is the stale one.
            e.fail_if = Some("poison");
            let writer = writer.clone();
            *e.on_first_call.lock() = Some(Box::new(move || {
                let c = alice_core::store::open_reader(&db).unwrap();
                let id: String = c
                    .query_row("SELECT id FROM memories ORDER BY seq LIMIT 1", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                writer
                    .submit_blocking(WriteOp::Update {
                        id,
                        update: MemoryUpdate {
                            body: Some("poison: edited while embedding".into()),
                            ..Default::default()
                        },
                        actor: alice_core::store::events::Actor::User,
                    })
                    .unwrap();
            }));
        });
        let (_, seq) = r.remember("original text");
        wait_until("the edited text to be given up", || {
            r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [seq]) == "failed"
        });
        let stored: i64 = r.read("SELECT count(*) FROM embeddings", []);
        assert_eq!(stored, 0, "the stale vector was not stored");
        assert_eq!(r.index_len(), 0, "the stale vector was not indexed");
        let body: String = r.read("SELECT body FROM memories WHERE seq = ?1", [seq]);
        assert!(body.starts_with("poison"), "{body}");
    }

    #[test]
    fn nothing_is_embedded_while_the_index_is_rebuilding() {
        let r = rig(None, |_, _, _| {});
        let (_, seq) = r.remember("wait for the index");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(r.embedder.calls.load(Ordering::SeqCst), 0);
        assert_eq!(backlog(&r), 1);
        *r.index.write() = fresh_index().map(Arc::new);
        wait_until("the item to be embedded", || {
            r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [seq]) == "done"
        });
        wait_until("the index to hold it", || r.index_len() == 1);
    }

    #[test]
    fn stopping_saves_a_changed_index() {
        let mut r = rig(fresh_index(), |_, _, _| {});
        r.remember("save me");
        wait_until("the index to hold it", || r.index_len() == 1);
        let started = Instant::now();
        r.stop();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the idle worker stops promptly"
        );
        let reopened = VectorIndex::open(&r.vector_path(), "fake", 16).unwrap();
        assert_eq!(reopened.len(), 1);
    }

    /// A project that exists in the store but not in the global list: `Detected` creates it.
    fn remember_in(r: &Rig, slug: &str, text: &str) -> i64 {
        let project = alice_core::project::ProjectRef {
            key: format!("path:/tmp/{slug}"),
            slug: slug.into(),
            name: slug.into(),
            root: None,
            remote: None,
        };
        match r
            .writer
            .submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: text.into(),
                    ..Default::default()
                },
                project: ProjectChoice::Detected(project),
            })
            .unwrap()
        {
            WriteOutcome::Remembered(InsertOutcome::New { seq, .. }, _) => seq,
            other => panic!("{other:?}"),
        }
    }

    fn state(r: &Rig, seq: i64) -> String {
        r.read::<String>("SELECT embed_state FROM memories WHERE seq = ?1", [seq])
    }

    fn deny_secret_proj() -> Option<Arc<dyn JevPolicy>> {
        Some(Arc::new(
            FixedPolicy::allow_all(Redactor::builtin()).denying(&["secret-proj"]),
        ))
    }

    /// H1: with a remote embedder the cloud policy is asked per project; text of a denied
    /// project is never handed to the embedder and the row says why.
    #[test]
    fn a_remote_embedder_gets_only_what_the_policy_allows() {
        let r = rig_full(
            fresh_index(),
            SAVE_EVERY,
            32,
            deny_secret_proj(),
            |e, _, _| {
                e.remote = true;
            },
        );
        let open = remember_in(&r, "open-proj", "the pelican nests by the harbour");
        let secret = remember_in(&r, "secret-proj", "the narwhal tusk is in the vault");
        wait_until("the open memory to be embedded", || {
            state(&r, open) == "done"
        });
        wait_until("the denied memory to be held back", || {
            state(&r, secret) == "skipped_cloud"
        });
        let seen = r.embedder.seen.lock().join("\n");
        assert!(seen.contains("pelican"), "{seen}");
        assert!(
            !seen.contains("narwhal") && !seen.contains("vault"),
            "the denied text reached the embedder: {seen}"
        );
        assert_eq!(backlog(&r), 0, "a held-back row is not a backlog");
        assert_eq!(r.index_len(), 1);
        // The memory is still there to be found by words: it is live, not failed.
        let status: String = r.read("SELECT status FROM memories WHERE seq = ?1", [secret]);
        assert_ne!(status, "trashed");
    }

    #[test]
    fn a_remote_embedder_without_a_policy_sends_nothing() {
        let r = rig_full(fresh_index(), SAVE_EVERY, 32, None, |e, _, _| {
            e.remote = true
        });
        let seq = remember_in(&r, "open-proj", "the heron stands in the reeds");
        wait_until("the row to be held back", || {
            state(&r, seq) == "skipped_cloud"
        });
        assert!(r.embedder.seen.lock().is_empty());
    }

    /// Rows held back come back when the embedder becomes a local one, or when the policy
    /// lets their project through.
    #[test]
    fn rows_held_back_are_queued_again_when_the_embedder_turns_local() {
        let r = rig_full(
            fresh_index(),
            SAVE_EVERY,
            32,
            deny_secret_proj(),
            |e, _, _| {
                e.remote = true;
            },
        );
        let secret = remember_in(&r, "secret-proj", "the narwhal tusk is in the vault");
        wait_until("the row to be held back", || {
            state(&r, secret) == "skipped_cloud"
        });
        // The provider changes to a local one.
        let local = Arc::new(Scripted::new());
        let vector_path = r.active.read().vector_path.clone();
        *r.active.write() = Arc::new(Active {
            embedder: local.clone(),
            model: "fake".into(),
            vector_path,
        });
        r.recheck.store(true, Ordering::SeqCst);
        wait_until("the row to be embedded", || state(&r, secret) == "done");
        assert!(local.seen.lock().iter().any(|t| t.contains("narwhal")));
    }

    #[test]
    fn rows_of_a_project_the_policy_now_allows_are_embedded_and_the_others_stay_held_back() {
        let denied: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec!["a-proj", "b-proj"]));
        struct Live(Arc<Mutex<Vec<&'static str>>>);
        impl JevPolicy for Live {
            fn cloud_allowed(&self, p: &str) -> bool {
                !self.0.lock().contains(&p)
            }
            fn redactor(&self, _: &str) -> Arc<Redactor> {
                Arc::new(Redactor::builtin())
            }
        }
        let r = rig_full(
            fresh_index(),
            SAVE_EVERY,
            32,
            Some(Arc::new(Live(denied.clone()))),
            |e, _, _| e.remote = true,
        );
        let a = remember_in(&r, "a-proj", "text of project a");
        let b = remember_in(&r, "b-proj", "text of project b");
        wait_until("both held back", || {
            state(&r, a) == "skipped_cloud" && state(&r, b) == "skipped_cloud"
        });
        denied.lock().retain(|p| *p != "a-proj");
        r.recheck.store(true, Ordering::SeqCst);
        wait_until("a to be embedded", || state(&r, a) == "done");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(state(&r, b), "skipped_cloud");
        assert!(
            !r.embedder
                .seen
                .lock()
                .iter()
                .any(|t| t.contains("project b"))
        );
    }

    /// A provider that is down says nothing about the text: it stays queued however long the
    /// outage lasts (a local failure is given up after MAX_ATTEMPTS).
    #[test]
    fn an_outage_never_costs_a_text_its_place() {
        let r = rig_full(
            fresh_index(),
            SAVE_EVERY,
            32,
            deny_secret_proj(),
            |e, _, _| {
                e.remote = true;
                e.unavailable_first = MAX_ATTEMPTS as usize + 3;
            },
        );
        let seq = remember_in(&r, "open-proj", "the otter slides down the bank");
        wait_until("more failures than the attempts allowed", || {
            r.embedder.calls.load(Ordering::SeqCst) > MAX_ATTEMPTS as usize + 1
        });
        assert_eq!(state(&r, seq), "retry", "still queued, not failed");
        assert!(
            r.error
                .read()
                .as_deref()
                .is_some_and(|e| e.starts_with(UNAVAILABLE_NOTE) && e.contains("scripted outage")),
            "health names the cause: {:?}",
            r.error.read()
        );
        wait_until(
            "the provider to come back and the text to be embedded",
            || state(&r, seq) == "done",
        );
        wait_until("the note to be taken back", || r.error.read().is_none());
    }

    /// C1: a project denied while the provider is down is not sent when the provider is back:
    /// the policy is asked again before every attempt.
    #[test]
    fn a_project_denied_during_an_outage_is_not_sent_after_it() {
        let denied: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        struct Live(Arc<Mutex<Vec<&'static str>>>);
        impl JevPolicy for Live {
            fn cloud_allowed(&self, p: &str) -> bool {
                !self.0.lock().contains(&p)
            }
            fn redactor(&self, _: &str) -> Arc<Redactor> {
                Arc::new(Redactor::builtin())
            }
        }
        let r = rig_full(
            fresh_index(),
            SAVE_EVERY,
            32,
            Some(Arc::new(Live(denied.clone()))),
            |e, _, _| {
                e.remote = true;
                e.unavailable_first = 4;
            },
        );
        let seq = remember_in(&r, "a-proj", "the narwhal tusk is in the vault");
        wait_until("the first failed attempt", || {
            r.embedder.calls.load(Ordering::SeqCst) >= 1
        });
        // Denied during the outage; the provider then comes back.
        denied.lock().push("a-proj");
        let sent_before = r.embedder.seen.lock().len();
        wait_until("the row to be held back", || {
            state(&r, seq) == "skipped_cloud"
        });
        std::thread::sleep(Duration::from_millis(300));
        let seen = r.embedder.seen.lock().clone();
        assert!(
            !seen[sent_before..].iter().any(|t| t.contains("narwhal")),
            "text of a project denied meanwhile was sent: {seen:?}"
        );
    }

    /// A remote provider that refuses a big batch: the batch shrinks until it fits, and no
    /// text is given up for it.
    #[test]
    fn a_batch_a_remote_provider_refuses_is_halved_until_it_fits() {
        let r = rig_full(None, SAVE_EVERY, 32, deny_secret_proj(), |e, _, _| {
            e.remote = true;
            e.max_batch = 2;
        });
        let seqs: Vec<i64> = (0..8)
            .map(|i| remember_in(&r, "open-proj", &format!("memory number {i} about otters")))
            .collect();
        *r.index.write() = fresh_index().map(Arc::new);
        wait_until("every memory to be embedded", || {
            seqs.iter().all(|s| state(&r, *s) == "done")
        });
    }

    #[test]
    fn a_paused_worker_embeds_nothing_and_says_it_has_stopped() {
        let r = rig(fresh_index(), |_, _, _| {});
        r.paused.store(true, Ordering::SeqCst);
        wait_until("the worker to park", || r.parked.load(Ordering::SeqCst));
        let (_, seq) = r.remember("written while paused");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(state(&r, seq), "pending");
        assert_eq!(r.embedder.calls.load(Ordering::SeqCst), 0);
        r.paused.store(false, Ordering::SeqCst);
        wait_until("the memory to be embedded", || state(&r, seq) == "done");
        assert!(!r.parked.load(Ordering::SeqCst));
    }

    /// The index in the slot belongs to the model in force: an index of another model is left
    /// alone (a switch is in progress).
    #[test]
    fn an_index_of_another_model_is_not_written_to() {
        let r = rig(
            Some(VectorIndex::for_model("other-model", 16).unwrap()),
            |_, _, _| {},
        );
        let (_, seq) = r.remember("waits for the right index");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(state(&r, seq), "pending");
        assert_eq!(r.index_len(), 0);
        *r.index.write() = fresh_index().map(Arc::new);
        wait_until("the memory to be embedded", || state(&r, seq) == "done");
    }
}
