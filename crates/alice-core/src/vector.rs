//! Vector index over the `embeddings` table. The table is the source of truth; this index is a
//! disposable accelerator that can always be rebuilt.
//!
//! Two parts, each with its own locks and dirty flag:
//! - **memories**: a usearch HNSW (cosine, int8), searched on every recall;
//! - **chunks**: a flat sign-bit index (`crate::bq`, 48 bytes per 384-dimension vector instead
//!   of ~510 in an int8 HNSW). `search_chunks` ranks in three steps: the
//!   `chunk_shortlist(N)` nearest codes by Hamming distance (the shortlist grows with √N),
//!   those re-scored in RAM against the float query, and the best `max(k, MIN_RESCORE)` of
//!   them rescored with the exact f16 vectors of this index's model from the `embeddings`
//!   table. An index without a model (`VectorIndex::new`) returns no chunk results there.
//!   `search(.., KeyFilter::Chunks)` returns the in-RAM estimates, for ordering only.
//!
//! A memory search never walks the chunk part and never waits for chunk ingest. No search
//! covers both parts: their similarities are on different scales.
//!
//! usearch facts the memory part handles (verified in the spike and the usearch 2.26.4 source):
//! - capacity never grows by itself: `reserve` before adding, also after `load`;
//! - concurrent searches are limited by "thread slots": a gate keeps callers <= THREADS;
//! - updating a key means remove + add; duplicate keys are rejected;
//! - `u64::MAX` marks a removed slot, so no stored key may encode to it;
//! - an add counts its node before it publishes the node's vector and the entry point, a later
//!   add that raises the top level rewrites the entry point, and reusing a removed slot copies
//!   the new vector in place; searches read all of these without a lock (a search beside the
//!   first add dereferenced a null vector: SIGSEGV). Adds and removes therefore take the
//!   part's write lock;
//! - it opens files through C paths, which fail for names that are not ASCII on Windows and
//!   not UTF-8 on Unix: the part is serialized in memory and its file read and written by Rust
//!   (`load_from_buffer` / `save_to_buffer`);
//! - it is built without NumKong (`default-features = false`), whose SIMD kernels do not link
//!   under MSVC; its own portable distance code is used on every OS.
//!
//! Locking, per part: searches, `len` and `save` take the read lock; writes (and capacity
//! growth) take the write lock. The lock is task-fair, so a write first waits for every
//! in-flight search on that part, new searches on that part queue behind it, and they all wait
//! until the whole batch is in: searches stall for (longest in-flight search + batch time).
//! Only searches that include that part pay it. Writes and `save` also take the part's writer
//! mutex first, so a write that arrives during a long `save` waits on that mutex instead of on
//! the `RwLock`, where it would hold back every new search for the rest of the save.

use crate::bq::{BOOTSTRAP, BinaryIndex, mean_of};
use crate::error::{AliceError, Result};
use crate::store::embeddings::{self, VecKey};
use parking_lot::{Condvar, Mutex, RwLock};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const THREADS: usize = 64;
const MIN_CAPACITY: usize = 1_024;
/// Chunk rows read from the `embeddings` table per search, at least (k when larger). Chosen
/// by the plan's rule at scale (the smallest of 200, 400, 800, 1,600 that keeps recall@10 >=
/// 0.97 at 96,000 real passages): 200 kept 0.963 there, 400 keeps 0.980 live and 0.984 rebuilt
/// (`docs/benchmarks/m2.md`, `alice-embed/tests/binary_chunk_recall.rs`).
pub const MIN_RESCORE: usize = 400;
/// The Hamming shortlist re-ranked in RAM: `max(SHORTLIST_MIN, SHORTLIST_PER_ROOT · √N)` for N
/// chunk codes (24k: 4,960; 96k: 9,920; 1M: 32,000). At 96,000 real passages recall stopped
/// growing past ~8,000; at 1M a 32,000 shortlist costs ~9 ms in RAM.
pub const SHORTLIST_MIN: usize = 4_000;
pub const SHORTLIST_PER_ROOT: usize = 32;

/// The Hamming shortlist `search_chunks` re-ranks for an index of `n` chunk codes.
pub fn chunk_shortlist(n: usize) -> usize {
    let root = (n as f64).sqrt().ceil() as usize;
    SHORTLIST_MIN.max(SHORTLIST_PER_ROOT.saturating_mul(root))
}

/// Which part [`VectorIndex::search`] reads. There is no "both": memory similarities (int8
/// cosine) and chunk estimates are on different scales and must not be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFilter {
    Memories,
    Chunks,
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

/// The two files `save` writes for `path`: `m.usearch` -> `m.memories.usearch`, `m.chunks.bq`.
fn part_paths(path: &Path) -> (PathBuf, PathBuf) {
    (
        path.with_extension("memories.usearch"),
        path.with_extension("chunks.bq"),
    )
}

/// The M1 chunk HNSW, replaced by `chunks.bq`.
fn legacy_chunk_path(path: &Path) -> PathBuf {
    path.with_extension("chunks.usearch")
}

