//! Synthetic multilingual gold set. Uses the fake embedder, so it measures the lexical
//! side and the fusion logic; the real-model run lives in `alice bench --quality`.

use alice_core::embed::{Embedder, FakeEmbedder, memory_embed_text};
use alice_core::model::Kind;
use alice_core::project::ProjectRef;
use alice_core::search::{SearchParams, fusion::Weights, run};
use alice_core::store::embeddings::VecKey;
use alice_core::store::{memories, open_writer, projects};
use alice_core::vector::VectorIndex;
use std::collections::HashMap;

#[test]
fn gold_queries_rank_the_expected_memory_high() {
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
    let e = FakeEmbedder::new(384);
    let idx = VectorIndex::new(384).unwrap();
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut queries = Vec::new();
    for line in include_str!("fixtures/search_gold.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(text) = v["m"].as_str() {
            let kind = Kind::parse(v["kind"].as_str().unwrap()).unwrap();
            let m = memories::prepare(
                memories::NewMemoryInput {
                    content: text.into(),
                    kind: Some(kind),
                    ..Default::default()
                },
                &p,
                "h".into(),
            )
            .unwrap();
            let memories::InsertOutcome::New { id, seq } = memories::insert(&c, &m).unwrap() else {
                panic!()
            };
            idx.upsert(
                VecKey::Memory(seq),
                &e.embed_passages(&[memory_embed_text(&m.title, &m.body)])
                    .unwrap()[0],
            )
            .unwrap();
            ids.insert(v["id"].as_str().unwrap().into(), id);
        } else {
            queries.push((
                v["q"].as_str().unwrap().to_string(),
                v["want"].as_str().unwrap().to_string(),
            ));
        }
    }
    assert!(queries.len() >= 13);
    let mut reciprocal_ranks = Vec::new();
    let mut report = String::new();
    for (q, want) in &queries {
        let params = SearchParams {
            query: q.clone(),
            current_project_id: Some(p.id.clone()),
            limit: 10,
            relevance_floor: 0.82,
            raw_min_results: 0,
            ..Default::default()
        };
        let out = run(
            &c,
            Some(&idx),
            &e,
            &Weights::default(),
            &params,
            chrono::Utc::now(),
        )
        .unwrap();
        let rank = out
            .hits
            .iter()
            .position(|h| h.id == ids[want])
            .map(|i| i + 1);
        report.push_str(&format!("{rank:?}\t{want}\t{q}\n"));
        reciprocal_ranks.push(rank.map(|r| 1.0 / r as f64).unwrap_or(0.0));
        assert!(
            rank.is_some_and(|r| r <= 3),
            "query {q:?}: expected {want} in top 3, got rank {rank:?}"
        );
    }
    let mrr = reciprocal_ranks.iter().sum::<f64>() / reciprocal_ranks.len() as f64;
    println!(
        "{report}MRR {mrr:.3} over {} queries",
        reciprocal_ranks.len()
    );
    assert!(mrr >= 0.8, "MRR {mrr:.2} below 0.8");
}
