//! Search quality with the real model on the gold set that `alice-core/tests/search_quality.rs`
//! runs with the fake embedder, plus unrelated queries that must take the raw fallback. It
//! reports MRR, the rank of every gold query, the cosine similarity of relevant and unrelated
//! query–memory pairs, and a sweep of `relevance_floor` values; it asserts the default floor.
//!
//! Run with (reads the model, never downloads it; `alice model fetch` puts it in ~/.alice/models):
//!   ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release \
//!     --test real_search_quality -- --ignored --nocapture
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine, memory_embed_text};
use alice_core::model::Kind;
use alice_core::project::ProjectRef;
use alice_core::search::{DEFAULT_RELEVANCE_FLOOR, SearchParams, fusion::Weights, run};
use alice_core::store::embeddings::VecKey;
use alice_core::store::{memories, open_writer, projects, sources};
use alice_core::vector::{KeyFilter, VectorIndex};
use alice_embed::FastEmbedder;
use std::collections::HashMap;
use std::path::PathBuf;

/// Queries about nothing in the gold memories, each with a raw chunk that answers it.
const UNRELATED: &[(&str, &str)] = &[
    (
        "tom yum soup recipe",
        "we cooked tom yum soup with extra lemongrass for lunch",
    ),
    ("สูตรต้มยำกุ้งน้ำข้น", "แม่ส่งสูตรต้มยำกุ้งน้ำข้นมาให้ในแชทเมื่อวาน"),
    (
        "football world cup final score",
        "the world cup final score was 2-1 after extra time",
    ),
    (
        "hiking trails near Chiang Mai",
        "hiking trails near Chiang Mai are muddy in the rainy season",
    ),
    (
        "how to grow tomatoes on a balcony",
        "tomatoes on a balcony need six hours of sun and a deep pot",
    ),
    ("ราคาทองคำวันนี้", "ราคาทองคำวันนี้ขึ้นอีกสองร้อยบาท"),
];

fn models_dir() -> PathBuf {
    std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".alice").join("models")))
        .expect("set ALICE_MODELS_DIR to the directory `alice model fetch` filled")
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn stats(v: &mut [f32]) -> String {
    v.sort_by(f32::total_cmp);
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!(
        "n {:>3}  min {:.3}  p10 {:.3}  median {:.3}  p90 {:.3}  max {:.3}",
        v.len(),
        at(0.0),
        at(0.1),
        at(0.5),
        at(0.9),
        at(1.0)
    )
}

struct Outcome {
    /// Rank (1-based) of the wanted memory per gold query.
    ranks: Vec<Option<usize>>,
    /// Per unrelated query: knowledge results that counted as relevant, and whether the raw
    /// fallback fired.
    unrelated: Vec<(usize, bool)>,
}

