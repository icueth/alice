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
/// `deployed_on` → `deploy_on`, `running_on` → `run_on`, `matches` → `match`,
/// `processing` and `processes` → `process`. Words with any non-ASCII character (Thai, …) are
/// left as normalised, unstemmed. `-es` is stripped only after `ch`, `sh`, `x`, `ss`; words
/// like `caches` or `buses` are reconciled by [`predicate_similarity`], which also tries the
/// other readings.
pub fn stem_predicate(p: &str) -> String {
    normalize_predicate(p)
        .split('_')
        .map(stem_word)
        .collect::<Vec<_>>()
        .join("_")
}

fn stem_word(w: &str) -> String {
    if !w.is_ascii() {
        return w.to_string();
    }
    let n = w.len();
    if n > 5 && w.ends_with("ing") {
        let base = &w[..n - 3];
        let mut cs = base.chars().rev();
        let (last, prev) = (cs.next(), cs.next());
        // Undouble a final consonant (`running` → `run`) but keep `process`, `call`, `buzz`.
        match (last, prev) {
            (Some(a), Some(b)) if a == b && !matches!(a, 's' | 'l' | 'z' | 'f') => {
                base[..base.len() - 1].to_string()
            }
            _ => base.to_string(),
        }
    } else if n > 4 && w.ends_with("ied") {
        format!("{}y", &w[..n - 3])
    } else if (n > 4 && w.ends_with("ed")) || (n > 3 && w.ends_with("es") && es_plural(&w[..n - 2]))
    {
        w[..n - 2].to_string()
    } else if n > 3 && w.ends_with('s') && !w.ends_with("ss") {
        w[..n - 1].to_string()
    } else {
        w.to_string()
    }
}

/// Whether `-es` is certainly the plural ending of `stem` (`match|es`, `fix|es`).
fn es_plural(stem: &str) -> bool {
    ["ch", "sh", "x", "ss"].iter().any(|e| stem.ends_with(e))
}

/// Every plausible stem of one word: the canonical stem plus, for `-es` words, the plain `-s`
/// form and the `-es`-stripped form (`caches` reaches `cache`, `buses` reaches `bus`, `heroes`
/// reaches `hero`). Extra candidates shorter than three characters are dropped (`toes` does not
/// reach `to`), except `go` and `do`.
fn word_candidates(w: &str) -> Vec<String> {
    let mut out = vec![stem_word(w)];
    let n = w.len();
    if w.is_ascii() && n > 3 && w.ends_with("es") {
        let mut extra = vec![w[..n - 1].to_string(), w[..n - 2].to_string()];
        if w[..n - 2].ends_with("zz") {
            extra.push(w[..n - 3].to_string());
        }
        // A very short candidate (`to`, `sho`-like fragments of 1-2 letters) is noise, except
        // the irregular `go` and `do`.
        out.extend(
            extra
                .into_iter()
                .filter(|c| c.chars().count() >= 3 || matches!(c.as_str(), "go" | "do")),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// All stems of a predicate: the product of its words' candidates (capped).
fn predicate_candidates(p: &str) -> Vec<String> {
    let mut acc = vec![String::new()];
    for (i, w) in normalize_predicate(p).split('_').enumerate() {
        let cands = word_candidates(w);
        let mut next = Vec::new();
        for a in &acc {
            for c in &cands {
                next.push(if i == 0 {
                    c.clone()
                } else {
                    format!("{a}_{c}")
                });
            }
        }
        next.truncate(32);
        acc = next;
    }
    acc
}

/// The best Jaro-Winkler score over the candidate stems of both predicates; 1.0 means they
/// read as the same predicate.
pub fn predicate_similarity(a: &str, b: &str) -> f64 {
    let bs = predicate_candidates(b);
    predicate_candidates(a)
        .iter()
        .flat_map(|x| bs.iter().map(move |y| strsim::jaro_winkler(x, y)))
        .fold(0.0, f64::max)
}

/// Whether two predicates share a candidate stem.
pub fn same_predicate(a: &str, b: &str) -> bool {
    let bs = predicate_candidates(b);
    predicate_candidates(a).iter().any(|x| bs.contains(x))
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

    #[test]
    fn es_plurals_and_ing_forms_stem_to_the_base() {
        for (w, base) in [
            ("matches", "match"),
            ("fixes", "fix"),
            ("pushes", "push"),
            ("processes", "process"),
            ("processing", "process"),
            ("uses", "use"),
            ("replaces", "replace"),
            ("supersedes", "supersede"),
            ("running", "run"),
        ] {
            assert_eq!(stem_predicate(w), base, "{w}");
        }
    }

    #[test]
    fn candidate_stems_reconcile_es_words_without_over_stripping() {
        for (a, b) in [
            ("caches", "cache"),
            ("sizes", "size"),
            ("serializes", "serialize"),
            ("buses", "bus"),
            ("goes", "go"),
            ("does", "do"),
            ("matches", "match"),
            ("quizzes", "quiz"),
            ("processes", "processing"),
            ("heroes", "hero"),
            ("echoes", "echo"),
            ("undoes", "undo"),
            ("potatoes", "potato"),
            ("deployed_on", "deploys_on"),
        ] {
            assert!(
                same_predicate(a, b) && predicate_similarity(a, b) >= 1.0,
                "{a} ~ {b}"
            );
            assert!(same_predicate(b, a), "{b} ~ {a}");
        }
        for (a, b) in [("toes", "to"), ("owns", "deployed_on")] {
            assert!(
                !same_predicate(a, b) && predicate_similarity(a, b) < 1.0,
                "{a} !~ {b}"
            );
        }
    }

    #[test]
    fn non_ascii_words_never_panic_and_stay_unstemmed() {
        for w in [
            "กขค\u{0E38}ing",
            "कखग\u{093F}ing",
            "ใช้งานed",
            "कंप्यूटरes",
            "ééing",
        ] {
            let s = stem_predicate(w);
            assert_eq!(s, normalize_predicate(w), "{w}");
        }
        assert_eq!(stem_predicate("uses_ใช้งาน"), "use_ใช้งาน");
    }
}
