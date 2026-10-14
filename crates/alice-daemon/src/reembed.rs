//! Re-embedding after a model change (spec §4.2): the new model's vectors and index are filled
//! in the background while the old model serves queries; then the main embedding worker pauses,
//! a final pass catches rows written or edited meanwhile, and model, embedder and index are
//! swapped at once. Progress is in `meta`, so a restart resumes where it stopped.
//!
//! Safety rules this module keeps:
//! - **Cloud policy.** When the target embedder is remote, every batch goes through the same
//!   per-project check as the embedding worker (`SettingsHandle::cloud_allowed`, all identities
//!   of a project): rows of a denied project are not sent. They get no vector for the target
//!   model and, at the final pass, the state `skipped_cloud` (searchable by words).
//! - **One at a time.** The service keeps at most one re-embed running; starting another stops
//!   and joins the first.
//! - **Quiet.** Between batches it waits for requests to be over (maintenance's `Activity`), so
//!   a recall is not slowed by a re-embed.
//! - **The old model stays in force** until the new index is complete; any failure leaves it
//!   active and says why in `embedding_error`.
//! - **Stale vectors.** A fresh start deletes whatever vectors the target model has left from an
//!   earlier time (they may be stale); a resumed one keeps them.

use crate::maintenance::Activity;
use crate::service::{Active, ActiveSlot, IndexSlot, snapshot};
use crate::worker::{embed_guarded, nap, split_by_policy};
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};
pub use alice_core::api::ReembedStatus;
use alice_core::embed::Embedder;
use alice_core::error::AliceError;
use alice_core::judge::jev::JevPolicy;
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, Owner, PendingItem, VecKey};
use alice_core::vector::VectorIndex;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `meta` key holding the settings (JSON) of a re-embed in progress.
pub const TARGET_KEY: &str = "reembed.target";
/// `meta` key holding the settings (JSON) of the embedding in force.
pub const ACTIVE_KEY: &str = "active_embedding";
/// `meta` key listing (one per line) the models whose vectors a maintenance job should delete.
pub const PURGE_KEY: &str = "purge_model";

pub struct Progress {
    pub model: String,
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// Set just before the switch, or when the run ends without one: health stops showing it.
    /// (Set *before* the switch, so a reader that sees the new model never sees the progress.)
    pub finished: AtomicBool,
}

impl Progress {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.into(),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            finished: AtomicBool::new(false),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> ReembedStatus {
        let total = self.total.load(Ordering::Relaxed);
        ReembedStatus {
            model: self.model.clone(),
            done: self.done.load(Ordering::Relaxed).min(total),
            total,
        }
    }
}

pub struct ReembedConfig {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub target: Arc<dyn Embedder>,
    /// Settings JSON of the target, written to `meta.active_embedding` at the switch.
    pub target_settings: String,
    pub vector_path: PathBuf,
    pub active: Arc<ActiveSlot>,
    pub index: Arc<IndexSlot>,
    pub main_paused: Arc<AtomicBool>,
    /// Set by the main embedding worker once it has stopped (see `WorkerConfig::parked`).
    pub parked: Arc<AtomicBool>,
    pub stop: Arc<AtomicBool>,
    pub progress: Arc<Progress>,
    pub batch: usize,
    /// Set (with the reason) when the target model keeps failing.
    pub error: Arc<RwLock<Option<String>>>,
    /// The cloud policy asked before text goes to a remote target. `None`: nothing is sent.
    pub policy: Option<Arc<dyn JevPolicy>>,
    pub activity: Arc<Activity>,
    /// An interrupted run of this same target: keep its vectors and cursors.
    pub resume: bool,
    /// Told after the switch: rows held back by the cloud policy are looked at again.
    pub recheck_skipped: Arc<AtomicBool>,
    /// Tests shorten the waits between failed tries.
    pub retry_wait: Duration,
}

/// How long the re-embed waits for the main worker to finish the batch in hand.
const PARK_WAIT: Duration = Duration::from_secs(300);
/// Requests must have been over for this long before a batch is sent.
const QUIET: Duration = Duration::from_millis(300);
/// Consecutive tries of a batch the provider would not serve (down, busy, key refused).
const OUTAGE_TRIES: u32 = 12;
const MAX_WAIT: Duration = Duration::from_secs(60);
/// Rows removed per writer job when a fresh start clears an old target's vectors.
const DELETE_SLICE: usize = 500;

