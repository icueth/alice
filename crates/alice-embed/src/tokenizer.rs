//! The XLM-R Unigram tokenizer of multilingual-e5, without the Hugging Face `tokenizers` trie.
//!
//! `tokenizers` builds one hash map per trie node for the 250k-piece vocabulary (~250 MB
//! resident). This keeps the pieces in one hash map (~15 MB) and tries every prefix length at
//! each position, which builds the same lattice. It reproduces tokenizers 0.23 on this model's
//! `tokenizer.json` (checked by `tests/onnx_equivalence.rs`): the Precompiled (nmt_nfkc)
//! normalizer, runs of spaces collapsed, Metaspace (`▁` replaces spaces and is prepended; words
//! start at each `▁`), Unigram Viterbi with ties kept by the first path found, unknown
//! characters scored `min_score - 10` and consecutive unknowns in a word fused into one,
//! `<s> … </s>` around the ids, and truncation to `max_len` ids including those two.
//! Literal special-token text such as `</s>` in a passage is treated as text (Hugging Face
//! would emit the special id there). A `tokenizer.json` with any other pipeline is refused.
//!
//! ```
//! use alice_embed::tokenizer::UnigramTokenizer;
//! let json = r#"{
//!   "normalizer": {"type": "Replace", "pattern": {"Regex": " {2,}"}, "content": " "},
//!   "pre_tokenizer": {"type": "Metaspace", "replacement": "▁", "add_prefix_space": true},
//!   "post_processor": {"type": "TemplateProcessing", "single": [
//!     {"SpecialToken": {"id": "<s>", "type_id": 0}}, {"Sequence": {"id": "A", "type_id": 0}},
//!     {"SpecialToken": {"id": "</s>", "type_id": 0}}]},
//!   "model": {"type": "Unigram", "unk_id": 3, "vocab": [
//!     ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0], ["▁hi", -1.0]]}}"#;
//! let t = UnigramTokenizer::from_json(json.as_bytes()).unwrap();
//! assert_eq!(t.encode("hi", 512), vec![0, 4, 2]);
//! assert_eq!(t.encode("", 512), vec![0, 2]);
//! ```

use alice_core::error::{AliceError, Result};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

const UNK_PENALTY: f64 = 10.0;
const SPECIALS: [&str; 5] = ["<s>", "<pad>", "</s>", "<unk>", "<mask>"];

pub struct UnigramTokenizer {
    pieces: HashMap<Box<str>, u32>,
    scores: Vec<f64>,
    unk_id: u32,
    unk_score: f64,
    max_piece_chars: usize,
    precompiled: Option<spm_precompiled::Precompiled>,
    bos: u32,
    eos: u32,
}

/// The model type alone: read first, so another kind of file is refused before its
/// vocabulary is parsed (and without building a JSON tree of 250k entries).
#[derive(Deserialize)]
struct Head {
    model: HeadModel,
}

#[derive(Deserialize)]
struct HeadModel {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct FileJson<'a> {
    #[serde(default)]
    added_tokens: Vec<AddedToken>,
    #[serde(default, borrow)]
    normalizer: Option<NormalizerJson<'a>>,
    #[serde(default)]
    pre_tokenizer: Option<serde_json::Value>,
    #[serde(default)]
    post_processor: Option<serde_json::Value>,
    model: ModelJson,
}

#[derive(Deserialize)]
struct AddedToken {
    content: String,
}

/// A normalizer as far as this tokenizer reads it: its type, a Precompiled one's charsmap
/// (base64; `spm_precompiled` decodes it from a borrowed string) and a Sequence's members.
/// Other fields are ignored.
#[derive(Deserialize)]
struct NormalizerJson<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default, borrow)]
    precompiled_charsmap: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    normalizers: Vec<NormalizerJson<'a>>,
    #[serde(default)]
    pattern: Option<serde_json::Value>,
    #[serde(default, borrow)]
    content: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ModelJson {
    #[serde(default)]
    unk_id: Option<u32>,
    #[serde(default)]
    byte_fallback: Option<bool>,
    vocab: Vec<(String, f64)>,
}

