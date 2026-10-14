//! Helpers shared by the evaluation's CI check and the evaluation itself: baselines that never
//! read the memories' text, and the rules that make a pair a paraphrase or a hard negative.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

pub fn lines(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

/// (same title, order of ages, same kind): the only things a metadata-only rule may read.
/// Order is `n` (the new memory is newer), `e` (same age) or `o` (the new one is older).
pub fn metadata(p: &serde_json::Value) -> (bool, char, bool) {
    let (n, o) = (
        p["new"]["days_ago"].as_i64().unwrap(),
        p["old"]["days_ago"].as_i64().unwrap(),
    );
    (
        p["new"]["title"] == p["old"]["title"],
        if n < o {
            'n'
        } else if n == o {
            'e'
        } else {
            'o'
        },
        p["new"]["kind"] == p["old"]["kind"],
    )
}

/// The fixed metadata-only rule: same title and same age is a contradiction, same title and a
/// newer memory an update, different kinds unrelated, anything else a duplicate.
pub fn metadata_rule(p: &serde_json::Value) -> &'static str {
    match metadata(p) {
        (true, 'e', _) => "contradicts",
        (true, 'n', _) => "updates",
        (_, _, false) => "unrelated",
        _ => "duplicate",
    }
}

/// The best any metadata-only classifier can do on this set: each (title, order, kind) cell
/// predicts its most common label (fitted on the set itself, so an upper bound).
pub fn metadata_fitted(pairs: &[serde_json::Value]) -> Vec<String> {
    let mut cells: HashMap<(bool, char, bool), BTreeMap<String, usize>> = HashMap::new();
    for p in pairs {
        *cells
            .entry(metadata(p))
            .or_default()
            .entry(p["label"].as_str().unwrap().to_string())
            .or_default() += 1;
    }
    pairs
        .iter()
        .map(|p| {
            let c = &cells[&metadata(p)];
            c.iter().max_by_key(|(_, n)| **n).unwrap().0.clone()
        })
        .collect()
}

fn trigrams(t: &str) -> HashSet<String> {
    let c: Vec<char> = t.to_lowercase().chars().collect();
    c.windows(3).map(|w| w.iter().collect()).collect()
}

/// Jaccard similarity of the character trigrams of the two bodies (works for Thai too).
pub fn trigram_jaccard(a: &str, b: &str) -> f64 {
    let (x, y) = (trigrams(a), trigrams(b));
    x.intersection(&y).count() as f64 / x.union(&y).count().max(1) as f64
}

fn words(m: &serde_json::Value) -> HashSet<String> {
    format!(
        "{} {}",
        m["title"].as_str().unwrap(),
        m["body"].as_str().unwrap()
    )
    .to_lowercase()
    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
    .filter(|w| w.len() >= 4)
    .map(String::from)
    .collect()
}

/// The words (4+ ASCII characters) that more than 25 memories use: left out when looking for
/// a topic two memories share.
pub fn common_words(pairs: &[serde_json::Value]) -> HashSet<String> {
    let mut df: HashMap<String, usize> = HashMap::new();
    for p in pairs {
        for side in ["new", "old"] {
            for w in words(&p[side]) {
                *df.entry(w).or_default() += 1;
            }
        }
    }
    df.into_iter()
        .filter(|(_, n)| *n > 25)
        .map(|(w, _)| w)
        .collect()
}

/// Two memories that share an uncommon word of their title or body.
pub fn shares_a_topic_word(p: &serde_json::Value, common: &HashSet<String>) -> bool {
    words(&p["new"])
        .intersection(&words(&p["old"]))
        .any(|w| !common.contains(w))
}
