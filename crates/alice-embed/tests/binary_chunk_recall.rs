//! Recall of the sign-bit chunk index against exact cosine, with the real model. Passages are
//! lines of 40..400 characters from this repository (Rust code, English and Thai docs) and,
//! with `ALICE_RECALL_EXTRA_DIR`, from the `.rs` and `.md` files under that directory. For each
//! corpus size the queries are the first 60% of every (size / 300)th passage, the ground truth
//! is the exact cosine top 10 over the corpus, and recall@10 is the share of it that
//! `search_chunks(k = 10)` returns. Both states of a real index are measured: built by
//! inserts (live: mean of the first 4,096 vectors) and rebuilt from the table (mean of a
//! sample spread over all of them). Asserts recall@10 >= 0.97 at every size.
//!
//! Smoke run, 6,000 passages of this repository:
//!
//!   ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release \
//!     --test binary_chunk_recall -- --ignored --nocapture
//!
//! The gate at scale, 6,000 / 24,000 / 96,000 passages (`ALICE_RECALL_PASSAGES` raises the
//! largest; it must be at least 96,000):
//!
//!   ALICE_RECALL_EXTRA_DIR=~/.cargo/registry/src ALICE_MODELS_DIR=~/.alice/models \
//!     cargo test -p alice-embed --release --test binary_chunk_recall -- --ignored --nocapture
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine};
use alice_core::store::embeddings::{self, VecKey};
use alice_core::vector::{MIN_RESCORE, VectorIndex, chunk_shortlist};
use std::path::{Path, PathBuf};

const K: usize = 10;
const SMOKE: usize = 6_000;
const AT_SCALE: usize = 96_000;
const QUERIES: usize = 300;

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

/// Share of each query's exact top 10 that `found` returns.
fn recall(truth: &[Vec<i64>], queries: &[Vec<f32>], found: impl Fn(&[f32]) -> Vec<VecKey>) -> f64 {
    let mut hits = 0usize;
    for (q, want) in queries.iter().zip(truth) {
        let got: std::collections::HashSet<VecKey> = found(q).into_iter().collect();
        hits += want
            .iter()
            .filter(|s| got.contains(&VecKey::Chunk(**s)))
            .count();
    }
    hits as f64 / (queries.len() * K) as f64
}

#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn sign_bit_chunk_index_keeps_the_exact_top_ten() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let extra = std::env::var_os("ALICE_RECALL_EXTRA_DIR").map(PathBuf::from);
    let largest = match &extra {
        None => SMOKE,
        Some(_) => std::env::var("ALICE_RECALL_PASSAGES")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(AT_SCALE),
    };
    assert!(
        extra.is_none() || largest >= AT_SCALE,
        "the gate at scale needs at least {AT_SCALE} passages"
    );
    let texts = passages(&root, extra.as_deref(), largest);
    let enough = if extra.is_some() { largest } else { 3_000 };
    assert!(
        texts.len() >= enough,
        "only {} passages ({enough} needed)",
        texts.len()
    );
    let e = alice_embed::FastEmbedder::new(&models_dir()).unwrap();
    let model = e.model_id().to_string();
    let vectors: Vec<Vec<f32>> = texts
        .chunks(32)
        .flat_map(|b| e.embed_passages(b).unwrap())
        .collect();
    let sizes: Vec<usize> = match extra {
        None => vec![texts.len()],
        Some(_) => vec![SMOKE, 24_000, texts.len()],
    };
    let mut report = String::new();
    let mut failures = Vec::new();
    for n in sizes {
        let corpus = &vectors[..n];
        let d = tempfile::tempdir().unwrap();
        let c = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
        let live = VectorIndex::for_model(&model, e.dim()).unwrap();
        for (i, v) in corpus.iter().enumerate() {
            let key = VecKey::Chunk(i as i64 + 1);
            embeddings::put(&c, key, &model, v).unwrap();
            live.upsert(key, v).unwrap();
        }
        let rebuilt = VectorIndex::rebuild_from(&c, &model, e.dim()).unwrap();
        let queries: Vec<Vec<f32>> = texts[..n]
            .iter()
            .step_by((n / QUERIES).max(1))
            .map(|t| {
                let cut: String = t.chars().take(t.chars().count() * 6 / 10).collect();
                e.embed_query(&cut).unwrap()
            })
            .collect();
        let truth: Vec<Vec<i64>> = queries
            .iter()
            .map(|q| {
                let mut exact: Vec<(i64, f32)> = corpus
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i as i64 + 1, cosine(q, v)))
                    .collect();
                exact.sort_by(|a, b| b.1.total_cmp(&a.1));
                exact[..K].iter().map(|(s, _)| *s).collect()
            })
            .collect();
        let keys = |hits: Vec<(VecKey, f32)>| hits.into_iter().map(|(k, _)| k).collect();
        report.push_str(&format!(
            "{n} passages, {} queries, shortlist {}, {MIN_RESCORE} rows read\n",
            queries.len(),
            chunk_shortlist(n)
        ));
        for (state, idx) in [("live", &live), ("rebuilt", &rebuilt)] {
            let at_default = recall(&truth, &queries, |q| {
                keys(idx.search_chunks(&c, q, K).unwrap())
            });
            let mut line = format!("  {state:>7}: recall@10 {at_default:.4}");
            for rows in [100usize, 200, 400, 800] {
                let r = recall(&truth, &queries, |q| {
                    keys(idx.search_chunks_with(&c, q, K, None, rows).unwrap())
                });
                line.push_str(&format!(" · {rows} rows {r:.4}"));
            }
            let hamming_only = recall(&truth, &queries, |q| {
                keys(
                    idx.search_chunks_with(&c, q, K, Some(MIN_RESCORE), MIN_RESCORE)
                        .unwrap(),
                )
            });
            line.push_str(&format!(" · no re-rank {hamming_only:.4}"));
            // What recall uses: the raw tier asks `search_chunks` for its 100 best.
            let raw_tier = recall(&truth, &queries, |q| {
                keys(idx.search_chunks(&c, q, 100).unwrap())
            });
            line.push_str(&format!(" · raw tier list (k = 100) {raw_tier:.4}\n"));
            report.push_str(&line);
            if at_default < 0.97 {
                failures.push(format!("{n} passages, {state}: {at_default:.4}"));
            }
        }
    }
    println!("{report}");
    assert!(failures.is_empty(), "recall@10 below 0.97: {failures:?}");
}
