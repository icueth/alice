//! Search quality with the real model on the gold set that `alice-core/tests/search_quality.rs`
//! runs with the fake embedder, plus unrelated queries that must take the raw fallback. It
//! reports MRR, the rank of every gold query, the cosine similarity of relevant and unrelated
//! query–memory pairs, and a sweep of `relevance_floor` values; it asserts the default floor.
//! On top of the shared gold set it has queries that share no word with their memory
//! (`VECTOR_ONLY`: a Thai query for an English memory and the reverse, and English paraphrases),
//! which only the vector list can find: they test the floor's lower edge, since a result below
//! the floor without a lexical match is not returned at all.
//!
//! Run with (reads the model, never downloads it; `alice model fetch` puts it in ~/.alice/models):
//!   ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release \
//!     --test real_search_quality -- --ignored --nocapture
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine, memory_embed_text};
use alice_core::model::Kind;
use alice_core::project::ProjectRef;
use alice_core::search::{
    DEFAULT_RELEVANCE_FLOOR, DEFAULT_RELEVANCE_MARGIN, SearchParams, fusion::Weights, run,
};
use alice_core::store::embeddings::VecKey;
use alice_core::store::{memories, open_writer, projects, sources};
use alice_core::vector::{KeyFilter, VectorIndex};
use alice_embed::{OnnxEmbedder, OnnxOptions};
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

/// A gold query that shares no word with its memory (checked: the hit's only match is the
/// vector list), with what retrieval does with it at the default floor today.
struct VectorOnly {
    what: &'static str,
    query: &'static str,
    /// Memory id from the gold file.
    want: &'static str,
    /// Measured with the default policy (floor 0.79, margin 0.03) and the int8 memory index on Apple Silicon: in the
    /// top 3 (true) or not returned at all (false: no lexical match, similarity below the floor).
    /// A change to the floor policy or to the model changes this column, and
    /// docs/benchmarks/m2.md with it. Rows within `PIN_MARGIN` of the floor are not pinned.
    returned_at_default_floor: bool,
}

const VECTOR_ONLY: &[VectorOnly] = &[
    VectorOnly {
        what: "Thai for English",
        query: "เราดีพลอยระบบจริงไปไว้ที่ไหน",
        want: "deploy",
        returned_at_default_floor: false,
    },
    VectorOnly {
        what: "Thai for English",
        query: "ควรตอบเจ้าของด้วยภาษาอะไร",
        want: "pref-thai",
        returned_at_default_floor: true,
    },
    VectorOnly {
        what: "Thai for English",
        query: "เก็บจำนวนเงินเป็นสตริงเพื่อไม่ให้ปัดเศษผิด",
        want: "money",
        returned_at_default_floor: true,
    },
    VectorOnly {
        what: "Thai for English",
        query: "ทำไมบันทึกไดอารี่ถึงหมดเวลา",
        want: "lease",
        returned_at_default_floor: false,
    },
    VectorOnly {
        what: "English for Thai",
        query: "agents' memory has to search Thai text that has no spaces between words",
        want: "thai-mem",
        returned_at_default_floor: true,
    },
    VectorOnly {
        what: "paraphrase",
        query: "a lone dedicated committer keeps lookups from stalling",
        want: "wal",
        returned_at_default_floor: false,
    },
    VectorOnly {
        what: "paraphrase",
        query: "the connection retried endlessly once the laptop woke up",
        want: "reconnect",
        returned_at_default_floor: true,
    },
    VectorOnly {
        what: "paraphrase",
        query: "currency amounts travel as text so decimals stay exact",
        want: "money",
        returned_at_default_floor: true,
    },
    VectorOnly {
        what: "paraphrase",
        query: "binary envelopes are mandatory; a text payload cannot be decoded",
        want: "proto",
        returned_at_default_floor: true,
    },
    VectorOnly {
        what: "paraphrase",
        query: "which host serves our live site",
        want: "deploy",
        returned_at_default_floor: false,
    },
];

/// With every vector candidate let through (floor −1), at least this many of `VECTOR_ONLY` rank
/// in the top 3: the ranking itself, whatever the floor decides.
const VECTOR_ONLY_TOP3_UNFLOORED: usize = 8;

