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

/// The blocks the distiller reads passages from (spec §6.3); tool calls and results are not.
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

/// The rendered text of every block the distiller can read, one string per block (a block runs
/// from its tag line to the next tag line).
fn readable_blocks(rendered: &str) -> Vec<String> {
    let mut blocks: Vec<(bool, String)> = Vec::new();
    for line in rendered.lines() {
        if TAGS.iter().any(|t| line.starts_with(t)) {
            let readable = READABLE.iter().any(|t| line.starts_with(t));
            blocks.push((readable, line.to_owned()));
        } else if let Some((_, block)) = blocks.last_mut() {
            block.push('\n');
            block.push_str(line);
        }
    }
    blocks
        .into_iter()
        .filter_map(|(readable, block)| readable.then_some(block))
        .collect()
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
        let rendered = out
            .turns
            .iter()
            .flat_map(|t| &t.chunks)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if is_thai(&rendered) {
            thai.push(kind);
        }
        let blocks = readable_blocks(&rendered);
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
            // One passage holds the phrase: no other block the distiller reads repeats it.
            let hits: usize = blocks.iter().map(|b| b.matches(phrase).count()).sum();
            assert_eq!(hits, 1, "{file}: {phrase:?} in {hits} readable places");
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