#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn real_model_gold_set_and_relevance_floor() {
    let dir = models_dir();
    assert!(
        alice_embed::is_model_present(&dir),
        "no model in {}; run `alice model fetch` or set ALICE_MODELS_DIR",
        dir.display()
    );
    let e = FastEmbedder::new(&dir).unwrap();
    let d = tempfile::tempdir().unwrap();
    let c = open_writer(&d.path().join("a.db")).unwrap();
    let p = projects::upsert_project(
        &c,
        &ProjectRef {
            key: "k".into(),
            slug: "app".into(),
            name: "app".into(),
            root: None,
            remote: None,
        },
    )
    .unwrap();
    let idx = VectorIndex::new(e.dim()).unwrap();

    let mut ids: HashMap<String, String> = HashMap::new();
    let mut vectors: HashMap<String, Vec<f32>> = HashMap::new();
    let mut gold = Vec::new();
    for line in include_str!("../../alice-core/tests/fixtures/search_gold.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(text) = v["m"].as_str() {
            let m = memories::prepare(
                memories::NewMemoryInput {
                    content: text.into(),
                    kind: Some(Kind::parse(v["kind"].as_str().unwrap()).unwrap()),
                    ..Default::default()
                },
                &p,
                "h".into(),
            )
            .unwrap();
            let memories::InsertOutcome::New { id, seq } = memories::insert(&c, &m).unwrap() else {
                panic!("duplicate gold memory")
            };
            let vector = e
                .embed_passages(&[memory_embed_text(&m.title, &m.body)])
                .unwrap()
                .remove(0);
            idx.upsert(VecKey::Memory(seq), &vector).unwrap();
            let key = v["id"].as_str().unwrap().to_string();
            vectors.insert(key.clone(), vector);
            ids.insert(key, id);
        } else {
            gold.push((
                v["q"].as_str().unwrap().to_string(),
                v["want"].as_str().unwrap().to_string(),
            ));
        }
    }
    for (i, (_, chunk)) in UNRELATED.iter().enumerate() {
        let out = sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: format!("session-{i}"),
                ..Default::default()
            },
            &[sources::ChunkInput {
                ord: 0,
                text: chunk.to_string(),
                ..Default::default()
            }],
        )
        .unwrap();
        assert_eq!(out.inserted, 1);
    }

    // Similarity of every query against every memory: exact cosine, and as the int8 index
    // reports it (what the relevance floor is compared with at run time).
    let seq_of_key: HashMap<String, VecKey> = ids
        .iter()
        .map(|(key, id)| {
            let seq = memories::seq_of(&c, id).unwrap().unwrap();
            (key.clone(), VecKey::Memory(seq))
        })
        .collect();
    let index_sims = |qv: &[f32]| -> HashMap<VecKey, f32> {
        idx.search(qv, vectors.len(), KeyFilter::Memories)
            .unwrap()
            .into_iter()
            .collect()
    };
    let (mut relevant, mut irrelevant, mut unrelated_sims) = (Vec::new(), Vec::new(), Vec::new());
    let (mut relevant_i8, mut irrelevant_i8, mut unrelated_i8) =
        (Vec::new(), Vec::new(), Vec::new());
    for (q, want) in &gold {
        let qv = e.embed_query(q).unwrap();
        let by_index = index_sims(&qv);
        for (key, mv) in &vectors {
            let (exact, i8) = (cosine(&qv, mv), by_index[&seq_of_key[key]]);
            if key == want {
                relevant.push(exact);
                relevant_i8.push(i8);
            } else {
                irrelevant.push(exact);
                irrelevant_i8.push(i8);
            }
        }
    }
    for (q, _) in UNRELATED {
        let qv = e.embed_query(q).unwrap();
        unrelated_sims.extend(vectors.values().map(|mv| cosine(&qv, mv)));
        unrelated_i8.extend(index_sims(&qv).into_values());
    }
    println!("similarity, query vs memory (model {}):", e.model_id());
    println!("  exact cosine");
    println!("    gold query vs its memory     {}", stats(&mut relevant));
    println!(
        "    gold query vs other memories {}",
        stats(&mut irrelevant)
    );
    println!(
        "    unrelated query vs memories  {}",
        stats(&mut unrelated_sims)
    );
    println!("  int8 index");
    println!(
        "    gold query vs its memory     {}",
        stats(&mut relevant_i8)
    );
    println!(
        "    gold query vs other memories {}",
        stats(&mut irrelevant_i8)
    );
    println!(
        "    unrelated query vs memories  {}",
        stats(&mut unrelated_i8)
    );
    let above = |v: &[f32], floor: f32| v.iter().filter(|s| **s >= floor).count();

    let evaluate = |floor: f32| -> Outcome {
        let params = |q: &str| SearchParams {
            query: q.to_string(),
            current_project_id: Some(p.id.clone()),
            limit: 10,
            relevance_floor: floor,
            raw_min_results: 3,
            ..Default::default()
        };
        let search = |q: &str| {
            run(
                &c,
                Some(&idx),
                &e,
                &Weights::default(),
                &params(q),
                chrono::Utc::now(),
            )
            .unwrap()
        };
        let ranks = gold
            .iter()
            .map(|(q, want)| {
                search(q)
                    .hits
                    .iter()
                    .position(|h| h.id == ids[want])
                    .map(|i| i + 1)
            })
            .collect();
        let unrelated = UNRELATED
            .iter()
            .map(|(q, _)| {
                let out = search(q);
                let knowledge = out
                    .hits
                    .iter()
                    .filter(|h| h.tier == alice_core::api::Tier::Knowledge)
                    .count();
                (knowledge, out.raw_fallback)
            })
            .collect();
        Outcome { ranks, unrelated }
    };
    let mrr = |o: &Outcome| {
        o.ranks
            .iter()
            .map(|r| r.map(|r| 1.0 / r as f64).unwrap_or(0.0))
            .sum::<f64>()
            / o.ranks.len() as f64
    };

    println!(
        "\nfloor  pairs >= floor (int8): gold  other  unrelated   gold in top 3  MRR    fallback fired (knowledge results per unrelated query)"
    );
    for step in 0..=20 {
        let floor = 0.75 + step as f32 * 0.01;
        let o = evaluate(floor);
        let top3 = o.ranks.iter().filter(|r| r.is_some_and(|r| r <= 3)).count();
        let fired = o.unrelated.iter().filter(|(_, f)| *f).count();
        let knowledge: Vec<usize> = o.unrelated.iter().map(|(k, _)| *k).collect();
        println!(
            "{floor:.2}                          {:>2}/{}  {:>3}/{}  {:>2}/{}       {top3:>2}/{}          {:.3}  {fired}/{}  {knowledge:?}",
            above(&relevant_i8, floor),
            relevant_i8.len(),
            above(&irrelevant_i8, floor),
            irrelevant_i8.len(),
            above(&unrelated_i8, floor),
            unrelated_i8.len(),
            o.ranks.len(),
            mrr(&o),
            o.unrelated.len()
        );
    }

    let o = evaluate(DEFAULT_RELEVANCE_FLOOR);
    println!("\nat the default floor {DEFAULT_RELEVANCE_FLOOR}:");
    for ((q, want), rank) in gold.iter().zip(&o.ranks) {
        println!("  {rank:?}\t{want}\t{q}");
    }
    println!("  MRR {:.3} over {} queries", mrr(&o), o.ranks.len());
    for ((q, want), rank) in gold.iter().zip(&o.ranks) {
        assert!(
            rank.is_some_and(|r| r <= 3),
            "{q:?}: expected {want} in the top 3, got {rank:?}"
        );
    }
    for ((q, _), (knowledge, fired)) in UNRELATED.iter().zip(&o.unrelated) {
        assert!(
            *fired,
            "{q:?}: the raw fallback did not fire ({knowledge} knowledge results counted as relevant)"
        );
    }
    assert!(mrr(&o) >= 0.9, "MRR {:.3} below 0.9", mrr(&o));
    // The floor separates the two populations as the index sees them: every gold memory would
    // count as relevant on similarity alone, no memory counts for an unrelated query.
    let floor = DEFAULT_RELEVANCE_FLOOR;
    assert_eq!(
        above(&relevant_i8, floor),
        relevant_i8.len(),
        "a gold pair below {floor}"
    );
    assert_eq!(
        above(&unrelated_i8, floor),
        0,
        "an unrelated pair at or above {floor}"
    );
}
