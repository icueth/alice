//! The Codex CLI rollout line parser (format: `tests/fixtures/capture/codex/FORMAT.md`).
//!
//! A rollout line is `{timestamp, type, payload}`. The format is documented as unstable and only
//! one version was sampled, so a line is read as a `serde_json::Value`, never into derived
//! structs, and a shape the parser does not know degrades to [`LineResult::Unknown`].
//!
//! What is read (FORMAT.md "Parser checklist"):
//! - `response_item` / `message`: the typed prompt (`user`), the assistant's text, and nothing
//!   from `developer` or injected-context messages;
//! - `response_item` tool calls and their outputs, paired by `call_id`;
//! - `compacted`: its `message` is a summary turn of its own. `replacement_history` replays
//!   earlier lines and is never read again;
//! - `event_msg`, `turn_context`, `world_state`, `token_usage_record`, `session_meta` and
//!   `inter_agent_communication_metadata` are known and carry no conversation text that
//!   `response_item` does not also hold;
//! - a line of another `type` is bookkeeping when no key conversation is kept under appears in
//!   it at any depth ([`LineResult::Bookkeeping`]), else `Unknown`.
//!
//! A rollout line has no uuid, so a turn is keyed by the hash of its time and first text
//! ([`super::turn_key`] with `None`). Codex's shell is the custom tool `exec`, and `shell`,
//! `shell_command` and `exec_command` are its older siblings: all of them render as `Bash`, with
//! the command (or code) as the command line. `apply_patch` renders as `Edit` with the paths it
//! touches, never the patch body.

use serde_json::{Map, Value, json};

use super::claude::{LineResult, has_conversation_key};
use super::record::{Block, Record, RecordKind};

/// The first bytes of a zstd frame. A compressed rollout is counted, not read (spec R21).
pub(crate) const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Text that opens a harness-injected user message, for rollouts whose messages carry no
/// `content_item_kinds`.
const INJECTED_PREFIXES: &[&str] = &[
    "<environment_context>",
    "<user_instructions>",
    "<turn_aborted>",
    "<permissions",
    "<INSTRUCTIONS>",
    "# AGENTS.md instructions",
];

/// Tools that run a command: their call renders as `Bash`.
const SHELL_TOOLS: &[&str] = &[
    "exec",
    "shell",
    "shell_command",
    "exec_command",
    "container.exec",
    "local_shell",
    "unified_exec",
];

/// Parses one line (without its `\n`).
pub(crate) fn parse_line(line: &[u8]) -> LineResult {
    let Ok(text) = std::str::from_utf8(line) else {
        return LineResult::Invalid;
    };
    let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(text) else {
        return LineResult::Invalid;
    };
    let Some(kind) = obj.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return LineResult::Unknown;
    };
    let timestamp = string(&obj, "timestamp");
    let mut payload = || match obj.remove("payload") {
        Some(Value::Object(payload)) => payload,
        _ => Map::new(),
    };
    match kind.as_str() {
        // Settings, injected context, token counters, UI events: known, not conversation.
        "session_meta" | "turn_context" | "world_state" | "token_usage_record" | "event_msg" => {
            LineResult::Record(skip(timestamp))
        }
        // Multi-agent bookkeeping beside an `agent_message` item (`trigger_turn`).
        "inter_agent_communication_metadata" => LineResult::Record(skip(timestamp)),
        "compacted" => compacted(timestamp, &payload()),
        "response_item" => response_item(timestamp, payload()),
        _ => unknown_line(&obj),
    }
}

/// A line of a `type` the parser does not know: bookkeeping when no key conversation, a tool call
/// or its output is kept under appears in it at any depth, payload included, else `Unknown`. A
/// payload that is not an object may be the text itself, so it counts as one.
fn unknown_line(obj: &Map<String, Value>) -> LineResult {
    let opaque = obj
        .get("payload")
        .is_some_and(|p| !p.is_object() && !p.is_null());
    if opaque || has_conversation_key(obj) {
        LineResult::Unknown
    } else {
        LineResult::Bookkeeping
    }
}

fn skip(timestamp: Option<String>) -> Record {
    record(timestamp, None, RecordKind::Skip, Vec::new())
}

