//! English function words: articles, pronouns, auxiliaries, question words, prepositions and
//! conjunctions. They are dropped from word queries when the query has other words, because they
//! match nearly every text (slow at scale, and a match on them says nothing about relevance).
//! Thai needs no list in M1: Thai is searched by substring.

use std::collections::HashSet;
use std::sync::LazyLock;

#[rustfmt::skip]
pub const ENGLISH: &[&str] = &[
    // Articles and determiners.
    "a", "an", "the", "this", "that", "these", "those", "some", "any", "each", "every", "no",
    // Pronouns.
    "i", "me", "my", "mine", "myself", "we", "us", "our", "ours", "you", "your", "yours", "he",
    "him", "his", "she", "her", "hers", "it", "its", "itself", "they", "them", "their", "theirs",
    "themselves",
    // Auxiliaries and modals.
    "am", "is", "are", "was", "were", "be", "been", "being", "do", "does", "did", "doing", "done",
    "have", "has", "had", "having", "can", "could", "will", "would", "shall", "should", "may",
    "might", "must",
    // Question words.
    "what", "which", "who", "whom", "whose", "when", "where", "why", "how",
    // Prepositions.
    "about", "above", "after", "against", "at", "before", "below", "between", "by", "during",
    "for", "from", "in", "into", "of", "off", "on", "onto", "out", "over", "per", "through", "to",
    "under", "until", "up", "upon", "via", "with", "within", "without",
    // Conjunctions and fillers.
    "and", "or", "but", "if", "then", "than", "so", "as", "because", "while", "not", "also",
    "there", "here", "just", "very", "too",
];

static SET: LazyLock<HashSet<&'static str>> = LazyLock::new(|| ENGLISH.iter().copied().collect());

/// True for a lowercase English function word.
pub fn is_stopword(term: &str) -> bool {
    SET.contains(term)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_small_lowercase_and_unique() {
        assert!((80..=150).contains(&ENGLISH.len()), "{}", ENGLISH.len());
        assert_eq!(SET.len(), ENGLISH.len(), "duplicates in the list");
        assert!(
            ENGLISH
                .iter()
                .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase()))
        );
        for w in ["the", "why", "do", "which", "should", "for", "after"] {
            assert!(is_stopword(w), "{w}");
        }
        // Content words that look common in technical text stay searchable.
        for w in ["use", "time", "line", "type", "data", "fix", "port", "The"] {
            assert!(!is_stopword(w), "{w}");
        }
    }
}
