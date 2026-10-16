//! The synthetic distillation set (spec §11.2) reads cleanly and matches its labels. No network
//! and no daemon: the evaluation that runs the distiller over the set is `distill_eval.rs`.
//!
//! `fixtures/distill/sessions/sNN.jsonl` are hand-written Claude Code transcripts and
//! `fixtures/distill/labels.json` names each one's kind (`gold`, `nothing`, `already`) and its
//! gold items: a key phrase a correct memory must contain, and the memory kinds it may have.

use std::collections::HashSet;
use std::path::PathBuf;

use alice_core::capture::{AgentKind, ReadOptions, TAGS, read};
use alice_core::model::Kind;
use alice_core::normalize::nfc;
use alice_core::redact::Redactor;
use serde_json::Value;

/// The kinds a distilled memory can have (spec §6.4).
const DISTILL_KINDS: [&str; 7] = [
    "decision",
    "bugfix",
    "howto",
    "warning",
    "preference",
    "fact",
    "handoff",
];

/// Blocks a person or a model wrote, as opposed to tool calls and results.
const READABLE: [&str; 4] = ["[user]", "[assistant]", "[summary]", "[subagent "];

const REMEMBER: &str = "mcp__alice__remember";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/distill")
}

fn labels() -> Vec<Value> {
    let text = std::fs::read_to_string(dir().join("labels.json")).unwrap();
    let labels: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(keys(&labels), ["sessions"]);
    labels["sessions"].as_array().unwrap().clone()
}

fn session_bytes(file: &str) -> Vec<u8> {
    std::fs::read(dir().join("sessions").join(file)).unwrap()
}

/// One rendered block: a tag line and the lines up to the next tag.
struct Block {
    /// `[user]`, `[assistant]`, `[subagent Explore]`, `[tool Bash]`, …
    tag: String,
    /// The text after the tag (and after a split block's `(cont.)`).
    body: String,
    /// The distiller takes passages from it (spec §6.3): a user prompt, a compaction summary, a
    /// subagent's report, or the last assistant text of the turn.
    candidate: bool,
}

impl Block {
    fn readable(&self) -> bool {
        READABLE.iter().any(|t| self.tag.starts_with(t))
    }
}

/// The blocks of one turn's rendered chunks.
fn turn_blocks(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for line in text.lines() {
        if !TAGS.iter().any(|t| line.starts_with(t)) {
            if let Some(block) = blocks.last_mut() {
                block.body.push('\n');
                block.body.push_str(line);
            }
            continue;
        }
        let (tag, rest) = line.split_at(line.find(']').unwrap() + 1);
        let body = rest.strip_prefix(' ').unwrap_or(rest);
        let body = body.strip_prefix("(cont.) ").unwrap_or(body);
        let candidate = match tag {
            "[user]" | "[summary]" => true,
            // A subagent call is `[subagent <type>] <description>`, then its report under the same
            // tag (no call in the set runs in parallel with another).
            t if t.starts_with("[subagent ") => {
                blocks.last().is_some_and(|b| b.tag == t && !b.candidate)
            }
            _ => false,
        };
        blocks.push(Block {
            tag: tag.to_owned(),
            body: body.to_owned(),
            candidate,
        });
    }
    if let Some(last) = blocks.iter_mut().rev().find(|b| b.tag == "[assistant]") {
        last.candidate = true;
    }
    blocks
}

/// Where a gold phrase sits must be a passage the distiller proposes (spec §6.3; the brief's
/// placement rule): a whole user prompt of 20–800 characters, or one paragraph or bullet group
/// of 80–1,200 characters; and the phrase ends within the passage's first 1,000 characters, so an
/// extractive memory of the passage holds it.
fn check_placement(file: &str, phrase: &str, block: &Block) {
    let chars = |s: &str| s.chars().count();
    let passage = if block.tag == "[user]" {
        assert!(
            (20..=800).contains(&chars(&block.body)),
            "{file}: {phrase:?}: a user prompt of {} characters",
            chars(&block.body)
        );
        block.body.as_str()
    } else {
        let paragraph = block
            .body
            .split("\n\n")
            .find(|p| p.contains(phrase))
            .unwrap();
        assert!(
            (80..=1200).contains(&chars(paragraph)),
            "{file}: {phrase:?}: a passage of {} characters",
            chars(paragraph)
        );
        paragraph
    };
    let end = chars(&passage[..passage.find(phrase).unwrap()]) + chars(phrase);
    assert!(end <= 1000, "{file}: {phrase:?} ends at character {end}");
}

/// The `content` of every `mcp__alice__remember` call in the raw transcript.
fn remembered(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for line in std::str::from_utf8(bytes).unwrap().lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["type"] != "assistant" {
            continue;
        }
        for block in v["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_use" && block["name"] == REMEMBER {
                out.push(block["input"]["content"].as_str().unwrap().to_owned());
            }
        }
    }
    out
}

/// An object's keys, sorted.
fn keys(v: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

fn is_thai(text: &str) -> bool {
    text.chars().any(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c))
}