/// Left by a clean stop next to the files it saved (`m.usearch` -> `m.clean`).
fn clean_mark_path(path: &Path) -> PathBuf {
    path.with_extension("clean")
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

#[cfg(test)]
type Hook = std::sync::Arc<dyn Fn() + Send + Sync>;

/// One usearch index (all memories or all chunks) with its own locks, slots and dirty flag.
struct Part {
    /// Searches, `len` and `save` read; adds, removes and capacity growth write.
    inner: RwLock<Index>,
    /// Serialises writes and `save` (see the module docs).
    writer: Mutex<()>,
    /// Thread slots reserved in usearch; also the gate's limit.
    threads: usize,
    dirty: AtomicBool,
    /// The file this part was last opened from or saved to. With `dirty` unset, that file holds
    /// exactly this part, so saving to it again can be skipped.
    synced: Mutex<Option<PathBuf>>,
    gate: Gate,
    /// Runs inside `put` right after the write lock is taken, so tests can hold it.
    #[cfg(test)]
    after_write_lock: Mutex<Option<Hook>>,
}

impl Part {
    fn wrap(index: Index, threads: usize) -> Self {
        Self {
            inner: RwLock::new(index),
            writer: Mutex::new(()),
            threads,
            dirty: AtomicBool::new(false),
            synced: Mutex::new(None),
            gate: Gate {
                busy: Mutex::new(0),
                cv: Condvar::new(),
                limit: threads,
            },
            #[cfg(test)]
            after_write_lock: Mutex::new(None),
        }
    }

    fn new(dim: usize, threads: usize) -> Result<Self> {
        let index = Index::new(&options(dim)).map_err(err)?;
        index
            .reserve_capacity_and_threads(MIN_CAPACITY, threads)
            .map_err(err)?;
        Ok(Self::wrap(index, threads))
    }

    /// Fails if the file is missing, unreadable or was built with another dimension, metric
    /// or quantization.
    fn open(path: &Path, dim: usize) -> Result<Self> {
        // Read by Rust, not by usearch: its C paths fail for names that are not ASCII on
        // Windows (a Thai user name in ALICE_HOME) and for names that are not UTF-8 on Unix.
        let bytes = std::fs::read(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                AliceError::NotFound(format!("vector index {}", path.display()))
            }
            _ => AliceError::Io(e),
        })?;
        let index = Index::new(&options(dim)).map_err(err)?;
        // `load_from_buffer` copies (unlike `view_from_buffer`), so the bytes can go.
        index.load_from_buffer(&bytes).map_err(err)?;
        drop(bytes);
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
        let part = Self::wrap(index, THREADS);
        *part.synced.lock() = Some(path.to_path_buf());
        Ok(part)
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

    /// Insert or replace every item under one write lock. Items are already validated.
    fn put(&self, items: &[(u64, &[f32])]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let _writer = self.writer.lock();
        let i = self.inner.write();
        #[cfg(test)]
        {
            let hook = self.after_write_lock.lock().clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        self.ensure_capacity(&i, items.len())?;
        for &(k, v) in items {
            if i.remove(k).map_err(err)? > 0 {
                self.dirty.store(true, Ordering::Relaxed);
            }
            i.add(k, v).map_err(err)?;
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn remove(&self, k: u64) -> Result<()> {
        let _writer = self.writer.lock();
        if self.inner.write().remove(k).map_err(err)? > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Unfiltered top-k as (key, similarity), best first.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
        if k == 0 || self.len() == 0 {
            return Ok(Vec::new());
        }
        let _slot = self.gate.enter();
        let m = self.inner.read().search(query, k).map_err(err)?;
        Ok(m.keys
            .iter()
            .zip(m.distances.iter())
            .map(|(k, d)| (*k, 1.0 - d))
            .collect())
    }

    fn len(&self) -> usize {
        self.inner.read().size()
    }

    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Atomic save: write and sync a temp file, then rename it over the old one. Skipped when
    /// nothing changed since this part was opened from or saved to `path` and the file is still
    /// there (the chunk part can be hundreds of MB; one new memory must not rewrite it).
    fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("usearch.tmp");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Writes take this mutex too, so the dirty flag cannot change until the save is done.
        let _writer = self.writer.lock();
        if !self.is_dirty() && self.synced.lock().as_deref() == Some(path) && path.exists() {
            return Ok(());
        }
        // Serialized in memory and written by Rust (see `open`). The read guard lives for this
        // block only: searches go on, writers wait above.
        let snapshot: Result<Vec<u8>> = {
            let i = self.inner.read();
            let mut buf = vec![0u8; i.serialized_length()];
            i.save_to_buffer(&mut buf).map_err(err).map(|()| buf)
        };
        let written = snapshot.and_then(|buf| {
            let mut f = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut f, &buf)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)?;
            Ok(())
        });
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written?;
        self.dirty.store(false, Ordering::Relaxed);
        *self.synced.lock() = Some(path.to_path_buf());
        Ok(())
    }
}

/// The chunk part: a sign-bit index (see `crate::bq`) with the same locks and save rules as
/// `Part`. It needs no thread slots: a search is a scan under the read lock.
struct ChunkPart {
    inner: RwLock<BinaryIndex>,
    writer: Mutex<()>,
    dirty: AtomicBool,
    synced: Mutex<Option<PathBuf>>,
    /// Runs inside `put` right after the write lock is taken, so tests can hold it.
    #[cfg(test)]
    after_write_lock: Mutex<Option<Hook>>,
}

impl ChunkPart {
    fn new(dim: usize) -> Self {
        Self {
            inner: RwLock::new(BinaryIndex::new(dim)),
            writer: Mutex::new(()),
            dirty: AtomicBool::new(false),
            synced: Mutex::new(None),
            #[cfg(test)]
            after_write_lock: Mutex::new(None),
        }
    }

    /// Fails if the file is missing, damaged or holds another dimension.
    fn open(path: &Path, dim: usize) -> Result<Self> {
        let f = std::fs::File::open(path)
            .map_err(|e| AliceError::NotFound(format!("chunk index {}: {e}", path.display())))?;
        let idx = BinaryIndex::read_from(std::io::BufReader::new(f), dim)?;
        let part = Self::new(dim);
        *part.inner.write() = idx;
        *part.synced.lock() = Some(path.to_path_buf());
        Ok(part)
    }