fn record(
    timestamp: Option<String>,
    message_id: Option<String>,
    kind: RecordKind,
    blocks: Vec<Block>,
) -> Record {
    Record {
        uuid: None,
        message_id,
        timestamp,
        kind,
        blocks,
    }
}

fn string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A compaction: its recap is a turn of its own. An empty recap leaves nothing to capture.
fn compacted(timestamp: Option<String>, payload: &Map<String, Value>) -> LineResult {
    match payload.get("message").and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => LineResult::Record(record(
            timestamp,
            None,
            RecordKind::Summary,
            vec![Block::Text(text.to_owned())],
        )),
        _ => LineResult::Record(skip(timestamp)),
    }
}

fn response_item(timestamp: Option<String>, mut payload: Map<String, Value>) -> LineResult {
    let Some(kind) = string(&payload, "type") else {
        return LineResult::Unknown;
    };
    match kind.as_str() {
        "message" => message(timestamp, payload),
        "reasoning" => {
            let text = reasoning_text(payload.get("summary"));
            let kind = if text.is_empty() {
                RecordKind::Skip
            } else {
                RecordKind::Assistant
            };
            let blocks = if text.is_empty() {
                Vec::new()
            } else {
                vec![Block::Thinking(text)]
            };
            LineResult::Record(record(timestamp, None, kind, blocks))
        }
        "function_call" | "custom_tool_call" => {
            let Some(name) = string(&payload, "name") else {
                return LineResult::Unknown;
            };
            let name = mcp_name(string(&payload, "namespace").as_deref(), name);
            let raw = payload
                .remove("arguments")
                .or_else(|| payload.remove("input"))
                .unwrap_or(Value::Null);
            tool_call(timestamp, string(&payload, "call_id"), &name, raw)
        }
        "local_shell_call" => {
            let command = payload
                .get("action")
                .and_then(|a| a.get("command"))
                .map(command_text)
                .unwrap_or_default();
            let id = string(&payload, "call_id").or_else(|| string(&payload, "id"));
            LineResult::Record(record(
                timestamp,
                None,
                RecordKind::Assistant,
                vec![Block::ToolUse {
                    id: id.unwrap_or_default(),
                    name: "Bash".into(),
                    input: json!({"command": command}),
                }],
            ))
        }
        "web_search_call" => {
            let query = payload
                .get("action")
                .and_then(|a| a.get("query"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // The search has no output line to pair with: an empty id never holds a turn open.
            LineResult::Record(record(
                timestamp,
                None,
                RecordKind::Assistant,
                vec![Block::ToolUse {
                    id: String::new(),
                    name: "WebSearch".into(),
                    input: json!({"query": query}),
                }],
            ))
        }
        "function_call_output" | "custom_tool_call_output" => tool_output(timestamp, payload),
        "agent_message" => agent_message(timestamp, payload),
        // Known items with nothing to keep.
        "ghost_snapshot" | "compaction" => LineResult::Record(skip(timestamp)),
        _ => LineResult::Unknown,
    }
}

/// A message between agents of a multi-agent session: its text blocks, joined by `\n`, as one
/// block of the turn it occurs in (it never starts one). `encrypted_content` is opaque, so it is
/// skipped; any other block type is drift, as in a `message`.
fn agent_message(timestamp: Option<String>, mut payload: Map<String, Value>) -> LineResult {
    let Some(Value::Array(items)) = payload.remove("content") else {
        return LineResult::Unknown;
    };
    let mut texts = Vec::new();
    let mut unknown = false;
    for item in items {
        let Value::Object(mut block) = item else {
            unknown = true;
            continue;
        };
        match block.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => match block.remove("text") {
                Some(Value::String(text)) if !text.is_empty() => texts.push(text),
                Some(Value::String(_)) => {}
                _ => unknown = true,
            },
            Some("encrypted_content") => {}
            _ => unknown = true,
        }
    }
    if texts.is_empty() {
        return if unknown {
            LineResult::Unknown
        } else {
            LineResult::Record(skip(timestamp))
        };
    }
    let rec = record(
        timestamp,
        None,
        RecordKind::Assistant,
        vec![Block::AgentMessage {
            author: string(&payload, "author").unwrap_or_default(),
            recipient: string(&payload, "recipient").unwrap_or_default(),
            text: texts.join("\n"),
        }],
    );
    if unknown {
        LineResult::Drift(rec)
    } else {
        LineResult::Record(rec)
    }
}

/// The text of a reasoning item's `summary` (`[{type: "summary_text", text}]`).
fn reasoning_text(summary: Option<&Value>) -> String {
    let Some(Value::Array(items)) = summary else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|i| i.get("text").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn message(timestamp: Option<String>, mut payload: Map<String, Value>) -> LineResult {
    let role = string(&payload, "role").unwrap_or_default();
    let message_id = string(&payload, "id");
    let kinds: Option<Vec<String>> = payload
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|m| m.get("content_item_kinds"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|k| k.as_str().unwrap_or("").to_owned())
                .collect()
        });
    let (kind, is_user) = match role.as_str() {
        "user" => (RecordKind::Prompt, true),
        "assistant" => (RecordKind::Assistant, false),
        // Harness instructions: not conversation.
        "developer" | "system" => return LineResult::Record(skip(timestamp)),
        _ => return LineResult::Unknown,
    };
    let Some(Value::Array(items)) = payload.remove("content") else {
        return LineResult::Unknown;
    };
    let kinds = kinds.filter(|k| k.len() == items.len());
    let mut blocks = Vec::new();
    let mut unknown = false;
    for (i, item) in items.into_iter().enumerate() {
        let Value::Object(mut block) = item else {
            unknown = true;
            continue;
        };
        let block_type = string(&block, "type").unwrap_or_default();
        let parsed: Option<Block> = match block_type.as_str() {
            "input_text" | "output_text" | "text" => match block.remove("text") {
                Some(Value::String(text)) => {
                    let injected = match &kinds {
                        // The kind says what the block is; only the person's own are kept.
                        Some(kinds) if is_user => !kinds[i].starts_with("user."),
                        _ => {
                            is_user
                                && INJECTED_PREFIXES
                                    .iter()
                                    .any(|p| text.trim_start().starts_with(p))
                        }
                    };
                    if injected || text.is_empty() {
                        continue;
                    }
                    Some(Block::Text(text))
                }
                _ => {
                    unknown = true;
                    continue;
                }
            },
            "refusal" => match block.remove("refusal") {
                Some(Value::String(text)) if !text.is_empty() => Some(Block::Text(text)),
                _ => continue,
            },
            "input_image" | "output_image" | "image" => {
                let injected = kinds
                    .as_ref()
                    .is_some_and(|k| is_user && !k[i].starts_with("user."));
                if injected {
                    continue;
                }
                Some(Block::Image)
            }
            _ => {
                unknown = true;
                continue;
            }
        };
        blocks.extend(parsed);
    }
    if blocks.is_empty() && unknown {
        return LineResult::Unknown;
    }
    let kind = if blocks.is_empty() {
        RecordKind::Skip
    } else {
        kind
    };
    let message_id = if is_user { None } else { message_id };
    let rec = record(timestamp, message_id, kind, blocks);
    if unknown && rec.kind != RecordKind::Skip {
        LineResult::Drift(rec)
    } else {
        LineResult::Record(rec)
    }
}

