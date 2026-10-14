//! Search-quality comparison against MemPalace (spec section 12, method of section 10).
//!
//! This file starts with the consistency check of the frozen synthetic corpus
//! (`tests/data/gold_corpus.json`, written by `scripts/gen_gold_corpus.py`); the
//! harness that runs both systems on it is added later. The corpus must not be
//! edited after a result has been seen: a corpus bug is fixed in its own commit
//! that names the reason, and both systems are re-run.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

fn corpus() -> Value {
    serde_json::from_str(include_str!("data/gold_corpus.json")).expect("gold_corpus.json parses")
}

#[test]
fn the_gold_corpus_is_consistent() {
    let c = corpus();
    let drawers = c["drawers"].as_array().unwrap();
    let ids: HashSet<&str> = drawers.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), drawers.len(), "unique drawer ids");
    for d in drawers {
        assert!(
            !d["text"].as_str().unwrap().trim().is_empty(),
            "{} has no text",
            d["id"]
        );
        assert!(d["meta"]["wing"].is_string(), "{} has no wing", d["id"]);
    }
    let queries = c["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 48);
    let qids: HashSet<&str> = queries.iter().map(|q| q["id"].as_str().unwrap()).collect();
    assert_eq!(qids.len(), queries.len(), "unique query ids");
    for q in queries {
        let relevant = q["relevant"].as_object().unwrap();
        assert!(!relevant.is_empty(), "{} has no relevant drawer", q["id"]);
        assert!(
            relevant.values().any(|g| g.as_u64() == Some(2)),
            "{} has no grade-2 drawer",
            q["id"]
        );
        assert!(q["lexical_ok"].is_boolean(), "{} lexical_ok", q["id"]);
        for (d, g) in relevant {
            assert!(ids.contains(d.as_str()), "{d}");
            assert!(matches!(g.as_u64(), Some(1 | 2)));
        }
    }
    let cats: BTreeMap<String, usize> = queries.iter().fold(Default::default(), |mut m, q| {
        *m.entry(q["category"].as_str().unwrap().to_string())
            .or_default() += 1;
        m
    });
    assert_eq!(cats["fact"], 14);
    assert_eq!(cats["thai"], 8);
    assert_eq!(cats["mixed"], 6);
    assert_eq!(cats["identifier"], 8);
    assert_eq!(cats["paraphrase"], 6);
    assert_eq!(cats["cross_lingual"], 3);
    assert_eq!(cats["raw_only"], 3);
}