    /// Insert or replace every item under one write lock. Items are already validated.
    fn put(&self, items: &[(i64, &[f32])]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let _writer = self.writer.lock();
        let mut i = self.inner.write();
        #[cfg(test)]
        {
            let hook = self.after_write_lock.lock().clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        // Mark first: an error part-way still leaves the earlier items changed.
        self.dirty.store(true, Ordering::Relaxed);
        for &(seq, v) in items {
            i.upsert(seq, v)?;
        }
        Ok(())
    }

    fn remove_many(&self, seqs: &[i64]) -> usize {
        let _writer = self.writer.lock();
        let n = self.inner.write().remove_many(seqs);
        if n > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        n
    }

    fn nearest(&self, query: &[f32], rows: usize) -> Vec<(i64, f32)> {
        let i = self.inner.read();
        i.nearest(query, chunk_shortlist(i.len()), rows)
    }

    fn nearest_with(&self, query: &[f32], shortlist: usize, rows: usize) -> Vec<(i64, f32)> {
        self.inner.read().nearest(query, shortlist, rows)
    }

    /// Swap in a freshly built index (a rebuild).
    fn replace(&self, idx: BinaryIndex) {
        let _writer = self.writer.lock();
        *self.inner.write() = idx;
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn len(&self) -> usize {
        self.inner.read().len()
    }

    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Atomic: temp file, sync, rename. Skipped when unchanged since the last open or save of
    /// `path` and the file is still there.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Writes take this mutex too, so the dirty flag cannot change until the save is done.
        let _writer = self.writer.lock();
        if !self.is_dirty() && self.synced.lock().as_deref() == Some(path) && path.exists() {
            return Ok(());
        }
        let tmp = path.with_extension("bq.tmp");
        let written = (|| -> Result<()> {
            let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            // The read guard lives for this statement only: searches go on, writers wait above.
            self.inner.read().write_to(&mut w)?;
            let f = w.into_inner().map_err(|e| AliceError::Io(e.into_error()))?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written?;
        self.dirty.store(false, Ordering::Relaxed);
        *self.synced.lock() = Some(path.to_path_buf());
        Ok(())
    }
}

pub struct VectorIndex {
    dim: usize,
    /// The model whose stored vectors rescore chunk candidates ("" = none: no chunk results
    /// from `search_chunks`).
    model: String,
    /// Even keys only.
    memories: Part,
    chunks: ChunkPart,
}

impl VectorIndex {
    /// An index without a model: `search_chunks` returns nothing (tests, lexical-only tools).
    pub fn new(dim: usize) -> Result<Self> {
        Self::for_model("", dim)
    }

    pub fn for_model(model: &str, dim: usize) -> Result<Self> {
        Self::with_threads(model, dim, THREADS)
    }

    /// `threads` below THREADS lets tests overrun the slots with a handful of threads.
    fn with_threads(model: &str, dim: usize, threads: usize) -> Result<Self> {
        Ok(Self {
            dim,
            model: model.to_string(),
            memories: Part::new(dim, threads)?,
            chunks: ChunkPart::new(dim),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Load the two files written by `save(path)`. Fails if either is missing, unreadable or
    /// was built with another dimension (or, for memories, metric or quantization); the caller
    /// then rebuilds. An M1 `chunks.usearch` next to them is ignored. Any path the OS accepts
    /// works (not ASCII, not UTF-8): both parts are read by Rust.
    pub fn open(path: &Path, model: &str, dim: usize) -> Result<Self> {
        let (memories, chunks) = part_paths(path);
        Ok(Self {
            dim,
            model: model.to_string(),
            memories: Part::open(&memories, dim)?,
            chunks: ChunkPart::open(&chunks, dim)?,
        })
    }

    pub fn upsert(&self, key: VecKey, v: &[f32]) -> Result<()> {
        self.put(&[(key, v)])
    }

    /// Insert or replace many vectors: every item is checked first (one bad item rejects the
    /// batch and nothing is written), then each part takes its write lock once for its share
    /// (memories first). A usearch failure part-way (out of memory) leaves the items before it
    /// written; the index then disagrees with the `embeddings` table and the daemon rebuilds.
    pub fn upsert_batch(&self, items: &[(VecKey, Vec<f32>)]) -> Result<()> {
        let items: Vec<(VecKey, &[f32])> = items.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        self.put(&items)
    }

    fn put(&self, items: &[(VecKey, &[f32])]) -> Result<()> {
        let (mut memories, mut chunks) = (Vec::new(), Vec::new());
        for &(key, v) in items {
            check_dim("vector", v.len(), self.dim)?;
            let k = checked_key(key)?;
            match key {
                VecKey::Memory(_) => memories.push((k, v)),
                VecKey::Chunk(seq) => chunks.push((seq, v)),
            }
        }
        self.memories.put(&memories)?;
        self.chunks.put(&chunks)
    }

    pub fn remove(&self, key: VecKey) -> Result<()> {
        let k = checked_key(key)?;
        match key {
            VecKey::Memory(_) => self.memories.remove(k),
            VecKey::Chunk(seq) => {
                self.chunks.remove_many(&[seq]);
                Ok(())
            }
        }
    }

    /// Remove many chunk vectors at once (retention, purges); returns how many were present.
    pub fn remove_chunks(&self, seqs: &[i64]) -> Result<usize> {
        Ok(self.chunks.remove_many(seqs))
    }

    /// Best first, from one part. Memories: similarity = 1 - cosine distance in the int8 HNSW.
    /// Chunks: the in-RAM estimate `search_chunks` ranks by before it reads the stored vectors,
    /// for ordering only (recall uses [`Self::search_chunks`]).
    pub fn search(&self, query: &[f32], k: usize, filter: KeyFilter) -> Result<Vec<(VecKey, f32)>> {
        check_dim("query", query.len(), self.dim)?;
        Ok(match filter {
            KeyFilter::Memories => self
                .memories
                .search(query, k)?
                .into_iter()
                .map(|(key, s)| (decode_key(key), s))
                .collect(),
            KeyFilter::Chunks => self
                .chunks
                .nearest(query, k)
                .into_iter()
                .map(|(seq, s)| (VecKey::Chunk(seq), s))
                .collect(),
        })
    }

    /// Nearest chunks by exact cosine. The [`chunk_shortlist`] nearest codes by Hamming
    /// distance are re-scored in RAM against the float query, and the best `max(k,
    /// MIN_RESCORE)` of those are rescored with this model's f16 vectors from the `embeddings`
    /// table (one primary-key read each). Empty for an index without a model.
    pub fn search_chunks(
        &self,
        conn: &Connection,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(VecKey, f32)>> {
        self.search_chunks_with(conn, query, k, None, MIN_RESCORE)
    }

    /// [`Self::search_chunks`] with another shortlist (`None`: [`chunk_shortlist`]) and another
    /// floor for the rows read (`max(k, rows)` are read), for recall measurements.
    pub fn search_chunks_with(
        &self,
        conn: &Connection,
        query: &[f32],
        k: usize,
        shortlist: Option<usize>,
        rows: usize,
    ) -> Result<Vec<(VecKey, f32)>> {
        check_dim("query", query.len(), self.dim)?;
        if k == 0 || self.model.is_empty() {
            return Ok(Vec::new());
        }
        let rows = rows.max(k);
        let ranked = match shortlist {
            Some(s) => self.chunks.nearest_with(query, s, rows),
            None => self.chunks.nearest(query, rows),
        };
        let keys: Vec<VecKey> = ranked
            .into_iter()
            .map(|(seq, _)| VecKey::Chunk(seq))
            .collect();
        let mut scored: Vec<(VecKey, f32)> = embeddings::get_many(conn, &self.model, &keys)?
            .into_iter()
            .map(|(key, v)| (key, crate::embed::cosine(query, &v)))
            .collect();
        // Best first; ties go to the newer chunk.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
        scored.truncate(k);
        Ok(scored)
    }

    pub fn len(&self) -> usize {
        self.memories.len() + self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when the index has changed since it was created, opened or last saved.
    pub fn is_dirty(&self) -> bool {
        self.memories.is_dirty() || self.chunks.is_dirty()
    }

    /// Write `path.with_extension("memories.usearch")` and `path.with_extension("chunks.bq")`,
    /// each atomically. A part that has not changed since it was opened from or saved to that
    /// same file (which still exists) is not rewritten; every other part is. Then the M1 chunk
    /// file (`chunks.usearch`), if any, is deleted. Any path the OS accepts works.
    pub fn save(&self, path: &Path) -> Result<()> {
        let (memories, chunks) = part_paths(path);
        self.memories.save(&memories)?;
        self.chunks.save(&chunks)?;
        let legacy = legacy_chunk_path(path);
        if let Err(e) = std::fs::remove_file(&legacy)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                error = %e,
                file = %legacy.display(),
                "could not delete the M1 chunk index; it is unused and can be deleted by hand"
            );
        }
        Ok(())
    }

    /// Record that the files at `path` hold the index as last saved. A clean stop calls this
    /// after its final save; a start trusts the chunk file only when the mark is there.
    pub fn mark_clean(path: &Path) -> Result<()> {
        std::fs::write(clean_mark_path(path), b"")?;
        Ok(())
    }

    /// Whether the last stop left the clean mark next to `path`. The mark is removed, so a
    /// crash from now on leaves none; a mark that cannot be removed counts as absent.
    pub fn take_clean_mark(path: &Path) -> bool {
        let mark = clean_mark_path(path);
        if !mark.exists() {
            return false;
        }
        match std::fs::remove_file(&mark) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, file = %mark.display(), "could not remove the clean-stop mark");
                false
            }
        }
    }

    /// A fresh index for `model` holding every stored vector of it. Vectors that cannot be
    /// added (wrong dimension, out-of-range key) are skipped and counted in a warning.
    pub fn rebuild_from(conn: &Connection, model: &str, dim: usize) -> Result<Self> {
        let idx = Self::for_model(model, dim)?;
        idx.fill_from(conn, true)?;
        Ok(idx)
    }

    /// Rebuild the chunk part in place from the stored vectors of this index's model, keeping
    /// the memory part: after an unclean stop the chunk file can miss chunks or hold stale ones
    /// even when the counts agree.
    pub fn rebuild_chunks_from(&self, conn: &Connection) -> Result<()> {
        self.fill_from(conn, false)
    }

    /// Fill the chunk part (and the memory part with `memories`) from the table. With at least
    /// `BOOTSTRAP` chunk vectors the chunk part is built in one go: its mean fitted on a sample
    /// spread over all of them (not the first rows), codes sorted by seq once at the end.
    fn fill_from(&self, conn: &Connection, memories: bool) -> Result<()> {
        // Free the old codes first: nothing searches an index while it is rebuilt.
        self.chunks.replace(BinaryIndex::new(self.dim));
        let sample: Vec<Vec<f32>> =
            if embeddings::count_chunks_up_to(conn, &self.model, BOOTSTRAP)? >= BOOTSTRAP {
                embeddings::sample_chunks(conn, &self.model, BOOTSTRAP)?
                    .into_iter()
                    .filter(|v| v.len() == self.dim)
                    .collect()
            } else {
                Vec::new()
            };
        let mut bulk = match mean_of(self.dim, &sample) {
            Some(mean) => Some(BinaryIndex::bulk(self.dim, mean)?),
            None => None,
        };
        drop(sample);
        let mut failed = 0usize;
        let mut first_error = None;
        embeddings::for_each(conn, &self.model, |key, v| {
            let added = match (key, bulk.as_mut()) {
                (VecKey::Memory(_), _) if !memories => Ok(()),
                (VecKey::Chunk(seq), Some(b)) => check_dim("vector", v.len(), self.dim)
                    .and_then(|()| checked_key(key))
                    .and_then(|_| b.push(seq, &v)),
                _ => self.upsert(key, &v),
            };
            if let Err(e) = added {
                failed += 1;
                first_error.get_or_insert_with(|| format!("{key:?}: {e}"));
            }
        })?;
        if let Some(b) = bulk {
            self.chunks.replace(b.finish());
        }
        if failed > 0 {
            tracing::warn!(
                failed,
                first_error = first_error.as_deref().unwrap_or_default(),
                "some vectors could not be added during rebuild"
            );
        }
        Ok(())
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
        let back = VectorIndex::open(&path, "fake", 384).unwrap();
        assert_eq!(back.len(), 3_000);
        back.upsert(VecKey::Memory(5_000), &v(&e, "after load"))
            .unwrap();
        assert_eq!(
            back.search(&v(&e, "after load"), 1, KeyFilter::Memories)
                .unwrap()[0]
                .0,
            VecKey::Memory(5_000)
        );
        assert!(VectorIndex::open(&d.path().join("missing.usearch"), "fake", 384).is_err());
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
                        idx.search(&e.embed_query("t1 i1").unwrap(), 5, KeyFilter::Chunks)
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

    /// Each filter reads its own part only (no search merges the two similarity scales).
    #[test]
    fn empty_index_zero_k_wrong_query_dimension_and_each_filter() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        assert!(idx.is_empty());
        for filter in [KeyFilter::Memories, KeyFilter::Chunks] {
            assert!(idx.search(&v(&e, "x"), 5, filter).unwrap().is_empty());
        }
        idx.upsert(VecKey::Memory(1), &v(&e, "sqlite writer lease"))
            .unwrap();
        idx.upsert(VecKey::Chunk(1), &v(&e, "sqlite writer lease"))
            .unwrap();
        for filter in [KeyFilter::Memories, KeyFilter::Chunks] {
            assert!(idx.search(&v(&e, "sqlite"), 0, filter).unwrap().is_empty());
            assert!(
                matches!(
                    idx.search(&[0.0; 10], 5, filter),
                    Err(AliceError::Invalid(_))
                ),
                "a query of the wrong dimension is the caller's mistake"
            );
        }
        let keys = |filter| -> Vec<VecKey> {
            idx.search(&v(&e, "sqlite writer lease"), 5, filter)
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        assert_eq!(keys(KeyFilter::Memories), vec![VecKey::Memory(1)]);
        assert_eq!(keys(KeyFilter::Chunks), vec![VecKey::Chunk(1)]);
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
            idx.search(&v(&e, "edge"), 1, KeyFilter::Memories).unwrap()[0].0,
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
                            idx.search(&q, 1, KeyFilter::Memories).unwrap();
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
        let idx = Arc::new(VectorIndex::with_threads("", 384, 2).unwrap());
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
                                KeyFilter::Memories,
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
        let back = Arc::new(VectorIndex::open(&path, "fake", 384).unwrap());
        let start = Arc::new(std::sync::Barrier::new(100));
        let handles: Vec<_> = (0..100)
            .map(|t| {
                let (e, back, start) = (e.clone(), back.clone(), start.clone());
                std::thread::spawn(move || {
                    let q = e.embed_query(&format!("note {t}")).unwrap();
                    start.wait();
                    for _ in 0..20 {
                        back.search(&q, 5, KeyFilter::Memories).unwrap();
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
        let back = VectorIndex::open(&path, "fake", 384).unwrap();
        assert!(back.is_empty());
        for filter in [KeyFilter::Memories, KeyFilter::Chunks] {
            assert!(back.search(&v(&e, "x"), 3, filter).unwrap().is_empty());
        }
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
        assert!(no_temp_files(d.path()), "no temp file left behind");
        let back = VectorIndex::open(&path, "fake", 384).unwrap();
        assert!(!back.is_dirty());
        assert_eq!(back.len(), 9);
        let hits = back
            .search(&v(&e, "note 3"), 9, KeyFilter::Memories)
            .unwrap();
        assert_eq!(hits.len(), 9);
        assert!(hits.iter().all(|(k, _)| *k != VecKey::Memory(3)));
        back.upsert(VecKey::Memory(3), &v(&e, "note 3")).unwrap();
        assert_eq!(back.len(), 10);
        assert_eq!(
            back.search(&v(&e, "note 3"), 1, KeyFilter::Memories)
                .unwrap()[0]
                .0,
            VecKey::Memory(3)
        );
        assert!(
            VectorIndex::open(&path, "fake", 128).is_err(),
            "dimension mismatch"
        );
    }

    fn no_temp_files(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .unwrap()
            .all(|f| !f.unwrap().file_name().to_string_lossy().ends_with(".tmp"))
    }

    // Fix round 1: memories and chunks live in separate sub-indexes.

    #[test]
    fn memory_search_ignores_a_large_chunk_set() {
        // Chunks are built to resemble the memory queries; a memory search must not see them.
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        let mems: Vec<_> = (0..50)
            .map(|i| {
                (
                    VecKey::Memory(i),
                    v(&e, &format!("memory note {i} about topic {i}")),
                )
            })
            .collect();
        idx.upsert_batch(&mems).unwrap();
        let chunks: Vec<_> = (0..20_000)
            .map(|i| {
                (
                    VecKey::Chunk(i),
                    v(&e, &format!("memory note {i} about topic {i} chunk")),
                )
            })
            .collect();
        idx.upsert_batch(&chunks).unwrap();
        assert_eq!((idx.memories.len(), idx.chunks.len()), (50, 20_000));
        let q = v(&e, "memory note 7 about topic 7");
        let hits = idx.search(&q, 100, KeyFilter::Memories).unwrap();
        assert_eq!(hits.len(), 50, "every memory and nothing else");
        assert!(hits.iter().all(|(k, _)| matches!(k, VecKey::Memory(_))));
        assert_eq!(hits[0].0, VecKey::Memory(7));
        // Chunk results are estimates (ordering only; exact order needs the stored vectors).
        let chunk_hits = idx.search(&q, 5, KeyFilter::Chunks).unwrap();
        assert!(
            chunk_hits.iter().any(|(k, _)| *k == VecKey::Chunk(7)),
            "{chunk_hits:?}"
        );
        assert!(
            chunk_hits
                .iter()
                .all(|(k, _)| matches!(k, VecKey::Chunk(_)))
        );
    }

    #[test]
    fn upsert_batch_splits_by_kind_replaces_and_validates_first() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        idx.upsert_batch(&[]).unwrap();
        assert!(!idx.is_dirty(), "an empty batch changes nothing");
        idx.upsert_batch(&[
            (VecKey::Memory(1), v(&e, "alpha")),
            (VecKey::Chunk(1), v(&e, "alpha")),
            (VecKey::Memory(2), v(&e, "beta")),
            (VecKey::Chunk(2), v(&e, "gamma")),
        ])
        .unwrap();
        assert_eq!((idx.memories.len(), idx.chunks.len()), (2, 2));
        idx.upsert_batch(&[
            (VecKey::Memory(1), v(&e, "delta")),
            (VecKey::Chunk(3), v(&e, "epsilon")),
        ])
        .unwrap();
        assert_eq!((idx.memories.len(), idx.chunks.len()), (2, 3));
        let hit = idx.search(&v(&e, "delta"), 1, KeyFilter::Memories).unwrap()[0];
        assert_eq!(hit.0, VecKey::Memory(1));
        assert!(hit.1 > 0.99, "the new vector replaced the old one");
        // One bad item rejects the whole batch before anything is written.
        assert!(matches!(
            idx.upsert_batch(&[
                (VecKey::Memory(9), v(&e, "ok")),
                (VecKey::Chunk(9), vec![0.0; 10])
            ]),
            Err(AliceError::Invalid(_))
        ));
        assert!(matches!(
            idx.upsert_batch(&[
                (VecKey::Chunk(10), v(&e, "ok")),
                (VecKey::Memory(-1), v(&e, "ok"))
            ]),
            Err(AliceError::Invalid(_))
        ));
        assert_eq!(idx.len(), 5);
    }

    #[test]
    fn save_writes_two_files_and_open_needs_both() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::for_model("m", 384).unwrap();
        idx.upsert_batch(&[
            (VecKey::Memory(1), v(&e, "sqlite writer lease")),
            (VecKey::Chunk(1), v(&e, "tom yum soup recipe")),
            (VecKey::Chunk(2), v(&e, "kubernetes pod scheduling")),
        ])
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("vectors");
        let path = dir.join("m.usearch");
        idx.save(&path).unwrap();
        let (mem, chunks) = (dir.join("m.memories.usearch"), dir.join("m.chunks.bq"));
        assert!(mem.exists() && chunks.exists());
        assert!(!path.exists(), "only the two derived files are written");
        assert!(no_temp_files(&dir));
        let back = VectorIndex::open(&path, "m", 384).unwrap();
        assert_eq!((back.memories.len(), back.chunks.len()), (1, 2));
        assert_eq!(
            back.search(&v(&e, "tom yum soup recipe"), 1, KeyFilter::Chunks)
                .unwrap()[0]
                .0,
            VecKey::Chunk(1)
        );
        assert_eq!(
            back.search(&v(&e, "sqlite writer lease"), 1, KeyFilter::Memories)
                .unwrap()[0]
                .0,
            VecKey::Memory(1)
        );
        std::fs::remove_file(&chunks).unwrap();
        assert!(
            VectorIndex::open(&path, "m", 384).is_err(),
            "chunks file missing"
        );
        idx.save(&path).unwrap();
        std::fs::remove_file(&mem).unwrap();
        assert!(
            VectorIndex::open(&path, "m", 384).is_err(),
            "memories file missing"
        );
    }

    /// (inode, mtime): a rewrite by temp file + rename changes the inode.
    fn fingerprint(p: &Path) -> (u64, std::time::SystemTime) {
        let m = std::fs::metadata(p).unwrap();
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(&m);
        #[cfg(not(unix))]
        let ino = 0;
        (ino, m.modified().unwrap())
    }

    /// A save after a single remember must not rewrite the (huge) clean chunk part.
    #[test]
    fn save_rewrites_only_the_parts_that_changed() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        idx.upsert_batch(&[
            (VecKey::Memory(1), v(&e, "alpha")),
            (VecKey::Chunk(1), v(&e, "beta")),
        ])
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("m.usearch");
        idx.save(&path).unwrap();
        let (mem, chunks) = part_paths(&path);
        let chunks_before = fingerprint(&chunks);
        let mem_bytes = std::fs::read(&mem).unwrap();
        // Long enough for a rewrite to show in the mtime on coarse filesystems too.
        std::thread::sleep(std::time::Duration::from_millis(50));

        idx.upsert(VecKey::Memory(2), &v(&e, "gamma")).unwrap();
        assert!(idx.memories.is_dirty() && !idx.chunks.is_dirty());
        idx.save(&path).unwrap();
        assert_eq!(
            fingerprint(&chunks),
            chunks_before,
            "the clean chunk part was rewritten"
        );
        assert_ne!(
            std::fs::read(&mem).unwrap(),
            mem_bytes,
            "the memory part was saved"
        );
        assert!(!idx.is_dirty());
        assert!(no_temp_files(d.path()));
        let back = VectorIndex::open(&path, "fake", 384).unwrap();
        assert_eq!((back.memories.len(), back.chunks.len()), (2, 1));

        let both = (fingerprint(&mem), fingerprint(&chunks));
        std::thread::sleep(std::time::Duration::from_millis(50));
        idx.save(&path).unwrap();
        assert_eq!(
            (fingerprint(&mem), fingerprint(&chunks)),
            both,
            "nothing changed"
        );
    }

    /// Skipping is only safe when the part's file is this index's own copy.
    #[test]
    fn clean_parts_are_written_where_this_index_has_no_file() {
        let e = FakeEmbedder::new(384);
        let d = tempfile::tempdir().unwrap();
        let first = d.path().join("a.usearch");
        let idx = VectorIndex::new(384).unwrap();
        idx.upsert(VecKey::Memory(1), &v(&e, "alpha")).unwrap();
        assert!(!idx.chunks.is_dirty(), "the empty chunk part never changed");
        idx.save(&first).unwrap();
        let (_, first_chunks) = part_paths(&first);
        assert!(first_chunks.exists(), "a never-saved part is written");
        assert_eq!(VectorIndex::open(&first, "fake", 384).unwrap().len(), 1);

        let second = d.path().join("b.usearch");
        idx.save(&second).unwrap();
        assert_eq!(
            VectorIndex::open(&second, "fake", 384).unwrap().len(),
            1,
            "another path gets both parts"
        );

        let opened = VectorIndex::open(&first, "fake", 384).unwrap();
        let third = d.path().join("c.usearch");
        opened.save(&third).unwrap();
        assert_eq!(VectorIndex::open(&third, "fake", 384).unwrap().len(), 1);

        std::fs::remove_file(&first_chunks).unwrap();
        opened.save(&first).unwrap();
        assert!(
            first_chunks.exists(),
            "a clean part whose file is gone is written again"
        );
    }

    #[test]
    fn rebuild_fills_both_sub_indexes() {
        use crate::store::{embeddings, open_writer};
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let e = FakeEmbedder::new(384);
        for s in 1..=3 {
            embeddings::put(
                &c,
                VecKey::Memory(s),
                "fake",
                &v(&e, &format!("memory {s}")),
            )
            .unwrap();
        }
        for s in 1..=5 {
            embeddings::put(&c, VecKey::Chunk(s), "fake", &v(&e, &format!("chunk {s}"))).unwrap();
        }
        embeddings::put(&c, VecKey::Chunk(6), "fake", &[0.5; 8]).unwrap();
        let idx = VectorIndex::rebuild_from(&c, "fake", 384).unwrap();
        assert_eq!(
            (idx.memories.len(), idx.chunks.len()),
            (3, 5),
            "the 8-dimension vector is skipped"
        );
    }

    #[test]
    fn memory_search_proceeds_while_a_chunk_batch_holds_the_chunk_write_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let e = Arc::new(FakeEmbedder::new(384));
        let idx = Arc::new(VectorIndex::new(384).unwrap());
        idx.upsert_batch(&[
            (VecKey::Memory(1), v(&e, "sqlite writer lease")),
            (VecKey::Chunk(1), v(&e, "sqlite writer lease")),
        ])
        .unwrap();
        // The hook runs inside the batch, right after it takes the chunk write lock.
        // Every wait is bounded, so a regression fails instead of hanging the test run.
        let (locked_tx, locked_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (release_rx, armed) = (Mutex::new(release_rx), AtomicBool::new(true));
        *idx.chunks.after_write_lock.lock() = Some(Arc::new(move || {
            if armed.swap(false, Ordering::SeqCst) {
                let _ = locked_tx.send(());
                let _ = release_rx.lock().recv_timeout(Duration::from_secs(30));
            }
        }));
        let batch = {
            let (idx, e) = (idx.clone(), e.clone());
            std::thread::spawn(move || {
                idx.upsert_batch(&[
                    (VecKey::Chunk(2), e.embed_query("chunk two").unwrap()),
                    (VecKey::Chunk(3), e.embed_query("chunk three").unwrap()),
                ])
            })
        };
        locked_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the chunk batch never took the chunk write lock");
        let q = v(&e, "sqlite writer lease");
        let search = |filter: KeyFilter| {
            let (tx, rx) = mpsc::channel();
            let (idx, q) = (idx.clone(), q.clone());
            std::thread::spawn(move || {
                let _ = tx.send(idx.search(&q, 5, filter).unwrap());
            });
            rx
        };
        let memories = search(KeyFilter::Memories)
            .recv_timeout(Duration::from_secs(10))
            .expect("a memory search must not wait for the chunk batch");
        assert_eq!(memories[0].0, VecKey::Memory(1));
        let chunks = search(KeyFilter::Chunks);
        assert!(
            chunks.recv_timeout(Duration::from_millis(200)).is_err(),
            "a chunk search does wait: the hook really holds the lock"
        );
        release_tx.send(()).unwrap(); // let the batch finish
        batch.join().unwrap().unwrap();
        *idx.chunks.after_write_lock.lock() = None;
        assert_eq!(
            chunks.recv_timeout(Duration::from_secs(10)).unwrap().len(),
            3
        );
        assert_eq!(idx.len(), 4);
    }

    /// usearch opens files through C paths; the parts are read and written by Rust instead,
    /// so a data directory with a Thai name (or, on Unix, non-UTF-8 bytes) works.
    #[test]
    fn index_files_save_and_open_under_unusual_directory_names() {
        let e = FakeEmbedder::new(384);
        let d = tempfile::tempdir().unwrap();
        let thai = d.path().join("ข้อมูล ทดสอบ").join("vectors");
        // A Windows path is UTF-16 and cannot hold bytes that are not UTF-8.
        #[cfg(unix)]
        let dirs = {
            use std::os::unix::ffi::OsStrExt;
            vec![
                thai,
                d.path().join(std::ffi::OsStr::from_bytes(b"bad\xffdir")),
            ]
        };
        #[cfg(not(unix))]
        let dirs = vec![thai];
        for dir in dirs {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => {}
                // APFS refuses names that are not UTF-8 (EILSEQ); Linux filesystems take any
                // bytes, so CI on Linux covers that directory.
                Err(_) if cfg!(target_os = "macos") && dir.to_str().is_none() => continue,
                Err(e) => panic!("{}: {e}", dir.display()),
            }
            let base = dir.join("fake.usearch");
            let idx = VectorIndex::for_model("fake", 384).unwrap();
            idx.upsert(VecKey::Memory(1), &v(&e, "unicode path"))
                .unwrap();
            idx.upsert(VecKey::Chunk(1), &v(&e, "chunk")).unwrap();
            idx.save(&base).unwrap();
            let back = VectorIndex::open(&base, "fake", 384).unwrap();
            assert_eq!(back.len(), 2, "{}", dir.display());
            assert_eq!(
                back.search(&v(&e, "unicode path"), 1, KeyFilter::Memories)
                    .unwrap()[0]
                    .0,
                VecKey::Memory(1)
            );
        }
    }

    // M2 Task 1: the chunk part is a sign-bit index rescored from the `embeddings` table.

    /// The chunk part ranks by sign bits and `search_chunks` rescores with the stored vectors.
    #[test]
    fn chunk_search_rescores_candidates_with_the_stored_vectors() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::for_model("fake", 384).unwrap();
        for i in 1..=5_000i64 {
            let v = v(
                &e,
                &format!("chunk {i} on subject {} detail {}", i % 41, i % 97),
            );
            crate::store::embeddings::put(&c, VecKey::Chunk(i), "fake", &v).unwrap();
            idx.upsert(VecKey::Chunk(i), &v).unwrap();
        }
        let q = v(&e, "chunk 1234 on subject 4 detail 70");
        let hits = idx.search_chunks(&c, &q, 10).unwrap();
        assert_eq!(hits[0].0, VecKey::Chunk(1234));
        assert!(hits.windows(2).all(|w| w[0].1 >= w[1].1));
        // Every score is the exact cosine with the stored (f16) vector, not an estimate.
        let stored =
            crate::store::embeddings::get_many(&c, "fake", &[hits[0].0, hits[6].0]).unwrap();
        for ((key, score), (stored_key, vector)) in [hits[0], hits[6]].iter().zip(&stored) {
            assert_eq!(key, stored_key);
            assert_eq!(*score, crate::embed::cosine(&q, vector), "{key:?}");
        }
        assert_eq!(idx.len(), 5_000);
        // Without a model there are no stored vectors to rescore with.
        let anonymous = VectorIndex::new(384).unwrap();
        anonymous.upsert(VecKey::Chunk(1), &q).unwrap();
        assert!(anonymous.search_chunks(&c, &q, 5).unwrap().is_empty());
    }

    #[test]
    fn chunk_part_saves_reopens_and_deletes_the_m1_chunk_file() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("vectors").join("fake.usearch");
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        let legacy = base.with_extension("chunks.usearch");
        std::fs::write(&legacy, b"old hnsw").unwrap();
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::for_model("fake", 384).unwrap();
        for i in 1..=50i64 {
            idx.upsert(VecKey::Chunk(i), &v(&e, &format!("c{i}")))
                .unwrap();
        }
        idx.upsert(VecKey::Memory(7), &v(&e, "m7")).unwrap();
        idx.save(&base).unwrap();
        assert!(!legacy.exists(), "the M1 chunk HNSW file is removed");
        assert!(base.with_extension("chunks.bq").exists());
        let back = VectorIndex::open(&base, "fake", 384).unwrap();
        assert_eq!(back.len(), 51);
        assert_eq!(back.model(), "fake");
        assert!(!back.is_dirty());
    }

    #[test]
    fn rebuild_fills_both_parts_from_the_table() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let e = FakeEmbedder::new(384);
        for i in 1..=4_200i64 {
            crate::store::embeddings::put(&c, VecKey::Chunk(i), "fake", &v(&e, &format!("c{i}")))
                .unwrap();
        }
        crate::store::embeddings::put(&c, VecKey::Memory(3), "fake", &v(&e, "m")).unwrap();
        let idx = VectorIndex::rebuild_from(&c, "fake", 384).unwrap();
        assert_eq!(idx.len(), 4_201);
        let q = v(&e, "c4100");
        assert_eq!(
            idx.search_chunks(&c, &q, 1).unwrap()[0].0,
            VecKey::Chunk(4100)
        );
    }

    // Beyond the brief.

    /// Retention and purges remove many chunks at once; an absent seq changes nothing.
    #[test]
    fn remove_chunks_forgets_many_and_only_then_marks_the_index_dirty() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::for_model("fake", 384).unwrap();
        for i in 1..=10i64 {
            idx.upsert(VecKey::Chunk(i), &v(&e, &format!("c{i}")))
                .unwrap();
        }
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("fake.usearch");
        idx.save(&path).unwrap();
        assert_eq!(idx.remove_chunks(&[42, 43]).unwrap(), 0);
        assert!(!idx.is_dirty(), "nothing was there to remove");
        assert_eq!(idx.remove_chunks(&[2, 4, 42]).unwrap(), 2);
        idx.remove(VecKey::Chunk(6)).unwrap();
        assert!(idx.is_dirty());
        assert_eq!(idx.len(), 7);
        let left: Vec<VecKey> = idx
            .search(&v(&e, "c2"), 10, KeyFilter::Chunks)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(left.len(), 7);
        assert!(![2, 4, 6].iter().any(|s| left.contains(&VecKey::Chunk(*s))));
    }

    // Fix round 1.

    /// M7, M8: a rebuild fits the chunk mean on a sample spread over every chunk (not the
    /// first rows read) and builds the chunk part in one go, sorting once, even when the table
    /// returns the chunks out of seq order.
    #[test]
    fn a_rebuild_fits_the_mean_on_all_chunks_and_sorts_the_rows_once() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let e = FakeEmbedder::new(384);
        let n = 2 * BOOTSTRAP as i64;
        // Written newest first, so rows come back in descending seq order; the older half is
        // about one subject and the newer half about another.
        let text = |i: i64| {
            if i <= n / 2 {
                format!("alpha subject number {i}")
            } else {
                format!("omega topic item {i}")
            }
        };
        let mut read_order = Vec::new();
        for i in (1..=n).rev() {
            let x = v(&e, &text(i));
            embeddings::put(&c, VecKey::Chunk(i), "fake", &x).unwrap();
            read_order.push(embeddings::decode_f16(&embeddings::encode_f16(&x)));
        }
        let idx = VectorIndex::rebuild_from(&c, "fake", 384).unwrap();
        assert_eq!(idx.len(), n as usize);
        let fitted = idx.chunks.inner.read().mean().unwrap().to_vec();
        let whole = mean_of(384, &read_order).unwrap();
        let first_rows = mean_of(384, &read_order[..BOOTSTRAP]).unwrap();
        let dist = |a: &[f32], b: &[f32]| -> f32 {
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y) * (x - y))
                .sum::<f32>()
                .sqrt()
        };
        assert!(
            dist(&fitted, &whole) < 0.2 * dist(&first_rows, &whole),
            "fitted mean {} from the whole corpus, first rows {}",
            dist(&fitted, &whole),
            dist(&first_rows, &whole)
        );
        let q = v(&e, &text(1_234));
        assert_eq!(
            idx.search_chunks(&c, &q, 1).unwrap()[0].0,
            VecKey::Chunk(1_234)
        );
        // Sorted by seq: the file reader rejects anything else.
        let path = d.path().join("fake.usearch");
        idx.save(&path).unwrap();
        assert_eq!(
            VectorIndex::open(&path, "fake", 384).unwrap().len(),
            n as usize
        );
    }

    /// M9: after an unclean stop the chunk part is rebuilt from the table; the memory part,
    /// loaded from its file, stays as it is.
    #[test]
    fn rebuild_chunks_from_replaces_the_chunk_part_and_keeps_the_memories() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let e = FakeEmbedder::new(384);
        for s in 1..=3 {
            embeddings::put(&c, VecKey::Chunk(s), "fake", &v(&e, &format!("chunk {s}"))).unwrap();
        }
        embeddings::put(&c, VecKey::Memory(1), "fake", &v(&e, "memory one")).unwrap();
        let idx = VectorIndex::for_model("fake", 384).unwrap();
        idx.upsert(VecKey::Memory(1), &v(&e, "memory one")).unwrap();
        idx.upsert(VecKey::Memory(2), &v(&e, "memory two")).unwrap();
        // A stale chunk part with the right count: chunk 3 missing, chunk 99 long gone.
        for s in [1, 2, 99] {
            idx.upsert(VecKey::Chunk(s), &v(&e, &format!("chunk {s}")))
                .unwrap();
        }
        let path = d.path().join("fake.usearch");
        idx.save(&path).unwrap();
        idx.rebuild_chunks_from(&c).unwrap();
        assert!(idx.is_dirty());
        let mut chunks: Vec<VecKey> = idx
            .search(&v(&e, "chunk"), 10, KeyFilter::Chunks)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        chunks.sort();
        assert_eq!(
            chunks,
            vec![VecKey::Chunk(1), VecKey::Chunk(2), VecKey::Chunk(3)]
        );
        assert_eq!(idx.memories.len(), 2, "the memory part is kept as loaded");
    }

    /// M5: a leftover M1 file that cannot be deleted (here a directory in its place) is
    /// logged, and the save still succeeds.
    #[test]
    fn an_undeletable_m1_chunk_file_does_not_fail_the_save() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("fake.usearch");
        let legacy = base.with_extension("chunks.usearch");
        std::fs::create_dir_all(legacy.join("inside")).unwrap();
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::for_model("fake", 384).unwrap();
        idx.upsert(VecKey::Chunk(1), &v(&e, "c1")).unwrap();
        idx.save(&base).unwrap();
        assert!(legacy.exists());
        assert_eq!(VectorIndex::open(&base, "fake", 384).unwrap().len(), 1);
    }

    #[test]
    fn the_clean_mark_is_taken_once() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("vectors").join("m.usearch");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(!VectorIndex::take_clean_mark(&path));
        VectorIndex::mark_clean(&path).unwrap();
        assert!(path.with_extension("clean").exists());
        assert!(VectorIndex::take_clean_mark(&path));
        assert!(
            !VectorIndex::take_clean_mark(&path),
            "taken at start: a crash from now on leaves none"
        );
    }

    #[test]
    fn the_shortlist_grows_with_the_root_of_the_chunk_count() {
        assert_eq!(chunk_shortlist(0), SHORTLIST_MIN);
        assert_eq!(
            chunk_shortlist(10_000),
            SHORTLIST_MIN.max(SHORTLIST_PER_ROOT * 100)
        );
        assert_eq!(
            chunk_shortlist(1_000_000),
            SHORTLIST_MIN.max(SHORTLIST_PER_ROOT * 1_000)
        );
        assert!(chunk_shortlist(4_000_000) >= chunk_shortlist(1_000_000));
    }
}
