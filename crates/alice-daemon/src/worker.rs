//! Embedding worker. Reads pending items, embeds them off the writer thread, hands the vectors
//! to the writer (which stores them only if the text did not change meanwhile), then adds the
//! stored ones to the in-memory HNSW index.

use crate::service::IndexSlot;
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};
use alice_core::embed::Embedder;
use alice_core::error::AliceError;
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, PendingItem, VecKey};
use alice_core::vector::VectorIndex;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::path::PathBuf;
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

pub struct WorkerConfig {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    pub embedder: Arc<dyn Embedder>,
    pub wake: Receiver<()>,
    pub stop: Arc<AtomicBool>,
    pub vector_path: PathBuf,
    pub batch: usize,
    /// Told after each batch that reached the writer: memories whose embedding step is over wait
    /// for the judge (`bounded(1)`, so wake-ups coalesce).
    pub judge_wake: Option<Sender<()>>,
    /// How often a changed index is saved while the worker runs (`SAVE_EVERY`).
    pub save_every: Duration,
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
fn save_if_due(cfg: &WorkerConfig, index: &VectorIndex, last_save: &mut Instant) {
    if index.is_dirty() && last_save.elapsed() >= cfg.save_every {
        if let Err(e) = index.save(&cfg.vector_path) {
            tracing::warn!(error = %e, "saving the vector index failed");
        }
        *last_save = Instant::now();
    }
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

/// `embed_passages`, with a panic inside the embedder (tokenizer or runtime on an odd text)
/// turned into an error instead of killing the worker thread.
fn embed_guarded(
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
/// one text the model cannot handle does not fail the others.
fn embed_each(embedder: &dyn Embedder, items: &[PendingItem]) -> Vec<Result<Vec<f32>, String>> {
    let texts: Vec<String> = items.iter().map(|i| i.text.clone()).collect();
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
        Err(e) if items.len() == 1 => vec![Err(e.to_string())],
        Err(_) => texts
            .iter()
            .map(|t| match embed_guarded(embedder, std::slice::from_ref(t)) {
                Ok(mut v) if v.len() == 1 => Ok(v.remove(0)),
                Ok(v) => Err(format!(
                    "the embedder returned {} vectors for 1 text",
                    v.len()
                )),
                Err(e) => Err(e.to_string()),
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

fn run(cfg: WorkerConfig) {
    let model = cfg.embedder.model_id().to_string();
    let batch = cfg.batch.max(1);
    let mut attempts = Attempts::default();
    let mut backoff = FIRST_BACKOFF;
    let mut last_save = Instant::now();
    while !cfg.stop.load(Ordering::Relaxed) {
        if cfg.writer.read_only.reason().is_some() {
            // Nothing could be stored; embedding now would only burn CPU.
            nap(&cfg.stop, IDLE_WAIT);
            continue;
        }
        let Some(index) = cfg.index.read().clone() else {
            // The index is being rebuilt from stored vectors; new ones would be lost meanwhile.
            nap(&cfg.stop, REBUILD_WAIT);
            continue;
        };
        let items = match cfg.readers.with(|c| embeddings::pending(c, batch)) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "reading the embedding queue failed");
                nap(&cfg.stop, IDLE_WAIT);
                continue;
            }
        };
        if items.is_empty() {
            save_if_due(&cfg, &index, &mut last_save);
            let _ = cfg.wake.recv_timeout(IDLE_WAIT);
            continue;
        }

        let results = embed_each(cfg.embedder.as_ref(), &items);
        let mut done = Vec::new();
        let (mut retry, mut give_up) = (Vec::new(), Vec::new());
        let mut first_error = None;
        for (item, r) in items.into_iter().zip(results) {
            match r {
                Ok(v) => done.push((item.key, item.stamp, v)),
                Err(e) => {
                    first_error.get_or_insert(e);
                    if attempts.fail(&item) {
                        give_up.push((item.key, item.stamp));
                    } else {
                        retry.push((item.key, item.stamp));
                    }
                }
            }
        }

        let mut healthy = first_error.is_none();
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
        save_if_due(&cfg, &index, &mut last_save);
        if healthy {
            backoff = FIRST_BACKOFF;
        } else {
            nap(&cfg.stop, backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
    if let Some(index) = cfg.index.read().clone()
        && index.is_dirty()
        && let Err(e) = index.save(&cfg.vector_path)
    {
        tracing::warn!(error = %e, "saving the vector index at shutdown failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::{ProjectChoice, WriteOp, WriteOutcome, spawn_writer};
    use alice_core::embed::FakeEmbedder;
    use alice_core::error::{AliceError, Result};
    use alice_core::store::embeddings::VecKey;
    use alice_core::store::memories::{InsertOutcome, MemoryUpdate, NewMemoryInput};
    use alice_core::store::sources::{ChunkInput, SourceInput};
    use alice_core::vector::{KeyFilter, VectorIndex};
    use parking_lot::{Mutex, RwLock};
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
        let worker = spawn_worker(WorkerConfig {
            readers: Arc::new(ReaderPool::new(db, 2)),
            writer: writer.clone(),
            index: index.clone(),
            embedder: embedder.clone(),
            wake: wake_rx,
            stop: stop.clone(),
            vector_path: dir.path().join("vectors").join("fake.usearch"),
            batch,
            judge_wake: Some(judge_wake_tx),
            save_every,
        });
        Rig {
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
        Some(VectorIndex::new(16).unwrap())
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
}
