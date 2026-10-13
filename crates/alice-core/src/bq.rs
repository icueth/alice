//! Flat sign-bit index for raw chunks.
//!
//! Each vector is kept as one bit per dimension: whether it lies above the mean of the first
//! `BOOTSTRAP` vectors. Centring matters: e5 vectors share a large common direction (unrelated
//! texts sit at cosine ~0.7), so uncentred sign bits would be nearly constant. A search ranks
//! every code by Hamming distance (popcount of XOR) and returns the nearest candidates; the
//! caller rescores them with the exact vectors kept in SQLite (`VectorIndex::search_chunks`).
//! The vector's own coordinates are used, not signs of random projections: with the real
//! model they kept more of the exact top 10 (`docs/benchmarks/m2.md`), though for sparse
//! vectors (the test embedder's) they mostly count non-zero coordinates.
//!
//! Until `BOOTSTRAP` vectors have arrived there is no mean: those vectors are kept as f32 and
//! searched exactly. At `BOOTSTRAP` the mean is computed once and kept for the life of this
//! index; a rebuild computes a fresh one.
//!
//! Memory: 48 bytes of code and 8 bytes of key per 384-dimension vector (~56 MB at 1M chunks),
//! against ~560 MB for the int8 HNSW it replaces. Search is a linear scan (~48 MB at 1M).
//!
//! File format (little-endian): magic `ALICEBQ1`, dimension (u32), state (u32: 1 while
//! bootstrapping), count (u64); then either `count` × (seq i64, `dim` × f32), or the mean
//! (`dim` × f32), `count` sorted seqs (i64) and `count` codes (`dim / 64` rounded up × u64).

use crate::error::{AliceError, Result};
use std::collections::BinaryHeap;
use std::io::{Read, Write};

/// Vectors kept exactly before the mean is fixed.
pub const BOOTSTRAP: usize = 4_096;
const MAGIC: &[u8; 8] = b"ALICEBQ1";
/// Bytes read at a time when loading, so a damaged count never allocates what the file lacks.
const READ_PIECE: usize = 64 * 1024;

pub struct BinaryIndex {
    dim: usize,
    /// u64 words per code.
    words: usize,
    mean: Option<Vec<f32>>,
    /// Before the mean is fixed: (seq, vector), sorted by seq.
    boot: Vec<(i64, Vec<f32>)>,
    /// After: sorted seqs, and `words` u64 of code per seq in the same order.
    keys: Vec<i64>,
    codes: Vec<u64>,
}

fn bad(msg: impl Into<String>) -> AliceError {
    AliceError::Other(format!("chunk index file: {}", msg.into()))
}

/// One bit per dimension: set where `v` lies above `mean`.
fn encode(mean: &[f32], words: usize, v: &[f32]) -> Vec<u64> {
    let mut out = vec![0u64; words];
    for (i, (x, m)) in v.iter().zip(mean).enumerate() {
        if x > m {
            out[i / 64] |= 1u64 << (i % 64);
        }
    }
    out
}

