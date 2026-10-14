//! Text normalisation shared by storage, dedup and ranking.

use crate::model::{Kind, limits};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

const NIKHAHIT: char = '\u{0E4D}';
const SARA_AA: char = '\u{0E32}';
const SARA_AM: char = '\u{0E33}';

/// NFC plus the Thai sara am fold: "ํา" (U+0E4D U+0E32) becomes "ำ" (U+0E33), and a tone mark
/// typed between the two moves before it. NFC alone keeps them apart (U+0E33 decomposes only
/// under NFKC), but people type both spellings of the same word.
pub fn nfc(s: &str) -> String {
    let composed: String = s.nfc().collect();
    if !composed.contains(NIKHAHIT) {
        return composed;
    }
    // Moving a tone mark can leave it after another combining mark, so compose again.
    fold_sara_am(&composed).nfc().collect()
}

fn fold_sara_am(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == NIKHAHIT {
            match (chars.get(i + 1), chars.get(i + 2)) {
                (Some(&SARA_AA), _) => {
                    out.push(SARA_AM);
                    i += 2;
                    continue;
                }
                (Some(&tone @ '\u{0E48}'..='\u{0E4B}'), Some(&SARA_AA)) => {
                    out.push(tone);
                    out.push(SARA_AM);
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_separator(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_control()
        || (ch.is_ascii() && !ch.is_ascii_alphanumeric())
        || matches!(ch, '\u{00B7}' | '\u{2010}'..='\u{2027}' | '\u{3000}'..='\u{303F}')
}

/// Lowercase kebab-case slug. Non-ASCII letters and combining marks (Thai, CJK) are kept.
pub fn slugify(s: &str) -> String {
    let lower = nfc(s).to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut pending_dash = false;
    for ch in lower.chars() {
        if is_separator(ch) {
            pending_dash = !out.is_empty();
        } else {
            if pending_dash {
                out.push('-');
                pending_dash = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Topic tags are slugs capped at 64 characters; `None` when nothing is left.
pub fn normalize_topic(s: &str) -> Option<String> {
    let slug: String = slugify(s).chars().take(limits::MAX_TOPIC_CHARS).collect();
    let slug = slug.trim_end_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Dedup key: same project, kind, title and body (NFC, whitespace-collapsed) => same hash.
pub fn content_hash(project: &str, kind: Kind, title: &str, body: &str) -> String {
    let normalized = format!(
        "{project}\u{1f}{}\u{1f}{}\u{1f}{}",
        kind.as_str(),
        collapse_whitespace(&nfc(title)),
        collapse_whitespace(&nfc(body)),
    );
    sha256_hex(normalized.as_bytes())
}

/// Whether two memories hold the same title and body under the identity `content_hash` uses
/// (NFC, whitespace collapsed): case and punctuation count, the kind and project do not.
pub fn same_text(a_title: &str, a_body: &str, b_title: &str, b_body: &str) -> bool {
    let norm = |s: &str| collapse_whitespace(&nfc(s));
    norm(a_title) == norm(b_title) && norm(a_body) == norm(b_body)
}

/// Rough token count used for budgets: one token per four UTF-8 bytes, rounded up.
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// Cut to `max` characters, appending `…` when something was removed.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Title for a memory saved without one: its first non-empty line, at most 80 characters.
pub fn derive_title(content: &str) -> String {
    let line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    #[test]
    fn slugify_folds_case_and_separators() {
        assert_eq!(slugify("PG_AI"), "pg-ai");
        assert_eq!(slugify("pg-ai"), "pg-ai");
        assert_eq!(slugify("  My App!! "), "my-app");
        assert_eq!(slugify("tara.stock_market"), "tara-stock-market");
        assert_eq!(slugify("wing_pg_ai"), "wing-pg-ai");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn slugify_keeps_thai_marks() {
        assert_eq!(slugify("ระบบ สต็อก"), "ระบบ-สต็อก");
        assert_eq!(slugify("ไม้เอก่"), "ไม้เอก่");
    }

    #[test]
    fn topics_are_slugs_with_limits() {
        assert_eq!(normalize_topic(" Bug Fix "), Some("bug-fix".to_string()));
        assert_eq!(normalize_topic("!!!"), None);
        assert_eq!(
            normalize_topic(&"a".repeat(80)).unwrap().chars().count(),
            64
        );
    }

    #[test]
    fn content_hash_ignores_whitespace_and_composition() {
        // "ำ" (U+0E33) vs "ํา" (U+0E4D U+0E32) normalise to the same text: `nfc` folds sara am.
        let a = content_hash("p", Kind::Note, "t", "จำ  ไว้\n");
        let b = content_hash("p", Kind::Note, "t", "จ\u{0E4D}\u{0E32} ไว้");
        assert_eq!(a, b);
        assert_ne!(a, content_hash("p", Kind::Fact, "t", "จำ ไว้"));
        assert_ne!(a, content_hash("q", Kind::Note, "t", "จำ ไว้"));
        assert_eq!(a.len(), 64);
        // Canonically equivalent mark order (sara u + mai ek typed either way) is one NFC text.
        assert_eq!(
            content_hash("p", Kind::Note, "t", "ก\u{0E38}\u{0E48}"),
            content_hash("p", Kind::Note, "t", "ก\u{0E48}\u{0E38}"),
        );
    }

    #[test]
    fn nfc_folds_thai_sara_am() {
        assert_eq!(nfc("จ\u{0E4D}\u{0E32}"), "จำ");
        // A tone mark typed between nikhahit and sara aa moves before the folded sara am.
        assert_eq!(nfc("น้ำ"), nfc("น\u{0E4D}\u{0E49}\u{0E32}"));
        assert_eq!(nfc("น\u{0E4D}\u{0E49}\u{0E32}"), "น\u{0E49}\u{0E33}");
        assert_eq!(nfc("น\u{0E49}\u{0E4D}\u{0E32}"), "น\u{0E49}\u{0E33}");
        // Already-folded, plain and code text pass through; a lone nikhahit is kept.
        for s in [
            "ำ",
            "น้ำ",
            "plain ASCII",
            "HostLinkReconnect internal/a_b.go",
            "ก\u{0E4D}",
        ] {
            assert_eq!(nfc(s), s);
        }
        // The fold can put a tone mark after another combining mark; the result stays NFC.
        let folded = nfc("ก\u{0301}\u{0E4D}\u{0E48}\u{0E32}");
        assert!(unicode_normalization::is_nfc(&folded), "{folded:?}");
        assert_eq!(nfc(&folded), folded);
    }

    #[test]
    fn derive_title_uses_first_line() {
        assert_eq!(derive_title("  first line\nsecond"), "first line");
        let long = "ก".repeat(300);
        assert_eq!(derive_title(&long).chars().count(), 80);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_chars("สวัสดีครับ", 3), "สวั…");
        assert_eq!(truncate_chars("abc", 5), "abc");
    }

    #[test]
    fn token_estimate_is_bytes_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