fn broken(why: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!(
        "the tokenizer file cannot be used ({why}); run \"alice model fetch\" to repair the model files"
    ))
}

/// The Precompiled normalizer, decoded by the crate's own deserializer as `tokenizers` does.
/// It reads the base64 text as a borrowed string, which `from_str` can give (a `Value` cannot).
fn precompiled(n: &NormalizerJson) -> Result<spm_precompiled::Precompiled> {
    let charsmap = n
        .precompiled_charsmap
        .as_deref()
        .ok_or_else(|| broken("a Precompiled normalizer without its charsmap"))?;
    let json = format!(
        r#"{{"type":"Precompiled","precompiled_charsmap":{}}}"#,
        serde_json::to_string(charsmap).map_err(broken)?
    );
    serde_json::from_str(&json).map_err(broken)
}

/// `Replace(" {2,}" → " ")`: runs of spaces collapsed to one.
fn collapses_spaces(n: &NormalizerJson) -> bool {
    n.kind == "Replace"
        && n.pattern
            .as_ref()
            .and_then(|p| p.get("Regex"))
            .and_then(|r| r.as_str())
            == Some(" {2,}")
        && n.content.as_deref() == Some(" ")
}

/// The normalizer this tokenizer implements: an optional Precompiled one, then runs of spaces
/// collapsed (alone or as a Sequence). Returns the Precompiled one.
fn normalizer(n: Option<&NormalizerJson>) -> Result<Option<spm_precompiled::Precompiled>> {
    let wrong = || {
        broken(
            "its normalizer is not the one this tokenizer implements: an optional Precompiled \
             normalizer, then Replace(\" {2,}\" with \" \")",
        )
    };
    let n = n.ok_or_else(wrong)?;
    let members: Vec<&NormalizerJson> = if n.kind == "Sequence" {
        n.normalizers.iter().collect()
    } else {
        vec![n]
    };
    let (found, rest) = match members.split_first() {
        Some((first, rest)) if first.kind == "Precompiled" => (Some(precompiled(first)?), rest),
        _ => (None, &members[..]),
    };
    match rest {
        [only] if collapses_spaces(only) => Ok(found),
        _ => Err(wrong()),
    }
}

/// Metaspace with `▁`, prepended to the text (legacy `add_prefix_space: true` or
/// `prepend_scheme: "always"`), words split at it.
fn check_pre_tokenizer(p: Option<&serde_json::Value>) -> Result<()> {
    let absent_or = |p: &serde_json::Value, key: &str, ok: &dyn Fn(&serde_json::Value) -> bool| {
        p.get(key).is_none_or(|v| v.is_null() || ok(v))
    };
    let fits = p.is_some_and(|p| {
        p["type"].as_str() == Some("Metaspace")
            && p["replacement"].as_str() == Some("▁")
            && absent_or(p, "add_prefix_space", &|v| v.as_bool() == Some(true))
            && absent_or(p, "prepend_scheme", &|v| v.as_str() == Some("always"))
            && absent_or(p, "split", &|v| v.as_bool() == Some(true))
    });
    if fits {
        Ok(())
    } else {
        Err(broken(
            "its pre-tokenizer is not the one this tokenizer implements: Metaspace with \"▁\" \
             prepended and words split at it",
        ))
    }
}

/// `<s> $A </s>`, with the vocabulary's ids for the two special tokens.
fn check_post_processor(p: Option<&serde_json::Value>, bos: u32, eos: u32) -> Result<()> {
    let wrong = || {
        broken(
            "its post-processor is not the one this tokenizer implements: the text between \
             <s> and </s>",
        )
    };
    let p = p.ok_or_else(wrong)?;
    if p["type"].as_str() != Some("TemplateProcessing") {
        return Err(wrong());
    }
    let single: Vec<(&str, &str)> = p["single"]
        .as_array()
        .ok_or_else(wrong)?
        .iter()
        .map(
            |piece| match (piece.get("SpecialToken"), piece.get("Sequence")) {
                (Some(t), None) => ("special", t["id"].as_str().unwrap_or("")),
                (None, Some(t)) => ("sequence", t["id"].as_str().unwrap_or("")),
                _ => ("other", ""),
            },
        )
        .collect();
    if single != [("special", "<s>"), ("sequence", "A"), ("special", "</s>")] {
        return Err(wrong());
    }
    for (name, id) in [("<s>", bos), ("</s>", eos)] {
        let ids = p
            .get("special_tokens")
            .and_then(|t| t.get(name))
            .and_then(|t| t.get("ids"));
        if ids.is_some_and(|ids| *ids != serde_json::json!([id])) {
            return Err(wrong());
        }
    }
    Ok(())
}

