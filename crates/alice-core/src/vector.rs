//! HNSW vector index (usearch, cosine, int8). The `embeddings` table is the source of truth;
//! this index is a disposable accelerator that can always be rebuilt.
//!
//! usearch facts this wrapper handles (verified in the spike and the usearch 2.26.4 source):
//! - capacity never grows by itself: `reserve` before adding, also after `load`;
//! - concurrent searches are limited by "thread slots": a gate keeps callers <= THREADS;
//! - updating a key means remove + add; duplicate keys are rejected;
//! - `u64::MAX` marks a removed slot, so no stored key may encode to it;
//! - an add counts its node before it publishes the node's vector and the entry point, and a
//!   search reads the entry point without a lock: a search beside the first add dereferenced a
//!   null vector (SIGSEGV). Adds and removes therefore take the write lock.
//!
//! Locking: searches, `len` and `save` take the read lock; `upsert`, `remove` and capacity
//! growth take the write lock. `upsert`, `remove` and `save` also take the writer mutex first,
//! so a write queued behind a long `save` waits on that mutex instead of on the `RwLock`, where
//! a waiting writer would hold back every new search.

use crate::error::{AliceError, Result};
use crate::store::embeddings::{self, VecKey};
use parking_lot::{Condvar, Mutex, RwLock};
use rusqlite::Connection;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const THREADS: usize = 64;
const MIN_CAPACITY: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFilter {
    Memories,
    Chunks,
    Any,
}

/// Memories use even keys, chunks odd keys.
pub fn encode_key(k: VecKey) -> u64 {
    match k {
        VecKey::Memory(s) => (s as u64) << 1,
        VecKey::Chunk(s) => ((s as u64) << 1) | 1,
    }
}

pub fn decode_key(k: u64) -> VecKey {
    let seq = (k >> 1) as i64;
    if k & 1 == 0 {
        VecKey::Memory(seq)
    } else {
        VecKey::Chunk(seq)
    }
}

/// The encoded key, if it decodes back to `key` and is not usearch's removed-slot marker.
fn checked_key(key: VecKey) -> Result<u64> {
    let (VecKey::Memory(seq) | VecKey::Chunk(seq)) = key;
    let k = encode_key(key);
    if seq < 0 || k == u64::MAX {
        return Err(AliceError::invalid(format!(
            "{key:?} cannot be stored in the vector index; row seqs must be between 0 and {}",
            i64::MAX - 1
        )));
    }
    Ok(k)
}

fn options(dim: usize) -> IndexOptions {
    IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::I8,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    }
}

fn err(e: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!("vector index: {e}"))
}

fn check_dim(what: &str, got: usize, dim: usize) -> Result<()> {
    if got != dim {
        return Err(AliceError::invalid(format!(
            "{what} has {got} dimensions; the index uses {dim}"
        )));
    }
    Ok(())
}

/// Limits concurrent searches to the reserved thread slots.
struct Gate {
    busy: Mutex<usize>,
    cv: Condvar,
    limit: usize,
}

struct GateGuard<'a>(&'a Gate);

impl Gate {
    fn enter(&self) -> GateGuard<'_> {
        let mut busy = self.busy.lock();
        while *busy >= self.limit {
            self.cv.wait(&mut busy);
        }
        *busy += 1;
        GateGuard(self)
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock() -= 1;
        self.0.cv.notify_one();
    }
}

pub struct VectorIndex {
    /// Searches, `len` and `save` read; adds, removes and capacity growth write.
    inner: RwLock<Index>,
    /// Serialises `upsert`, `remove` and `save` (see the module docs).
    writer: Mutex<()>,
    dim: usize,
    /// Thread slots reserved in usearch; also the gate's limit.
    threads: usize,
    dirty: AtomicBool,
    gate: Gate,
}

impl VectorIndex {
    fn wrap(index: Index, dim: usize, threads: usize) -> Self {
        Self {
            inner: RwLock::new(index),
            writer: Mutex::new(()),
            dim,
            threads,
            dirty: AtomicBool::new(false),
            gate: Gate {
                busy: Mutex::new(0),
                cv: Condvar::new(),
                limit: threads,
            },
        }
    }

    pub fn new(dim: usize) -> Result<Self> {
        Self::with_threads(dim, THREADS)
    }

    /// `threads` below THREADS lets tests overrun the slots with a handful of threads.
    fn with_threads(dim: usize, threads: usize) -> Result<Self> {
        let index = Index::new(&options(dim)).map_err(err)?;
        index
            .reserve_capacity_and_threads(MIN_CAPACITY, threads)
            .map_err(err)?;
        Ok(Self::wrap(index, dim, threads))
    }

