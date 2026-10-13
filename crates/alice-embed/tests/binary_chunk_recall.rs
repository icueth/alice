//! Recall of the sign-bit chunk index against exact cosine, with the real model, on text from
//! this repository (Rust code, English and Thai docs). For each query (the first 60% of a
//! passage) the ground truth is the exact cosine top 10 over every passage; recall@10 is the
//! share of it that `search_chunks_with` returns. Prints recall per oversampling factor (with
//! k = 10, `MIN_RESCORE` decides the candidate count up to a factor of 20, so the sweep goes
//! on to 1,600 candidates), and the share of the exact top 10 in the list the raw tier uses
//! (k = 100), and asserts recall@10 at `CHUNK_OVERSAMPLE`.
//!
//!   ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release \
//!     --test binary_chunk_recall -- --ignored --nocapture
//!
//! `ALICE_RECALL_PASSAGES=<n>` (default 6,000) and `ALICE_RECALL_EXTRA_DIR=<dir>` (more `.rs`
//! and `.md` files, read after the repository's) measure how recall changes with the number
//! of passages; the assertion is meant for the default.
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine};
use alice_core::store::embeddings::{self, VecKey};
use alice_core::vector::{CHUNK_OVERSAMPLE, MIN_RESCORE, VectorIndex};
use std::path::{Path, PathBuf};

const K: usize = 10;

fn models_dir() -> PathBuf {
    std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".alice/models")))
        .expect("set ALICE_MODELS_DIR")
}

/// Every `.md` and `.rs` file under `dir` (skipping `target`), sorted.
fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() && !p.ends_with("target") {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "md" || x == "rs") {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

/// Lines of 40..400 characters from the repo's docs and sources (then `extra`'s),
/// deduplicated, in file order.
fn passages(root: &Path, extra: Option<&Path>, max: usize) -> Vec<String> {
    let mut files = sources(&root.join("docs"));
    files.extend(sources(&root.join("crates")));
    files.push(root.join("README.md"));
    files.push(root.join("README.th.md"));
    files.sort();
    if let Some(extra) = extra {
        files.extend(sources(extra));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for f in files {
        for line in std::fs::read_to_string(&f).unwrap_or_default().lines() {
            let t = line.trim();
            let n = t.chars().count();
            if (40..=400).contains(&n) && seen.insert(t.to_string()) {
                out.push(t.to_string());
                if out.len() == max {
                    return out;
                }
            }
        }
    }
    out
}

#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn sign_bit_chunk_index_keeps_the_exact_top_ten() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let max = std::env::var("ALICE_RECALL_PASSAGES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(6_000);
    let extra = std::env::var_os("ALICE_RECALL_EXTRA_DIR").map(PathBuf::from);
    let texts = passages(&root, extra.as_deref(), max);
    assert!(texts.len() >= 3_000, "only {} passages", texts.len());
    let e = alice_embed::FastEmbedder::new(&models_dir()).unwrap();
    let model = e.model_id().to_string();
    let vectors: Vec<Vec<f32>> = texts
        .chunks(32)
        .flat_map(|b| e.embed_passages(b).unwrap())
        .collect();
    let d = tempfile::tempdir().unwrap();
    let c = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
    let idx = VectorIndex::for_model(&model, e.dim()).unwrap();
    for (i, v) in vectors.iter().enumerate() {
        let key = VecKey::Chunk(i as i64 + 1);
        embeddings::put(&c, key, &model, v).unwrap();
        idx.upsert(key, v).unwrap();
    }
    let queries: Vec<(String, Vec<f32>)> = texts
        .iter()
        .step_by((texts.len() / 300).max(1))
        .map(|t| {
            let cut: String = t.chars().take(t.chars().count() * 6 / 10).collect();
            let v = e.embed_query(&cut).unwrap();
            (cut, v)
        })
        .collect();
    // The exact top 10 of every query, once.
    let truth: Vec<Vec<i64>> = queries
        .iter()
        .map(|(_, q)| {
            let mut exact: Vec<(i64, f32)> = vectors
                .iter()
                .enumerate()
                .map(|(i, v)| (i as i64 + 1, cosine(q, v)))
                .collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1));
            exact[..K].iter().map(|(s, _)| *s).collect()
        })
        .collect();
    let mut report = String::new();
    let mut at_default = 0.0;
    for oversample in [1usize, 2, 4, 8, 16, 40, 80, 160] {
        let mut found = 0usize;
        for ((_, q), want) in queries.iter().zip(&truth) {
            let got: std::collections::HashSet<VecKey> = idx
                .search_chunks_with(&c, q, K, oversample)
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            found += want
                .iter()
                .filter(|s| got.contains(&VecKey::Chunk(**s)))
                .count();
        }
        let recall = found as f64 / (queries.len() * K) as f64;
        let candidates = (K * oversample).max(MIN_RESCORE);
        report.push_str(&format!(
            "oversample {oversample:>3} ({candidates:>4} candidates): recall@10 {recall:.4}\n"
        ));
        if oversample == CHUNK_OVERSAMPLE {
            at_default = recall;
        }
    }
    // What recall uses: the raw tier asks `search_chunks` for its 100 best (400 candidates).
    let mut found = 0usize;
    for ((_, q), want) in queries.iter().zip(&truth) {
        let got: std::collections::HashSet<VecKey> = idx
            .search_chunks(&c, q, 100)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        found += want
            .iter()
            .filter(|s| got.contains(&VecKey::Chunk(**s)))
            .count();
    }
    report.push_str(&format!(
        "raw tier list (k = 100, {} candidates): exact top 10 found {:.4}\n",
        (100 * CHUNK_OVERSAMPLE).max(MIN_RESCORE),
        found as f64 / (queries.len() * K) as f64
    ));
    println!(
        "{} passages, {} queries\n{report}",
        texts.len(),
        queries.len()
    );
    assert!(
        at_default >= 0.97,
        "recall@10 {at_default:.4} at CHUNK_OVERSAMPLE {CHUNK_OVERSAMPLE}"
    );
}