/// The same model gives slightly different vectors on another CPU: ONNX Runtime's int8 kernels
/// differ between x86-64 and ARM, and on windows-latest the vector-only similarities moved by up
/// to 0.02 against Apple Silicon (`docs/benchmarks/m2.md`, Windows). A vector-only row this close
/// to the floor can land on either side of it, so its outcome is reported, not pinned; if it is
/// returned it must still be in the top 3.
const PIN_MARGIN: f32 = 0.02;

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
    /// Rank (1-based) of the wanted memory per gold query (the shared ones, then `VECTOR_ONLY`).
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
    let e = OnnxEmbedder::new(&dir, OnnxOptions::default()).unwrap();
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
    let shared_gold = gold.len();
    gold.extend(
        VECTOR_ONLY
            .iter()
            .map(|v| (v.query.to_string(), v.want.to_string())),
    );
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
    // The shared gold pairs (as M1 measured them) and the vector-only pairs, kept apart.
    let (mut relevant, mut irrelevant, mut unrelated_sims) = (Vec::new(), Vec::new(), Vec::new());
    let (mut relevant_i8, mut irrelevant_i8, mut unrelated_i8) =
        (Vec::new(), Vec::new(), Vec::new());
    // (exact, int8) per `VECTOR_ONLY` entry, in order.
    let mut vector_only_sims: Vec<(f32, f32)> = Vec::new();
    for (n, (q, want)) in gold.iter().enumerate() {
        let qv = e.embed_query(q).unwrap();
        let by_index = index_sims(&qv);
        for (key, mv) in &vectors {
            let (exact, i8) = (cosine(&qv, mv), by_index[&seq_of_key[key]]);
            match (key == want, n < shared_gold) {
                (true, true) => {
                    relevant.push(exact);
                    relevant_i8.push(i8);
                }
                (true, false) => vector_only_sims.push((exact, i8)),
                (false, true) => {
                    irrelevant.push(exact);
                    irrelevant_i8.push(i8);
                }
                (false, false) => {}
            }
        }
    }
    let vector_only_i8: Vec<f32> = vector_only_sims.iter().map(|(_, i8)| *i8).collect();
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
    println!("  queries sharing no word with their memory (exact / int8 index)");
    for ((exact, i8), v) in vector_only_sims.iter().zip(VECTOR_ONLY) {
        println!(
            "    {exact:.3} / {i8:.3}  {:<16} {:<10} {}",
            v.what, v.want, v.query
        );
    }
    let above = |v: &[f32], floor: f32| v.iter().filter(|s| **s >= floor).count();
    let params = |q: &str, floor: f32| SearchParams {
        query: q.to_string(),
        current_project_id: Some(p.id.clone()),
        limit: 10,
        relevance_floor: floor,
        // The shipped policy is the floor and the margin; "no floor" (a negative one) lets
        // every candidate through, the margin included.
        relevance_margin: if floor < 0.0 {
            0.0
        } else {
            DEFAULT_RELEVANCE_MARGIN
        },
        raw_min_results: 3,
        ..Default::default()
    };
    let search = |q: &str, floor: f32| {
        run(
            &c,
            Some(&idx),
            &e,
            &Weights::default(),
            &params(q, floor),
            chrono::Utc::now(),
        )
        .unwrap()
    };

    let evaluate = |floor: f32| -> Outcome {
        let ranks = gold
            .iter()
            .map(|(q, want)| {
                search(q, floor)
                    .hits
                    .iter()
                    .position(|h| h.id == ids[want])
                    .map(|i| i + 1)
            })
            .collect();
        let unrelated = UNRELATED
            .iter()
            .map(|(q, _)| {
                let out = search(q, floor);
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
    let mrr = |ranks: &[Option<usize>]| {
        ranks
            .iter()
            .map(|r| r.map(|r| 1.0 / r as f64).unwrap_or(0.0))
            .sum::<f64>()
            / ranks.len() as f64
    };
    let top3 = |ranks: &[Option<usize>]| ranks.iter().filter(|r| r.is_some_and(|r| r <= 3)).count();

    println!(
        "\nfloor  pairs >= floor (int8): gold  other  unrelated  no shared word   in top 3: gold  no shared word   MRR (gold)  fallback fired (knowledge results per unrelated query)"
    );
    for step in 0..=20 {
        let floor = 0.70 + step as f32 * 0.01;
        let o = evaluate(floor);
        let (shared, vector_only) = o.ranks.split_at(shared_gold);
        let fired = o.unrelated.iter().filter(|(_, f)| *f).count();
        let knowledge: Vec<usize> = o.unrelated.iter().map(|(k, _)| *k).collect();
        println!(
            "{floor:.2}                          {:>2}/{}  {:>3}/{}  {:>2}/{}      {:>2}/{}             {:>2}/{}  {:>2}/{}            {:.3}       {fired}/{}  {knowledge:?}",
            above(&relevant_i8, floor),
            relevant_i8.len(),
            above(&irrelevant_i8, floor),
            irrelevant_i8.len(),
            above(&unrelated_i8, floor),
            unrelated_i8.len(),
            above(&vector_only_i8, floor),
            vector_only_i8.len(),
            top3(shared),
            shared.len(),
            top3(vector_only),
            vector_only.len(),
            mrr(shared),
            o.unrelated.len()
        );
    }

    let floor = DEFAULT_RELEVANCE_FLOOR;
    let o = evaluate(floor);
    let (shared, vector_only) = o.ranks.split_at(shared_gold);
    println!("\nat the default floor {floor}:");
    for ((q, want), rank) in gold.iter().zip(&o.ranks) {
        println!("  {rank:?}\t{want}\t{q}");
    }
    println!(
        "  MRR {:.3} over the {} gold queries",
        mrr(shared),
        shared.len()
    );
    for ((q, want), rank) in gold.iter().zip(shared) {
        assert!(
            rank.is_some_and(|r| r <= 3),
            "{q:?}: expected {want} in the top 3, got {rank:?}"
        );
    }
    // Fixture check (not a retrieval check: with ten memories and floor −1 every memory is
    // returned): each vector-only query reaches its memory through the vector list alone, so it
    // really shares no word with it.
    for v in VECTOR_ONLY {
        let out = search(v.query, -1.0);
        let hit = out.hits.iter().find(|h| h.id == ids[v.want]);
        assert_eq!(
            hit.map(|h| h.matched.clone()),
            Some(vec![alice_core::api::MatchVia::Vector]),
            "{:?} must reach {} through the vector list only",
            v.query,
            v.want
        );
    }
    // The ranking, whatever the floor: with every candidate let through, most vector-only
    // queries put their memory in the top 3.
    let unfloored = evaluate(-1.0);
    let unfloored_top3 = top3(&unfloored.ranks[shared_gold..]);
    println!(
        "  vector-only queries in the top 3 with no floor: {unfloored_top3} of {}",
        VECTOR_ONLY.len()
    );
    assert!(
        unfloored_top3 >= VECTOR_ONLY_TOP3_UNFLOORED,
        "only {unfloored_top3} vector-only queries rank their memory in the top 3 without a floor"
    );
    // The floor's lower edge, pinned row by row: the rows marked returned are in the top 3, the
    // others are not returned at all. A change in either direction must update VECTOR_ONLY and
    // docs/benchmarks/m2.md. Rows within PIN_MARGIN of the floor are only reported.
    let mut below = Vec::new();
    for ((v, rank), sim) in VECTOR_ONLY.iter().zip(vector_only).zip(&vector_only_i8) {
        if (sim - floor).abs() < PIN_MARGIN {
            println!(
                "  near the floor, not pinned: {sim:.3} {}: {} -> {rank:?}",
                v.what, v.query
            );
            assert!(
                rank.is_none_or(|r| r <= 3),
                "{:?} ({sim:.3}) is returned but not in the top 3: {rank:?}",
                v.query
            );
            if rank.is_none() {
                below.push(format!("{sim:.3} {}: {}", v.what, v.query));
            }
            continue;
        }
        if v.returned_at_default_floor {
            assert!(
                rank.is_some_and(|r| r <= 3),
                "{:?} ({sim:.3}) should rank {} in the top 3 at the default floor, got {rank:?}",
                v.query,
                v.want
            );
        } else {
            assert_eq!(
                *rank, None,
                "{:?} ({sim:.3}) now returns {}: update VECTOR_ONLY and docs/benchmarks/m2.md",
                v.query, v.want
            );
            below.push(format!("{sim:.3} {}: {}", v.what, v.query));
        }
    }
    println!(
        "  vector-only pairs not returned at the floor {floor}: {} of {}",
        below.len(),
        VECTOR_ONLY.len()
    );
    for b in &below {
        println!("    {b}");
    }
    for ((q, _), (knowledge, fired)) in UNRELATED.iter().zip(&o.unrelated) {
        assert!(
            *fired,
            "{q:?}: the raw fallback did not fire ({knowledge} knowledge results counted as relevant)"
        );
    }
    assert!(mrr(shared) >= 0.9, "MRR {:.3} below 0.9", mrr(shared));
    // The floor separates the two populations as the index sees them: every gold memory would
    // count as relevant on similarity alone, no memory counts for an unrelated query.
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