    /// Load a saved index. Fails if the file is missing, unreadable or was built with
    /// another dimension, metric or quantization.
    pub fn open(path: &Path, dim: usize) -> Result<Self> {
        if !path.exists() {
            return Err(AliceError::NotFound(format!(
                "vector index {}",
                path.display()
            )));
        }
        let index = Index::new(&options(dim)).map_err(err)?;
        index.load(&path.to_string_lossy()).map_err(err)?;
        if index.dimensions() != dim
            || index.metric_kind() != MetricKind::Cos
            || index.scalar_kind() != ScalarKind::I8
        {
            return Err(err(format!(
                "{} holds a {}-dimension {:?}/{:?} index; {dim}-dimension Cos/I8 expected",
                path.display(),
                index.dimensions(),
                index.metric_kind(),
                index.scalar_kind()
            )));
        }
        // load() leaves the library's default thread slots and no spare capacity.
        let cap = (index.size() * 2).max(MIN_CAPACITY);
        index
            .reserve_capacity_and_threads(cap, THREADS)
            .map_err(err)?;
        Ok(Self::wrap(index, dim, THREADS))
    }

    /// Make room for `extra` more vectors. `i` is write-locked by the caller.
    fn ensure_capacity(&self, i: &Index, extra: usize) -> Result<()> {
        let need = i.size() + extra;
        if need > i.capacity() {
            let cap = (i.capacity() * 2).max(need).max(MIN_CAPACITY);
            i.reserve_capacity_and_threads(cap, self.threads)
                .map_err(err)?;
        }
        Ok(())
    }