fn meta(readers: &ReaderPool, key: &str) -> Option<String> {
    readers
        .with(|c| alice_core::store::maintenance::get_meta(c, key))
        .ok()
        .flatten()
}

fn set_meta(w: &WriterHandle, key: &str, value: &str) -> alice_core::Result<()> {
    w.submit_blocking(WriteOp::SetMeta {
        key: key.into(),
        value: value.into(),
    })
    .map(|_| ())
}

/// The models named in `meta.purge_model`.
pub fn purge_list(readers: &ReaderPool) -> Vec<String> {
    meta(readers, PURGE_KEY)
        .map(|v| {
            v.lines()
                .filter(|l| !l.trim().is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Stop this re-embed and say why in health (`embedding_error`); the old model stays active.
fn give_up(cfg: &ReembedConfig, what: &str, e: impl std::fmt::Display) {
    let msg = format!(
        "re-embedding stopped: {what} failed ({e}); the current model stays active (alice doctor)"
    );
    tracing::warn!("{msg}");
    *cfg.error.write() = Some(msg);
    cfg.progress.finished.store(true, Ordering::SeqCst);
    // Best effort: with the write path broken this fails too, and the next start resumes.
    let _ = cfg.writer.submit_blocking(WriteOp::SetReembedTarget(None));
    cfg.main_paused.store(false, Ordering::SeqCst);
}

pub fn spawn_reembed(cfg: ReembedConfig) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("alice-reembed".into())
        .spawn(move || run(cfg))
        .expect("spawn re-embed thread")
}

/// Why a pass ended early.
enum Halt {
    /// Asked to stop (shutdown, or another target was chosen).
    Stopped,
    /// The target model cannot be used; the message is the reason.
    Failed(String),
}

/// Requests must have been over for [`QUIET`] before a batch is sent, but a daemon that is
/// never quiet still gets one batch every `FORCE_AFTER`, so the re-embed always finishes.
const FORCE_AFTER: Duration = if cfg!(test) {
    Duration::from_millis(1500)
} else {
    Duration::from_secs(600)
};

/// Waits until requests have been over for [`QUIET`] (or `FORCE_AFTER` has passed since the
/// last batch); false when asked to stop meanwhile.
fn wait_for_quiet(cfg: &ReembedConfig, last_batch: &mut Instant) -> bool {
    while !cfg.stop.load(Ordering::Relaxed) {
        if cfg.activity.idle_for() >= QUIET || last_batch.elapsed() >= FORCE_AFTER {
            *last_batch = Instant::now();
            return true;
        }
        nap(&cfg.stop, Duration::from_millis(50));
    }
    false
}

/// What became of a batch: vectors, texts the provider refused, texts the cloud policy keeps
/// at home.
#[derive(Default)]
struct Gated {
    vectors: Vec<(PendingItem, Vec<f32>)>,
    refused: Vec<PendingItem>,
    denied: Vec<PendingItem>,
}

impl Gated {
    fn merge(&mut self, other: Gated) {
        self.vectors.extend(other.vectors);
        self.refused.extend(other.refused);
        self.denied.extend(other.denied);
    }
}

/// Vectors for `items` from the target. **The cloud policy is asked again before every
/// attempt** (not once per batch): while the provider is down the batch is tried again for
/// minutes, and a project denied meanwhile must not be sent when it comes back. A batch the
/// provider refuses is asked text by text (each gated afresh); a text refused on its own is
/// `refused`. An outage is waited for (up to [`OUTAGE_TRIES`] tries, a growing wait).
fn gated_embed(cfg: &ReembedConfig, items: Vec<PendingItem>) -> Result<Gated, Halt> {
    let remote = cfg.target.is_remote();
    let mut out = Gated::default();
    let mut remaining = items;
    let mut failures = 0u32;
    loop {
        if cfg.stop.load(Ordering::Relaxed) {
            return Err(Halt::Stopped);
        }
        let (allowed, denied) =
            split_by_policy(&cfg.readers, cfg.policy.as_ref(), remote, remaining);
        out.denied.extend(denied);
        if allowed.is_empty() {
            return Ok(out);
        }
        let texts: Vec<String> = allowed.iter().map(|i| i.text.clone()).collect();
        match embed_guarded(cfg.target.as_ref(), &texts) {
            Ok(v) if v.len() == texts.len() => {
                *cfg.error.write() = None;
                out.vectors.extend(allowed.into_iter().zip(v));
                return Ok(out);
            }
            Err(AliceError::Unavailable(msg)) => {
                failures += 1;
                if failures >= OUTAGE_TRIES {
                    return Err(Halt::Failed(format!(
                        "re-embedding with {} keeps failing: {msg}",
                        cfg.target.model_id()
                    )));
                }
                *cfg.error.write() = Some(format!(
                    "re-embedding is waiting for {}: {msg} (try {failures} of {OUTAGE_TRIES})",
                    cfg.target.model_id()
                ));
                let wait = cfg
                    .retry_wait
                    .saturating_mul(2u32.saturating_pow(failures.min(6)))
                    .min(MAX_WAIT);
                nap(&cfg.stop, wait);
                remaining = allowed;
            }
            // The provider answered, but not with vectors for these texts.
            Ok(_) | Err(_) => {
                if allowed.len() == 1 {
                    out.refused.extend(allowed);
                } else {
                    for item in allowed {
                        out.merge(gated_embed(cfg, vec![item])?);
                    }
                }
                return Ok(out);
            }
        }
    }
}

fn seq_of(key: VecKey) -> i64 {
    match key {
        VecKey::Memory(s) | VecKey::Chunk(s) => s,
    }
}

fn cursor_key(model: &str, owner: Owner) -> String {
    format!(
        "reembed.{model}.cursor.{}",
        if owner == Owner::Memory {
            "memory"
        } else {
            "chunk"
        }
    )
}

/// One pass over the rows of `owner` that have no vector for the target, after the saved cursor
/// (`first`) or from the start (the final pass, with the main worker parked). Embeds what the
/// cloud policy allows and stores it. In the final pass rows left without a vector are marked,
/// so their state tells why: `skipped_cloud` (policy) or `retry` (the provider refused the text:
/// the embedding worker tries it again, as it does any text).
fn pass(
    cfg: &ReembedConfig,
    index: &VectorIndex,
    model: &str,
    owner: Owner,
    first: bool,
) -> Result<(), Halt> {
    let key = cursor_key(model, owner);
    let mut cursor: i64 = if first {
        meta(&cfg.readers, &key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let fail = |what: &str, e: alice_core::AliceError| Halt::Failed(format!("{what} ({e})"));
    let mut last_batch = Instant::now();
    while !cfg.stop.load(Ordering::Relaxed) {
        // The final pass runs with the main worker parked and must not wait for a busy hour.
        if first && !wait_for_quiet(cfg, &mut last_batch) {
            return Err(Halt::Stopped);
        }
        let items = cfg
            .readers
            .with(|c| embeddings::missing(c, owner, model, cursor, cfg.batch))
            .map_err(|e| fail("reading the rows to embed", e))?;
        let Some(last) = items.last().map(|i| seq_of(i.key)) else {
            return Ok(());
        };
        let n = items.len() as u64;
        let Gated {
            vectors,
            refused,
            denied,
        } = gated_embed(cfg, items)?;
        let batch: Vec<(VecKey, String, Vec<f32>)> = vectors
            .into_iter()
            .map(|(i, v)| (i.key, i.stamp, v))
            .collect();
        let refused: Vec<(VecKey, String)> =
            refused.into_iter().map(|i| (i.key, i.stamp)).collect();
        if !batch.is_empty() {
            let stored = cfg
                .writer
                .submit_blocking(WriteOp::StoreModelVectors {
                    model: model.into(),
                    items: batch,
                })
                .map_err(|e| fail("storing vectors", e))?;
            if let WriteOutcome::Embedded(applied) = stored {
                index
                    .upsert_batch(&applied)
                    .map_err(|e| fail("updating the new index", e))?;
            }
        }
        if !first {
            // Final pass: say in the rows why they have no vector for the new model.
            let held: Vec<(VecKey, String)> =
                denied.into_iter().map(|i| (i.key, i.stamp)).collect();
            if !held.is_empty() {
                cfg.writer
                    .submit_blocking(WriteOp::EmbedSkipped { keys: held })
                    .map_err(|e| fail("recording rows held back", e))?;
            }
            if !refused.is_empty() {
                // The active model's worker tries them again, with its usual limit.
                tracing::warn!(
                    rows = refused.len(),
                    "the new model refused some texts; they are queued for the embedding worker"
                );
                cfg.writer
                    .submit_blocking(WriteOp::EmbedFailed {
                        keys: refused,
                        retry: true,
                    })
                    .map_err(|e| fail("recording refused rows", e))?;
            }
        } else {
            cfg.progress.done.fetch_add(n, Ordering::Relaxed);
        }
        cursor = last;
        if first {
            set_meta(&cfg.writer, &key, &cursor.to_string())
                .map_err(|e| fail("recording progress", e))?;
        }
    }
    Err(Halt::Stopped)
}

/// Removes every vector of `model` and the saved cursors: a fresh start for a model whose
/// leftovers may be stale.
fn clear_target(cfg: &ReembedConfig, model: &str) -> alice_core::Result<()> {
    loop {
        match cfg.writer.submit_blocking(WriteOp::DeleteModelVectors {
            model: model.into(),
            limit: DELETE_SLICE,
        })? {
            WriteOutcome::Count(0) => break,
            WriteOutcome::Count(_) => {}
            _ => break,
        }
        if cfg.stop.load(Ordering::Relaxed) {
            break;
        }
    }
    for owner in [Owner::Memory, Owner::Chunk] {
        set_meta(&cfg.writer, &cursor_key(model, owner), "0")?;
    }
    // Index files of an earlier run of this model would be stale too.
    for ext in ["memories.usearch", "chunks.bq"] {
        let _ = std::fs::remove_file(cfg.vector_path.with_extension(ext));
    }
    Ok(())
}

/// Takes `model` out of `meta.purge_model` (it is wanted again).
fn unqueue_purge(cfg: &ReembedConfig, model: &str) {
    let list = purge_list(&cfg.readers);
    if list.iter().any(|m| m == model) {
        let rest: Vec<String> = list.into_iter().filter(|m| m != model).collect();
        let _ = set_meta(&cfg.writer, PURGE_KEY, &rest.join("\n"));
    }
}

fn run(cfg: ReembedConfig) {
    let model = cfg.target.model_id().to_string();
    let (active_now, _) = snapshot(&cfg.active, &cfg.index);
    // The model in force is the target already (it only has rows without vectors): its own
    // vectors and index are filled in place.
    let in_place = active_now.model == model;
    if let Err(e) = cfg.writer.submit_blocking(WriteOp::SetReembedTarget(
        (!in_place).then(|| model.clone()),
    )) {
        return give_up(&cfg, "recording the target model", e);
    }
    if let Err(e) = set_meta(&cfg.writer, TARGET_KEY, &cfg.target_settings) {
        return give_up(&cfg, "recording the target settings", e);
    }
    if !cfg.resume && !in_place {
        if let Err(e) = clear_target(&cfg, &model) {
            return give_up(&cfg, "clearing the target's old vectors", e);
        }
        unqueue_purge(&cfg, &model);
    }
    let total = cfg
        .readers
        .with(|c| {
            let (m, ch) = embeddings::embeddable_count(c)?;
            Ok((m + ch) as u64)
        })
        .unwrap_or(0);
    cfg.progress.total.store(total, Ordering::Relaxed);
    let have = cfg
        .readers
        .with(|c| embeddings::count(c, &model))
        .unwrap_or(0) as u64;
    cfg.progress.done.store(have, Ordering::Relaxed);

    let index: Arc<VectorIndex> = if in_place {
        match snapshot(&cfg.active, &cfg.index).1 {
            Some(i) => i,
            None => {
                *cfg.error.write() =
                    Some("the vector index is being rebuilt; re-embedding starts later".into());
                return;
            }
        }
    } else {
        let dim = cfg.target.dim();
        let opened = VectorIndex::open(&cfg.vector_path, &model, dim)
            .and_then(|i| cfg.readers.with(|c| i.catch_up(c)).map(|_| i))
            .or_else(|_| {
                cfg.readers
                    .with(|c| VectorIndex::rebuild_from(c, &model, dim))
            });
        match opened {
            Ok(i) => Arc::new(i),
            Err(e) => return give_up(&cfg, "preparing the new index", e),
        }
    };
    // On a stop or failure, keep what was done: the index file must match the stored
    // vectors, or a resumed run could skip rows (they have vectors) that the index lacks.
    let stop_here = |cfg: &ReembedConfig, halt: Halt| {
        if !in_place && let Err(e) = index.save(&cfg.vector_path) {
            return give_up(cfg, "saving the new index", e);
        }
        match halt {
            Halt::Stopped => {
                cfg.progress.finished.store(true, Ordering::SeqCst);
                cfg.main_paused.store(false, Ordering::SeqCst);
            }
            Halt::Failed(msg) => give_up(cfg, "embedding", msg),
        }
    };
    // Phase 1: everything, resumable, while the old model serves.
    for owner in [Owner::Memory, Owner::Chunk] {
        if let Err(h) = pass(&cfg, &index, &model, owner, true) {
            return stop_here(&cfg, h);
        }
    }
    // Phase 2: park the main worker, catch up with rows written or edited meanwhile, switch.
    cfg.parked.store(false, Ordering::SeqCst);
    cfg.main_paused.store(true, Ordering::SeqCst);
    let parked_by = Instant::now() + PARK_WAIT;
    while !cfg.parked.load(Ordering::SeqCst) {
        if cfg.stop.load(Ordering::Relaxed) {
            return stop_here(&cfg, Halt::Stopped);
        }
        if Instant::now() > parked_by {
            return give_up(
                &cfg,
                "pausing the embedding worker",
                "it did not stop in time",
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for owner in [Owner::Memory, Owner::Chunk] {
        if let Err(h) = pass(&cfg, &index, &model, owner, false) {
            return stop_here(&cfg, h);
        }
    }
    // Rows purged meanwhile (trash, retention) left the live index but not this one.
    if !in_place && let Err(e) = cfg.readers.with(|c| index.catch_up(c)) {
        return give_up(&cfg, "bringing the new index up to date", e);
    }
    // Persist first, then switch in memory: if any step fails, the old model stays active and
    // health says why (a restart then resumes from the saved state).
    if !in_place && let Err(e) = index.save(&cfg.vector_path) {
        return give_up(&cfg, "saving the new index", e);
    }
    if let Err(e) = set_meta(&cfg.writer, ACTIVE_KEY, &cfg.target_settings)
        .and_then(|_| set_meta(&cfg.writer, TARGET_KEY, ""))
        .and_then(|_| {
            cfg.writer
                .submit_blocking(WriteOp::SetReembedTarget(None))
                .map(|_| ())
        })
    {
        return give_up(&cfg, "recording the switch", e);
    }
    let old = active_now.model.clone();
    cfg.progress.finished.store(true, Ordering::SeqCst);
    {
        // Both slots at once, in the order readers take them (`snapshot`), so no reader sees
        // the new embedder with the old index.
        let mut a = cfg.active.write();
        let mut i = cfg.index.write();
        *a = Arc::new(Active {
            embedder: cfg.target.clone(),
            model: model.clone(),
            vector_path: cfg.vector_path.clone(),
        });
        if !in_place {
            *i = Some(index);
        }
    }
    cfg.main_paused.store(false, Ordering::SeqCst);
    cfg.recheck_skipped.store(true, Ordering::SeqCst);
    *cfg.error.write() = None;
    if old != model {
        let mut list = purge_list(&cfg.readers);
        if !list.contains(&old) {
            list.push(old.clone());
        }
        if let Err(e) = set_meta(&cfg.writer, PURGE_KEY, &list.join("\n")) {
            // The switch stands; only the clean-up of the old vectors waits for the next switch.
            *cfg.error.write() = Some(format!(
                "switched to {model}, but the old vectors of {old} were not queued for purging ({e})"
            ));
        }
    }
    tracing::info!(from = %old, to = %model, "switched embedding model");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::testkit::*;
    use crate::maintenance::Job;
    use crate::paths::AliceHome;
    use crate::service::{Service, ServiceOptions};
    use alice_core::api::{IngestRequest, MatchVia, OutputFormat, RecallRequest, RememberRequest};
    use alice_core::embed::FakeEmbedder;
    use alice_core::store::maintenance::get_meta;
    use alice_core::store::sources::{ChunkInput, SourceInput};
    use alice_embed::MODEL_ID;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicUsize;

    async fn new_vectors(s: &Service, model: &str) -> i64 {
        let model = model.to_string();
        s.read(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM embeddings WHERE model = ?1",
                [&model],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
    }

    fn set_embedding(provider: &str, model: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("embedding.provider".into(), serde_json::json!(provider));
        m.insert("embedding.model".into(), serde_json::json!(model));
        m
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_change_re_embeds_in_the_background_then_switches_atomically() {
        let d = tempfile::tempdir().unwrap();
        let s = start(d.path(), options(fake_factory()), "");
        let cwd = repo(&d, "proj");
        for i in 0..40 {
            s.remember(RememberRequest {
                content: format!("memory number {i} about deployment"),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            chunks: (0..60)
                .map(|i| ChunkInput {
                    ord: i,
                    text: format!("chunk {i} of a session about lease timeouts"),
                    ..Default::default()
                })
                .collect(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        let out = s
            .patch_settings(set_embedding("ollama", "nomic-embed-text"))
            .unwrap();
        assert!(out.restart_required.is_empty(), "applies live");
        // Until the switch, the old model answers queries.
        let mut saw_progress = false;
        for _ in 0..500 {
            let h = s.health().await.unwrap();
            if h.embedding_model == "ollama:nomic-embed-text" && h.reembed.is_none() {
                break;
            }
            if let Some(r) = &h.reembed {
                saw_progress |= r.total > 0;
                assert_eq!(h.embedding_model, MODEL_ID);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let h = s.health().await.unwrap();
        assert_eq!(h.embedding_model, "ollama:nomic-embed-text");
        assert!(h.reembed.is_none() && h.embedding_error.is_none(), "{h:?}");
        let _ = saw_progress;
        // Every memory and chunk has a vector for the new model; search uses it.
        assert_eq!(new_vectors(&s, "ollama:nomic-embed-text").await, 100);
        assert_eq!(s.model(), "ollama:nomic-embed-text");
        let found = s
            .recall(RecallRequest {
                query: "memory number 7 about deployment".into(),
                cwd: cwd.clone(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(found.results[0].matched.contains(&MatchVia::Vector));
        assert!(found.notes.is_empty(), "{:?}", found.notes);
        // The old vectors go with the daily purge.
        assert!(new_vectors(&s, MODEL_ID).await > 0);
        s.run_job(Job::ModelPurge).await.unwrap();
        assert_eq!(new_vectors(&s, MODEL_ID).await, 0);
        assert!(
            !s.home()
                .vector_file(MODEL_ID)
                .with_extension("chunks.bq")
                .exists()
        );
        // New text is embedded with the new model, and the settings are recorded.
        s.remember(RememberRequest {
            content: "written after the switch".into(),
            cwd,
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        assert_eq!(new_vectors(&s, "ollama:nomic-embed-text").await, 101);
        let recorded = s.read(|c| get_meta(c, ACTIVE_KEY)).await.unwrap().unwrap();
        assert!(recorded.contains("nomic-embed-text"), "{recorded}");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_changed_while_stopped_is_picked_up_at_start_and_resumes() {
        let d = tempfile::tempdir().unwrap();
        let begin = |f: alice_embed::EmbedderFactory| start(d.path(), options(f), "");
        let s = begin(counting_factory(
            Arc::new(AtomicUsize::new(0)),
            Duration::ZERO,
            false,
        ));
        for i in 0..200 {
            s.remember(RememberRequest {
                content: format!("note {i}"),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        settled(&s).await;
        s.shutdown();
        drop(s);
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(
            home.config_file(),
            "[embedding]\nprovider = \"voyage\"\nmodel = \"voyage-3-lite\"\n",
        )
        .unwrap();
        // A slow target: stop the service once some progress is saved, part-way through.
        let first = Arc::new(AtomicUsize::new(0));
        let s = begin(counting_factory(
            first.clone(),
            Duration::from_millis(100),
            false,
        ));
        let key = "reembed.voyage:voyage-3-lite.cursor.memory";
        until("saved progress", 10, || async {
            let cursor: Option<String> = s.read(move |c| get_meta(c, key)).await.unwrap();
            cursor
                .and_then(|v| v.parse::<i64>().ok())
                .is_some_and(|c| c > 0)
        })
        .await;
        s.shutdown(); // sets the re-embed's stop flag and joins its thread
        drop(s);
        let before = first.load(Ordering::SeqCst);
        assert!(
            before > 0 && before < 200,
            "stopped part-way: {before} of 200"
        );
        // Restart: it resumes and only embeds what is left, then switches.
        let second = Arc::new(AtomicUsize::new(0));
        let s = begin(counting_factory(second.clone(), Duration::ZERO, false));
        until("the resumed re-embed to switch", 10, || async {
            s.health().await.unwrap().embedding_model == "voyage:voyage-3-lite"
        })
        .await;
        assert_eq!(
            new_vectors(&s, "voyage:voyage-3-lite").await,
            200,
            "every memory has a vector for the new model"
        );
        let after = second.load(Ordering::SeqCst);
        assert!(
            after < 200 && before + after >= 200,
            "resumed, not restarted: {before} before the stop, {after} after"
        );
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_re_embed_that_cannot_record_its_progress_stops_and_says_why() {
        let d = tempfile::tempdir().unwrap();
        let s = start(d.path(), options(fake_factory()), "");
        s.remember(RememberRequest {
            content: "one memory".into(),
            cwd: repo(&d, "proj"),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        s.force_read_only("the disk is full");
        // config.toml is a file, not the database: this still works.
        s.patch_settings(set_embedding("ollama", "nomic-embed-text"))
            .unwrap();
        let found: Arc<Mutex<Option<(String, String)>>> = Arc::default();
        until("the failure to reach health", 5, || async {
            let h = s.health().await.unwrap();
            let seen = h.embedding_error.clone().map(|e| (e, h.embedding_model));
            *found.lock() = seen.clone();
            seen.is_some()
        })
        .await;
        let (error, model) = found.lock().clone().unwrap();
        assert!(error.contains("re-embedding stopped"), "{error}");
        assert_eq!(model, MODEL_ID, "the old model stays active");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_model_change_replaces_the_first_and_two_never_run_at_once() {
        let d = tempfile::tempdir().unwrap();
        let busy: Arc<(AtomicUsize, AtomicUsize)> = Arc::default();
        let texts = Arc::new(AtomicUsize::new(0));
        let s = start(
            d.path(),
            options(counting_factory_shared(
                texts.clone(),
                Duration::from_millis(60),
                false,
                busy.clone(),
            )),
            "",
        );
        for i in 0..150 {
            s.remember(RememberRequest {
                content: format!("note {i}"),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        settled(&s).await;
        s.patch_settings(set_embedding("ollama", "first")).unwrap();
        until("the first re-embed to start", 10, || async {
            s.health().await.unwrap().reembed.is_some()
        })
        .await;
        s.patch_settings(set_embedding("ollama", "second")).unwrap();
        until("the switch to the second model", 20, || async {
            s.health().await.unwrap().embedding_model == "ollama:second"
        })
        .await;
        assert_eq!(
            busy.1.load(Ordering::SeqCst),
            1,
            "never two embedding at once"
        );
        assert_eq!(new_vectors(&s, "ollama:second").await, 150);
        assert_eq!(s.model(), "ollama:second");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_back_to_the_model_in_force_cancels_the_re_embed() {
        let d = tempfile::tempdir().unwrap();
        let s = start(
            d.path(),
            options(counting_factory(
                Arc::new(AtomicUsize::new(0)),
                Duration::from_millis(100),
                false,
            )),
            "",
        );
        for i in 0..150 {
            s.remember(RememberRequest {
                content: format!("note {i}"),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        settled(&s).await;
        s.patch_settings(set_embedding("ollama", "other")).unwrap();
        until("the re-embed to start", 10, || async {
            s.health().await.unwrap().reembed.is_some()
        })
        .await;
        s.patch_settings(set_embedding("local", MODEL_ID)).unwrap();
        until("the re-embed to be cancelled", 10, || async {
            s.health().await.unwrap().reembed.is_none()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let h = s.health().await.unwrap();
        assert_eq!(h.embedding_model, MODEL_ID);
        assert!(h.reembed.is_none() && h.embedding_error.is_none(), "{h:?}");
        let target: Option<String> = s.read(|c| get_meta(c, TARGET_KEY)).await.unwrap();
        assert!(
            target.is_none_or(|t| t.is_empty()),
            "the target is forgotten"
        );
        // The main worker runs again (a new memory is embedded).
        s.remember(RememberRequest {
            content: "after the cancel".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_edit_made_during_the_re_embed_is_embedded_again_with_the_new_text() {
        let d = tempfile::tempdir().unwrap();
        let s = start(
            d.path(),
            options(counting_factory(
                Arc::new(AtomicUsize::new(0)),
                Duration::from_millis(30),
                false,
            )),
            "",
        );
        let mut ids = Vec::new();
        for i in 0..100 {
            let r = s
                .remember(RememberRequest {
                    content: format!("note {i} original"),
                    ..Default::default()
                })
                .await
                .unwrap();
            ids.push(r.id.unwrap());
        }
        settled(&s).await;
        s.patch_settings(set_embedding("ollama", "m")).unwrap();
        // Edit the first memories while the re-embed is going (and again just before the end).
        until("the re-embed to start", 10, || async {
            s.health().await.unwrap().reembed.is_some()
        })
        .await;
        for (i, id) in ids.iter().enumerate().take(5) {
            s.update(alice_core::api::UpdateRequest {
                id: id.clone(),
                update: alice_core::store::memories::MemoryUpdate {
                    body: Some(format!("an edited body {i} about lighthouses")),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
        }
        until("the switch", 30, || async {
            s.health().await.unwrap().embedding_model == "ollama:m"
        })
        .await;
        settled(&s).await;
        // The stored vector of an edited memory is of its new text.
        let seq: i64 = {
            let id = ids[0].clone();
            s.read(move |c| {
                Ok(
                    c.query_row("SELECT seq FROM memories WHERE id = ?1", [&id], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap()
        };
        let stored = s
            .read(move |c| {
                alice_core::store::embeddings::get_many(
                    c,
                    "ollama:m",
                    &[alice_core::store::embeddings::VecKey::Memory(seq)],
                )
            })
            .await
            .unwrap();
        let (title, body): (String, String) = s
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT title, body FROM memories WHERE seq = ?1",
                    [seq],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert!(body.contains("lighthouses"), "the edit was applied: {body}");
        let want = FakeEmbedder::named("x", 64)
            .embed_passages(&[alice_core::embed::memory_embed_text(&title, &body)])
            .unwrap()
            .remove(0);
        let sim = alice_core::embed::cosine(&stored[0].1, &want);
        assert!(sim > 0.99, "the vector is of the edited text: {sim}");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_model_in_force_and_the_models_asked_for_are_never_purged() {
        let d = tempfile::tempdir().unwrap();
        let s = start(d.path(), options(fake_factory()), "");
        s.remember(RememberRequest {
            content: "a memory".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        // A stale list naming the model in force and one that is long gone (with a vector).
        let ghost = "ollama:ghost";
        let (seq, stamp): (i64, String) = s
            .read(|c| {
                Ok(
                    c.query_row("SELECT seq, content_hash FROM memories LIMIT 1", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?,
                )
            })
            .await
            .unwrap();
        let w = s.writer_for_tests();
        w.submit(WriteOp::StoreModelVectors {
            model: ghost.into(),
            items: vec![(VecKey::Memory(seq), stamp, vec![0.5; 8])],
        })
        .await
        .unwrap();
        w.submit(WriteOp::SetMeta {
            key: PURGE_KEY.into(),
            value: format!("{MODEL_ID}\n{ghost}"),
        })
        .await
        .unwrap();
        assert_eq!(new_vectors(&s, ghost).await, 1);
        let before = new_vectors(&s, MODEL_ID).await;
        assert!(before > 0);
        s.run_job(Job::ModelPurge).await.unwrap();
        assert_eq!(
            new_vectors(&s, MODEL_ID).await,
            before,
            "the model in force stays"
        );
        assert_eq!(new_vectors(&s, ghost).await, 0, "the other one goes");
        let left: Option<String> = s.read(|c| get_meta(c, PURGE_KEY)).await.unwrap();
        assert_eq!(
            left.as_deref(),
            Some(MODEL_ID),
            "only the model in force is still listed"
        );
        s.shutdown();
    }

    #[test]
    fn a_status_never_shows_more_done_than_total() {
        let p = Progress::new("m");
        p.total.store(10, Ordering::Relaxed);
        p.done.store(25, Ordering::Relaxed);
        assert_eq!((p.status().done, p.status().total), (10, 10));
        let _ = ServiceOptions::default();
    }
}
