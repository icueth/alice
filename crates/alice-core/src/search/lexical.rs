//! FTS5 query construction and candidate retrieval. Every user term is quoted, so FTS5
//! operators in the query are treated as literal words.

use super::stopwords::is_stopword;
use crate::codetokens::{expand_code_tokens, is_codeish};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// Most terms one query sends to an index; longer queries keep their first terms.
const MAX_TERMS: usize = 32;
/// Longest term sent to an index. Longer word terms cannot be real words and are dropped;
/// longer trigram terms are cut (a prefix of a substring is still a substring), which bounds
/// the work a pasted wall of text causes.
const MAX_TERM_CHARS: usize = 64;

pub fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

pub fn or_query(terms: &[String]) -> Option<String> {
    (!terms.is_empty()).then(|| {
        terms
            .iter()
            .map(|t| quote(t))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// Terms in first-seen order without duplicates. It stops accepting at `cap`, so builders can
/// stop reading the query: building is linear in the part of the query actually used.
struct TermList {
    terms: Vec<String>,
    seen: HashSet<String>,
    cap: usize,
}

impl TermList {
    fn new(cap: usize) -> Self {
        Self {
            terms: Vec::new(),
            seen: HashSet::new(),
            cap,
        }
    }

    fn is_full(&self) -> bool {
        self.terms.len() >= self.cap
    }

    /// Keeps a non-empty term of at most `MAX_TERM_CHARS` with a letter or digit, once.
    fn push(&mut self, w: String) {
        if !self.is_full()
            && !w.is_empty()
            && w.chars().count() <= MAX_TERM_CHARS
            && w.chars().any(char::is_alphanumeric)
            && !self.seen.contains(&w)
        {
            self.seen.insert(w.clone());
            self.terms.push(w);
        }
    }
}

fn longer_than(s: &str, chars: usize) -> bool {
    s.chars().nth(chars).is_some()
}

/// Word separators for the unicode61 terms: whitespace, control characters and ASCII
/// punctuation except `_` (identifiers stay whole and become a phrase in FTS5).
fn is_word_separator(c: char) -> bool {
    c.is_whitespace() || c.is_control() || (c.is_ascii_punctuation() && c != '_')
}

/// Whitespace- or control-separated pieces with ASCII punctuation trimmed from both ends.
fn pieces(query: &str) -> impl Iterator<Item = &str> {
    query
        .split(|c: char| c.is_whitespace() || c.is_control())
        .map(|w| w.trim_matches(|c: char| c.is_ascii_punctuation()))
        .filter(|w| !w.is_empty())
}

/// Words, then identifier parts, that pass `keep`; at most 32.
fn collect_words(query: &str, keep: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = TermList::new(MAX_TERMS);
    for w in query.split(is_word_separator) {
        if out.is_full() {
            return out.terms;
        }
        let w = w.trim();
        if !longer_than(w, MAX_TERM_CHARS) {
            let w = w.to_lowercase();
            if keep(&w) {
                out.push(w);
            }
        }
    }
    for w in expand_code_tokens(query).split(' ') {
        if out.is_full() {
            break;
        }
        if keep(w) {
            out.push(w.to_string());
        }
    }
    out.terms
}

/// Lowercase words for the unicode61 index plus identifier parts. At most 32 terms.
pub fn word_terms(query: &str) -> Vec<String> {
    collect_words(query, |_| true)
}

/// The terms the unicode61 lists search: `word_terms` without English stopwords, or all of
/// `word_terms` when the query has nothing but stopwords. At most 32 terms.
pub fn content_terms(query: &str) -> Vec<String> {
    let content = collect_words(query, |w| !is_stopword(w));
    if content.is_empty() {
        word_terms(query)
    } else {
        content
    }
}

fn is_vowel(c: u8) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u')
}

/// Ends in a doubled consonant that English adds before -ed/-ing (`capp`, `runn`), not `ll`/`ss`
/// and not in a three-letter stem (`add`).
fn doubled_consonant(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 4 && {
        let (x, y) = (b[b.len() - 2], b[b.len() - 1]);
        x == y && !is_vowel(y) && !matches!(y, b'l' | b's' | b'z' | b'f')
    }
}

/// Consonant-vowel-consonant ending that doubles before -ed/-ing (`stop` -> `stopped`).
fn doubles_before_suffix(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() <= 6
        && b.len() >= 3
        && !is_vowel(b[b.len() - 3])
        && is_vowel(b[b.len() - 2])
        && !is_vowel(b[b.len() - 1])
        && !matches!(b[b.len() - 1], b'w' | b'x' | b'y')
}

/// Likely base forms of an English word with one inflection removed. The word itself
/// when no rule applies.
fn english_bases(w: &str) -> Vec<String> {
    let stem_ok = |s: &str| s.len() >= 3 && s.bytes().any(|c| is_vowel(c) || c == b'y');
    if let Some(s) = w.strip_suffix("ies").or_else(|| w.strip_suffix("ied"))
        && s.len() >= 2
    {
        return vec![format!("{s}y")];
    }
    if let Some(s) = w.strip_suffix("es")
        && stem_ok(s)
    {
        // `fixes` -> `fix`, `frames` -> `frame`; after ch/sh/s both are plausible (`caches`, `matches`).
        let sibilant = ["s", "x", "z", "ch", "sh"].iter().any(|e| s.ends_with(e));
        let after_e = format!("{s}e");
        return match (
            sibilant,
            s.ends_with("ch") || s.ends_with("sh") || s.ends_with('s'),
        ) {
            (true, true) => vec![s.to_string(), after_e],
            (true, false) => vec![s.to_string()],
            _ => vec![after_e],
        };
    }
    if let Some(s) = w.strip_suffix('s')
        && !["ss", "us", "is"].iter().any(|e| w.ends_with(e))
        && stem_ok(s)
    {
        return vec![s.to_string()];
    }
    for suffix in ["ing", "ed"] {
        if let Some(s) = w.strip_suffix(suffix)
            && stem_ok(s)
        {
            return if doubled_consonant(s) {
                vec![s[..s.len() - 1].to_string()]
            } else {
                vec![s.to_string(), format!("{s}e")]
            };
        }
    }
    vec![w.to_string()]
}

/// Regular inflections of an English base form.
fn english_forms(b: &str) -> Vec<String> {
    let bytes = b.as_bytes();
    if bytes.len() < 3 {
        return Vec::new();
    }
    let last = bytes[bytes.len() - 1];
    if last == b'e' {
        let stem = &b[..b.len() - 1];
        return vec![format!("{b}s"), format!("{b}d"), format!("{stem}ing")];
    }
    if last == b'y' && !is_vowel(bytes[bytes.len() - 2]) {
        let stem = &b[..b.len() - 1];
        return vec![
            format!("{stem}ies"),
            format!("{stem}ied"),
            format!("{b}ing"),
        ];
    }
    if ["s", "x", "z", "ch", "sh"].iter().any(|e| b.ends_with(e)) {
        return vec![format!("{b}es"), format!("{b}ed"), format!("{b}ing")];
    }
    let mut out = vec![format!("{b}s"), format!("{b}ed"), format!("{b}ing")];
    if doubles_before_suffix(b) {
        let c = last as char;
        out.push(format!("{b}{c}ed"));
        out.push(format!("{b}{c}ing"));
    }
    out
}

/// Other regular forms of the plain English content words in the query (`replies` -> `reply`,
/// `deploy` -> `deploys`, `deployed`, `deploying`), because the unicode61 index does not stem.
/// Only lowercase or capitalised ASCII words of four or more letters that are not stopwords are
/// inflected; identifiers, acronyms, numbers and other scripts are left alone. Query words and
/// stopwords are never returned. At most 32 terms.
pub fn variant_terms(query: &str) -> Vec<String> {
    let words: HashSet<String> = word_terms(query).into_iter().collect();
    let mut out = TermList::new(MAX_TERMS);
    for raw in query.split(is_word_separator) {
        if out.is_full() {
            break;
        }
        let plain = (4..=MAX_TERM_CHARS).contains(&raw.len())
            && raw.bytes().all(|c| c.is_ascii_alphabetic())
            && raw.bytes().skip(1).all(|c| c.is_ascii_lowercase());
        if !plain {
            continue;
        }
        let w = raw.to_ascii_lowercase();
        if is_stopword(&w) {
            continue;
        }
        for base in english_bases(&w) {
            let forms = english_forms(&base);
            for v in std::iter::once(base).chain(forms) {
                if v.len() >= 3 && v != w && !words.contains(&v) && !is_stopword(&v) {
                    out.push(v);
                }
            }
        }
    }
    out.terms
}

/// Joined identifiers (`HostLinkReconnect` -> `hostlinkreconnect`) for exact code matches.
pub fn code_terms(query: &str) -> Vec<String> {
    let mut out = TermList::new(MAX_TERMS);
    for tok in query.split(|c: char| c.is_whitespace() || c.is_control()) {
        if out.is_full() {
            break;
        }
        let tok = tok
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '/' && c != '.');
        if tok.len() >= 3 && is_codeish(tok) {
            out.push(
                tok.chars()
                    .filter(char::is_ascii_alphanumeric)
                    .collect::<String>()
                    .to_lowercase(),
            );
        }
    }
    out.terms
}

/// Substring terms for the trigram index: whitespace-separated pieces of at least 3 characters.
/// Non-ASCII pieces always qualify; ASCII pieces only in queries of one or two words, so long
/// English questions rely on the word index, and never English stopwords (`the` is a substring
/// of nearly every text).
pub fn trigram_terms(query: &str) -> Vec<String> {
    let short_query = query.split_whitespace().nth(2).is_none();
    let mut out = TermList::new(MAX_TERMS);
    for w in pieces(query) {
        if out.is_full() {
            break;
        }
        // Cut before and after lowercasing: lowercase can add characters (`İ`).
        let lower = w
            .chars()
            .take(MAX_TERM_CHARS)
            .collect::<String>()
            .to_lowercase();
        let w: String = lower.chars().take(MAX_TERM_CHARS).collect();
        let usable = if w.is_ascii() {
            short_query && !is_stopword(&w)
        } else {
            true
        };
        if usable && w.chars().count() >= 3 {
            out.push(w);
        }
    }
    out.terms
}

/// Trigram search helps scripts without spaces and very short queries.
pub fn should_use_trigram(query: &str) -> bool {
    !query.is_ascii() || query.split_whitespace().nth(2).is_none()
}

/// Run one candidate query. Never fails: an FTS error is logged and means no matches.
fn run(conn: &Connection, sql: &str, fts_query: Option<String>, k: usize) -> Vec<i64> {
    let Some(q) = fts_query else {
        return Vec::new();
    };
    let result = (|| -> rusqlite::Result<Vec<i64>> {
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(rusqlite::params![q, k as i64], |r| r.get(0))?;
        rows.collect()
    })();
    match result {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "fts query failed; treating as no lexical matches");
            Vec::new()
        }
    }
}

