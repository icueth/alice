//! Flat sign-bit index for raw chunks.
//!
//! Each vector is kept as one bit per dimension: whether it lies above a mean vector. Centring
//! matters: e5 vectors share a large common direction (unrelated texts sit at cosine ~0.7),
//! so uncentred sign bits would be nearly constant. The vector's own coordinates are used, not
//! signs of random projections: with the real model they kept more of the exact top 10
//! (`docs/benchmarks/m2.md`).
//!
//! A search ([`BinaryIndex::nearest`]) ranks in RAM in two steps: every code by Hamming
//! distance (popcount of XOR) down to a shortlist, then the shortlist re-scored against the
//! float query, which keeps the query's magnitudes that the Hamming distance throws away. The
//! caller reads the best few of those from SQLite and rescores them exactly
//! (`VectorIndex::search_chunks`).
//!
//! The mean: an index filled by inserts keeps its first `BOOTSTRAP` vectors as f32 (searched
//! exactly) and then fixes the mean of those for the life of the index. A rebuild
//! ([`BinaryIndex::bulk`]) is given a mean fitted on a sample spread over the whole corpus and
//! sorts its codes by seq once.
//!
//! Memory: 48 bytes of code and 8 bytes of key per 384-dimension vector (~56 MB at 1M chunks),
//! against ~560 MB for the int8 HNSW it replaces. Each search scans the codes twice (~48 MB
//! each at 1M) and keeps only its shortlist.
//!
//! File format (little-endian): magic `ALICEBQ1`, dimension (u32), state (u32: 1 while
//! bootstrapping), count (u64); then either `count` × (seq i64, `dim` × f32), or the mean
//! (`dim` × f32), `count` sorted seqs (i64) and `count` codes (`dim / 64` rounded up × u64).

use crate::error::{AliceError, Result};
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

/// The mean of `vectors` (all of dimension `dim`), or `None` when there are none.
pub fn mean_of<V: AsRef<[f32]>>(dim: usize, vectors: &[V]) -> Option<Vec<f32>> {
    if vectors.is_empty() {
        return None;
    }
    let mut mean = vec![0f64; dim];
    for v in vectors {
        for (m, x) in mean.iter_mut().zip(v.as_ref()) {
            *m += f64::from(*x);
        }
    }
    let n = vectors.len() as f64;
    Some(mean.into_iter().map(|m| (m / n) as f32).collect())
}

/// An index built in one go (see [`BinaryIndex::bulk`]): codes are appended in arrival order
/// and sorted by seq once, in [`BulkBuild::finish`], instead of one insertion each.
pub struct BulkBuild {
    idx: BinaryIndex,
    sorted: bool,
}

