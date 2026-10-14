//! The Jev evaluation set is balanced, well-formed and clean (runs in CI; no network, no model).
mod common;
use common::*;
use std::collections::{HashMap, HashSet};

#[test]
fn the_labelled_set_meets_its_composition_rules() {
    let pairs = lines(include_str!("fixtures/judge_eval_pairs.jsonl"));
    assert!(pairs.len() >= 200, "{} pairs", pairs.len());
    let mut ids = HashSet::new();
    let mut per: HashMap<(String, String), usize> = HashMap::new();
    let mut kinds: HashMap<String, usize> = HashMap::new();
    let (mut same_age_contradicts, mut redactor_hits) = (0, 0);
    let r = alice_core::redact::Redactor::builtin();
    for p in &pairs {
        assert!(
            ids.insert(p["id"].as_str().unwrap().to_string()),
            "duplicate id {}",
            p["id"]
        );
        let label = p["label"].as_str().unwrap();
        assert!(
            alice_core::judge::RelationKind::parse(label).is_some(),
            "{label}"
        );
        let lang = p["lang"].as_str().unwrap();
        assert!(["th", "en", "mixed", "code"].contains(&lang), "{lang}");
        *per.entry((label.into(), lang.into())).or_default() += 1;
        let k = p["new_kind"].as_str().unwrap();
        assert!(alice_core::model::Kind::parse(k).is_some(), "{k}");
        *kinds.entry(k.into()).or_default() += 1;
        assert_eq!(
            p["new"]["kind"].as_str().unwrap(),
            k,
            "{}: new_kind is the kind of the new memory",
            p["id"]
        );
        for side in ["new", "old"] {
            let m = &p[side];
            assert!(alice_core::model::Kind::parse(m["kind"].as_str().unwrap()).is_some());
            let (title, body) = (m["title"].as_str().unwrap(), m["body"].as_str().unwrap());
            assert!(
                !body.is_empty() && body.chars().count() <= 600,
                "{}",
                p["id"]
            );
            assert!(!title.is_empty(), "{}", p["id"]);
            for text in [title, body] {
                redactor_hits += r.apply(text).found.len();
                // Nothing that names a person's machine or mailbox.
                assert!(
                    !text.contains("/Users/") && !text.contains("/home/") && !text.contains('@'),
                    "{}: {text}",
                    p["id"]
                );
            }
        }
        let (n, o) = (
            p["new"]["days_ago"].as_i64().unwrap(),
            p["old"]["days_ago"].as_i64().unwrap(),
        );
        if label == "updates" {
            assert!(
                n <= o,
                "{}: an update is not older than what it updates",
                p["id"]
            );
        }
        if label == "contradicts" && n == o {
            same_age_contradicts += 1;
        }
    }
    for label in ["duplicate", "updates", "contradicts", "unrelated"] {
        assert!(
            per.iter()
                .filter(|((l, _), _)| l == label)
                .map(|(_, n)| n)
                .sum::<usize>()
                >= 50,
            "{label}"
        );
        for (lang, min) in [("th", 15), ("en", 15), ("mixed", 10), ("code", 10)] {
            let n = per
                .get(&(label.to_string(), lang.to_string()))
                .copied()
                .unwrap_or(0);
            assert!(n >= min, "{label}/{lang}: {n} < {min}");
        }
    }
    for k in alice_core::model::Kind::ALL {
        assert!(
            kinds.get(k.as_str()).copied().unwrap_or(0) >= 6,
            "kind {} under-represented",
            k.as_str()
        );
    }
    assert!(same_age_contradicts >= 15);

    // Step 5 rules: hard negatives and paraphrases.
    let common = common_words(&pairs);
    let hard = pairs
        .iter()
        .filter(|p| p["label"] == "unrelated" && shares_a_topic_word(p, &common))
        .count();
    assert!(hard >= 20, "{hard} unrelated pairs share a topic word");
    let paraphrases = pairs
        .iter()
        .filter(|p| {
            p["label"] == "duplicate"
                && trigram_jaccard(
                    p["new"]["body"].as_str().unwrap(),
                    p["old"]["body"].as_str().unwrap(),
                ) < 0.3
        })
        .count();
    assert!(paraphrases >= 20, "{paraphrases} paraphrase duplicates");

    // The metadata must not decide: neither the fixed rule nor the best table over
    // (same title, age order, same kind) may get half of the pairs right.
    let rule = pairs
        .iter()
        .filter(|p| metadata_rule(p) == p["label"].as_str().unwrap())
        .count() as f64
        / pairs.len() as f64;
    let fitted = metadata_fitted(&pairs);
    let best = pairs
        .iter()
        .zip(&fitted)
        .filter(|(p, f)| p["label"].as_str().unwrap() == f.as_str())
        .count() as f64
        / pairs.len() as f64;
    assert!(rule <= 0.5, "the metadata-only rule scores {rule:.3}");
    assert!(best <= 0.5, "the best metadata-only table scores {best:.3}");

    // Real memories are paragraphs, not one-liners.
    let mut lens: Vec<usize> = pairs
        .iter()
        .flat_map(|p| ["new", "old"].map(|s| p[s]["body"].as_str().unwrap().chars().count()))
        .collect();
    lens.sort();
    assert!(
        lens[lens.len() / 2] >= 150,
        "median body {} characters",
        lens[lens.len() / 2]
    );
    assert_eq!(
        redactor_hits, 0,
        "the set must contain nothing secret-shaped"
    );
}

#[test]
fn the_query_set_covers_every_language_and_the_floor_evidence_kinds() {
    let queries = lines(include_str!("fixtures/judge_eval_queries.jsonl"));
    assert!(queries.len() >= 40, "{} queries", queries.len());
    let mut by_lang: HashMap<String, usize> = HashMap::new();
    let mut by_kind: HashMap<String, usize> = HashMap::new();
    let mut unanswerable = 0;
    let r = alice_core::redact::Redactor::builtin();
    for q in &queries {
        let text = q["q"].as_str().unwrap();
        assert!(!text.trim().is_empty());
        assert!(
            r.apply(text).found.is_empty() && !text.contains("/Users/") && !text.contains('@'),
            "{text}"
        );
        *by_lang
            .entry(q["lang"].as_str().unwrap().into())
            .or_default() += 1;
        let kind = q["kind"].as_str().unwrap();
        assert!(["same", "cross", "para"].contains(&kind), "{kind}");
        *by_kind.entry(kind.into()).or_default() += 1;
        if q.get("answerable").and_then(|a| a.as_bool()) == Some(false) {
            unanswerable += 1;
        }
    }
    for lang in ["th", "en", "mixed", "code"] {
        assert!(by_lang.get(lang).copied().unwrap_or(0) >= 10, "{lang}");
    }
    let hard =
        by_kind.get("cross").copied().unwrap_or(0) + by_kind.get("para").copied().unwrap_or(0);
    assert!(hard >= 12, "{hard} cross-lingual or paraphrase queries");
    assert!(by_kind.get("cross").copied().unwrap_or(0) >= 4);
    assert!(by_kind.get("para").copied().unwrap_or(0) >= 4);
    assert!(unanswerable >= 8, "{unanswerable} queries nothing answers");
}