/// Content words of the query and their English variants, for the unicode61 indexes. Because
/// stopwords are left out whenever the query has other words, a hit here is a content match.
fn words_query(q: &str) -> Option<String> {
    let mut terms = content_terms(q);
    terms.extend(variant_terms(q));
    or_query(&terms)
}

fn code_query(q: &str) -> Option<String> {
    or_query(&code_terms(q)).map(|o| format!("code_tokens : ({o})"))
}

pub fn memory_words(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    run(
        conn,
        "SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts, 2.0, 1.0, 1.5, 1.5) LIMIT ?2",
        words_query(q),
        k,
    )
}

pub fn memory_trigram(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    run(
        conn,
        "SELECT rowid FROM memories_tri WHERE memories_tri MATCH ?1 ORDER BY bm25(memories_tri, 2.0, 1.0) LIMIT ?2",
        or_query(&trigram_terms(q)),
        k,
    )
}

pub fn memory_code(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    run(
        conn,
        "SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts) LIMIT ?2",
        code_query(q),
        k,
    )
}

/// Raw-tier lists bound their cost per term. FTS5 computes BM25 for every row a query matches
/// before it can return the best few: a word found in 40% of 1M chunks took ~300 ms per list.
/// So every term that has a match gets its own share of the budget, max(k, `RAW_WINDOW` /
/// number of such terms) rows, and is ranked by BM25 among its newest matches up to that share
/// (FTS5 reads newest first and stops there): no term can crowd another out, and at most about
/// `RAW_WINDOW` rows are scored in all. A term with at most its share of matches is ranked over
/// all of them; matches beyond a term's share are not read (newest first: recall for very
/// common terms is not exhaustive). A share of a single word costs ~16 ms at 1M chunks; a
/// phrase term (`writer_lease` is the phrase "writer lease") also reads its words' match lists
/// once for BM25's IDF.
pub const RAW_WINDOW: usize = 5_000;