impl UnigramTokenizer {
    pub fn from_file(path: &Path) -> Result<Self> {
        Self::from_json(&std::fs::read(path)?)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let head: Head = serde_json::from_slice(bytes).map_err(broken)?;
        if head.model.kind != "Unigram" {
            return Err(broken(format!("a {} model, not Unigram", head.model.kind)));
        }
        let f: FileJson = serde_json::from_slice(bytes).map_err(broken)?;
        let precompiled = normalizer(f.normalizer.as_ref())?;
        check_pre_tokenizer(f.pre_tokenizer.as_ref())?;
        if f.model.byte_fallback == Some(true) {
            return Err(broken(
                "its model uses byte fallback, which this tokenizer does not implement",
            ));
        }
        if let Some(t) = f
            .added_tokens
            .iter()
            .find(|t| !SPECIALS.contains(&t.content.as_str()))
        {
            return Err(broken(format!(
                "it has added tokens other than {SPECIALS:?}, such as {:?}, which this tokenizer \
                 does not implement",
                t.content
            )));
        }
        let vocab = f.model.vocab;
        let mut pieces = HashMap::with_capacity(vocab.len());
        let mut scores = Vec::with_capacity(vocab.len());
        let (mut max_chars, mut min_score) = (1usize, f64::INFINITY);
        let mut special_ids = HashMap::new();
        for (i, (piece, score)) in vocab.into_iter().enumerate() {
            let id = u32::try_from(i).map_err(|_| broken("too many pieces"))?;
            scores.push(score);
            min_score = min_score.min(score);
            if SPECIALS.contains(&piece.as_str()) {
                special_ids.insert(piece, id);
                continue; // never matched from text
            }
            max_chars = max_chars.max(piece.chars().count());
            pieces.insert(piece.into_boxed_str(), id);
        }
        let id_of = |name: &str| {
            special_ids
                .get(name)
                .copied()
                .ok_or_else(|| broken(format!("no {name} token")))
        };
        let unk_id = match f.model.unk_id {
            Some(id) => id,
            None => id_of("<unk>")?,
        };
        if unk_id as usize >= scores.len() {
            return Err(broken(format!("unk_id {unk_id} is not in the vocabulary")));
        }
        let (bos, eos) = (id_of("<s>")?, id_of("</s>")?);
        check_post_processor(f.post_processor.as_ref(), bos, eos)?;
        Ok(Self {
            pieces,
            scores,
            unk_id,
            unk_score: min_score - UNK_PENALTY,
            max_piece_chars: max_chars,
            precompiled,
            bos,
            eos,
        })
    }

    /// Normalizer and Metaspace: the string whose `▁`-led words the model encodes.
    pub fn normalize(&self, text: &str) -> String {
        let s = match &self.precompiled {
            Some(p) => p.normalize_string(text),
            None => text.to_string(),
        };
        let mut out = String::with_capacity(s.len() + 3);
        let mut prev_space = false;
        for c in s.chars() {
            if c == ' ' {
                if !prev_space {
                    out.push('▁');
                }
                prev_space = true;
            } else {
                out.push(c);
                prev_space = false;
            }
        }
        // Metaspace prepends to a non-empty string only (`tokenizers` prepends nothing to an
        // empty one), so empty text encodes as `<s> </s>`.
        if !out.is_empty() && !out.starts_with('▁') {
            out.insert(0, '▁');
        }
        out
    }

