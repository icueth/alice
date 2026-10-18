//! The Claude Code transcript line parser (format: `tests/fixtures/capture/claude/FORMAT.md`).
//!
//! The format is undocumented and changes between versions, so a line is read as a
//! `serde_json::Value`, never into derived structs: every field except `type` is optional, and a
//! shape the parser does not know degrades to [`LineResult::Unknown`] instead of failing. A line
//! of a `type` it does not know that has no key conversation is kept under, at any depth, is
//! [`LineResult::Bookkeeping`]: Claude Code adds such lines with most releases.

use serde_json::{Map, Value};

use super::record::{Block, Record, RecordKind};

/// What one line is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LineResult {
    Record(Record),
    /// A record with an unrecognised block beside the known ones. The known blocks are kept;
    /// the caller counts the line as unknown, so format drift shows up in the stats.
    Drift(Record),
    /// A JSON object whose shape the parser does not recognise (counted, never fatal).
    Unknown,
    /// A line of a `type` the parser does not know that has none of [`CONVERSATION_KEYS`] at any
    /// depth: the bookkeeping a newer agent build added. Dropped, and counted apart from `Unknown`, so it
    /// is never taken for format drift.
    Bookkeeping,
    /// Not UTF-8, not JSON, or a JSON value that is not an object.
    Invalid,
}

/// Line types that are known and deliberately dropped: they hold no conversation (FORMAT.md
/// "Line types"; spec §3.3 "What is kept"). They are recognised, so they never count as unknown.
/// `system` lines (every subtype, `compact_boundary` included) are dropped too; see
/// [`parse_line`].
const DROPPED_TYPES: &[&str] = &[
    // Harness-injected context: hook output, environment, file reads, tool lists, reminders.
    "attachment",
    // Bookkeeping, re-appended about once per turn.
    "last-prompt",
    "mode",
    "permission-mode",
    "ai-title",
    "atis-latch",
    "queue-operation",
    "file-history-snapshot",
    "file-history-delta",
    // Raw API request logging. `api-request-blob` carries a `message` with user, assistant and
    // system roles and real content blocks: it is a request-log copy, and reading it would count
    // the conversation twice (FORMAT.md "Dispatch on the line `type` first").
    "api-request-shape",
    "api-request",
    "api-request-blob",
    // Older builds (spec §2.1, §3.3): tool progress events and the session-title line.
    "progress",
    "summary",
    // Newer builds (seen 2026-10): artifact pages linked to the session, and the ledgers of their
    // comment watch and auto-replies.
    "frame-link",
    "artifact-comment-monitor",
    "artifact-autoreact-ledger",
    // Newer builds: remote-control bridge, cost counters and history-sync state.
    "bridge-session",
    "cost-state",
    "history-suppression",
    // Newer builds: a title or agent name the person set.
    "custom-title",
    "agent-name",
];

/// The keys conversation, tool calls and their results are kept under, in either agent's
/// transcripts (fix round 1, I3, ruling). A line of a `type` the parser does not know is
/// bookkeeping only when none of them appears at any depth ([`LineResult::Bookkeeping`]), else
/// format drift.
const CONVERSATION_KEYS: &[&str] = &[
    "message",
    "content",
    "text",
    "summary",
    "arguments",
    "input",
    "output",
    "prompt",
    "command",
    "result",
    "error",
    "thinking",
    "reasoning",
];

/// Whether `obj`, or any object nested in it at any depth (inside arrays too), has one of
/// [`CONVERSATION_KEYS`], whatever its value. The depth is bounded by `serde_json`'s parse limit.
pub(crate) fn has_conversation_key(obj: &Map<String, Value>) -> bool {
    obj.iter()
        .any(|(k, v)| CONVERSATION_KEYS.contains(&k.as_str()) || value_has_conversation_key(v))
}

fn value_has_conversation_key(v: &Value) -> bool {
    match v {
        Value::Object(obj) => has_conversation_key(obj),
        Value::Array(items) => items.iter().any(value_has_conversation_key),
        _ => false,
    }
}