/// Rows each of `n` terms may read: an equal share of `window`, at least `k`.
fn share(window: usize, n: usize, k: usize) -> usize {
    (window / n.max(1)).max(k)
}

/// Whether the FTS query `fts` matches any row of `table` (newest first, so FTS5 stops at the
/// first). Never fails: an FTS error is logged and counts as no match.
fn has_match(conn: &Connection, table: &str, fts: &str) -> bool {
    let sql = format!("SELECT 1 FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid DESC LIMIT 1");
    let found = conn
        .prepare_cached(&sql)
        .and_then(|mut stmt| stmt.exists(rusqlite::params![fts]));
    found.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "fts query failed; treating as no lexical matches");
        false
    })
}

/// `(rowid, score)` of the best `k` rows for one candidate query, best (lowest) score first.
/// Never fails, like [`run`].
fn scored(conn: &Connection, sql: &str, fts: &str, k: usize) -> Vec<(i64, f64)> {
    let result = (|| -> rusqlite::Result<Vec<(i64, f64)>> {
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(rusqlite::params![fts, k as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "fts query failed; treating as no lexical matches");
        Vec::new()
    })
}

/// The best `k` raw chunks by BM25 for `terms` in `table`; `wrap` turns a quoted term into the
/// FTS query (a column filter for identifiers). Each term is read up to its share as
/// [`RAW_WINDOW`] describes, with `window` in its place; BM25 is a sum over the query's terms,
/// so a chunk found for several terms scores the sum of its parts.
fn raw_bm25(
    conn: &Connection,
    table: &str,
    weights: &str,
    terms: &[String],
    wrap: impl Fn(String) -> String,
    k: usize,
    window: usize,
) -> Vec<i64> {
    let present: Vec<String> = terms
        .iter()
        .map(|t| wrap(quote(t)))
        .filter(|fts| has_match(conn, table, fts))
        .collect();
    let newest_matches = format!(
        "SELECT rowid, score FROM (SELECT rowid, bm25({table}{weights}) AS score FROM {table} \
         WHERE {table} MATCH ?1 ORDER BY rowid DESC LIMIT {}) ORDER BY score LIMIT ?2",
        share(window, present.len(), k)
    );
    let mut total: HashMap<i64, f64> = HashMap::new();
    for fts in &present {
        for (id, score) in scored(conn, &newest_matches, fts, k) {
            *total.entry(id).or_default() += score;
        }
    }
    best_first(total, k, |a, b| a.total_cmp(&b))
}

/// The `k` best ids of `scores` by `better` (ties to the newer, higher id).
fn best_first(
    scores: HashMap<i64, f64>,
    k: usize,
    better: impl Fn(f64, f64) -> std::cmp::Ordering,
) -> Vec<i64> {
    let mut v: Vec<(i64, f64)> = scores.into_iter().collect();
    v.sort_by(|a, b| better(a.1, b.1).then(b.0.cmp(&a.0)));
    v.truncate(k);
    v.into_iter().map(|(id, _)| id).collect()
}

fn chunk_words_in(conn: &Connection, q: &str, k: usize, window: usize) -> Vec<i64> {
    let mut terms = content_terms(q);
    terms.extend(variant_terms(q));
    raw_bm25(conn, "chunks_fts", ", 1.0, 1.5", &terms, |o| o, k, window)
}

pub fn chunk_words(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    chunk_words_in(conn, q, k, RAW_WINDOW)
}

fn chunk_code_in(conn: &Connection, q: &str, k: usize, window: usize) -> Vec<i64> {
    let in_code = |o: String| format!("code_tokens : ({o})");
    raw_bm25(conn, "chunks_fts", "", &code_terms(q), in_code, k, window)
}

pub fn chunk_code(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    chunk_code_in(conn, q, k, RAW_WINDOW)
}

/// Raw chunks holding query substrings. A term in a script without spaces (Thai, CJK), for
/// which this is the only lexical list, reads its newest matches up to its own share of the
/// window, max(k, `RAW_WINDOW` / number of such terms), so its rarity is measured up to that
/// share. An ASCII term reads its newest k only: it also has its own share in the word list, and
/// reading the share of a rare ASCII phrase (`lease_token_frame`, a made-up word) means scanning
/// the intersection of common trigrams, 15–45 ms per term at 1M chunks. A chunk scores the sum,
/// over the terms that matched it, of 1 / that term's match count (capped at what it read): a
/// chunk with a substring found in fewer chunks ranks above one with a more common substring,
/// ties to the newer chunk. Matches beyond a term's limit are not read (newest first). Not
/// BM25: its IDF needs every match of each phrase, 60–400 ms per list at 1M chunks. `k` is 100,
/// or 400 when filters apply.
pub fn chunk_trigram(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    chunk_trigram_in(conn, q, k, RAW_WINDOW)
}

fn chunk_trigram_in(conn: &Connection, q: &str, k: usize, window: usize) -> Vec<i64> {
    let terms = trigram_terms(q);
    let unspaced = terms.iter().filter(|t| !t.is_ascii()).count();
    let mut total: HashMap<i64, f64> = HashMap::new();
    for t in terms {
        let limit = if t.is_ascii() {
            k
        } else {
            share(window, unspaced, k)
        };
        let ids = run(
            conn,
            "SELECT rowid FROM chunks_tri WHERE chunks_tri MATCH ?1 ORDER BY rowid DESC LIMIT ?2",
            Some(quote(&t)),
            limit,
        );
        let weight = 1.0 / ids.len().max(1) as f64;
        for id in ids {
            *total.entry(id).or_default() += weight;
        }
    }
    best_first(total, k, |a, b| b.total_cmp(&a))
}

/// Non-ASCII terms of 1–2 code points (e.g. Thai "จำ" = จ + ำ) are below the trigram minimum.
/// At most 8 terms.
pub fn short_terms(query: &str) -> Vec<String> {
    let mut out = TermList::new(8);
    for w in pieces(query) {
        if out.is_full() {
            break;
        }
        if !w.is_ascii() && !longer_than(w, 2) {
            out.push(w.to_lowercase());
        }
    }
    out.terms
}

fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// LIKE scan of the knowledge tier for short non-ASCII terms (newest first). Never fails.
pub fn memory_like(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    let terms = short_terms(q);
    if terms.is_empty() {
        return Vec::new();
    }
    let clause =
        vec!["(title LIKE ? ESCAPE '\\' OR body LIKE ? ESCAPE '\\')"; terms.len()].join(" OR ");
    let sql = format!(
        "SELECT seq FROM memories WHERE status <> 'trashed' AND ({clause}) ORDER BY seq DESC LIMIT {k}"
    );
    let params: Vec<String> = terms
        .iter()
        .flat_map(|t| {
            let p = like_pattern(t);
            [p.clone(), p]
        })
        .collect();
    let result = (|| -> rusqlite::Result<Vec<i64>> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| r.get(0))?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "like scan failed; treating as no matches");
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::{memories, open_writer, projects, sources};

    #[test]
    fn terms_are_quoted_and_operators_are_inert() {
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        let q = or_query(&word_terms("NEAR(foo AND -bar*) \"")).unwrap();
        assert_eq!(q, "\"near\" OR \"foo\" OR \"and\" OR \"bar\"");
        assert!(word_terms("😀 !!").is_empty());
        assert_eq!(or_query(&[]), None);
        // Control characters separate words instead of reaching the FTS parser.
        assert_eq!(
            word_terms("a\u{0}b"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn code_terms_come_from_identifiers() {
        assert!(word_terms("why HostLinkReconnect").contains(&"reconnect".to_string()));
        assert_eq!(
            code_terms("fix writer_lease now"),
            vec!["writerlease".to_string()]
        );
        assert!(code_terms("plain words").is_empty());
    }

    #[test]
    fn trigram_rules() {
        assert!(should_use_trigram("ความจำ"));
        assert!(should_use_trigram("lease timeout"));
        assert!(!should_use_trigram(
            "why does the writer lease time out so often"
        ));
        assert_eq!(trigram_terms("AI ความจำ go"), vec!["ความจำ".to_string()]);
    }

    #[test]
    fn english_inflections_become_extra_terms() {
        let v = variant_terms("which replies");
        assert!(v.contains(&"reply".to_string()), "{v:?}");
        assert!(
            !v.contains(&"replies".to_string()),
            "word terms are not repeated: {v:?}"
        );
        for (word, want) in [
            ("deploy", "deploys"),
            ("loop", "looped"),
            ("frames", "frame"),
            ("fixes", "fix"),
            ("capped", "cap"),
            ("timing", "time"),
            ("saved", "save"),
            ("entry", "entries"),
            ("added", "add"),
            ("stopping", "stop"),
            ("stop", "stopped"),
        ] {
            let v = variant_terms(word);
            assert!(
                v.contains(&want.to_string()),
                "{word} -> {want} missing: {v:?}"
            );
        }
        // Short words, identifiers, digits and non-English text get no guesses.
        for q in [
            "use",
            "AI",
            "v4",
            "writer_lease",
            "HostLinkReconnect",
            "ความจำ",
            "x86s",
        ] {
            assert!(variant_terms(q).is_empty(), "{q}: {:?}", variant_terms(q));
        }
        let many = "alpha bravo charlie delta echoes foxtrot golfing hotel india juliet kilos lima";
        assert!(word_terms(many).len() + variant_terms(many).len() <= 64);
        assert!(!variant_terms("added").contains(&"ad".to_string()));
    }

    #[test]
    fn term_builders_stop_at_the_cap_on_huge_queries() {
        // Thousands of distinct words: English-like, Thai and identifiers.
        let latin = |mut i: usize| {
            let mut w = String::new();
            for _ in 0..5 {
                w.push((b'a' + (i % 26) as u8) as char);
                i /= 26;
            }
            w
        };
        let thai = |mut i: usize| {
            let mut w = String::new();
            for _ in 0..4 {
                w.push(char::from_u32(0x0E01 + (i % 40) as u32).unwrap());
                i /= 40;
            }
            w
        };
        let mut q = String::new();
        for i in 0..5_000 {
            q.push_str(&format!("{} {} v{i}Frame ", latin(i), thai(i)));
        }
        let t = std::time::Instant::now();
        let sizes = [
            word_terms(&q).len(),
            variant_terms(&q).len(),
            code_terms(&q).len(),
            trigram_terms(&q).len(),
            content_terms(&q).len(),
            short_terms(&q).len(),
        ];
        let took = t.elapsed();
        assert_eq!(sizes[..5], [MAX_TERMS; 5], "{sizes:?}");
        assert!(sizes[5] <= MAX_TERMS);
        assert!(took < std::time::Duration::from_millis(200), "{took:?}");
    }

    #[test]
    fn stopwords_leave_the_lists_when_content_remains() {
        assert_eq!(
            content_terms("why do diary writes time out"),
            vec!["diary".to_string(), "writes".into(), "time".into()]
        );
        assert_eq!(
            word_terms("why do diary writes time out").len(),
            6,
            "word_terms keeps every word"
        );
        assert_eq!(
            content_terms("what is this"),
            word_terms("what is this"),
            "only stopwords: all are kept"
        );
        let v = variant_terms("which replies does");
        assert!(v.contains(&"reply".to_string()), "{v:?}");
        assert!(
            v.iter()
                .all(|t| !t.starts_with("which") && t != "doing" && t != "do"),
            "variants only for content words: {v:?}"
        );
        assert_eq!(trigram_terms("the api"), vec!["api".to_string()]);
        assert!(trigram_terms("what is").is_empty());
        assert_eq!(trigram_terms("ไทย the"), vec!["ไทย".to_string()]);
        let (_d, c) = db_with(&["what is the plan for the move", "frobnicator calibration"]);
        assert_eq!(memory_words(&c, "what is the frobnicator", 10).len(), 1);
        assert_eq!(chunk_words(&c, "what is the frobnicator", 10).len(), 0);
        assert_eq!(
            memory_words(&c, "what is the", 10).len(),
            1,
            "only stopwords still match"
        );
        assert!(memory_trigram(&c, "the api", 10).is_empty());
    }

    #[test]
    fn raw_word_lists_rank_by_bm25_within_the_newest_matches() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        // The oldest chunk is the best BM25 match; the others score lower the longer they are.
        let texts = [
            "lease lease lease renewal",
            "lease a1",
            "lease b1 b2",
            "lease c1 c2 c3",
            "lease d1 d2 d3 d4",
            "lease HostLinkReconnect e2 e3 e4 e5",
        ];
        let chunks: Vec<sources::ChunkInput> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| sources::ChunkInput {
                ord: i as i64,
                text: t.to_string(),
                ..Default::default()
            })
            .collect();
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            &chunks,
        )
        .unwrap();
        let seqs: Vec<i64> = c
            .prepare("SELECT seq FROM chunks ORDER BY ord")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let full: Vec<i64> = c
            .prepare("SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH '\"lease\"' ORDER BY bm25(chunks_fts, 1.0, 1.5) LIMIT 10")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(full.len(), 6);
        assert_eq!(full[0], seqs[0], "the oldest chunk is the best match");
        // Fewer matches than the window: exactly the plain BM25 order.
        assert_eq!(chunk_words_in(&c, "lease", 10, 6), full);
        assert_eq!(chunk_words(&c, "lease", 10), full);
        assert_eq!(chunk_words_in(&c, "lease", 2, 100), full[..2]);
        // More matches than the window: only the three newest compete, still by BM25.
        let newest: Vec<i64> = full
            .iter()
            .copied()
            .filter(|s| seqs[3..].contains(s))
            .collect();
        assert_eq!(chunk_words_in(&c, "lease", 2, 3), newest[..2]);
        // A term's share is never below k: with k = 10 all six compete again.
        assert_eq!(chunk_words_in(&c, "lease", 10, 3), full);
        // The code list uses the same window.
        assert_eq!(chunk_code(&c, "HostLinkReconnect", 10), vec![seqs[5]]);
        // The trigram list returns the newest substring matches first.
        let newest_first: Vec<i64> = seqs.iter().rev().copied().collect();
        assert_eq!(chunk_trigram(&c, "lease", 10), newest_first);
        assert_eq!(chunk_trigram(&c, "lease", 2), newest_first[..2]);
        assert_eq!(chunk_trigram(&c, "renewal", 10), vec![seqs[0]]);
    }

    /// Review finding I1: a common term must not push an old chunk holding a rare term out of
    /// the raw lists. Ten new chunks share a common word, identifier and Thai word; only the
    /// oldest holds the rare ones. With a window of 3 and k = 3, the oldest still comes first.
    #[test]
    fn a_common_term_never_hides_an_old_chunk_with_a_rare_term() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let mut texts = vec![
            "lease frobnicate writer_lease HostLinkReconnect ภาษาไทย ความจำเอเจนต์".to_string(),
        ];
        texts.extend((0..10).map(|i| format!("lease writer_lease ภาษาไทย note{i}")));
        let chunks: Vec<sources::ChunkInput> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| sources::ChunkInput {
                ord: i as i64,
                text: t.clone(),
                ..Default::default()
            })
            .collect();
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            &chunks,
        )
        .unwrap();
        let oldest: i64 = c
            .query_row("SELECT seq FROM chunks WHERE ord = 0", [], |r| r.get(0))
            .unwrap();
        let words = chunk_words_in(&c, "lease frobnicate", 3, 3);
        assert_eq!(words.first(), Some(&oldest), "word list: {words:?}");
        let code = chunk_code_in(&c, "writer_lease HostLinkReconnect", 3, 3);
        assert_eq!(code.first(), Some(&oldest), "code list: {code:?}");
        let thai = chunk_trigram(&c, "ภาษาไทย ความจำเอเจนต์", 3);
        assert_eq!(thai.first(), Some(&oldest), "trigram list: {thai:?}");
        // Each list still returns k results, the common term's newest matches after it.
        assert_eq!((words.len(), code.len(), thai.len()), (3, 3, 3));
    }

    /// Ingests `texts` as one source, oldest first; returns their seqs in the same order.
    fn chunks_db(texts: &[String]) -> (tempfile::TempDir, rusqlite::Connection, Vec<i64>) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let chunks: Vec<sources::ChunkInput> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| sources::ChunkInput {
                ord: i as i64,
                text: t.clone(),
                ..Default::default()
            })
            .collect();
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            &chunks,
        )
        .unwrap();
        let seqs: Vec<i64> = c
            .prepare("SELECT seq FROM chunks ORDER BY ord")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (d, c, seqs)
    }

    /// Fix round 2 (ruling): every common term gets its own share of the window,
    /// max(k, window / common terms). Two common terms, window 10, k 3: each reads its newest
    /// 5 matches. The less common term's best chunk is older than all the other term's matches
    /// but inside its own newest 5, so it is found (one shared window of 10 held only the other
    /// term's chunks).
    #[test]
    fn each_common_term_gets_its_own_share_of_the_window() {
        let mut texts: Vec<String> = (0..11)
            .map(|i| format!("bbb filler{i} filler filler filler"))
            .collect();
        texts[8] = "bbb bbb bbb".into(); // the best bbb chunk: among bbb's newest 5 (6..=10)
        texts.extend((0..15).map(|i| format!("aaa other{i}")));
        let (_d, c, seqs) = chunks_db(&texts);
        let found = chunk_words_in(&c, "aaa bbb", 3, 10);
        assert_eq!(found.first(), Some(&seqs[8]), "{found:?}");
        // Outside its share (the 6th newest bbb match) a chunk is not read: newest first.
        let mut older = texts.clone();
        older[8] = "bbb filler8 filler filler filler".into();
        older[5] = "bbb bbb bbb".into();
        let (_d2, c2, seqs2) = chunks_db(&older);
        let found = chunk_words_in(&c2, "aaa bbb", 3, 10);
        assert!(!found.contains(&seqs2[5]), "{found:?}");
    }

    /// Terms without a match do not take a share: with four absent words beside two present
    /// ones, each present term still reads window / 2 rows (the 4th newest bbb match is found).
    #[test]
    fn terms_without_a_match_take_no_share() {
        let mut texts: Vec<String> = (0..11)
            .map(|i| format!("bbb filler{i} filler filler filler"))
            .collect();
        texts[7] = "bbb bbb bbb".into();
        texts.extend((0..15).map(|i| format!("aaa other{i}")));
        let (_d, c, seqs) = chunks_db(&texts);
        let found = chunk_words_in(&c, "aaa bbb nosuch1 nosuch2 nosuch3 nosuch4", 3, 10);
        assert_eq!(found.first(), Some(&seqs[7]), "{found:?}");
    }

    /// Fix round 2: trigram rarity is measured up to each term's share of the window, not just
    /// k. A Thai word in 5 old chunks (more than k = 3) beside a common one in 15 newer chunks
    /// (more than its share, 10): the rare word's chunks outrank the common word's.
    #[test]
    fn a_rare_substring_in_more_than_k_chunks_still_outranks_a_common_one() {
        let mut texts: Vec<String> = (0..5).map(|i| format!("ความจำเอเจนต์ x{i}")).collect();
        texts.extend((0..15).map(|i| format!("ภาษาไทย y{i}")));
        let (_d, c, seqs) = chunks_db(&texts);
        let found = chunk_trigram_in(&c, "ภาษาไทย ความจำเอเจนต์", 3, 20);
        assert_eq!(found.len(), 3);
        assert!(found.iter().all(|s| seqs[..5].contains(s)), "{found:?}");
    }

    #[test]
    fn walls_of_text_give_bounded_terms() {
        let wall = "ก".repeat(20_000);
        assert!(
            word_terms(&wall).is_empty(),
            "no word is 20,000 letters long"
        );
        assert_eq!(trigram_terms(&wall), vec!["ก".repeat(64)]);
        let words =
            "lorem ".repeat(5_000) + &(0..500).map(|i| format!("w{i} ")).collect::<String>();
        assert!(word_terms(&words).len() <= 32);
        assert!(variant_terms(&words).len() <= 32);
        let idents: String = (0..1_000).map(|i| format!("a_{i} ")).collect();
        assert_eq!(code_terms(&idents).len(), 32);
    }

    fn db_with(texts: &[&str]) -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        for t in texts {
            let m = memories::prepare(
                memories::NewMemoryInput {
                    content: (*t).into(),
                    ..Default::default()
                },
                &p,
                "h".into(),
            )
            .unwrap();
            memories::insert(&c, &m).unwrap();
        }
        (d, c)
    }

    /// Every FTS5 operator and quoting mistake from Review Focus #1, plus short and Thai input.
    const HOSTILE: [&str; 22] = [
        "\"",
        "*",
        "-",
        ":",
        "(",
        ")",
        "NEAR",
        "NEAR(a b)",
        "AND",
        "AND OR NOT",
        "a:b",
        "x\"y",
        "\"unclosed",
        "foo*",
        "^start",
        "col:term",
        "AI",
        "ไทย",
        "😀",
        "''",
        "\u{0}",
        "\u{0E31}",
    ];

    #[test]
    fn hostile_queries_never_error() {
        let (_d, c) = db_with(&["writer lease timeout", "ระบบความจำสำหรับเอเจนต์"]);
        for q in HOSTILE {
            let _ = memory_words(&c, q, 10);
            let _ = memory_trigram(&c, q, 10);
            let _ = memory_code(&c, q, 10);
            let _ = memory_like(&c, q, 10);
            for fq in [
                or_query(&word_terms(q)),
                or_query(&variant_terms(q)),
                or_query(&trigram_terms(q)),
            ]
            .into_iter()
            .flatten()
            {
                for table in ["memories_fts", "memories_tri"] {
                    let sql = format!("SELECT count(*) FROM {table} WHERE {table} MATCH ?1");
                    let r: rusqlite::Result<i64> = c.query_row(&sql, [&fq], |r| r.get(0));
                    assert!(r.is_ok(), "{q:?} built an invalid FTS query {fq:?}: {r:?}");
                }
            }
        }
        assert_eq!(memory_trigram(&c, "ความจำ", 10).len(), 1);
        assert_eq!(memory_words(&c, "lease", 10).len(), 1);
        assert_eq!(
            memory_trigram(&c, "จำ", 10).len(),
            0,
            "2 code points: below trigram minimum"
        );
        assert_eq!(memory_like(&c, "จำ", 10).len(), 1, "LIKE fallback finds it");
        assert!(
            memory_like(&c, "%", 10).is_empty(),
            "ASCII and wildcards are not LIKE-scanned"
        );
    }

    #[test]
    fn operators_are_searched_as_literal_words() {
        let (_d, c) = db_with(&["NEAR the AND gate", "unrelated text"]);
        for q in [
            "NEAR",
            "AND",
            "-gate",
            "gate*",
            "NEAR(gate)",
            "\"gate",
            "the:gate",
            "AND OR NOT",
        ] {
            assert_eq!(memory_words(&c, q, 10).len(), 1, "{q}");
        }
        // "-" is not NOT: a query for the other memory still finds it.
        assert_eq!(memory_words(&c, "unrelated -gate", 10).len(), 2);
    }

    #[test]
    fn inflected_queries_find_base_forms_and_back() {
        let (_d, c) = db_with(&[
            "Always reply to the owner in Thai",
            "Production deploys go to Easypanel",
        ]);
        assert_eq!(memory_words(&c, "which replies", 10).len(), 1);
        assert_eq!(memory_words(&c, "deploy", 10).len(), 1);
    }

    #[test]
    fn code_queries_match_identifier_tokens() {
        let (_d, c) = db_with(&[
            "HostLinkReconnect looped",
            "fix in writer_lease.rs",
            "plain",
        ]);
        assert_eq!(memory_code(&c, "HostLinkReconnect", 10).len(), 1);
        assert_eq!(memory_code(&c, "writer_lease.rs", 10).len(), 1);
        assert!(memory_code(&c, "plain", 10).is_empty());
    }

    #[test]
    fn chunk_lists_search_the_raw_tier() {
        let (_d, c) = db_with(&[]);
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            &[
                sources::ChunkInput {
                    ord: 0,
                    text: "HostLinkReconnect frobnicator ความจำระยะยาว".into(),
                    ..Default::default()
                },
                sources::ChunkInput {
                    ord: 1,
                    text: "nothing to see".into(),
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        assert_eq!(chunk_words(&c, "frobnicator", 10).len(), 1);
        assert_eq!(chunk_trigram(&c, "ความจำ", 10).len(), 1);
        assert_eq!(chunk_code(&c, "HostLinkReconnect", 10).len(), 1);
        for q in HOSTILE {
            let _ = (
                chunk_words(&c, q, 10),
                chunk_trigram(&c, q, 10),
                chunk_code(&c, q, 10),
            );
        }
    }
}