/// A function or custom tool call.
fn tool_call(timestamp: Option<String>, id: Option<String>, name: &str, raw: Value) -> LineResult {
    // `arguments` is JSON text; a custom tool's `input` is free text (kept as a string).
    let input = match raw {
        Value::String(text) => serde_json::from_str::<Value>(&text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or(Value::String(text)),
        other => other,
    };
    let (name, input) = normalise(name, input);
    LineResult::Record(record(
        timestamp,
        None,
        RecordKind::Assistant,
        vec![Block::ToolUse {
            id: id.unwrap_or_default(),
            name,
            input,
        }],
    ))
}

/// The Claude Code form `mcp__<server>__<tool>` of a Codex MCP call, so the renderer finds the
/// memory tools by their last segment. Codex writes the server as a `namespace` (`alice`,
/// `alice__` or `mcp__alice__`) or inside the name (`alice__remember`); neither form is in the
/// fixture, so every one is rebuilt rather than trusted. A bare name with no namespace is a
/// Codex built-in and stays as it is.
fn mcp_name(namespace: Option<&str>, name: String) -> String {
    if name.starts_with("mcp__") {
        return name;
    }
    let server = namespace.map(|ns| {
        let ns = ns.strip_prefix("mcp__").unwrap_or(ns);
        ns.strip_suffix("__").unwrap_or(ns)
    });
    match server {
        Some(server) if !server.is_empty() => format!("mcp__{server}__{name}"),
        _ if name.contains("__") && !name.starts_with("__") && !name.ends_with("__") => {
            format!("mcp__{name}")
        }
        _ => name,
    }
}

/// Codex's tools under the names the rendering table knows.
fn normalise(name: &str, input: Value) -> (String, Value) {
    let short = name.rsplit("__").next().unwrap_or(name);
    if SHELL_TOOLS.contains(&name) {
        if let Some(command) = shell_command(&input) {
            return ("Bash".into(), json!({"command": command}));
        }
    } else if matches!(short, "apply_patch" | "applypatch") {
        let patch = match &input {
            Value::String(text) => Some(text.as_str()),
            other => ["input", "patch", "diff"]
                .iter()
                .find_map(|k| other.get(*k).and_then(Value::as_str)),
        };
        if let Some(patch) = patch {
            return ("Edit".into(), json!({"file_path": patch_paths(patch)}));
        }
    } else if name == "update_plan" {
        // Plan bookkeeping, like TodoWrite.
        return ("TodoWrite".into(), Value::Null);
    }
    (name.to_owned(), input)
}

/// The command line of a shell call: free text, or `command` / `cmd` (a string or an argv).
fn shell_command(input: &Value) -> Option<String> {
    match input {
        Value::String(text) => Some(text.clone()),
        Value::Object(obj) => ["command", "cmd"]
            .iter()
            .find_map(|k| obj.get(*k))
            .map(command_text)
            .filter(|c| !c.is_empty()),
        _ => None,
    }
}

/// A command given as text or as argv. `["bash", "-lc", "ls"]` is the command `ls`.
fn command_text(command: &Value) -> String {
    match command {
        Value::String(text) => text.clone(),
        Value::Array(argv) => {
            let argv: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
            match argv.as_slice() {
                [_, flag, script] if matches!(*flag, "-lc" | "-c" | "-ic") => (*script).to_owned(),
                _ => argv.join(" "),
            }
        }
        _ => String::new(),
    }
}

/// The files an `apply_patch` body touches, in order, joined by `, `.
fn patch_paths(patch: &str) -> String {
    let mut paths: Vec<&str> = Vec::new();
    for line in patch.lines() {
        for marker in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            if let Some(path) = line.strip_prefix(marker) {
                let path = path.trim();
                if !path.is_empty() && !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths.join(", ")
}

/// A tool call's output.
fn tool_output(timestamp: Option<String>, mut payload: Map<String, Value>) -> LineResult {
    let id = string(&payload, "call_id").unwrap_or_default();
    let (text, is_error, attached) = flatten_output(payload.remove("output"));
    let mut blocks = vec![Block::ToolResult {
        tool_use_id: id,
        text,
        is_error,
    }];
    blocks.extend(attached);
    LineResult::Record(record(timestamp, None, RecordKind::ToolResults, blocks))
}

/// An output's text, whether it failed, and the media it carried. A string is the text, unless
/// it is the JSON `{"output": …, "metadata": {"exit_code": …}}` older builds wrote. An array
/// contributes its `input_text` parts; images become blocks, never base64 text.
fn flatten_output(output: Option<Value>) -> (String, bool, Vec<Block>) {
    match output {
        Some(Value::String(text)) => {
            let wrapped = text
                .trim_start()
                .starts_with('{')
                .then(|| serde_json::from_str::<Value>(&text).ok())
                .flatten();
            if let Some(Value::Object(obj)) = wrapped
                && let Some(Value::String(inner)) = obj.get("output")
            {
                let failed = obj
                    .get("metadata")
                    .and_then(|m| m.get("exit_code"))
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0);
                return (inner.clone(), failed, Vec::new());
            }
            (text, false, Vec::new())
        }
        Some(Value::Array(items)) => {
            let mut parts: Vec<String> = Vec::new();
            let mut attached = Vec::new();
            for item in items {
                match item {
                    Value::String(text) => parts.push(text),
                    Value::Object(mut part) => match part.get("type").and_then(Value::as_str) {
                        Some("input_text" | "output_text" | "text") => {
                            if let Some(Value::String(text)) = part.remove("text") {
                                parts.push(text);
                            }
                        }
                        Some("input_image" | "image") => attached.push(Block::Image),
                        _ => {}
                    },
                    _ => {}
                }
            }
            (parts.join("\n"), false, attached)
        }
        Some(scalar @ (Value::Number(_) | Value::Bool(_))) => {
            (scalar.to_string(), false, Vec::new())
        }
        _ => (String::new(), false, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> LineResult {
        parse_line(&serde_json::to_vec(&v).unwrap())
    }

    fn record_of(v: Value) -> Record {
        match parse(v) {
            LineResult::Record(r) => r,
            other => panic!("expected a record, got {other:?}"),
        }
    }

    fn item(payload: Value) -> Value {
        json!({"timestamp": "t", "type": "response_item", "payload": payload})
    }

    #[test]
    fn typed_text_is_told_from_injected_context_by_kind_or_prefix() {
        let user = |kinds: Value, text: &str| {
            let mut meta = json!({"turn_id": "x"});
            if !kinds.is_null() {
                meta["content_item_kinds"] = kinds;
            }
            record_of(item(json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}],
                "internal_chat_message_metadata_passthrough": meta,
            })))
        };
        assert_eq!(user(json!(["user.text"]), "Hi.").kind, RecordKind::Prompt);
        let injected = user(json!(["environments.environment_context"]), "<x>");
        assert_eq!(injected.kind, RecordKind::Skip);
        // No kinds (an older build): the opening tag decides.
        assert_eq!(user(Value::Null, "Hi.").kind, RecordKind::Prompt);
        assert_eq!(
            user(Value::Null, "<environment_context>…").kind,
            RecordKind::Skip
        );
    }

    #[test]
    fn shell_tools_and_patches_take_the_names_the_renderer_knows() {
        let call = |name: &str, arguments: Value| {
            let r = record_of(item(json!({
                "type": "function_call", "call_id": "c", "name": name, "arguments": arguments,
            })));
            let Block::ToolUse { name, input, .. } = r.blocks[0].clone() else {
                panic!("tool use")
            };
            (name, input)
        };
        let (name, input) = call(
            "shell",
            json!("{\"command\":[\"bash\",\"-lc\",\"ls -la\"]}"),
        );
        assert_eq!(
            (name.as_str(), input),
            ("Bash", json!({"command": "ls -la"}))
        );
        let (_, input) = call("exec_command", json!("{\"cmd\":\"git status\"}"));
        assert_eq!(input, json!({"command": "git status"}));
        let (name, input) = call(
            "apply_patch",
            json!("{\"input\":\"*** Update File: a.rs\\n*** Delete File: b.rs\"}"),
        );
        assert_eq!(
            (name.as_str(), input),
            ("Edit", json!({"file_path": "a.rs, b.rs"}))
        );
        // Anything else keeps its name and parsed input.
        let (name, input) = call("clock", json!("{\"ms\":5}"));
        assert_eq!((name.as_str(), input), ("clock", json!({"ms": 5})));
        // Text that is not JSON stays text.
        let (_, input) = call("clock", json!("lorem"));
        assert_eq!(input, json!("lorem"));
    }

    #[test]
    fn mcp_calls_always_take_the_mcp_server_tool_name() {
        let call = |namespace: Option<&str>, name: &str| {
            let mut payload = json!({
                "type": "function_call", "call_id": "c", "name": name, "arguments": "{}",
            });
            if let Some(ns) = namespace {
                payload["namespace"] = json!(ns);
            }
            let Block::ToolUse { name, .. } = record_of(item(payload)).blocks[0].clone() else {
                panic!("tool use")
            };
            name
        };
        // The three shapes of a qualified name, and the one already in the Claude form.
        assert_eq!(
            call(Some("mcp__alice__"), "remember"),
            "mcp__alice__remember"
        );
        assert_eq!(call(Some("mcp__alice"), "remember"), "mcp__alice__remember");
        assert_eq!(call(Some("alice"), "remember"), "mcp__alice__remember");
        assert_eq!(call(None, "alice__remember"), "mcp__alice__remember");
        assert_eq!(call(None, "mcp__alice__remember"), "mcp__alice__remember");
        assert_eq!(
            call(Some("alice__"), "mcp__alice__remember"),
            "mcp__alice__remember"
        );
        // Another server's tool keeps its server, so it is not taken for an Alice tool.
        assert_eq!(call(Some("github"), "context"), "mcp__github__context");
        // A bare name stays bare.
        assert_eq!(call(None, "update_plan"), "TodoWrite");
        assert_eq!(call(None, "clock"), "clock");
    }

    #[test]
    fn a_codex_memory_call_renders_as_a_memory_line_whatever_its_name_shape() {
        for (namespace, name) in [
            (None, "alice__remember"),
            (Some("alice"), "remember"),
            (Some("mcp__alice__"), "remember"),
        ] {
            let Block::ToolUse { name, .. } = ({
                let mut payload = json!({
                    "type": "function_call", "call_id": "c", "name": name,
                    "arguments": "{\"content\":\"secret body\"}",
                });
                if let Some(ns) = namespace {
                    payload["namespace"] = json!(ns);
                }
                record_of(item(payload)).blocks[0].clone()
            }) else {
                panic!("tool use")
            };
            assert!(
                crate::capture::render::memory_tool(&name).is_some(),
                "{name} is a memory tool"
            );
        }
    }

    #[test]
    fn outputs_in_older_json_form_carry_their_exit_code() {
        let out = |output: Value| {
            let r = record_of(item(
                json!({"type": "function_call_output", "call_id": "c", "output": output}),
            ));
            assert_eq!(r.kind, RecordKind::ToolResults);
            r.blocks
        };
        assert_eq!(
            out(json!(
                "{\"output\":\"boom\",\"metadata\":{\"exit_code\":2}}"
            )),
            [Block::ToolResult {
                tool_use_id: "c".into(),
                text: "boom".into(),
                is_error: true
            }]
        );
        assert_eq!(
            out(
                json!([{"type": "input_text", "text": "a"}, {"type": "input_image", "image_url": "data:x"}])
            ),
            [
                Block::ToolResult {
                    tool_use_id: "c".into(),
                    text: "a".into(),
                    is_error: false
                },
                Block::Image,
            ]
        );
    }

    #[test]
    fn invalid_and_unknown_shapes() {
        assert_eq!(parse_line(b"nope"), LineResult::Invalid);
        assert_eq!(parse_line(b"[1]"), LineResult::Invalid);
        assert_eq!(parse_line(b"{}"), LineResult::Unknown);
        assert_eq!(
            parse(json!({"type": "future", "payload": {"content": []}})),
            LineResult::Unknown
        );
        assert_eq!(
            parse(item(json!({"type": "hologram"}))),
            LineResult::Unknown
        );
    }

    /// Follow-up 2: a multi-agent message (`response_item` / `agent_message`) is read as a block
    /// of the turn it occurs in, with its author and recipient. An `encrypted_content` block is
    /// opaque and skipped (no drift); an encrypted-only message leaves nothing to keep; an
    /// unknown block beside known ones is drift, as in any message.
    #[test]
    fn agent_messages_are_read_with_their_author_and_recipient() {
        let msg = |content: Value| {
            parse(item(
                json!({"type": "agent_message", "id": "am1", "author": "/root/review_w1",
                "recipient": "/root", "content": content,
                "internal_chat_message_metadata_passthrough": {"turn_id": "t1"}}),
            ))
        };
        let LineResult::Record(r) = msg(json!([
            {"type": "input_text", "text": "Review done: two findings."},
            {"type": "encrypted_content", "encrypted_content": "ZW5j"},
        ])) else {
            panic!("a record")
        };
        assert_eq!(r.kind, RecordKind::Assistant);
        assert!(!r.starts_turn(), "an agent message never starts a turn");
        assert_eq!(
            r.blocks,
            [Block::AgentMessage {
                author: "/root/review_w1".into(),
                recipient: "/root".into(),
                text: "Review done: two findings.".into(),
            }]
        );
        let LineResult::Record(r) = msg(json!([
            {"type": "input_text", "text": "Part one."},
            {"type": "input_text", "text": "Part two."},
        ])) else {
            panic!("a record")
        };
        assert_eq!(
            r.blocks,
            [Block::AgentMessage {
                author: "/root/review_w1".into(),
                recipient: "/root".into(),
                text: "Part one.\nPart two.".into(),
            }]
        );
        // Encrypted only: known, nothing to keep.
        let r = record_of(item(json!({"type": "agent_message", "author": "/root",
            "recipient": "/root/w2", "content": [{"type": "encrypted_content",
            "encrypted_content": "ZW5j"}]})));
        assert_eq!(r.kind, RecordKind::Skip);
        assert!(r.blocks.is_empty());
        // An unknown block beside a known one: kept, and drift; alone: unknown.
        assert!(matches!(
            msg(json!([{"type": "input_text", "text": "Hi."}, {"type": "hologram"}])),
            LineResult::Drift(_)
        ));
        assert_eq!(msg(json!([{"type": "hologram"}])), LineResult::Unknown);
        assert_eq!(msg(json!("not blocks")), LineResult::Unknown);
    }

    /// The multi-agent metadata line of newer builds (FORMAT.md "`type` / `payload` mapping"):
    /// known, so dropped as a `Skip` record, not left to the bookkeeping rule.
    #[test]
    fn inter_agent_metadata_is_known() {
        let r = record_of(
            json!({"timestamp": "t", "type": "inter_agent_communication_metadata",
                                 "ordinal": 3, "payload": {"trigger_turn": true}}),
        );
        assert_eq!(r.kind, RecordKind::Skip);
        assert!(r.blocks.is_empty());
    }

    /// A line `type` the parser does not know is bookkeeping when no key conversation, a tool call
    /// or its output is kept under appears in it at any depth, payload included. With one
    /// anywhere, or with a payload that is not an object (it may be the text), it stays
    /// `Unknown`; so does an unknown `payload.type` of a known line type.
    #[test]
    fn an_unknown_line_type_without_conversation_keys_is_bookkeeping() {
        let line = |payload: Value| {
            parse(
                json!({"timestamp": "t", "type": "future_ledger", "ordinal": 1,
                         "payload": payload}),
            )
        };
        assert_eq!(
            line(json!({"thread_id": "x", "counts": {"a": 1}})),
            LineResult::Bookkeeping
        );
        assert_eq!(line(json!({})), LineResult::Bookkeeping);
        assert_eq!(
            parse(json!({"timestamp": "t", "type": "future_ledger"})),
            LineResult::Bookkeeping
        );
        for key in [
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
        ] {
            assert_eq!(line(json!({key: "words"})), LineResult::Unknown, "{key}");
            assert_eq!(
                line(json!({"items": [{"deeper": {key: "words"}}]})),
                LineResult::Unknown,
                "nested {key}"
            );
            let mut top = json!({"timestamp": "t", "type": "future_ledger", "payload": {}});
            top[key] = json!("words");
            assert_eq!(parse(top), LineResult::Unknown, "top-level {key}");
        }
        // Tool calls and their outputs moved to a new line type (fix round 1, I3).
        assert_eq!(
            parse(
                json!({"timestamp": "t", "type": "turn_item", "payload": {"type": "function_call",
                "name": "exec", "arguments": "{\"cmd\":\"ls\"}", "call_id": "c"}})
            ),
            LineResult::Unknown
        );
        assert_eq!(
            parse(json!({"timestamp": "t", "type": "turn_item", "payload": {
                "type": "function_call_output", "call_id": "c", "output": "done"}})),
            LineResult::Unknown
        );
        assert_eq!(
            line(
                json!({"thread_id": "x", "turns": [{"usage": {"input_tokens": 1}}],
                        "outputTokens": 2, "note": "output"})
            ),
            LineResult::Bookkeeping
        );
        assert_eq!(line(json!("words")), LineResult::Unknown);
        assert_eq!(line(json!(["words"])), LineResult::Unknown);
        assert_eq!(
            parse(item(
                json!({"type": "future_message", "author": "a", "recipient": "b",
                              "content": [{"type": "input_text", "text": "words"}]})
            )),
            LineResult::Unknown
        );
    }
}
