//! Text comparisons for the heuristic judge.

use crate::normalize::nfc;
use crate::store::kg::normalize_predicate;

pub fn normalize_title(s: &str) -> String {
    let lower = nfc(s).to_lowercase();
    let cleaned: String = lower
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || unicode_normalization::char::is_combining_mark(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn title_similarity(a: &str, b: &str) -> f64 {
    strsim::jaro_winkler(&normalize_title(a), &normalize_title(b))
}

/// Snake-case predicate with English inflections removed per word: `uses` → `use`,
/// `deployed_on` → `deploy_on`, `running_on` → `run_on`.
pub fn stem_predicate(p: &str) -> String {
    normalize_predicate(p)
        .split('_')
        .map(|w| {
            let n = w.chars().count();
            if n > 5 && w.ends_with("ing") {
                let base = &w[..w.len() - 3];
                let b = base.as_bytes();
                if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] {
                    base[..base.len() - 1].to_string()
                } else {
                    base.to_string()
                }
            } else if n > 4 && w.ends_with("ied") {
                format!("{}y", &w[..w.len() - 3])
            } else if n > 4 && w.ends_with("ed") {
                w[..w.len() - 2].to_string()
            } else if n > 3 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string()
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("_")
}

pub fn predicate_similarity(a: &str, b: &str) -> f64 {
    strsim::jaro_winkler(&stem_predicate(a), &stem_predicate(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_and_similarities() {
        assert_eq!(stem_predicate("uses"), "use");
        assert_eq!(stem_predicate("Deployed on"), "deploy_on");
        assert_eq!(stem_predicate("running_on"), "run_on");
        assert_eq!(stem_predicate("ใช้งาน"), "ใช้งาน");
        assert!(title_similarity("Use WAL mode!", "use wal MODE") > 0.99);
        assert!(predicate_similarity("uses", "use") > 0.99);
        assert!(predicate_similarity("owns", "deployed_on") < 0.9);
    }
}