#[test]
fn distill_fixtures_read_cleanly_and_match_their_labels() {
    let sessions = labels();
    let files: Vec<&str> = sessions
        .iter()
        .map(|s| s["file"].as_str().unwrap())
        .collect();
    let expected: Vec<String> = (1..=30).map(|i| format!("s{i:02}.jsonl")).collect();
    assert_eq!(files, expected, "labels name s01…s30 once each, in order");
    let on_disk: HashSet<String> = std::fs::read_dir(dir().join("sessions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(on_disk, expected.iter().cloned().collect::<HashSet<_>>());

    let opts = ReadOptions {
        at_file_start: true,
        close_trailing: true,
        ..Default::default()
    };
    let (mut gold_sessions, mut nothing_sessions, mut already_sessions) = (0, 0, 0);
    let mut gold_items = 0;
    let mut kinds_seen = HashSet::new();
    let mut thai = Vec::new();
    for s in &sessions {
        let file = s["file"].as_str().unwrap();
        assert_eq!(keys(s), ["file", "gold", "kind"], "{file}");
        let kind = s["kind"].as_str().unwrap();
        let gold = s["gold"].as_array().unwrap();
        match kind {
            "gold" => {
                gold_sessions += 1;
                assert!(
                    (1..=4).contains(&gold.len()),
                    "{file}: {} items",
                    gold.len()
                );
                gold_items += gold.len();
            }
            "nothing" => {
                nothing_sessions += 1;
                assert!(gold.is_empty(), "{file}: a nothing session has no gold");
            }
            "already" => {
                already_sessions += 1;
                assert!(!gold.is_empty(), "{file}: an already session has items");
            }
            other => panic!("{file}: kind {other}"),
        }

        let bytes = session_bytes(file);
        let out = read(AgentKind::ClaudeCode, &bytes, &opts);
        assert_eq!(out.stats.unknown, 0, "{file}: {:?}", out.stats);
        assert_eq!(out.stats.invalid, 0, "{file}: {:?}", out.stats);
        assert_eq!(out.stats.too_large, 0, "{file}: {:?}", out.stats);
        assert_eq!(out.consumed, bytes.len() as u64, "{file}: read to the end");
        assert!(out.tail.is_empty(), "{file}");
        assert!(
            (4..=12).contains(&out.turns.len()),
            "{file}: {} turns",
            out.turns.len()
        );
        let turns: Vec<String> = out
            .turns
            .iter()
            .map(|t| {
                let chunks: Vec<&str> = t.chunks.iter().map(|c| c.text.as_str()).collect();
                chunks.join("\n")
            })
            .collect();
        let rendered = turns.join("\n");
        if is_thai(&rendered) {
            thai.push(kind);
        }
        let blocks: Vec<Block> = turns.iter().flat_map(|t| turn_blocks(t)).collect();
        let saved = remembered(&bytes);
        assert_eq!(
            kind == "already",
            !saved.is_empty(),
            "{file}: only `already` sessions call {REMEMBER}"
        );

        for item in gold {
            assert_eq!(keys(item), ["kinds", "phrase"], "{file}");
            let phrase = item["phrase"].as_str().unwrap();
            let kinds = item["kinds"].as_array().unwrap();
            assert!(!kinds.is_empty(), "{file}: {phrase}");
            let mut distinct = HashSet::new();
            for k in kinds {
                let k = k.as_str().unwrap();
                assert!(
                    DISTILL_KINDS.contains(&k) && Kind::parse(k).is_some(),
                    "{file}: kind {k}"
                );
                assert!(distinct.insert(k), "{file}: {phrase}: {k} twice");
                kinds_seen.insert(k);
            }
            assert!(
                rendered.contains(phrase),
                "{file}: {phrase:?} is not in the rendered text"
            );
            // One passage holds the phrase: no other written block repeats it, and the one that
            // holds it is a candidate passage.
            let hits: usize = blocks
                .iter()
                .filter(|b| b.readable())
                .map(|b| b.body.matches(phrase).count())
                .sum();
            assert_eq!(hits, 1, "{file}: {phrase:?} in {hits} readable places");
            let holder = blocks
                .iter()
                .find(|b| b.readable() && b.body.contains(phrase))
                .unwrap();
            assert!(
                holder.candidate,
                "{file}: {phrase:?} is in a {} block that is not a candidate passage",
                holder.tag
            );
            check_placement(file, phrase, holder);
            if kind == "already" {
                assert!(
                    saved.iter().any(|c| c.contains(phrase)),
                    "{file}: {phrase:?} is not in a {REMEMBER} content"
                );
            }
        }
    }
    assert_eq!(
        (gold_sessions, nothing_sessions, already_sessions),
        (20, 6, 4)
    );
    assert!((48..=52).contains(&gold_items), "{gold_items} gold items");
    for k in DISTILL_KINDS {
        assert!(kinds_seen.contains(k), "no gold item of kind {k}");
    }
    assert!(
        (9..=11).contains(&thai.len()),
        "{} Thai sessions",
        thai.len()
    );
    assert!(thai.contains(&"nothing") && thai.contains(&"already") && thai.contains(&"gold"));
}

#[test]
fn distill_fixtures_hold_no_secrets_and_only_nfc_text() {
    let redactor = Redactor::builtin();
    for s in labels() {
        let file = s["file"].as_str().unwrap();
        let bytes = session_bytes(file);
        let raw = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(nfc(raw), raw, "{file}: text is NFC");
        assert!(raw.ends_with('\n'), "{file}");
        // Redaction would rewrite a secret-shaped string, and the evaluation would then measure
        // different text than the labels describe.
        let found = redactor.apply(raw).found;
        assert!(found.is_empty(), "{file}: {found:?}");
        for line in raw.lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(v.is_object() && v["type"].is_string(), "{file}: {line}");
        }
    }
}