    pub fn upsert(&self, key: VecKey, v: &[f32]) -> Result<()> {
        check_dim("vector", v.len(), self.dim)?;
        let k = checked_key(key)?;
        let _writer = self.writer.lock();
        let i = self.inner.write();
        self.ensure_capacity(&i, 1)?;
        if i.remove(k).map_err(err)? > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        i.add(k, v).map_err(err)?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn remove(&self, key: VecKey) -> Result<()> {
        let k = checked_key(key)?;
        let _writer = self.writer.lock();
        if self.inner.write().remove(k).map_err(err)? > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Best first; similarity = 1 - cosine distance.
    pub fn search(&self, query: &[f32], k: usize, filter: KeyFilter) -> Result<Vec<(VecKey, f32)>> {
        check_dim("query", query.len(), self.dim)?;
        if k == 0 || self.is_empty() {
            return Ok(Vec::new());
        }
        let _slot = self.gate.enter();
        let i = self.inner.read();
        let m = match filter {
            KeyFilter::Any => i.search(query, k),
            KeyFilter::Memories => i.filtered_search(query, k, |key| key & 1 == 0),
            KeyFilter::Chunks => i.filtered_search(query, k, |key| key & 1 == 1),
        }
        .map_err(err)?;
        Ok(m.keys
            .iter()
            .zip(m.distances.iter())
            .map(|(k, d)| (decode_key(*k), 1.0 - d))
            .collect())
    }

    pub fn len(&self) -> usize {
        self.inner.read().size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when the index has changed since it was created, opened or last saved.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Atomic save: write and sync a temp file, then rename it over the old one.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("usearch.tmp");
        let _writer = self.writer.lock();
        // The read guard lives for this statement only: searches go on, writers wait above.
        let snapshot = self.inner.read().save(&tmp.to_string_lossy()).map_err(err);
        let written = snapshot.and_then(|()| {
            // Write access: Windows cannot flush a read-only handle.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&tmp)?
                .sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        });
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written?;
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// A fresh index holding every stored vector of `model`. Vectors that cannot be added
    /// (wrong dimension, out-of-range key) are skipped and counted in a warning.
    pub fn rebuild_from(conn: &Connection, model: &str, dim: usize) -> Result<Self> {
        let idx = Self::new(dim)?;
        let stored = embeddings::count(conn, model)?;
        idx.ensure_capacity(&idx.inner.write(), stored)?;
        let mut failed = 0usize;
        let mut first_error = None;
        embeddings::for_each(conn, model, |key, v| {
            if let Err(e) = idx.upsert(key, &v) {
                failed += 1;
                first_error.get_or_insert_with(|| format!("{key:?}: {e}"));
            }
        })?;
        if failed > 0 {
            tracing::warn!(
                failed,
                first_error = first_error.as_deref().unwrap_or_default(),
                "some vectors could not be added during rebuild"
            );
        }
        Ok(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{Embedder, FakeEmbedder};
    use std::sync::Arc;

    fn v(e: &FakeEmbedder, s: &str) -> Vec<f32> {
        e.embed_query(s).unwrap()
    }

    #[test]
    fn keys_round_trip() {
        for k in [
            VecKey::Memory(1),
            VecKey::Chunk(1),
            VecKey::Memory(9_000_000),
            VecKey::Chunk(0),
        ] {
            assert_eq!(decode_key(encode_key(k)), k);
        }
    }

    #[test]
    fn upsert_search_filter_and_replace() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        idx.upsert(VecKey::Memory(1), &v(&e, "sqlite writer lease"))
            .unwrap();
        idx.upsert(VecKey::Chunk(1), &v(&e, "sqlite writer lease"))
            .unwrap();
        idx.upsert(VecKey::Memory(2), &v(&e, "tom yum soup recipe"))
            .unwrap();
        let hits = idx
            .search(&v(&e, "sqlite writer lease"), 2, KeyFilter::Memories)
            .unwrap();
        assert_eq!(hits[0].0, VecKey::Memory(1));
        assert!(hits[0].1 > 0.99);
        assert!(hits.iter().all(|(k, _)| matches!(k, VecKey::Memory(_))));
        let chunks = idx.search(&v(&e, "sqlite"), 5, KeyFilter::Chunks).unwrap();
        assert_eq!(chunks.len(), 1);
        idx.upsert(VecKey::Memory(1), &v(&e, "completely different"))
            .unwrap();
        assert_eq!(idx.len(), 3, "upsert replaces");
        idx.remove(VecKey::Memory(2)).unwrap();
        assert_eq!(idx.len(), 2);
        assert!(
            idx.upsert(VecKey::Memory(3), &[0.0; 10]).is_err(),
            "wrong dimension"
        );
    }

    #[test]
    fn grows_past_initial_capacity_and_survives_save_open() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        for i in 0..3_000 {
            idx.upsert(VecKey::Memory(i), &v(&e, &format!("note {i}")))
                .unwrap();
        }
        assert_eq!(idx.len(), 3_000);
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("vectors").join("m.usearch");
        idx.save(&path).unwrap();
        assert!(!idx.is_dirty());
        let back = VectorIndex::open(&path, 384).unwrap();
        assert_eq!(back.len(), 3_000);
        back.upsert(VecKey::Memory(5_000), &v(&e, "after load"))
            .unwrap();
        assert_eq!(
            back.search(&v(&e, "after load"), 1, KeyFilter::Any)
                .unwrap()[0]
                .0,
            VecKey::Memory(5_000)
        );
        assert!(VectorIndex::open(&d.path().join("missing.usearch"), 384).is_err());
    }

    #[test]
    fn many_threads_search_and_insert_without_errors() {
        let e = Arc::new(FakeEmbedder::new(384));
        let idx = Arc::new(VectorIndex::new(384).unwrap());
        let mut handles = Vec::new();
        for t in 0..200 {
            let (e, idx) = (e.clone(), idx.clone());
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    if t % 5 == 0 {
                        idx.upsert(
                            VecKey::Chunk(t * 100 + i),
                            &e.embed_query(&format!("t{t} i{i}")).unwrap(),
                        )
                        .unwrap();
                    } else {
                        idx.search(&e.embed_query("t1 i1").unwrap(), 5, KeyFilter::Any)
                            .unwrap();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(idx.len(), 40 * 20);
    }

    #[test]
    fn rebuild_from_the_embeddings_table() {
        use crate::project::ProjectRef;
        use crate::store::{embeddings, memories, open_writer, projects};
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let e = FakeEmbedder::new(384);
        for t in ["one", "two", "three"] {
            let m = memories::prepare(
                memories::NewMemoryInput {
                    content: t.into(),
                    ..Default::default()
                },
                &p,
                "h".into(),
            )
            .unwrap();
            let memories::InsertOutcome::New { seq, .. } = memories::insert(&c, &m).unwrap() else {
                panic!()
            };
            embeddings::put(&c, VecKey::Memory(seq), "fake", &v(&e, t)).unwrap();
        }
        let idx = VectorIndex::rebuild_from(&c, "fake", 384).unwrap();
        assert_eq!(idx.len(), 3);
    }

    // Tests below go beyond the brief: they pin the behaviour consumers rely on.

    #[test]
    fn index_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VectorIndex>();
    }

    #[test]
    fn empty_index_zero_k_wrong_query_dimension_and_any_filter() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        assert!(idx.is_empty());
        assert!(
            idx.search(&v(&e, "x"), 5, KeyFilter::Any)
                .unwrap()
                .is_empty()
        );
        idx.upsert(VecKey::Memory(1), &v(&e, "sqlite writer lease"))
            .unwrap();
        idx.upsert(VecKey::Chunk(1), &v(&e, "sqlite writer lease"))
            .unwrap();
        assert!(
            idx.search(&v(&e, "sqlite"), 0, KeyFilter::Any)
                .unwrap()
                .is_empty()
        );
        assert!(
            matches!(
                idx.search(&[0.0; 10], 5, KeyFilter::Any),
                Err(AliceError::Invalid(_))
            ),
            "a query of the wrong dimension is the caller's mistake"
        );
        let mut both: Vec<VecKey> = idx
            .search(&v(&e, "sqlite writer lease"), 5, KeyFilter::Any)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        both.sort();
        assert_eq!(both, vec![VecKey::Memory(1), VecKey::Chunk(1)]);
    }

    #[test]
    fn keys_that_cannot_round_trip_are_rejected() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        // Negative seqs lose their sign; Chunk(i64::MAX) encodes to u64::MAX, usearch's "free slot" key.
        for k in [
            VecKey::Memory(-1),
            VecKey::Chunk(-5),
            VecKey::Chunk(i64::MAX),
        ] {
            assert!(
                matches!(idx.upsert(k, &v(&e, "x")), Err(AliceError::Invalid(_))),
                "{k:?}"
            );
        }
        assert_eq!(idx.len(), 0);
        idx.upsert(VecKey::Memory(i64::MAX), &v(&e, "edge"))
            .unwrap();
        assert_eq!(
            idx.search(&v(&e, "edge"), 1, KeyFilter::Any).unwrap()[0].0,
            VecKey::Memory(i64::MAX)
        );
    }

    #[test]
    fn concurrent_upserts_of_one_key_never_collide() {
        let e = Arc::new(FakeEmbedder::new(384));
        let idx = Arc::new(VectorIndex::new(384).unwrap());
        let handles: Vec<_> = (0..16)
            .map(|t| {
                let (e, idx) = (e.clone(), idx.clone());
                std::thread::spawn(move || {
                    for i in 0..50 {
                        idx.upsert(
                            VecKey::Memory(7),
                            &e.embed_query(&format!("t{t} i{i}")).unwrap(),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn concurrent_inserts_grow_capacity_without_errors() {
        // 32 writers x 100 distinct keys crosses the initial capacity twice while others insert.
        let e = Arc::new(FakeEmbedder::new(384));
        let idx = Arc::new(VectorIndex::new(384).unwrap());
        let handles: Vec<_> = (0..32)
            .map(|t| {
                let (e, idx) = (e.clone(), idx.clone());
                std::thread::spawn(move || {
                    for i in 0..100 {
                        idx.upsert(
                            VecKey::Chunk(t * 1_000 + i),
                            &e.embed_query(&format!("t{t} i{i}")).unwrap(),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(idx.len(), 3_200);
    }

    #[test]
    fn searches_racing_the_first_insert_never_crash() {
        // usearch counts a new node before it publishes the node's vector and the entry point;
        // a search inside that window dereferenced a null vector and killed the process.
        use std::sync::atomic::AtomicUsize;
        let e = FakeEmbedder::new(384);
        let q = v(&e, "first");
        for _ in 0..300 {
            let idx = Arc::new(VectorIndex::new(384).unwrap());
            let (stop, ready) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicUsize::new(0)),
            );
            let searchers: Vec<_> = (0..4)
                .map(|_| {
                    let (idx, stop, ready, q) =
                        (idx.clone(), stop.clone(), ready.clone(), q.clone());
                    std::thread::spawn(move || {
                        ready.fetch_add(1, Ordering::SeqCst);
                        while !stop.load(Ordering::Relaxed) {
                            idx.search(&q, 1, KeyFilter::Any).unwrap();
                        }
                    })
                })
                .collect();
            while ready.load(Ordering::SeqCst) < 4 {
                std::hint::spin_loop();
            }
            idx.upsert(VecKey::Memory(1), &q).unwrap();
            stop.store(true, Ordering::Relaxed);
            for s in searchers {
                s.join().unwrap();
            }
        }
    }

    #[test]
    fn the_gate_keeps_callers_within_the_thread_slots() {
        // Two slots and 32 threads: past the slots usearch fails "Reserve capacity ahead of searches!".
        let e = Arc::new(FakeEmbedder::new(384));
        let idx = Arc::new(VectorIndex::with_threads(384, 2).unwrap());
        for i in 0..100 {
            idx.upsert(
                VecKey::Memory(i),
                &e.embed_query(&format!("note {i}")).unwrap(),
            )
            .unwrap();
        }
        let start = Arc::new(std::sync::Barrier::new(32));
        let handles: Vec<_> = (0..32)
            .map(|t| {
                let (e, idx, start) = (e.clone(), idx.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for i in 0..50 {
                        if t % 8 == 0 {
                            idx.upsert(
                                VecKey::Chunk(t * 100 + i),
                                &e.embed_query(&format!("t{t} i{i}")).unwrap(),
                            )
                            .unwrap();
                        } else {
                            idx.search(
                                &e.embed_query(&format!("note {i}")).unwrap(),
                                5,
                                KeyFilter::Any,
                            )
                            .unwrap();
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(idx.len(), 100 + 4 * 50);
    }

    #[test]
    fn an_opened_index_serves_many_concurrent_searches() {
        // load() keeps the library's default thread slots; open must reserve THREADS again.
        let e = Arc::new(FakeEmbedder::new(384));
        let idx = VectorIndex::new(384).unwrap();
        for i in 0..200 {
            idx.upsert(
                VecKey::Memory(i),
                &e.embed_query(&format!("note {i}")).unwrap(),
            )
            .unwrap();
        }
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("m.usearch");
        idx.save(&path).unwrap();
        let back = Arc::new(VectorIndex::open(&path, 384).unwrap());
        let start = Arc::new(std::sync::Barrier::new(100));
        let handles: Vec<_> = (0..100)
            .map(|t| {
                let (e, back, start) = (e.clone(), back.clone(), start.clone());
                std::thread::spawn(move || {
                    let q = e.embed_query(&format!("note {t}")).unwrap();
                    start.wait();
                    for _ in 0..20 {
                        back.search(&q, 5, KeyFilter::Any).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn an_empty_index_saves_and_opens() {
        let e = FakeEmbedder::new(384);
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("m.usearch");
        VectorIndex::new(384).unwrap().save(&path).unwrap();
        let back = VectorIndex::open(&path, 384).unwrap();
        assert!(back.is_empty());
        assert!(
            back.search(&v(&e, "x"), 3, KeyFilter::Any)
                .unwrap()
                .is_empty()
        );
        back.upsert(VecKey::Chunk(4), &v(&e, "x")).unwrap();
        assert_eq!(
            back.search(&v(&e, "x"), 3, KeyFilter::Chunks).unwrap()[0].0,
            VecKey::Chunk(4)
        );
    }

    #[test]
    fn dirty_flag_and_removals_survive_save_open() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        assert!(!idx.is_dirty());
        for i in 0..10 {
            idx.upsert(VecKey::Memory(i), &v(&e, &format!("note {i}")))
                .unwrap();
        }
        assert!(idx.is_dirty());
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("m.usearch");
        idx.save(&path).unwrap();
        assert!(!idx.is_dirty());
        idx.remove(VecKey::Memory(99)).unwrap();
        assert!(!idx.is_dirty(), "removing an absent key changes nothing");
        idx.remove(VecKey::Memory(3)).unwrap();
        assert!(idx.is_dirty());
        idx.save(&path).unwrap();
        assert!(
            !path.with_extension("usearch.tmp").exists(),
            "no temp file left behind"
        );
        let back = VectorIndex::open(&path, 384).unwrap();
        assert!(!back.is_dirty());
        assert_eq!(back.len(), 9);
        let hits = back.search(&v(&e, "note 3"), 9, KeyFilter::Any).unwrap();
        assert_eq!(hits.len(), 9);
        assert!(hits.iter().all(|(k, _)| *k != VecKey::Memory(3)));
        back.upsert(VecKey::Memory(3), &v(&e, "note 3")).unwrap();
        assert_eq!(back.len(), 10);
        assert_eq!(
            back.search(&v(&e, "note 3"), 1, KeyFilter::Any).unwrap()[0].0,
            VecKey::Memory(3)
        );
        assert!(VectorIndex::open(&path, 128).is_err(), "dimension mismatch");
    }
}
