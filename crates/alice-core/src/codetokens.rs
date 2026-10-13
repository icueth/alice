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

/// Looks like an identifier or a path rather than a plain word.
pub fn is_codeish(tok: &str) -> bool {
    tok.contains(['_', '/', '.', ':']) || has_case_change(tok)
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

/// Lowercase words derived from code-like tokens in `text`, space separated, without duplicates.
/// Plain prose yields an empty string.
pub fn expand_code_tokens(text: &str) -> String {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut push = |w: String, out: &mut Vec<String>| {
        if w.chars().count() >= 2 && seen.insert(w.clone()) {
            out.push(w);
        }
    };
    for m in TOKEN.find_iter(text) {
        let tok = m.as_str();
        if tok.len() < 3 || !is_codeish(tok) {
            continue;
        }
        let joined: String = tok
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_lowercase();
        push(joined, &mut out);
        for segment in tok
            .split(['_', '/', '.', ':', '-'])
            .filter(|s| !s.is_empty())
        {
            let pieces = split_camel(segment);
            if pieces.len() > 1 {
                push(segment.to_lowercase(), &mut out);
            }
            for piece in pieces {
                push(piece.to_lowercase(), &mut out);
            }
        }
    }
    out.join(" ")
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