    /// Token ids of `text`, `<s>` and `</s>` included, at most `max_len` (at least 2) of them.
    /// `normalize` starts every non-empty string with `▁`, so every word starts at a `▁`.
    pub fn encode(&self, text: &str, max_len: usize) -> Vec<u32> {
        let normalized = self.normalize(text);
        let starts: Vec<usize> = normalized
            .char_indices()
            .filter(|(_, c)| *c == '▁')
            .map(|(i, _)| i)
            .collect();
        let mut ids = vec![self.bos];
        for (n, &start) in starts.iter().enumerate() {
            let end = starts.get(n + 1).copied().unwrap_or(normalized.len());
            self.encode_word(&normalized[start..end], &mut ids);
            if ids.len() >= max_len {
                break;
            }
        }
        ids.truncate(max_len.saturating_sub(1).max(1));
        ids.push(self.eos);
        ids
    }

    fn encode_word(&self, word: &str, out: &mut Vec<u32>) {
        let bounds: Vec<usize> = word
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(word.len()))
            .collect();
        let n = bounds.len() - 1;
        // best[i] = (score, start, id) of the best path ending at char i.
        let mut best: Vec<Option<(f64, usize, u32)>> = vec![None; n + 1];
        best[0] = Some((0.0, 0, 0));
        for start in 0..n {
            let Some((base, _, _)) = best[start] else {
                continue;
            };
            let mut has_single = false;
            for len in 1..=self.max_piece_chars.min(n - start) {
                let end = start + len;
                if let Some(&id) = self.pieces.get(&word[bounds[start]..bounds[end]]) {
                    let score = base + self.scores[id as usize];
                    if best[end].is_none_or(|(s, _, _)| score > s) {
                        best[end] = Some((score, start, id));
                    }
                    has_single |= len == 1;
                }
            }
            if !has_single {
                let score = base + self.unk_score;
                if best[start + 1].is_none_or(|(s, _, _)| score > s) {
                    best[start + 1] = Some((score, start, self.unk_id));
                }
            }
        }
        let mut ids = Vec::new();
        let mut pos = n;
        while pos > 0 {
            // Every position is reachable: an unknown character always extends a path by one.
            let Some((_, start, id)) = best[pos] else {
                unreachable!("char boundary {pos} of {n} has no path")
            };
            ids.push(id);
            pos = start;
        }
        ids.reverse();
        let mut prev_unk = false;
        for id in ids {
            let unk = id == self.unk_id;
            if !(unk && prev_unk) {
                out.push(id);
            }
            prev_unk = unk;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny Unigram vocabulary in the shape of the real tokenizer.json (its pre-tokenizer
    /// and post-processor; no Precompiled normalizer: identity), so Viterbi, Metaspace,
    /// unknowns and truncation are tested offline.
    fn tiny_json() -> serde_json::Value {
        serde_json::json!({
            "normalizer": {"type": "Sequence", "normalizers": [
                {"type": "Replace", "pattern": {"Regex": " {2,}"}, "content": " "}
            ]},
            "pre_tokenizer": {"type": "Metaspace", "replacement": "▁", "add_prefix_space": true},
            "post_processor": {"type": "TemplateProcessing",
                "single": [
                    {"SpecialToken": {"id": "<s>", "type_id": 0}},
                    {"Sequence": {"id": "A", "type_id": 0}},
                    {"SpecialToken": {"id": "</s>", "type_id": 0}}
                ],
                "pair": [],
                "special_tokens": {
                    "</s>": {"id": "</s>", "ids": [2], "tokens": ["</s>"]},
                    "<s>": {"id": "<s>", "ids": [0], "tokens": ["<s>"]}
                }},
            "model": {"type": "Unigram", "unk_id": 3, "vocab": [
                ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
                ["▁", -2.0], ["▁hello", -3.0], ["▁hell", -4.0], ["o", -4.5],
                ["▁world", -3.5], ["h", -6.0], ["e", -6.0], ["l", -6.0], ["w", -6.0],
                ["r", -6.0], ["d", -6.0], ["▁ไทย", -5.0], ["ไ", -7.0], ["ท", -7.0], ["ย", -7.0]
            ]}
        })
    }

    fn tiny() -> UnigramTokenizer {
        UnigramTokenizer::from_json(tiny_json().to_string().as_bytes()).unwrap()
    }

    /// Review M2: like Hugging Face (Metaspace prepends nothing to an empty string), empty
    /// text is `<s> </s>`.
    #[test]
    fn empty_text_is_just_the_special_tokens() {
        let t = tiny();
        assert_eq!(t.normalize(""), "");
        assert_eq!(t.encode("", 512), vec![0, 2]);
        assert_eq!(
            t.encode(" ", 512),
            vec![0, 4, 2],
            "one space is a word of its own"
        );
    }

    /// Review M3: a tokenizer.json whose pipeline is not the one implemented here is refused
    /// with the next step, instead of being tokenized differently from Hugging Face.
    #[test]
    fn a_tokenizer_file_with_another_pipeline_is_refused() {
        let change = |path: &[&str], value: serde_json::Value| {
            let mut j = tiny_json();
            let (last, parents) = path.split_last().unwrap();
            let mut at = &mut j;
            for p in parents {
                at = match p.parse::<usize>() {
                    Ok(i) => &mut at[i],
                    Err(_) => &mut at[*p],
                };
            }
            match value {
                serde_json::Value::Null => {
                    at.as_object_mut().unwrap().remove(*last);
                }
                v => at[*last] = v,
            }
            j
        };
        use serde_json::json;
        let cases = [
            (
                "normalizer",
                change(&["normalizer"], serde_json::Value::Null),
            ),
            (
                "normalizer",
                change(
                    &["normalizer", "normalizers", "0", "pattern"],
                    json!({"Regex": " +"}),
                ),
            ),
            (
                "normalizer",
                change(
                    &["normalizer", "normalizers"],
                    json!([{"type": "Replace", "pattern": {"Regex": " {2,}"}, "content": " "},
                           {"type": "Lowercase"}]),
                ),
            ),
            (
                "pre-tokenizer",
                change(&["pre_tokenizer"], json!({"type": "Whitespace"})),
            ),
            (
                "pre-tokenizer",
                change(&["pre_tokenizer", "add_prefix_space"], json!(false)),
            ),
            (
                "pre-tokenizer",
                change(&["pre_tokenizer", "replacement"], json!("_")),
            ),
            (
                "pre-tokenizer",
                change(&["pre_tokenizer", "split"], json!(false)),
            ),
            (
                "post-processor",
                change(&["post_processor"], serde_json::Value::Null),
            ),
            (
                "post-processor",
                change(&["post_processor", "type"], json!("BertProcessing")),
            ),
            (
                "post-processor",
                change(
                    &["post_processor", "single"],
                    json!([{"SpecialToken": {"id": "<s>", "type_id": 0}},
                           {"Sequence": {"id": "A", "type_id": 0}}]),
                ),
            ),
            (
                "post-processor",
                change(
                    &["post_processor", "special_tokens", "<s>", "ids"],
                    json!([1]),
                ),
            ),
            (
                "byte fallback",
                change(&["model", "byte_fallback"], json!(true)),
            ),
            (
                "added tokens",
                change(
                    &["added_tokens"],
                    json!([{"id": 0, "content": "<s>"}, {"id": 19, "content": "[CODE]"}]),
                ),
            ),
        ];
        for (what, j) in cases {
            let err = UnigramTokenizer::from_json(j.to_string().as_bytes())
                .err()
                .unwrap_or_else(|| panic!("accepted a file with another {what}: {j}"))
                .to_string();
            assert!(
                err.contains(what) && err.contains("alice model fetch"),
                "{what}: {err}"
            );
        }
        // The real file's legacy and current Metaspace spellings are both accepted.
        let current = change(
            &["pre_tokenizer"],
            json!({"type": "Metaspace", "replacement": "▁", "prepend_scheme": "always",
                   "split": true}),
        );
        assert!(UnigramTokenizer::from_json(current.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn viterbi_prefers_the_best_scoring_split_and_wraps_in_specials() {
        let t = tiny();
        // ▁hello (-3) beats ▁hell + o (-8.5).
        assert_eq!(t.encode("hello world", 512), vec![0, 5, 8, 2]);
        // Runs of spaces collapse; a leading space does not add a second ▁.
        assert_eq!(t.encode("  hello    world", 512), vec![0, 5, 8, 2]);
        assert_eq!(t.encode("ไทย", 512), vec![0, 15, 2]);
    }

    #[test]
    fn unknown_characters_become_one_fused_unk_per_run() {
        let t = tiny();
        // "▁" then "zz" unknown (fused into one <unk>), then "o".
        assert_eq!(t.encode("zzo", 512), vec![0, 4, 3, 7, 2]);
        // Two runs separated by a known char stay two unks.
        assert_eq!(t.encode("zozz", 512), vec![0, 4, 3, 7, 3, 2]);
    }

    #[test]
    fn literal_special_tokens_are_text_and_truncation_keeps_both_ends() {
        let t = tiny();
        let ids = t.encode("</s>", 512);
        assert_eq!(ids.first(), Some(&0));
        assert_eq!(ids.last(), Some(&2));
        assert!(ids[1..ids.len() - 1].iter().all(|&i| i != 2), "{ids:?}");
        let long = "hello ".repeat(600);
        let ids = t.encode(&long, 512);
        assert_eq!(ids.len(), 512);
        assert_eq!((ids[0], ids[511]), (0, 2));
    }

    fn base64(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= c.len() {
                    out.push(A[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// A sentencepiece `precompiled_charsmap` that maps the single byte `A` to `a` and leaves
    /// everything else alone: a darts-clone double array of 256 units (root at 0 with offset 0,
    /// the unit for `A` at 0x41 with a leaf and offset 1, its leaf at 0x40 holding the index
    /// of "a\0" in the normalized strings), then those strings.
    fn charsmap_a_to_lowercase() -> String {
        let mut units = vec![0u32; 256];
        units[0x41] = (1 << 10) | (1 << 8) | 0x41;
        units[0x40] = 1 << 31;
        let mut bytes = (units.len() as u32 * 4).to_le_bytes().to_vec();
        units.iter().for_each(|u| bytes.extend(u.to_le_bytes()));
        bytes.extend(b"a\0");
        base64(&bytes)
    }

    /// The real file's normalizer is a Sequence holding a Precompiled one; its charsmap is
    /// read from the file and applied before Metaspace.
    #[test]
    fn the_precompiled_normalizer_is_read_and_applied() {
        let json = format!(
            r#"{{"normalizer": {{"type": "Sequence", "normalizers": [
                {{"type": "Precompiled", "precompiled_charsmap": "{}"}},
                {{"type": "Replace", "pattern": {{"Regex": " {{2,}}"}}, "content": " "}}]}},
              "pre_tokenizer": {{"type": "Metaspace", "replacement": "▁", "add_prefix_space": true}},
              "post_processor": {{"type": "TemplateProcessing", "single": [
                {{"SpecialToken": {{"id": "<s>", "type_id": 0}}}},
                {{"Sequence": {{"id": "A", "type_id": 0}}}},
                {{"SpecialToken": {{"id": "</s>", "type_id": 0}}}}]}},
              "model": {{"type": "Unigram", "unk_id": 3, "vocab": [
                ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
                ["▁abc", -1.0], ["▁", -2.0], ["A", -3.0], ["b", -3.0], ["c", -3.0]]}}}}"#,
            charsmap_a_to_lowercase()
        );
        let t = UnigramTokenizer::from_json(json.as_bytes()).unwrap();
        assert_eq!(t.normalize("Abc  A"), "▁abc▁a");
        assert_eq!(t.encode("Abc", 512), vec![0, 4, 2]);
        let broken = json.replace(&charsmap_a_to_lowercase(), "not base64!");
        let err = UnigramTokenizer::from_json(broken.as_bytes())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("alice model fetch"), "{err}");
    }

    #[test]
    fn a_non_unigram_file_is_refused_with_a_next_step() {
        let json = r#"{"model": {"type": "WordPiece", "vocab": {}}}"#;
        let err = UnigramTokenizer::from_json(json.as_bytes())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("alice model fetch"), "{err}");
    }
}
