//! Identifier and path expansion so FTS can match code names by their parts.

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9_][A-Za-z0-9_./:\-]*[A-Za-z0-9_]").expect("static regex")
});

fn has_case_change(tok: &str) -> bool {
    let chars: Vec<char> = tok.chars().collect();
    chars
        .windows(2)
        .any(|w| (w[0].is_lowercase() || w[0].is_ascii_digit()) && w[1].is_uppercase())
}

/// Looks like an identifier or a path rather than a plain word. A hyphen alone makes it
/// kebab-case only when every hyphen-separated part has a letter (`well-known`,
/// `lease-token-frame`), so dates and number ranges (`2026-10-14`, `15-45`) are not code.
pub fn is_codeish(tok: &str) -> bool {
    tok.contains(['_', '/', '.', ':']) || has_case_change(tok) || is_kebab(tok)
}

fn is_kebab(tok: &str) -> bool {
    tok.contains('-')
        && tok
            .split('-')
            .all(|part| part.chars().any(char::is_alphabetic))
}

/// Words of one name in a token: `lease_token_frame` -> lease, token, frame;
/// `LeaseTokenFrame` -> lease, token, frame. Lowercase, in order.
fn name_words(name: &str) -> Vec<String> {
    name.split(['_', '-'])
        .filter(|s| !s.is_empty())
        .flat_map(split_camel)
        .map(|p| p.to_lowercase())
        .collect()
}

/// Most words of one name used for joined runs, and the longest run: bounds the tokens a long
/// name adds (at most 12 words, runs of 2 to 6: 11 + 10 + 9 + 8 + 7 = 45 joins).
const MAX_RUN_WORDS: usize = 12;
const MAX_RUN: usize = 6;

/// The names a token is made of: split at path and qualifier separators (`/ . :`).
fn names(tok: &str) -> impl Iterator<Item = &str> {
    tok.split(['/', '.', ':']).filter(|s| !s.is_empty())
}

