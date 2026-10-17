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

/// The most characters of a derived title ([`derive_title`]).
pub const DERIVED_TITLE_CHARS: usize = 80;

/// Title for a memory saved without one: its first non-empty line, at most 80 characters.
///
/// A title that is a cut of the text (a first line of several, or a line longer than 80
/// characters) is cut from the text redacted with the built-in detectors (re-review N2): a store
/// that keeps secrets as written holds them whole in the body, and a cut through one would leave
/// a fragment in the title that no detector knows, which every Jev request about the memory
/// would carry. A title that is the whole text is the text as written; the cloud requests
/// redact it whole.
///
/// ```
/// use alice_core::normalize::derive_title;
///
/// let key = concat!("sk-", "proj-", "jD4XP_qW9yLWXUgo5RApWBKupwr-wVenptzp2muJRWt1wEklnUn27KT1A");
/// let title = derive_title(&format!("The CI bot authenticates to the model API with {key}"));
/// assert!(!title.contains("jD4XP") && title.contains("[REDACTED:openai_key]"));
/// assert_eq!(derive_title("password = hunter22x"), "password = hunter22x");
/// ```
pub fn derive_title(content: &str) -> String {
    let first = |text: &str| -> String {
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(DERIVED_TITLE_CHARS)
            .collect()
    };
    let title = first(content);
    if title == content.trim() {
        return title;
    }
    first(
        &crate::redact::Redactor::builtin_shared()
            .apply(content)
            .text,
    )
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

    /// Re-review N2: a title cut from a text that keeps secrets as written holds no piece of
    /// one, whatever the cut falls in: a key across the 80th character, a password whose
    /// keyword is on the title's line, a private key whose first line is all the title gets.
    #[test]
    fn a_derived_title_never_holds_a_piece_of_a_secret() {
        let key = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let across = format!("{} {key} for the deploy bot", "w".repeat(59));
        assert_eq!(&across[60..63], &key[..3]);
        let t = derive_title(&across);
        assert!(!t.contains(&key[..6]) && t.contains("[REDACTED:"), "{t}");

        let pem = concat!(
            "MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\n",
            "VTLw7onLRnrq0/IzW7yWR7QkrmBL7jTKEn5u+qKhbwKfBstIs+bMY2Zkp18gnTxK\n",
            "LxoS2tFczGkPLPgizskuemMghRniWaoLcyehkd3qqGElvW/VDL5AaWTg0nLVkjRo\n",
        );
        let t = derive_title(pem);
        assert!(!t.contains("MIIEow"), "{t}");

        // A text that is its own title is kept as written.
        let one = concat!("the db password is Thai", "land-2026");
        assert_eq!(derive_title(one), one);
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