impl BulkBuild {
    pub fn push(&mut self, seq: i64, v: &[f32]) -> Result<()> {
        let idx = &mut self.idx;
        if v.len() != idx.dim {
            return Err(AliceError::invalid(format!(
                "vector has {} dimensions; the index uses {}",
                v.len(),
                idx.dim
            )));
        }
        if seq < 0 {
            return Err(AliceError::invalid(format!("chunk seq {seq} is negative")));
        }
        let Some(mean) = idx.mean.as_deref() else {
            return Err(AliceError::Other("a bulk build has a mean".into()));
        };
        let code = encode(mean, idx.words, v);
        if idx.keys.last().is_some_and(|&last| last >= seq) {
            self.sorted = false;
        }
        idx.keys.push(seq);
        idx.codes.extend_from_slice(&code);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.idx.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sort by seq (a seq pushed twice keeps its last vector) and hand over the index.
    pub fn finish(self) -> BinaryIndex {
        let mut idx = self.idx;
        if self.sorted {
            return idx;
        }
        let w = idx.words;
        let mut order: Vec<usize> = (0..idx.keys.len()).collect();
        // Stable: among equal seqs the later push stays later, and is the one kept.
        order.sort_by_key(|&i| idx.keys[i]);
        let mut keys = Vec::with_capacity(order.len());
        let mut codes = Vec::with_capacity(order.len() * w);
        for (j, &i) in order.iter().enumerate() {
            if order
                .get(j + 1)
                .is_some_and(|&next| idx.keys[next] == idx.keys[i])
            {
                continue;
            }
            keys.push(idx.keys[i]);
            codes.extend_from_slice(&idx.codes[i * w..(i + 1) * w]);
        }
        idx.keys = keys;
        idx.codes = codes;
        idx
    }
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

    /// The mean the codes are centred on (`None` while bootstrapping).
    pub fn mean(&self) -> Option<&[f32]> {
        self.mean.as_deref()
    }

    /// While bootstrapping: the `n` nearest by exact cosine, ties to the older seq.
    fn exact(&self, query: &[f32], n: usize) -> Vec<(i64, f32)> {
        let mut all: Vec<(i64, f32)> = self
            .boot
            .iter()
            .map(|(s, v)| (*s, crate::embed::cosine(query, v)))
            .collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(n);
        all
    }

    /// Positions of the `n` codes nearest to `q` by Hamming distance, with the distance,
    /// nearest first; ties go to the older seq (the lower position). Two scans and no heap:
    /// the first counts the codes at each distance, which fixes the cut; the second collects
    /// every code under the cut and, in seq order, as many at the cut as are still wanted.
    /// Linear in the number of codes whatever `n` is (a shortlist runs to tens of thousands).
    fn hamming_top(&self, q: &[u64], n: usize) -> Vec<(u32, usize)> {
        let distance =
            |code: &[u64]| -> u32 { code.iter().zip(q).map(|(a, b)| (a ^ b).count_ones()).sum() };
        let codes = self.codes.chunks_exact(self.words);
        let mut at = vec![0usize; self.words * 64 + 1];
        for code in codes.clone() {
            at[distance(code) as usize] += 1;
        }
        let (mut cut, mut nearer) = (0usize, 0usize);
        while cut + 1 < at.len() && nearer + at[cut] < n {
            nearer += at[cut];
            cut += 1;
        }
        let mut at_cut = n.saturating_sub(nearer);
        let mut out = Vec::with_capacity(n.min(self.keys.len()));
        for (i, code) in codes.enumerate() {
            let d = distance(code);
            if (d as usize) < cut {
                out.push((d, i));
            } else if d as usize == cut && at_cut > 0 {
                out.push((d, i));
                at_cut -= 1;
            }
        }
        out.sort_unstable();
        out
    }

    /// Up to `n` seqs nearest to `query` by Hamming distance alone, best first, with an
    /// estimated cosine similarity (`cos(π · hamming / dim)`, for ordering only; exact while
    /// bootstrapping). Ties go to the older seq. A query of the wrong dimension finds nothing.
    pub fn candidates(&self, query: &[f32], n: usize) -> Vec<(i64, f32)> {
        if n == 0 || query.len() != self.dim {
            return Vec::new();
        }
        let Some(mean) = self.mean.as_deref() else {
            return self.exact(query, n);
        };
        let q = encode(mean, self.words, query);
        self.hamming_top(&q, n)
            .into_iter()
            .map(|(d, i)| {
                let est = (std::f32::consts::PI * d as f32 / self.dim as f32).cos();
                (self.keys[i], est)
            })
            .collect()
    }

    /// Up to `n` seqs ranked in RAM, best first: the `shortlist` nearest codes by Hamming
    /// distance, re-scored against the float query as `Σ (q_i − mean_i) · (bit_i ? 1 : −1)`
    /// divided by `Σ |q_i − mean_i|` (1 when every bit lies on the query's side of the mean; an
    /// estimate for ordering only). The query keeps its magnitudes, which the Hamming distance
    /// throws away, so this order is much closer to the cosine order. Ties go to the older seq.
    /// Exact cosine while bootstrapping; a query of the wrong dimension finds nothing.
    pub fn nearest(&self, query: &[f32], shortlist: usize, n: usize) -> Vec<(i64, f32)> {
        if n == 0 || query.len() != self.dim {
            return Vec::new();
        }
        let Some(mean) = self.mean.as_deref() else {
            return self.exact(query, n);
        };
        let q = encode(mean, self.words, query);
        let short = self.hamming_top(&q, shortlist.max(n));
        // Per code byte, the sum of the centred query over the bits set in that byte value.
        let centred: Vec<f32> = query.iter().zip(mean).map(|(x, m)| x - m).collect();
        let bytes = self.words * 8;
        let mut table = vec![0f32; bytes * 256];
        for b in 0..bytes {
            for value in 1..256usize {
                let low = b * 8 + value.trailing_zeros() as usize;
                table[b * 256 + value] = table[b * 256 + (value & (value - 1))]
                    + centred.get(low).copied().unwrap_or(0.0);
            }
        }
        let total: f32 = centred.iter().sum();
        let l1: f32 = centred
            .iter()
            .map(|x| x.abs())
            .sum::<f32>()
            .max(f32::MIN_POSITIVE);
        let w = self.words;
        let mut scored: Vec<(i64, f32)> = short
            .into_iter()
            .map(|(_, i)| {
                let set: f32 = self.codes[i * w..(i + 1) * w]
                    .iter()
                    .flat_map(|word| word.to_le_bytes())
                    .enumerate()
                    .map(|(b, byte)| table[b * 256 + byte as usize])
                    .sum();
                (self.keys[i], (2.0 * set - total) / l1)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(n);
        scored
    }

    /// Start a bulk build (a rebuild) with a mean fitted beforehand, e.g. by [`mean_of`] on a
    /// sample of the whole corpus. Fails for a mean of another dimension.
    pub fn bulk(dim: usize, mean: Vec<f32>) -> Result<BulkBuild> {
        if mean.len() != dim {
            return Err(AliceError::invalid(format!(
                "the mean has {} dimensions; the index uses {dim}",
                mean.len()
            )));
        }
        let mut idx = BinaryIndex::new(dim);
        idx.mean = Some(mean);
        Ok(BulkBuild { idx, sorted: true })
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
    use crate::embed::cosine;

    const DIM: usize = 384;
    const CLUSTERS: usize = 60;

    /// Seeded dense vectors shaped like a sentence model's: every vector leans 0.9 towards one
    /// shared all-positive direction (vectors of different clusters sit at cosine ~0.81, as
    /// unrelated e5 texts do at ~0.7), then 60 clusters with noise. Uncentred sign bits are
    /// nearly all 1 here; only centring makes them informative.
    struct Fixture {
        state: u64,
        centres: Vec<Vec<f32>>,
    }

    fn normalize(mut v: Vec<f32>) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    impl Fixture {
        fn new(seed: u64) -> Self {
            let mut f = Self {
                state: seed,
                centres: Vec::new(),
            };
            f.centres = (0..CLUSTERS).map(|_| normalize(f.gaussians())).collect();
            f
        }

        fn uniform(&mut self) -> f64 {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            (self.state >> 11) as f64 / (1u64 << 53) as f64
        }

        fn gaussians(&mut self) -> Vec<f32> {
            (0..DIM)
                .map(|_| {
                    let (a, b) = (self.uniform().max(1e-12), self.uniform());
                    ((-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()) as f32
                })
                .collect()
        }

        /// A vector of `cluster`: normalize(0.9·shared + 0.44·normalize(centre + 0.7·noise)).
        fn point(&mut self, cluster: usize) -> Vec<f32> {
            let noise = self.gaussians();
            let scale = 0.7 / (DIM as f32).sqrt();
            let own = normalize(
                self.centres[cluster % CLUSTERS]
                    .iter()
                    .zip(&noise)
                    .map(|(c, z)| c + scale * z)
                    .collect(),
            );
            let shared = 1.0 / (DIM as f32).sqrt();
            let (a, b) = (0.9f32, (1.0f32 - 0.81).sqrt());
            normalize(own.iter().map(|x| a * shared + b * x).collect())
        }
    }

    /// `n` fixture vectors under seqs 1..=n (vector i in cluster i % 60).
    fn filled(n: usize) -> (BinaryIndex, Vec<Vec<f32>>) {
        let mut f = Fixture::new(0x05ee_d0fa_11ce);
        let mut idx = BinaryIndex::new(DIM);
        let mut vs = Vec::new();
        for i in 0..n {
            let v = f.point(i);
            idx.upsert(i as i64 + 1, &v).unwrap();
            vs.push(v);
        }
        (idx, vs)
    }

    /// Share of each query's exact top 10 (by cosine) that `found` returns.
    fn recall(vs: &[Vec<f32>], queries: &[Vec<f32>], found: impl Fn(&[f32]) -> Vec<i64>) -> f64 {
        let mut hits = 0;
        for q in queries {
            let mut exact: Vec<(usize, f32)> = vs
                .iter()
                .enumerate()
                .map(|(i, v)| (i, cosine(q, v)))
                .collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1));
            let got: std::collections::HashSet<i64> = found(q).into_iter().collect();
            hits += exact[..10]
                .iter()
                .filter(|(i, _)| got.contains(&(*i as i64 + 1)))
                .count();
        }
        hits as f64 / (queries.len() * 10) as f64
    }

    fn queries(n: usize) -> Vec<Vec<f32>> {
        let mut f = Fixture::new(0x05ee_d0fa_11ce);
        f.state ^= 0x9e37_79b9_7f4a_7c15; // same clusters, other noise
        (0..n).map(|q| f.point(q * 7)).collect()
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

    /// Without centring (bits against 0 instead of the mean) this fixture keeps 0.45 of the
    /// exact top 10 within 200 candidates: the shared direction sets almost every bit.
    #[test]
    fn candidates_contain_the_exact_top_ten_for_most_queries() {
        let (idx, vs) = filled(6_000);
        let r = recall(&vs, &queries(50), |q| {
            idx.candidates(q, 200).into_iter().map(|(s, _)| s).collect()
        });
        assert!(
            r >= 0.95,
            "recall of the exact top 10 within 200 candidates: {r:.3}"
        );
    }

    /// The shortlist is re-ranked against the float query, so a list of a given length holds
    /// more of the exact top 10 than the Hamming list of that length. Measured on this
    /// fixture (whose clusters of 100 differ only by isotropic noise, the hard case): top 20 /
    /// 30 / 50 / 80 hold 0.58 / 0.72 / 0.88 / 0.98 re-ranked against 0.47 / 0.62 / 0.82 / 0.96.
    #[test]
    fn the_float_query_reranks_the_hamming_shortlist() {
        let (idx, vs) = filled(6_000);
        let qs = queries(50);
        let hamming = recall(&vs, &qs, |q| {
            idx.candidates(q, 30).into_iter().map(|(s, _)| s).collect()
        });
        let reranked = recall(&vs, &qs, |q| {
            idx.nearest(q, 1_000, 30)
                .into_iter()
                .map(|(s, _)| s)
                .collect()
        });
        assert!(
            reranked >= hamming + 0.05,
            "top 30 by re-rank {reranked:.3}, by Hamming {hamming:.3}"
        );
        let wide = recall(&vs, &qs, |q| {
            idx.nearest(q, 1_000, 100)
                .into_iter()
                .map(|(s, _)| s)
                .collect()
        });
        assert!(wide >= 0.95, "top 100 by re-rank {wide:.3}");
        let best = idx.nearest(&vs[123], 1_000, 5);
        assert_eq!(best[0].0, 124, "a stored vector ranks itself first");
        assert!(best.windows(2).all(|w| w[0].1 >= w[1].1), "best first");
        assert!(best[0].1 > 0.0 && best[0].1 <= 1.0);
        assert!(idx.nearest(&vs[0], 1_000, 0).is_empty());
        assert!(idx.nearest(&[0.0; 3], 1_000, 5).is_empty());
        // While bootstrapping the ranking is exact cosine.
        let (small, vs) = filled(500);
        let exact = small.nearest(&vs[9], 1_000, 3);
        assert_eq!(exact[0].0, 10);
        assert!((exact[0].1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn upsert_replaces_inserts_out_of_order_and_remove_forgets() {
        let (mut idx, vs) = filled(BOOTSTRAP + 100);
        let other = Fixture::new(42).point(3);
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

    /// A rebuild: the mean is given, vectors arrive in any order (with a seq repeated: the last
    /// one wins) and are sorted once. The result equals a build from sorted input.
    #[test]
    fn a_bulk_build_sorts_once_and_matches_a_sorted_build() {
        let (_, vs) = filled(BOOTSTRAP + 300);
        let mean = mean_of(DIM, &vs[..1_000]).unwrap();
        let mut order: Vec<usize> = (0..vs.len()).collect();
        // A deterministic shuffle.
        for i in (1..order.len()).rev() {
            order.swap(i, (i * 7_919 + 13) % (i + 1));
        }
        let mut shuffled = BinaryIndex::bulk(DIM, mean.clone()).unwrap();
        shuffled.push(17, &vs[0]).unwrap(); // replaced below by the real vector of seq 17
        for &i in &order {
            shuffled.push(i as i64 + 1, &vs[i]).unwrap();
        }
        assert!(shuffled.push(-3, &vs[0]).is_err(), "negative seq");
        assert!(shuffled.push(3, &[0.0; 5]).is_err(), "wrong dimension");
        let shuffled = shuffled.finish();
        let mut sorted = BinaryIndex::bulk(DIM, mean.clone()).unwrap();
        for (i, v) in vs.iter().enumerate() {
            sorted.push(i as i64 + 1, v).unwrap();
        }
        let sorted = sorted.finish();
        assert_eq!(shuffled.len(), vs.len());
        assert!(!shuffled.is_bootstrapping());
        assert_eq!(shuffled.mean(), Some(&mean[..]));
        let bytes = |i: &BinaryIndex| {
            let mut b = Vec::new();
            i.write_to(&mut b).unwrap();
            b
        };
        assert_eq!(bytes(&shuffled), bytes(&sorted));
        assert_eq!(shuffled.candidates(&vs[16], 1)[0].0, 17);
        assert!(
            BinaryIndex::bulk(DIM, vec![0.0; 7]).is_err(),
            "mean of another dimension"
        );
        assert!(mean_of(DIM, &[] as &[Vec<f32>]).is_none());
    }

    #[test]
    fn save_and_load_round_trip_in_both_states_and_reject_damage() {
        for n in [100, BOOTSTRAP + 50] {
            let (idx, vs) = filled(n);
            let mut buf = Vec::new();
            idx.write_to(&mut buf).unwrap();
            let back = BinaryIndex::read_from(&buf[..], DIM).unwrap();
            assert_eq!(back.len(), n);
            assert_eq!(back.is_bootstrapping(), idx.is_bootstrapping());
            assert_eq!(back.candidates(&vs[42], 3), idx.candidates(&vs[42], 3));
            assert!(
                BinaryIndex::read_from(&buf[..buf.len() - 9], DIM).is_err(),
                "truncated"
            );
            assert!(
                BinaryIndex::read_from(&buf[..], 768).is_err(),
                "other dimension"
            );
            let mut bad = buf.clone();
            bad[0] = b'X';
            assert!(BinaryIndex::read_from(&bad[..], DIM).is_err(), "bad magic");
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
                    BinaryIndex::read_from(&bad[..], DIM).is_err(),
                    "n {n}, count {count}"
                );
            }
            let mut extra = buf.clone();
            extra.push(0);
            assert!(
                BinaryIndex::read_from(&extra[..], DIM).is_err(),
                "trailing bytes"
            );
        }
        // Two keys swapped in a coded file: the sorted order is what search and upsert rely on.
        let (idx, _) = filled(BOOTSTRAP + 50);
        let mut buf = Vec::new();
        idx.write_to(&mut buf).unwrap();
        let keys_at = COUNT_AT + 8 + DIM * 4;
        let mut bad = buf.clone();
        bad[keys_at..keys_at + 8].copy_from_slice(&buf[keys_at + 8..keys_at + 16]);
        bad[keys_at + 8..keys_at + 16].copy_from_slice(&buf[keys_at..keys_at + 8]);
        assert!(
            BinaryIndex::read_from(&bad[..], DIM).is_err(),
            "keys out of order"
        );
        // A bootstrapping file can never hold BOOTSTRAP vectors: the mean is fixed at that point.
        let mut boot = Vec::new();
        boot.extend_from_slice(b"ALICEBQ1");
        boot.extend_from_slice(&(DIM as u32).to_le_bytes());
        boot.extend_from_slice(&1u32.to_le_bytes());
        boot.extend_from_slice(&(BOOTSTRAP as u64).to_le_bytes());
        assert!(BinaryIndex::read_from(&boot[..], DIM).is_err());
    }

    #[test]
    fn an_empty_index_saves_loads_and_finds_nothing() {
        let idx = BinaryIndex::new(DIM);
        assert!(idx.is_empty() && idx.is_bootstrapping());
        assert_eq!(idx.dim(), DIM);
        assert_eq!(idx.mean(), None);
        let mut buf = Vec::new();
        idx.write_to(&mut buf).unwrap();
        let back = BinaryIndex::read_from(&buf[..], DIM).unwrap();
        assert!(back.is_empty());
        let q = Fixture::new(7).point(0);
        assert!(back.candidates(&q, 5).is_empty());
        assert!(back.nearest(&q, 100, 5).is_empty());
        let (full, vs) = filled(BOOTSTRAP + 1);
        assert!(full.candidates(&vs[0], 0).is_empty(), "n = 0");
        assert!(full.candidates(&[0.0; 3], 5).is_empty(), "wrong dimension");
    }
}
