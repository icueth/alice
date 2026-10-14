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
/// left as normalised, unstemmed.
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
    } else if (n > 4 && w.ends_with("ed")) || (n > 2 && w.ends_with("es") && es_plural(&w[..n - 2]))
    {
        w[..n - 2].to_string()
    } else if n > 3 && w.ends_with('s') && !w.ends_with("ss") {
        w[..n - 1].to_string()
    } else {
        w.to_string()
    }
}

/// Whether `-es` is the plural ending of `stem` (`match|es`, `fix|es`, `go|es`) rather than
/// `-s` added to a word ending in `e` (`use|s`, `replace|s`).
fn es_plural(stem: &str) -> bool {
    ["ch", "sh", "x", "ss", "z", "o"]
        .iter()
        .any(|e| stem.ends_with(e))
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

    #[test]
    fn es_plurals_and_ing_forms_stem_to_the_base() {
        for (w, base) in [
            ("matches", "match"),
            ("fixes", "fix"),
            ("goes", "go"),
            ("does", "do"),
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