fn joined(parts: &[String]) -> String {
    parts
        .concat()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// The joined form of each name in `tok` that has two or more words, for queries:
/// `crate::daemon::lease_token_frame` -> `leasetokenframe`.
pub fn joined_names(tok: &str) -> Vec<String> {
    names(tok)
        .map(name_words)
        .filter(|w| w.len() > 1)
        .map(|w| joined(&w))
        .collect()
}

/// Split camelCase and PascalCase, keeping acronyms together (`HTTPServer` -> `HTTP`, `Server`).
pub fn split_camel(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && !current.is_empty() {
            let prev = chars[i - 1];
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (c.is_uppercase() && (prev.is_lowercase() || prev.is_ascii_digit()))
                || (c.is_uppercase() && prev.is_uppercase() && next_is_lower);
            if boundary {
                parts.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Code-like tokens in `text`: identifiers and paths of at least 3 characters.
fn code_like(text: &str) -> impl Iterator<Item = &str> {
    TOKEN
        .find_iter(text)
        .map(|m| m.as_str())
        .filter(|tok| tok.len() >= 3 && is_codeish(tok))
}

/// Lowercase words of at least 2 characters, in first-seen order without duplicates.
#[derive(Default)]
struct Words {
    seen: HashSet<String>,
    out: Vec<String>,
}

impl Words {
    fn push(&mut self, w: String) {
        if w.chars().count() >= 2 && self.seen.insert(w.clone()) {
            self.out.push(w);
        }
    }

    /// The parts of `tok` as written: each segment between `_ / . : -`, and its camel-case
    /// words when it has several (`HostLink_reconnect` -> `hostlink`, `host`, `link`,
    /// `reconnect`).
    fn push_parts(&mut self, tok: &str) {
        for segment in tok
            .split(['_', '/', '.', ':', '-'])
            .filter(|s| !s.is_empty())
        {
            let pieces = split_camel(segment);
            if pieces.len() > 1 {
                self.push(segment.to_lowercase());
            }
            for piece in pieces {
                self.push(piece.to_lowercase());
            }
        }
    }
}

/// Lowercase words derived from code-like tokens in `text`, space separated, without duplicates,
/// for the index. Plain prose yields an empty string.
pub fn expand_code_tokens(text: &str) -> String {
    let mut words = Words::default();
    for tok in code_like(text) {
        words.push(
            tok.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_lowercase(),
        );
        words.push_parts(tok);
        // Joined runs of consecutive words within each name, so a qualified or compound form
        // (`self.lease_token_frame`, `store/lease_token_frame.rs`, `LEASE_TOKEN_FRAME_LIMIT`,
        // `lease-token-frame`) also carries `leasetokenframe`, the token a search for
        // `lease_token_frame` looks for. Never across `/ . :`. Index side only: a query
        // searches the joined form of each name it contains ([`joined_names`]).
        for name in names(tok) {
            let name_words = name_words(name);
            let name_words = &name_words[..name_words.len().min(MAX_RUN_WORDS)];
            for len in 2..=name_words.len().min(MAX_RUN) {
                for run in name_words.windows(len) {
                    words.push(joined(run));
                }
            }
        }
    }
    words.out.join(" ")
}

/// The parts of the code-like tokens in a query, as typed (segments and their camel-case words),
/// lowercase, without duplicates: what the word lists search for an identifier. No joined forms:
/// those are the identifier list's terms, and the word lists never search them, so a long
/// identifier adds one term per word it is made of, not every run of its words.
pub fn query_code_parts(query: &str) -> Vec<String> {
    let mut words = Words::default();
    for tok in code_like(query) {
        words.push_parts(tok);
    }
    words.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        expand_code_tokens(s)
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect()
    }

    #[test]
    fn camel_case_is_split() {
        let w = words("call HostLinkReconnect now");
        for want in ["hostlinkreconnect", "host", "link", "reconnect"] {
            assert!(w.contains(&want.to_string()), "{want} missing from {w:?}");
        }
    }

    #[test]
    fn snake_case_and_paths_are_split() {
        let w = words("writer_lease in internal/teacher/jev/client.go");
        for want in ["writer", "lease", "internal", "teacher", "jev", "client"] {
            assert!(w.contains(&want.to_string()), "{want} missing from {w:?}");
        }
    }

    #[test]
    fn acronyms_split_before_the_next_word() {
        assert_eq!(split_camel("HTTPServer"), vec!["HTTP", "Server"]);
        assert_eq!(split_camel("v4Frame"), vec!["v4", "Frame"]);
    }

    /// Fix round 3 (N2): an identifier qualified by a path, module, receiver or suffix, or
    /// written in another case style, still carries the joined form of the identifier itself,
    /// so a search for `lease_token_frame` finds it in the identifier list.
    #[test]
    fn qualified_and_compound_forms_carry_the_joined_identifier() {
        for form in [
            "self.lease_token_frame = None",
            "see store/lease_token_frame.rs for details",
            "the LEASE_TOKEN_FRAME_LIMIT constant",
            "crate::daemon::lease_token_frame()",
            "a lease-token-frame mismatch",
            "LeaseTokenFrame::new()",
            "lease_token_frame",
        ] {
            let w = words(form);
            assert!(w.contains(&"leasetokenframe".to_string()), "{form}: {w:?}");
        }
        let w = words("self.writer_lease");
        assert!(w.contains(&"writerlease".to_string()), "{w:?}");
        // Joins stay within one name: no join across the path separator.
        assert!(!w.contains(&"selfwriter".to_string()), "{w:?}");
    }

    /// Fix round 4 (R5): a hyphenated token is kebab-case code only when every hyphen-separated
    /// part has a letter; dates and number ranges are not identifiers.
    #[test]
    fn hyphenated_numbers_are_not_code() {
        assert_eq!(expand_code_tokens("on 2026-10-14 from 15-45"), "");
        assert!(!is_codeish("2026-10-14"));
        assert!(!is_codeish("15-45"));
        let w = words("a well-known lease-token-frame");
        for want in ["wellknown", "well", "known", "leasetokenframe"] {
            assert!(w.contains(&want.to_string()), "{want} missing from {w:?}");
        }
    }

    #[test]
    fn plain_prose_produces_nothing() {
        assert_eq!(expand_code_tokens("plain words here, nothing special."), "");
        assert_eq!(expand_code_tokens("ภาษาไทยล้วน"), "");
    }

    #[test]
    fn output_has_no_duplicates() {
        let w = words("foo_bar foo_bar");
        let mut d = w.clone();
        d.sort();
        d.dedup();
        assert_eq!(w.len(), d.len());
    }
}