/// Parses one line (without its `\n`).
pub(crate) fn parse_line(line: &[u8]) -> LineResult {
    let Ok(text) = std::str::from_utf8(line) else {
        return LineResult::Invalid;
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(text) else {
        return LineResult::Invalid;
    };
    let Some(kind) = obj.get("type").and_then(Value::as_str) else {
        return LineResult::Unknown;
    };
    match kind {
        "user" => user(obj),
        "assistant" => assistant(obj),
        // A `compact_boundary` only marks a compaction: the `isCompactSummary` user line after it
        // is the turn. `turn_duration`, `stop_hook_summary`, `informational` and any other
        // subtype are harness notices (spec §3.3: `system` lines are dropped).
        "system" => LineResult::Record(skip(&obj)),
        t if DROPPED_TYPES.contains(&t) => LineResult::Record(skip(&obj)),
        _ if has_conversation_key(&obj) => LineResult::Unknown,
        _ => LineResult::Bookkeeping,
    }
}

/// A recognised line that carries nothing to capture.
fn skip(obj: &Map<String, Value>) -> Record {
    Record {
        uuid: string(obj, "uuid"),
        message_id: None,
        timestamp: string(obj, "timestamp"),
        kind: RecordKind::Skip,
        blocks: Vec::new(),
    }
}

fn string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn flag(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A sidechain record inside a main session file. Every line of a subagent's own file is a
/// sidechain too, but it carries `agentId` (FORMAT.md "Files and layout"), and there its prompt
/// starts the subagent's turn. A main-file sidechain is dropped: the subagent is captured from
/// its own file (spec §3.4), and its inline copy would interleave with the main turn.
fn main_file_sidechain(obj: &Map<String, Value>) -> bool {
    flag(obj, "isSidechain") && !obj.get("agentId").is_some_and(Value::is_string)
}

/// The blocks of `message.content`, or `None` when `message` or `content` has an unusable shape.
fn content_blocks(obj: &mut Map<String, Value>) -> Option<Parsed> {
    let Some(Value::Object(mut message)) = obj.remove("message") else {
        return None;
    };
    match message.remove("content") {
        Some(Value::String(text)) => Some(Parsed {
            blocks: text_block(text).into_iter().collect(),
            unknown: false,
        }),
        Some(Value::Array(items)) => Some(blocks(items)),
        _ => None,
    }
}

fn text_block(text: String) -> Option<Block> {
    (!text.is_empty()).then_some(Block::Text(text))
}

/// A user/assistant record, or `Unknown` when nothing in it was recognised.
fn record(
    obj: &Map<String, Value>,
    message_id: Option<String>,
    kind: RecordKind,
    parsed: Parsed,
) -> LineResult {
    if parsed.blocks.is_empty() && parsed.unknown {
        return LineResult::Unknown;
    }
    let kind = if parsed.blocks.is_empty() {
        RecordKind::Skip
    } else {
        kind
    };
    let drift = parsed.unknown && kind != RecordKind::Skip;
    let record = Record {
        uuid: string(obj, "uuid"),
        message_id,
        timestamp: string(obj, "timestamp"),
        kind,
        blocks: parsed.blocks,
    };
    if drift {
        LineResult::Drift(record)
    } else {
        LineResult::Record(record)
    }
}

/// `type: "user"`: a prompt, tool results, injected harness text, or the compaction summary
/// (FORMAT.md "`user` lines").
fn user(mut obj: Map<String, Value>) -> LineResult {
    if flag(&obj, "isMeta") || main_file_sidechain(&obj) {
        return LineResult::Record(skip(&obj));
    }
    let Some(mut parsed) = content_blocks(&mut obj) else {
        return LineResult::Unknown;
    };
    if flag(&obj, "isCompactSummary") {
        // The summary is model-written recap: keep its text only.
        parsed.blocks.retain(|b| matches!(b, Block::Text(_)));
        return record(&obj, None, RecordKind::Summary, parsed);
    }
    let results = obj.contains_key("toolUseResult")
        || parsed
            .blocks
            .iter()
            .any(|b| matches!(b, Block::ToolResult { .. }));
    let kind = if results {
        RecordKind::ToolResults
    } else {
        RecordKind::Prompt
    };
    record(&obj, None, kind, parsed)
}

/// `type: "assistant"`: one block of a response; the lines of one response share `message.id`.
fn assistant(mut obj: Map<String, Value>) -> LineResult {
    if main_file_sidechain(&obj) {
        return LineResult::Record(skip(&obj));
    }
    let message_id = obj
        .get("message")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(parsed) = content_blocks(&mut obj) else {
        return LineResult::Unknown;
    };
    record(&obj, message_id, RecordKind::Assistant, parsed)
}

/// Content blocks, and whether any element was not recognised.
struct Parsed {
    blocks: Vec<Block>,
    unknown: bool,
}

fn blocks(items: Vec<Value>) -> Parsed {
    let mut out = Parsed {
        blocks: Vec::new(),
        unknown: false,
    };
    for item in items {
        let Value::Object(mut block) = item else {
            out.unknown = true;
            continue;
        };
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match kind.as_str() {
            "text" => match block.remove("text") {
                Some(Value::String(text)) => out.blocks.extend(text_block(text)),
                _ => out.unknown = true,
            },
            "thinking" => match block.remove("thinking") {
                Some(Value::String(text)) if !text.is_empty() => {
                    out.blocks.push(Block::Thinking(text));
                }
                Some(Value::String(_)) => {}
                _ => out.unknown = true,
            },
            // Encrypted reasoning: known, nothing readable to keep.
            "redacted_thinking" => {}
            "tool_use" | "server_tool_use" | "mcp_tool_use" => match block.remove("name") {
                Some(Value::String(name)) => out.blocks.push(Block::ToolUse {
                    id: string(&block, "id").unwrap_or_default(),
                    name,
                    input: block.remove("input").unwrap_or(Value::Null),
                }),
                _ => out.unknown = true,
            },
            k if k == "tool_result" || k.ends_with("_tool_result") => {
                let (text, attached) = flatten_result(block.remove("content"));
                out.blocks.push(Block::ToolResult {
                    tool_use_id: string(&block, "tool_use_id").unwrap_or_default(),
                    text,
                    is_error: flag(&block, "is_error"),
                });
                out.blocks.extend(attached);
            }
            "image" => out.blocks.push(Block::Image),
            "document" => out.blocks.push(Block::Document),
            "audio" | "input_audio" => out.blocks.push(Block::Audio),
            _ => out.unknown = true,
        }
    }
    out
}

/// A tool result's text, and the media blocks it carried. String content is the text. Array
/// content contributes its `text` parts and `tool_reference` names (joined by `\n`); its images,
/// documents and audio become blocks that follow the result, never base64 text.
fn flatten_result(content: Option<Value>) -> (String, Vec<Block>) {
    let items = match content {
        Some(Value::String(text)) => return (text, Vec::new()),
        Some(Value::Array(items)) => items,
        Some(scalar @ (Value::Number(_) | Value::Bool(_))) => {
            return (scalar.to_string(), Vec::new());
        }
        _ => return (String::new(), Vec::new()),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut attached = Vec::new();
    for item in items {
        match item {
            Value::String(text) => parts.push(text),
            Value::Object(mut part) => match part
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .as_deref()
            {
                Some("text") => {
                    if let Some(Value::String(text)) = part.remove("text") {
                        parts.push(text);
                    }
                }
                Some("tool_reference") => parts.extend(string(&part, "tool_name")),
                Some("image") => attached.push(Block::Image),
                Some("document") => attached.push(Block::Document),
                Some("audio" | "input_audio") => attached.push(Block::Audio),
                _ => {}
            },
            _ => {}
        }
    }
    (parts.join("\n"), attached)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(v: serde_json::Value) -> LineResult {
        parse_line(&serde_json::to_vec(&v).unwrap())
    }

    fn record_of(v: serde_json::Value) -> Record {
        match parse(v) {
            LineResult::Record(r) => r,
            other => panic!("expected a record, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_images_become_image_blocks() {
        let r = record_of(json!({
            "type": "user", "uuid": "u-1", "timestamp": "2026-10-14T09:00:00.000Z",
            "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_x",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
                    {"type": "text", "text": "Screenshot taken"},
                    {"type": "tool_reference", "tool_name": "Read"},
                ],
            }]},
            "toolUseResult": [{"type": "text", "text": "Screenshot taken"}],
        }));
        assert_eq!(r.kind, RecordKind::ToolResults);
        assert_eq!(
            r.blocks,
            [
                Block::ToolResult {
                    tool_use_id: "toolu_x".into(),
                    text: "Screenshot taken\nRead".into(),
                    is_error: false,
                },
                Block::Image,
            ]
        );
    }

    #[test]
    fn error_results_keep_their_flag() {
        let r = record_of(json!({
            "type": "user", "uuid": "u-2",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_y", "content": "exit 1", "is_error": true},
            ]},
        }));
        assert_eq!(
            r.blocks,
            [Block::ToolResult {
                tool_use_id: "toolu_y".into(),
                text: "exit 1".into(),
                is_error: true,
            }]
        );
    }

    #[test]
    fn assistant_blocks_and_message_id() {
        let line = |block: serde_json::Value| {
            json!({"type": "assistant", "uuid": "a-1", "timestamp": "t",
                   "message": {"id": "msg_1", "role": "assistant", "content": [block]}})
        };
        let r = record_of(line(
            json!({"type": "tool_use", "id": "toolu_1", "name": "Bash",
                                      "input": {"command": "ls"}, "caller": {"type": "direct"}}),
        ));
        assert_eq!(r.kind, RecordKind::Assistant);
        assert_eq!(r.message_id.as_deref(), Some("msg_1"));
        assert_eq!(
            r.blocks,
            [Block::ToolUse {
                id: "toolu_1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            }]
        );
        let r = record_of(line(
            json!({"type": "thinking", "thinking": "Hmm.", "signature": "c2ln"}),
        ));
        assert_eq!(r.blocks, [Block::Thinking("Hmm.".into())]);
        // Empty thinking (written while thinking display is "updates") and redacted thinking
        // are recognised and carry nothing.
        for block in [
            json!({"type": "thinking", "thinking": "", "signature": "c2ln"}),
            json!({"type": "redacted_thinking", "data": "ZW5j"}),
        ] {
            let r = record_of(line(block));
            assert_eq!(r.kind, RecordKind::Skip);
            assert!(r.blocks.is_empty());
        }
        assert_eq!(
            parse(line(json!({"type": "hologram"}))),
            LineResult::Unknown
        );
    }

    #[test]
    fn request_log_and_bookkeeping_lines_are_dropped_not_unknown() {
        for v in [
            json!({"type": "api-request-blob", "hash": "h", "sessionId": "s",
                   "message": {"role": "user", "content": [{"type": "text", "text": "copy"}]}}),
            json!({"type": "attachment", "uuid": "x", "attachment": {"type": "date", "date": "d"}}),
            json!({"type": "system", "subtype": "compact_boundary", "uuid": "b", "content": "c"}),
            json!({"type": "system", "subtype": "some_future_subtype"}),
            json!({"type": "progress", "data": {}}),
            json!({"type": "last-prompt", "leafUuid": "l", "sessionId": "s"}),
        ] {
            let r = record_of(v);
            assert_eq!(r.kind, RecordKind::Skip);
            assert!(r.blocks.is_empty());
        }
        assert_eq!(
            parse(json!({"type": "brand-new-type", "content": "Words."})),
            LineResult::Unknown
        );
    }

    /// The bookkeeping types of newer builds (FORMAT.md "Line types"; seen in one person's
    /// transcripts of 2026-10): known, so dropped as a `Skip` record, not left to the bookkeeping
    /// rule.
    #[test]
    fn newer_bookkeeping_types_are_known() {
        for v in [
            json!({"type": "frame-link", "sessionId": "s", "path": "/p", "frameUrl": "https://f",
                   "title": "T", "artifactCount": 1, "timestamp": "2026-10-18T09:00:00.000Z"}),
            json!({"type": "artifact-comment-monitor", "v": 1, "sessionId": "s",
                   "artifacts": {"a1": {"state": "watching", "writtenAtMs": 1, "title": "T"}}}),
            json!({"type": "artifact-autoreact-ledger", "v": 1, "sessionId": "s", "accountUuid": "u",
                   "artifacts": {"a1": {"savedAt": 1, "stampHighWater": 2, "everBaselined": true,
                                        "everHadThreads": false, "turnTimestamps": [], "threads": {}}}}),
            json!({"type": "bridge-session", "sessionId": "s", "bridgeSessionId": "b",
                   "lastSequenceNum": 3, "ownerAccountUuid": "u", "ownerOrganizationUuid": "o"}),
            json!({"type": "cost-state", "sessionId": "s", "totalCostUSD": 0.5,
                   "totalAPIDuration": 10, "modelUsage": {"m": {"inputTokens": 1}}}),
            json!({"type": "history-suppression", "sessionId": "s", "cause": "c",
                   "vetoedAgainstAccountUuid": "u", "ts": 1}),
            json!({"type": "custom-title", "customTitle": "T", "sessionId": "s"}),
            json!({"type": "agent-name", "agentName": "N", "sessionId": "s"}),
        ] {
            let r = record_of(v);
            assert_eq!(r.kind, RecordKind::Skip);
            assert!(r.blocks.is_empty());
        }
    }

    /// A type the parser does not know is bookkeeping when it has none of the keys conversation is
    /// kept under; with any of them it stays `Unknown`, format drift. A line without a string
    /// `type` is no type at all: `Unknown`.
    #[test]
    fn an_unknown_type_without_conversation_keys_is_bookkeeping() {
        assert_eq!(
            parse(
                json!({"type": "brand-new-ledger", "sessionId": "s", "title": "T",
                         "entries": {"e1": {"at": 1}}})
            ),
            LineResult::Bookkeeping
        );
        assert_eq!(
            parse(json!({"type": "brand-new-type"})),
            LineResult::Bookkeeping
        );
        for key in RULED_KEYS {
            let mut v = json!({"type": "brand-new-type", "sessionId": "s"});
            v[key] = json!(null);
            assert_eq!(parse(v), LineResult::Unknown, "{key}");
        }
        assert_eq!(parse(json!({"sessionId": "s"})), LineResult::Unknown);
        assert_eq!(
            parse(json!({"type": 7, "sessionId": "s"})),
            LineResult::Unknown
        );
        // A known type with no usable content is not bookkeeping either.
        assert_eq!(
            parse(json!({"type": "user", "uuid": "u"})),
            LineResult::Unknown
        );
    }

    /// The keys of the ruling (fix round 1, I3), written out so that a key missing from
    /// `CONVERSATION_KEYS` fails the tests.
    const RULED_KEYS: [&str; 13] = [
        "message",
        "content",
        "text",
        "summary",
        "arguments",
        "input",
        "output",
        "prompt",
        "command",
        "result",
        "error",
        "thinking",
        "reasoning",
    ];

    /// Fix round 1, I3 (ruling): a conversation key at any depth, in an object or inside an
    /// array, keeps an unknown type `Unknown`; only a line with none of them anywhere is
    /// bookkeeping, however deep it nests.
    #[test]
    fn a_conversation_key_at_any_depth_keeps_an_unknown_type_unknown() {
        assert_eq!(
            parse(
                json!({"type": "agent-progress", "data": {"message": {"role": "assistant",
                "content": [{"type": "text", "text": "Working."}]}}})
            ),
            LineResult::Unknown
        );
        for key in RULED_KEYS {
            let mut nested = json!({"leaf": 1});
            nested[key] = json!("words");
            for v in [
                json!({"type": "brand-new-type", "data": {"deeper": nested.clone()}}),
                json!({"type": "brand-new-type", "data": {"items": [1, {"deeper": [nested.clone()]}]}}),
            ] {
                assert_eq!(parse(v.clone()), LineResult::Unknown, "{key} in {v}");
            }
        }
        // Keys that only look alike, values that hold a key's name, and deep nesting: bookkeeping.
        assert_eq!(
            parse(json!({"type": "brand-new-ledger", "sessionId": "s",
                "data": {"a": {"b": [{"c": [1, 2, {"d": "message"}]}]}},
                "messageCount": 3, "textLength": 9, "contentHash": "h", "inputTokens": 5})),
            LineResult::Bookkeeping
        );
    }

    #[test]
    fn meta_sidechain_and_summary_lines() {
        let user = |extra: serde_json::Value| {
            let mut v = json!({"type": "user", "uuid": "u", "message": {"role": "user", "content": "Text."}});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            record_of(v)
        };
        assert_eq!(user(json!({})).kind, RecordKind::Prompt);
        assert_eq!(user(json!({"isMeta": true})).kind, RecordKind::Skip);
        assert!(user(json!({"isMeta": true})).blocks.is_empty());
        assert_eq!(user(json!({"isSidechain": true})).kind, RecordKind::Skip);
        assert_eq!(
            user(json!({"isSidechain": true, "agentId": "a1"})).kind,
            RecordKind::Prompt
        );
        assert_eq!(
            user(json!({"isCompactSummary": true})).kind,
            RecordKind::Summary
        );
        assert_eq!(
            user(json!({"toolUseResult": "Error: denied"})).kind,
            RecordKind::ToolResults
        );
    }

    #[test]
    fn invalid_and_unknown_shapes() {
        assert_eq!(parse_line(b"not json"), LineResult::Invalid);
        assert_eq!(parse_line(b"[1]"), LineResult::Invalid);
        assert_eq!(
            parse_line(b"{\"type\":\"user\",\"x\":\"\xff\"}"),
            LineResult::Invalid
        );
        assert_eq!(parse_line(b"{}"), LineResult::Unknown);
        assert_eq!(
            parse_line(b"{\"type\":\"user\",\"message\":null}"),
            LineResult::Unknown
        );
        assert_eq!(
            parse_line(b"{\"type\":\"user\",\"message\":{\"content\":[7]}}"),
            LineResult::Unknown
        );
    }
}