/// `count` little-endian values of `N` bytes, read piece by piece into a vector of exactly
/// that size. A count the file cannot hold fails as truncated (or as impossible to reserve).
fn read_values<const N: usize, T>(
    r: &mut impl Read,
    count: usize,
    from: fn([u8; N]) -> T,
) -> Result<Vec<T>> {
    let mut left = count
        .checked_mul(N)
        .ok_or_else(|| bad("impossible length"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| bad("impossible length"))?;
    let mut buf = vec![0u8; READ_PIECE.min(left)];
    while left > 0 {
        let n = left.min(READ_PIECE);
        r.read_exact(&mut buf[..n]).map_err(|_| bad("truncated"))?;
        out.extend(buf[..n].as_chunks::<N>().0.iter().map(|b| from(*b)));
        left -= n;
    }
    Ok(out)
}

fn strictly_increasing_from_zero(seqs: impl IntoIterator<Item = i64>) -> bool {
    let mut last = -1;
    seqs.into_iter().all(|s| {
        let ok = s > last;
        last = s;
        ok
    })
}

impl BinaryIndex {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            words: dim.div_ceil(64),
            mean: None,
            boot: Vec::new(),
            keys: Vec::new(),
            codes: Vec::new(),
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.keys.len() + self.boot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_bootstrapping(&self) -> bool {
        self.mean.is_none()
    }

    /// Insert or replace the vector of `seq`.
    pub fn upsert(&mut self, seq: i64, v: &[f32]) -> Result<()> {
        if v.len() != self.dim {
            return Err(AliceError::invalid(format!(
                "vector has {} dimensions; the index uses {}",
                v.len(),
                self.dim
            )));
        }
        if seq < 0 {
            return Err(AliceError::invalid(format!("chunk seq {seq} is negative")));
        }
        let Some(mean) = self.mean.as_deref() else {
            match self.boot.binary_search_by_key(&seq, |(s, _)| *s) {
                Ok(i) => self.boot[i].1 = v.to_vec(),
                Err(i) => self.boot.insert(i, (seq, v.to_vec())),
            }
            if self.boot.len() >= BOOTSTRAP {
                self.fix_mean();
            }
            return Ok(());
        };
        let code = encode(mean, self.words, v);
        self.put_code(seq, &code);
        Ok(())
    }

    /// Fix the mean of the bootstrap vectors and turn every one of them into a code.
    fn fix_mean(&mut self) {
        let mut mean = vec![0f32; self.dim];
        for (_, v) in &self.boot {
            for (m, x) in mean.iter_mut().zip(v) {
                *m += x;
            }
        }
        let n = self.boot.len().max(1) as f32;
        mean.iter_mut().for_each(|m| *m /= n);
        let boot = std::mem::take(&mut self.boot);
        self.keys.reserve(boot.len());
        self.codes.reserve(boot.len() * self.words);
        for (seq, v) in &boot {
            let code = encode(&mean, self.words, v);
            self.put_code(*seq, &code);
        }
        self.mean = Some(mean);
    }

    fn put_code(&mut self, seq: i64, code: &[u64]) {
        let w = self.words;
        // Chunks arrive in seq order almost always: append.
        if self.keys.last().is_none_or(|&last| last < seq) {
            self.keys.push(seq);
            self.codes.extend_from_slice(code);
            return;
        }
        match self.keys.binary_search(&seq) {
            Ok(i) => self.codes[i * w..(i + 1) * w].copy_from_slice(code),
            Err(i) => {
                self.keys.insert(i, seq);
                self.codes.splice(i * w..i * w, code.iter().copied());
            }
        }
    }

    pub fn remove(&mut self, seq: i64) -> bool {
        self.remove_many(&[seq]) == 1
    }

    /// Remove every listed seq in one pass over the codes after the first one found; returns
    /// how many were present.
    pub fn remove_many(&mut self, seqs: &[i64]) -> usize {
        let before = self.len();
        let mut sorted = seqs.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        self.boot.retain(|(s, _)| sorted.binary_search(s).is_err());
        let mut gone: Vec<usize> = sorted
            .iter()
            .filter_map(|s| self.keys.binary_search(s).ok())
            .collect();
        if let Some(&first) = gone.first() {
            gone.push(usize::MAX); // sentinel: never equal to a position
            let w = self.words;
            let (mut keep, mut next) = (first, 0);
            for i in first..self.keys.len() {
                if gone[next] == i {
                    next += 1;
                    continue;
                }
                self.keys[keep] = self.keys[i];
                self.codes.copy_within(i * w..(i + 1) * w, keep * w);
                keep += 1;
            }
            self.keys.truncate(keep);
            self.codes.truncate(keep * w);
        }
        before - self.len()
    }

    /// Up to `n` seqs nearest to `query`, best first, with an estimated cosine similarity
    /// (`cos(π · hamming / dim)`, for ordering only; exact while bootstrapping). Ties go to
    /// the older seq. A query of the wrong dimension finds nothing.
    pub fn candidates(&self, query: &[f32], n: usize) -> Vec<(i64, f32)> {
        if n == 0 || query.len() != self.dim {
            return Vec::new();
        }
        let Some(mean) = self.mean.as_deref() else {
            let mut all: Vec<(i64, f32)> = self
                .boot
                .iter()
                .map(|(s, v)| (*s, crate::embed::cosine(query, v)))
                .collect();
            all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            all.truncate(n);
            return all;
        };
        let q = encode(mean, self.words, query);
        let mut heap: BinaryHeap<(u32, usize)> = BinaryHeap::with_capacity(n + 1);
        for (i, code) in self.codes.chunks_exact(self.words).enumerate() {
            let d: u32 = code.iter().zip(&q).map(|(a, b)| (a ^ b).count_ones()).sum();
            if heap.len() < n {
                heap.push((d, i));
            } else if let Some(&(worst, _)) = heap.peek()
                && d < worst
            {
                heap.pop();
                heap.push((d, i));
            }
        }
        let mut out = heap.into_vec();
        out.sort_unstable();
        out.into_iter()
            .map(|(d, i)| {
                let est = (std::f32::consts::PI * d as f32 / self.dim as f32).cos();
                (self.keys[i], est)
            })
            .collect()
    }

    pub fn write_to(&self, mut w: impl Write) -> std::io::Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&(self.dim as u32).to_le_bytes())?;
        let boot = self.mean.is_none();
        w.write_all(&u32::from(boot).to_le_bytes())?;
        if let Some(mean) = &self.mean {
            w.write_all(&(self.keys.len() as u64).to_le_bytes())?;
            for x in mean {
                w.write_all(&x.to_le_bytes())?;
            }
            for k in &self.keys {
                w.write_all(&k.to_le_bytes())?;
            }
            for c in &self.codes {
                w.write_all(&c.to_le_bytes())?;
            }
        } else {
            w.write_all(&(self.boot.len() as u64).to_le_bytes())?;
            for (s, v) in &self.boot {
                w.write_all(&s.to_le_bytes())?;
                for x in v {
                    w.write_all(&x.to_le_bytes())?;
                }
            }
        }
        w.flush()
    }

    /// Fails on a wrong magic, another dimension, an impossible or truncated count, seqs out
    /// of order, or trailing bytes.
    pub fn read_from(mut r: impl Read, dim: usize) -> Result<Self> {
        fn take<const N: usize>(r: &mut impl Read) -> Result<[u8; N]> {
            let mut b = [0u8; N];
            r.read_exact(&mut b).map_err(|_| bad("truncated"))?;
            Ok(b)
        }
        if &take::<8>(&mut r)? != MAGIC {
            return Err(bad("not an Alice chunk index"));
        }
        let file_dim = u32::from_le_bytes(take::<4>(&mut r)?) as usize;
        if file_dim != dim {
            return Err(bad(format!("{file_dim} dimensions; {dim} expected")));
        }
        let boot = match u32::from_le_bytes(take::<4>(&mut r)?) {
            0 => false,
            1 => true,
            other => return Err(bad(format!("unknown state {other}"))),
        };
        let n = usize::try_from(u64::from_le_bytes(take::<8>(&mut r)?))
            .map_err(|_| bad("impossible length"))?;
        let mut idx = BinaryIndex::new(dim);
        if boot {
            // The mean is fixed as soon as BOOTSTRAP vectors are in.
            if n >= BOOTSTRAP {
                return Err(bad(format!("{n} vectors waiting for a mean")));
            }
            for _ in 0..n {
                let s = i64::from_le_bytes(take::<8>(&mut r)?);
                idx.boot
                    .push((s, read_values(&mut r, dim, f32::from_le_bytes)?));
            }
            if !strictly_increasing_from_zero(idx.boot.iter().map(|(s, _)| *s)) {
                return Err(bad("seqs out of order"));
            }
        } else {
            idx.mean = Some(read_values(&mut r, dim, f32::from_le_bytes)?);
            idx.keys = read_values(&mut r, n, i64::from_le_bytes)?;
            if !strictly_increasing_from_zero(idx.keys.iter().copied()) {
                return Err(bad("seqs out of order"));
            }
            let words = n
                .checked_mul(idx.words)
                .ok_or_else(|| bad("impossible length"))?;
            idx.codes = read_values(&mut r, words, u64::from_le_bytes)?;
        }
        let mut rest = [0u8; 1];
        if r.read(&mut rest).map_err(|e| bad(e.to_string()))? != 0 {
            return Err(bad("trailing bytes"));
        }
        Ok(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{Embedder, FakeEmbedder, cosine};

    fn fake() -> FakeEmbedder {
        FakeEmbedder::new(384)
    }

    fn text(i: usize) -> String {
        format!("chunk {i} about topic {} and detail {}", i % 37, i % 101)
    }

    fn filled(n: usize) -> (BinaryIndex, Vec<Vec<f32>>) {
        let e = fake();
        let mut idx = BinaryIndex::new(384);
        let mut vs = Vec::new();
        for i in 0..n {
            let v = e.embed_query(&text(i)).unwrap();
            idx.upsert(i as i64 + 1, &v).unwrap();
            vs.push(v);
        }
        (idx, vs)
    }

    #[test]
    fn bootstrap_is_exact_then_codes_take_over_at_the_threshold() {
        let (idx, vs) = filled(BOOTSTRAP - 1);
        assert!(idx.is_bootstrapping());
        let c = idx.candidates(&vs[10], 1);
        assert_eq!(c[0].0, 11);
        assert!((c[0].1 - 1.0).abs() < 1e-5, "exact while bootstrapping");
        let (idx, vs) = filled(BOOTSTRAP + 10);
        assert!(!idx.is_bootstrapping());
        assert_eq!(idx.len(), BOOTSTRAP + 10);
        for probe in [0usize, 777, BOOTSTRAP + 5] {
            let c = idx.candidates(&vs[probe], 5);
            assert_eq!(
                c[0].0,
                probe as i64 + 1,
                "a stored vector is its own nearest code"
            );
        }
    }

    /// The fake embedder's vectors are sparse (~43 of 384 coordinates set) and its exact top 10
    /// beyond rank 3 is a near tie (rank 10 and rank 30 differ by ~0.015 cosine), which sign
    /// bits cannot order: within 200 / 400 / 800 / 1,600 candidates they keep 0.73 / 0.86 /
    /// 0.94 / 0.99 of it. This test therefore reads 1,600 candidates; the real model's gate is
    /// `alice-embed/tests/binary_chunk_recall.rs` (0.985 within `MIN_RESCORE` = 400).
    #[test]
    fn candidates_contain_the_exact_top_ten_for_most_queries() {
        const CANDIDATES: usize = 1_600;
        let (idx, vs) = filled(6_000);
        let e = fake();
        let mut found = 0;
        let queries = 50;
        for q in 0..queries {
            let qv = e
                .embed_query(&format!("topic {} and detail {}", q % 37, q % 101))
                .unwrap();
            let mut exact: Vec<(usize, f32)> = vs
                .iter()
                .enumerate()
                .map(|(i, v)| (i, cosine(&qv, v)))
                .collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1));
            let got: std::collections::HashSet<i64> = idx
                .candidates(&qv, CANDIDATES)
                .into_iter()
                .map(|(s, _)| s)
                .collect();
            found += exact[..10]
                .iter()
                .filter(|(i, _)| got.contains(&(*i as i64 + 1)))
                .count();
        }
        let recall = found as f64 / (queries * 10) as f64;
        assert!(
            recall >= 0.95,
            "recall of the exact top 10 within {CANDIDATES} candidates: {recall:.3}"
        );
    }

    #[test]
    fn upsert_replaces_inserts_out_of_order_and_remove_forgets() {
        let (mut idx, vs) = filled(BOOTSTRAP + 100);
        let e = fake();
        let other = e.embed_query("something completely different").unwrap();
        idx.upsert(5, &other).unwrap();
        assert_eq!(idx.len(), BOOTSTRAP + 100, "replace, not add");
        assert_eq!(idx.candidates(&other, 1)[0].0, 5);
        idx.upsert(-1, &vs[3]).unwrap_err();
        idx.upsert(1_000_000, &vs[7]).unwrap();
        assert!(idx.remove(1_000_000));
        assert!(!idx.remove(1_000_000));
        assert_eq!(idx.remove_many(&[1, 2, 3, 999_999]), 3);
        assert_eq!(idx.len(), BOOTSTRAP + 97);
        assert!(idx.candidates(&vs[0], 3).iter().all(|(s, _)| *s != 1));
        assert!(idx.upsert(9, &[0.0; 10]).is_err(), "wrong dimension");
    }

    #[test]
    fn save_and_load_round_trip_in_both_states_and_reject_damage() {
        for n in [100, BOOTSTRAP + 50] {
            let (idx, vs) = filled(n);
            let mut buf = Vec::new();
            idx.write_to(&mut buf).unwrap();
            let back = BinaryIndex::read_from(&buf[..], 384).unwrap();
            assert_eq!(back.len(), n);
            assert_eq!(back.is_bootstrapping(), idx.is_bootstrapping());
            assert_eq!(back.candidates(&vs[42], 3), idx.candidates(&vs[42], 3));
            assert!(
                BinaryIndex::read_from(&buf[..buf.len() - 9], 384).is_err(),
                "truncated"
            );
            assert!(
                BinaryIndex::read_from(&buf[..], 768).is_err(),
                "other dimension"
            );
            let mut bad = buf.clone();
            bad[0] = b'X';
            assert!(BinaryIndex::read_from(&bad[..], 384).is_err(), "bad magic");
        }
    }

    // Beyond the brief: a damaged file must fail cleanly, never allocate what it claims to hold.

    /// Header: magic (8), dimension (4), state (4), count (8).
    const COUNT_AT: usize = 16;

    #[test]
    fn a_damaged_count_or_key_order_is_rejected_without_allocating_it() {
        for n in [100, BOOTSTRAP + 50] {
            let (idx, _) = filled(n);
            let mut buf = Vec::new();
            idx.write_to(&mut buf).unwrap();
            for count in [u64::MAX, u64::MAX / 8, 1 << 40, n as u64 + 1] {
                let mut bad = buf.clone();
                bad[COUNT_AT..COUNT_AT + 8].copy_from_slice(&count.to_le_bytes());
                assert!(
                    BinaryIndex::read_from(&bad[..], 384).is_err(),
                    "n {n}, count {count}"
                );
            }
            let mut extra = buf.clone();
            extra.push(0);
            assert!(
                BinaryIndex::read_from(&extra[..], 384).is_err(),
                "trailing bytes"
            );
        }
        // Two keys swapped in a coded file: the sorted order is what search and upsert rely on.
        let (idx, _) = filled(BOOTSTRAP + 50);
        let mut buf = Vec::new();
        idx.write_to(&mut buf).unwrap();
        let keys_at = COUNT_AT + 8 + 384 * 4;
        let mut bad = buf.clone();
        bad[keys_at..keys_at + 8].copy_from_slice(&buf[keys_at + 8..keys_at + 16]);
        bad[keys_at + 8..keys_at + 16].copy_from_slice(&buf[keys_at..keys_at + 8]);
        assert!(
            BinaryIndex::read_from(&bad[..], 384).is_err(),
            "keys out of order"
        );
        // A bootstrapping file can never hold BOOTSTRAP vectors: the mean is fixed at that point.
        let mut boot = Vec::new();
        boot.extend_from_slice(b"ALICEBQ1");
        boot.extend_from_slice(&384u32.to_le_bytes());
        boot.extend_from_slice(&1u32.to_le_bytes());
        boot.extend_from_slice(&(BOOTSTRAP as u64).to_le_bytes());
        assert!(BinaryIndex::read_from(&boot[..], 384).is_err());
    }

    #[test]
    fn an_empty_index_saves_loads_and_finds_nothing() {
        let idx = BinaryIndex::new(384);
        assert!(idx.is_empty() && idx.is_bootstrapping());
        assert_eq!(idx.dim(), 384);
        let mut buf = Vec::new();
        idx.write_to(&mut buf).unwrap();
        let back = BinaryIndex::read_from(&buf[..], 384).unwrap();
        assert!(back.is_empty());
        let q = fake().embed_query("anything").unwrap();
        assert!(back.candidates(&q, 5).is_empty());
        let (full, vs) = filled(BOOTSTRAP + 1);
        assert!(full.candidates(&vs[0], 0).is_empty(), "n = 0");
        assert!(full.candidates(&[0.0; 3], 5).is_empty(), "wrong dimension");
    }
}
