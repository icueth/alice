//! Rendering: turns become tagged text, cut into chunks (spec §3.3).
//!
//! The format is a contract: the distiller parses chunks back by [`TAGS`]. Each block is a line
//! that starts with its tag, and a block's text may run over several lines until the next tag.
//! A chunk holds only those lines, never the original JSON.

use std::collections::HashMap;

use serde_json::Value;

use super::record::{
    BRIEFING_PREFIX, Block, RawTurn, Record, RecordKind, TASK_NOTIFICATION, strip_system_reminders,
    tag_content,
};
use super::{ReadOptions, RenderedChunk};

/// The tags a block line starts with (spec §3.3). The distiller parses chunks by these.
pub const TAGS: &[&str] = &[
    "[user]",
    "[assistant]",
    "[tool ",
    "[result]",
    "[result error]",
    "[summary]",
    "[subagent ",
];

/// Alice's and MemPalace's own tools: their input becomes a title and their result is omitted,
/// so memory text never re-enters the raw tier. An MCP name `mcp__<server>__<tool>` is matched on
/// its last `__` segment; a name ending in `_` here matches as a prefix.
pub const MEMORY_TOOLS: &[&str] = &[
    "remember",
    "recall",
    "context",
    "get_memory",
    "update_memory",
    "query_facts",
    "assert_fact",
    "mempalace_",
];

/// Bookkeeping tools: neither the call nor its result is kept.
const DROPPED_TOOLS: &[&str] = &[
    "TodoWrite",
    "TodoRead",
    "TaskCreate",
    "TaskUpdate",
    "TaskList",
    "TaskGet",
    "TaskOutput",
    "TaskStop",
    "ToolSearch",
];

/// The most bytes of one chunk (the store's chunk limit).
const MAX_CHUNK_BYTES: usize = 32_768;
/// `capture.chunk_chars` is never taken below this, so a block's tag always leaves room for text.
const MIN_CHUNK_CHARS: usize = 128;
/// A tool's input line, a command, a wrapper's output.
const TOOL_LINE_CHARS: usize = 300;
const MEMORY_TITLE_CHARS: usize = 120;
const SEARCH_RESULT_CHARS: usize = 300;
const WEB_RESULT_CHARS: usize = 500;
const SUBAGENT_REPORT_CHARS: usize = 2000;
const TOOL_NAME_CHARS: usize = 64;
/// A run of this many base64-like characters is binary data.
const BASE64_RUN: usize = 200;
const BINARY_MARKER: &str = "[binary removed]";

/// `text` cut to `keep` characters: the first 5/8 and the last 3/8, at character boundaries,
/// with `[… n characters trimmed …]` between them (`n` grouped by thousands). Text of `keep`
/// characters or fewer is returned unchanged.
pub fn trim(text: &str, keep: usize) -> String {
    let total = text.chars().count();
    if total <= keep {
        return text.to_owned();
    }
    let head_chars = keep * 5 / 8;
    let tail_chars = keep - head_chars;
    let head_end = byte_of_char(text, head_chars);
    let tail_start = byte_of_char(text, total - tail_chars);
    format!(
        "{}\n{}\n{}",
        &text[..head_end],
        marker(total - head_chars - tail_chars),
        &text[tail_start..]
    )
}

/// The first `keep` characters, and a marker when something was cut.
fn head(text: &str, keep: usize) -> String {
    let total = text.chars().count();
    if total <= keep {
        return text.to_owned();
    }
    format!(
        "{}\n{}",
        &text[..byte_of_char(text, keep)],
        marker(total - keep)
    )
}

fn marker(trimmed: usize) -> String {
    format!("[… {} characters trimmed …]", grouped(trimmed))
}

fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The byte offset of the `n`th character (the end when there are fewer).
fn byte_of_char(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(i, _)| i)
}

/// At most `n` characters, with `…` when something was cut. Line breaks become spaces, so a
/// title stays on one line.
fn clip(text: &str, n: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= n {
        return one_line;
    }
    let mut out: String = one_line.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The memory tool a tool name is, as the name that renders (`remember`, `mempalace_search`).
pub(crate) fn memory_tool(name: &str) -> Option<&str> {
    let tool = if name.starts_with("mcp__") {
        name.rsplit("__").next().unwrap_or(name)
    } else {
        name
    };
    MEMORY_TOOLS
        .iter()
        .any(|m| {
            if m.ends_with('_') {
                tool.starts_with(m)
            } else {
                tool == *m
            }
        })
        .then_some(tool)
}

/// Renders one turn into chunks.
pub(crate) fn turn(raw: &RawTurn, opts: &ReadOptions) -> Vec<RenderedChunk> {
    records(&raw.records, opts)
}

/// Renders records (a turn's, or a late tail) into chunks. The chunks depend only on the
/// records and the options, so a re-render is byte-identical.
pub(crate) fn records(records: &[Record], opts: &ReadOptions) -> Vec<RenderedChunk> {
    pack(&pieces(records, opts), opts.chunk_chars)
}

/// One rendered block: a tag, its text, and its record's time. The line is `tag text`.
struct Piece {
    tag: String,
    body: String,
    at: Option<String>,
}

/// What a tool's result is rendered as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    /// Bookkeeping: call and result dropped.
    Dropped,
    /// A memory tool: its result is omitted.
    Memory,
    /// Read, Edit and friends: the result is file content or a diff, kept only as an error.
    File,
    Search,
    Web,
    Subagent,
    /// Bash and every other tool: the result is trimmed to `tool_output_chars`.
    Other,
}

fn classify(name: &str) -> ToolKind {
    if DROPPED_TOOLS.contains(&name) {
        ToolKind::Dropped
    } else if memory_tool(name).is_some() {
        ToolKind::Memory
    } else {
        match name {
            "Read" | "NotebookRead" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                ToolKind::File
            }
            "Grep" | "Glob" | "LS" => ToolKind::Search,
            "WebFetch" | "WebSearch" => ToolKind::Web,
            "Task" | "Agent" => ToolKind::Subagent,
            _ => ToolKind::Other,
        }
    }
}

fn pieces(records: &[Record], opts: &ReadOptions) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    // Tool names by `tool_use` id, so each result is rendered as its tool's result.
    let mut tools: HashMap<&str, ToolKind> = HashMap::new();
    for record in records {
        let at = record.timestamp.clone();
        let mut push = |tag: String, body: String| {
            out.push(Piece {
                tag,
                body,
                at: at.clone(),
            });
        };
        // Media that follows a result nobody keeps is not kept either.
        let mut quiet_media = false;
        for block in &record.blocks {
            match block {
                Block::Text(text) => {
                    quiet_media = false;
                    match record.kind {
                        RecordKind::Assistant => {
                            if let Some(body) = clean(text) {
                                push("[assistant]".into(), body);
                            }
                        }
                        RecordKind::Summary => {
                            if let Some(body) = clean(text) {
                                push("[summary]".into(), body);
                            }
                        }
                        _ => {
                            for (tag, body) in user_lines(text) {
                                push(tag, body);
                            }
                        }
                    }
                }
                Block::Thinking(text) => {
                    quiet_media = false;
                    if let Some(body) = clean(text).filter(|_| opts.include_thinking) {
                        push("[assistant]".into(), format!("(thinking) {body}"));
                    }
                }
                Block::ToolUse { id, name, input } => {
                    quiet_media = false;
                    let kind = classify(name);
                    tools.insert(id.as_str(), kind);
                    if let Some((tag, body)) = tool_line(name, kind, input) {
                        push(tag, body);
                    }
                }
                Block::ToolResult {
                    tool_use_id,
                    text,
                    is_error,
                } => {
                    let kind = tools
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or(ToolKind::Other);
                    let line = result_line(kind, text, *is_error, opts);
                    quiet_media = line.is_none()
                        || matches!(kind, ToolKind::Dropped | ToolKind::Memory | ToolKind::File);
                    if let Some((tag, body)) = line {
                        push(tag, body);
                    }
                }
                Block::Image => {
                    if !quiet_media {
                        push("[image]".into(), String::new());
                    }
                }
                Block::Document => {
                    if !quiet_media {
                        push("[document]".into(), String::new());
                    }
                }
                Block::Audio => {
                    if !quiet_media {
                        push("[audio]".into(), String::new());
                    }
                }
                Block::TooLarge => {
                    quiet_media = false;
                    push("[record skipped: too large]".into(), String::new());
                }
            }
        }
    }
    out
}

/// The text of a block with binary removed, or `None` when nothing is left.
fn clean(text: &str) -> Option<String> {
    let text = strip_binary(text);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Spec §3.3: text over 30 % unprintable is `[binary removed]` as a whole, and every run of 200
/// or more base64-like characters is replaced in place.
fn strip_binary(text: &str) -> String {
    let total = text.chars().count();
    let unprintable = text
        .chars()
        .filter(|&c| (c.is_control() && !matches!(c, '\n' | '\r' | '\t')) || c == '\u{FFFD}')
        .count();
    if total > 0 && unprintable * 10 > total * 3 {
        return BINARY_MARKER.to_owned();
    }
    let is_b64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
    let mut out = String::with_capacity(text.len());
    let mut run_start: Option<usize> = None;
    let mut run_chars = 0;
    for (i, c) in text.char_indices() {
        if is_b64(c) {
            if run_start.is_none() {
                run_start = Some(i);
                run_chars = 0;
            }
            run_chars += 1;
        } else {
            if let Some(start) = run_start.take() {
                flush_run(&mut out, &text[start..i], run_chars);
            }
            out.push(c);
        }
    }
    if let Some(start) = run_start {
        flush_run(&mut out, &text[start..], run_chars);
    }
    out
}

fn flush_run(out: &mut String, run: &str, chars: usize) {
    if chars >= BASE64_RUN {
        out.push_str(BINARY_MARKER);
    } else {
        out.push_str(run);
    }
}

/// The harness wrappers whose content is kept: command name and arguments, and command output.
const WRAPPER_TAGS: &[&str] = &[
    "command-name",
    "command-args",
    "local-command-stdout",
    "local-command-stderr",
    "bash-input",
    "bash-stdout",
    "bash-stderr",
];

/// Every other wrapper opening: dropped, with its content.
const DROPPED_WRAPPERS: &[&str] = &["<command-message>", "<local-command-caveat>"];

/// The lines a user text block renders as: the prompt, a command wrapper's parts, a task
/// notification's summary. Reminders and briefings are gone.
fn user_lines(text: &str) -> Vec<(String, String)> {
    let stripped = strip_system_reminders(text);
    let text = stripped.trim();
    if text.is_empty() || text.starts_with(BRIEFING_PREFIX) {
        return Vec::new();
    }
    if text.starts_with(TASK_NOTIFICATION) {
        return match tag_content(text, "summary").filter(|s| !s.is_empty()) {
            Some(summary) => vec![("[user]".into(), format!("(task notification) {summary}"))],
            None => Vec::new(),
        };
    }
    let wrapped = WRAPPER_TAGS
        .iter()
        .any(|t| text.starts_with(&format!("<{t}>")))
        || DROPPED_WRAPPERS.iter().any(|t| text.starts_with(t));
    if !wrapped {
        return clean(text)
            .map(|body| ("[user]".to_owned(), body))
            .into_iter()
            .collect();
    }
    wrapper_lines(text)
}

/// Walks a wrapper message tag by tag. The command name keeps its arguments on one line; output
/// is cut to 300 characters.
fn wrapper_lines(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (mut name, mut args): (Option<String>, String) = (None, String::new());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        rest = &rest[open..];
        let Some(tag) = WRAPPER_TAGS
            .iter()
            .find(|t| rest.starts_with(&format!("<{t}>")))
        else {
            // Another tag (a dropped wrapper, or a stray `<`): skip past its `>`.
            let skip_to = rest[1..].find('>').map_or(rest.len(), |p| p + 2);
            rest = &rest[skip_to..];
            continue;
        };
        let open_len = tag.len() + 2;
        let close = format!("</{tag}>");
        let (content, after) = match rest[open_len..].find(&close) {
            Some(end) => (
                &rest[open_len..open_len + end],
                open_len + end + close.len(),
            ),
            None => (&rest[open_len..], rest.len()),
        };
        let content = content.trim();
        match *tag {
            "command-name" => name = Some(content.to_owned()),
            "command-args" => args = content.to_owned(),
            "local-command-stdout" | "bash-stdout" => {
                if let Some(body) = clean(content) {
                    out.push(("[result]".to_owned(), head(&body, TOOL_LINE_CHARS)));
                }
            }
            "local-command-stderr" | "bash-stderr" => {
                if let Some(body) = clean(content) {
                    out.push(("[result error]".to_owned(), head(&body, TOOL_LINE_CHARS)));
                }
            }
            _ => {
                if !content.is_empty() {
                    out.push(("[tool Bash]".to_owned(), clip(content, TOOL_LINE_CHARS)));
                }
            }
        }
        rest = &rest[after..];
    }
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        let line = if args.is_empty() {
            name
        } else {
            format!("{name} {args}")
        };
        out.insert(0, ("[user]".to_owned(), clip(&line, TOOL_LINE_CHARS)));
    }
    out
}

/// The string at the first of `keys` that holds one.
fn input_str<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
}

/// A tool call's line, or `None` for a dropped tool.
fn tool_line(name: &str, kind: ToolKind, input: &Value) -> Option<(String, String)> {
    let shown: String = name.chars().take(TOOL_NAME_CHARS).collect();
    let tag = format!("[tool {shown}]");
    let line = |keys: &[&str], limit: usize| {
        input_str(input, keys)
            .map(|s| clip(s, limit))
            .unwrap_or_default()
    };
    Some(match kind {
        ToolKind::Dropped => return None,
        ToolKind::Memory => {
            let tool = memory_tool(name).unwrap_or(name);
            let tool: String = tool.chars().take(TOOL_NAME_CHARS).collect();
            let title = if tool == "remember" {
                input_str(input, &["title"]).or_else(|| input_str(input, &["content"]))
            } else {
                input_str(input, &["query", "content", "title", "entry", "text", "id"])
            };
            let first_line = title.and_then(|t| t.trim().lines().next()).unwrap_or("");
            (
                format!("[tool alice {tool}]"),
                clip(first_line, MEMORY_TITLE_CHARS),
            )
        }
        ToolKind::File => (
            tag,
            line(&["file_path", "notebook_path", "path"], TOOL_LINE_CHARS),
        ),
        ToolKind::Search => (tag, line(&["pattern", "path", "query"], TOOL_LINE_CHARS)),
        ToolKind::Web => (tag, line(&["url", "query"], TOOL_LINE_CHARS)),
        ToolKind::Subagent => {
            let kind: String = input_str(input, &["subagent_type"])
                .unwrap_or("general-purpose")
                .chars()
                .take(TOOL_NAME_CHARS)
                .collect();
            (
                format!("[subagent {kind}]"),
                line(&["description", "prompt"], TOOL_LINE_CHARS),
            )
        }
        ToolKind::Other => {
            if name == "Bash" {
                (tag, line(&["command"], TOOL_LINE_CHARS))
            } else if input.is_null() || input.as_object().is_some_and(|o| o.is_empty()) {
                (tag, String::new())
            } else {
                (tag, clip(&input.to_string(), TOOL_LINE_CHARS))
            }
        }
    })
}

/// A tool result's line, or `None` when it is not kept.
fn result_line(
    kind: ToolKind,
    text: &str,
    is_error: bool,
    opts: &ReadOptions,
) -> Option<(String, String)> {
    match kind {
        ToolKind::Dropped => return None,
        ToolKind::Memory => return Some(("[result omitted: memory tool]".into(), String::new())),
        ToolKind::File if !is_error => return None,
        _ => {}
    }
    let body = clean(text);
    if is_error {
        let body = body
            .map(|b| trim(&b, opts.tool_output_chars))
            .unwrap_or_default();
        return Some(("[result error]".into(), body));
    }
    let body = body?;
    let body = match kind {
        ToolKind::Search => head(&body, SEARCH_RESULT_CHARS),
        ToolKind::Web => head(&body, WEB_RESULT_CHARS),
        ToolKind::Subagent => head(&body, SUBAGENT_REPORT_CHARS),
        _ => trim(&body, opts.tool_output_chars),
    };
    Some(("[result]".into(), body))
}

impl Piece {
    fn line(&self) -> String {
        join_line(&self.tag, &self.body)
    }
}

fn join_line(tag: &str, body: &str) -> String {
    if body.is_empty() {
        tag.to_owned()
    } else {
        format!("{tag} {body}")
    }
}

/// A chunk being filled.
#[derive(Default)]
struct Open {
    text: String,
    chars: usize,
    at: Option<String>,
}

impl Open {
    fn push(&mut self, line: &str, chars: usize, at: &Option<String>) {
        if !self.text.is_empty() {
            self.text.push('\n');
            self.chars += 1;
        }
        self.text.push_str(line);
        self.chars += chars;
        if self.at.is_none() {
            self.at.clone_from(at);
        }
    }
}

/// Packs blocks greedily into chunks of at most `chunk_chars` characters (and 32,768 bytes).
/// A block that cannot fit a chunk of its own starts a fresh one and is cut into pieces.
fn pack(pieces: &[Piece], chunk_chars: usize) -> Vec<RenderedChunk> {
    let cap = chunk_chars.max(MIN_CHUNK_CHARS);
    let mut chunks = Vec::new();
    let mut cur = Open::default();
    let flush = |cur: &mut Open, chunks: &mut Vec<RenderedChunk>| {
        if !cur.text.is_empty() {
            let open = std::mem::take(cur);
            chunks.push(RenderedChunk {
                text: open.text,
                authored_at: open.at,
            });
        }
    };
    for piece in pieces {
        let line = piece.line();
        let chars = line.chars().count();
        if chars <= cap && line.len() <= MAX_CHUNK_BYTES {
            let fits = cur.text.is_empty()
                || (cur.chars + 1 + chars <= cap
                    && cur.text.len() + 1 + line.len() <= MAX_CHUNK_BYTES);
            if !fits {
                flush(&mut cur, &mut chunks);
            }
            cur.push(&line, chars, &piece.at);
            continue;
        }
        flush(&mut cur, &mut chunks);
        let parts = split_block(&piece.tag, &piece.body, cap);
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            cur.push(part, part.chars().count(), &piece.at);
            if i < last {
                flush(&mut cur, &mut chunks);
            }
        }
    }
    flush(&mut cur, &mut chunks);
    chunks
}

/// Cuts an oversized block into lines that fit: at paragraph ends, then sentence ends and line
/// ends, then hard. The first line is `tag text`; the others start `tag (cont.) `.
fn split_block(tag: &str, body: &str, cap: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = body.trim();
    let mut first = true;
    loop {
        let prefix = if first {
            format!("{tag} ")
        } else {
            format!("{tag} (cont.) ")
        };
        let max_chars = cap.saturating_sub(prefix.chars().count()).max(1);
        let max_bytes = MAX_CHUNK_BYTES.saturating_sub(prefix.len()).max(4);
        if rest.len() <= max_bytes && rest.chars().take(max_chars + 1).count() <= max_chars {
            parts.push(format!("{prefix}{rest}"));
            return parts;
        }
        let limit = prefix_len(rest, max_chars, max_bytes);
        let cut = cut_point(&rest[..limit]);
        parts.push(format!("{prefix}{}", rest[..cut].trim_end()));
        rest = rest[cut..].trim_start();
        first = false;
        if rest.is_empty() {
            return parts;
        }
    }
}

/// The longest prefix of `text` (a byte offset on a character boundary) within both limits.
fn prefix_len(text: &str, max_chars: usize, max_bytes: usize) -> usize {
    let mut end = 0;
    for (count, (i, c)) in text.char_indices().enumerate() {
        let next = i + c.len_utf8();
        if count >= max_chars || next > max_bytes {
            break;
        }
        end = next;
    }
    end.max(text.chars().next().map_or(0, char::len_utf8))
}

/// Where to cut `window` (a block's next stretch): after its last paragraph break, else after
/// its last sentence end or line break, else at its end. A cut in the first quarter is not used,
/// so a piece is never a sliver.
fn cut_point(window: &str) -> usize {
    let floor = window.len() / 4;
    let paragraph = window.rfind("\n\n").map(|i| i + 2);
    if let Some(cut) = paragraph.filter(|&c| c > floor) {
        return cut;
    }
    let sentence = [
        window.rfind(". ").map(|i| i + 2),
        window.rfind("! ").map(|i| i + 2),
        window.rfind("? ").map(|i| i + 2),
        window.rfind('。').map(|i| i + '。'.len_utf8()),
        window.rfind('\n').map(|i| i + 1),
    ]
    .into_iter()
    .flatten()
    .max();
    sentence.filter(|&c| c > floor).unwrap_or(window.len())
}

#[cfg(test)]
mod tests {
    use super::super::{AgentKind, ReadOptions, read};
    use super::*;

    const TOOLS: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/tools.jsonl");
    const INJECTED: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/injected.jsonl");
    const BINARY: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/binary.jsonl");
    const TURNS: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/turns.jsonl");

    const CLAUDE: AgentKind = AgentKind::ClaudeCode;

    fn closed() -> ReadOptions {
        ReadOptions {
            close_trailing: true,
            ..ReadOptions::default()
        }
    }

    fn render_all(input: &[u8], opts: &ReadOptions) -> String {
        let out = read(CLAUDE, input, opts);
        out.turns
            .iter()
            .flat_map(|t| &t.chunks)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The longest run of base64-alphabet characters.
    fn longest_b64_run(text: &str) -> usize {
        let (mut best, mut run) = (0, 0);
        for c in text.chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=') {
                run += 1;
                best = best.max(run);
            } else {
                run = 0;
            }
        }
        best
    }

    fn lines_with<'a>(out: &'a str, tag: &str) -> Vec<&'a str> {
        out.lines().filter(|l| l.starts_with(tag)).collect()
    }

    #[test]
    fn every_rendering_row_of_the_spec_table() {
        let out = render_all(TOOLS, &closed());
        assert!(
            out.contains("[tool Read] /home/user/project-x/src/main.rs")
                && !out.contains("fn main")
        );
        assert!(
            out.contains("[tool Edit] /home/user/project-x/src/lib.rs")
                && !out.contains("old_string")
        );
        assert!(!out.contains("OLDBODY") && !out.contains("has been updated"));
        assert!(out.contains("[tool Bash] git status --short"));
        assert!(out.contains("[… ") && out.contains(" characters trimmed …]"));
        assert!(
            out.contains("[tool alice remember] Use pnpm, not npm")
                && out.contains("[result omitted: memory tool]")
        );
        assert!(!out.contains("TodoWrite") && !out.contains("TODOITEM"));
        assert!(!out.contains("Todos have been modified"));
        assert!(out.contains("[subagent general-purpose]"));

        // The prompt, the closing text and the other rows.
        assert!(out.contains("[user] Check the project"));
        assert!(out.contains("[assistant] Saved the preference"));
        assert!(out.contains("[tool alice mempalace_search] package manager choice"));
        assert!(!out.contains("MEMORYTEXT-SHOULD-NOT-APPEAR"));
        assert!(!out.contains("MEMORYRESULT"));
        assert_eq!(lines_with(&out, "[result omitted: memory tool]").len(), 2);
        assert!(out.contains("[subagent general-purpose] Survey the repository layout"));
        assert!(out.contains("Report: the repository has two crates."));
        assert!(out.contains("[tool Grep] needle_pattern"));
        assert!(out.contains("[tool WebFetch] https://example.com/docs"));
        assert!(out.contains(
            r#"[tool mcp__server-a__page_eval] {"expression":"document.title","timeoutMs":1000}"#
        ));
        assert!(out.contains("[result] Title of the page"));
        assert!(out.contains("[result error] error: could not compile `x`"));
        assert!(!out.contains("[summary]"), "no summary in this fixture");

        // Result limits: Grep 300, WebFetch 500, Bash 800 (500 + 300 around the marker).
        let body_after = |tag: &str| -> String {
            let line = out.lines().find(|l| l.starts_with(tag)).unwrap();
            line[tag.len()..].to_owned()
        };
        assert_eq!(body_after("[result] hit ").chars().count(), 300 - 4);
        assert_eq!(body_after("[result] page ").chars().count(), 500 - 5);
        assert!(
            out.contains("[… 403 characters trimmed …]"),
            "grep: 703 - 300"
        );
        assert!(
            out.contains("[… 404 characters trimmed …]"),
            "web: 904 - 500"
        );
        let bash_start = out.find("line 0000").unwrap();
        let bash_end = out[bash_start..].find("[tool alice remember]").unwrap();
        let bash = &out[bash_start..bash_start + bash_end];
        assert!(bash.contains("line 0000") && bash.contains("line 0166"));
        assert!(
            bash.chars().count() < 1000,
            "trimmed to 800 plus the marker"
        );
        assert!(!out.contains("line 0100"));
    }

    #[test]
    fn every_line_starts_with_a_tag_or_continues_a_block() {
        let out = render_all(TOOLS, &closed());
        for tag in [
            "[user]",
            "[assistant]",
            "[tool ",
            "[result]",
            "[result error]",
            "[subagent ",
        ] {
            assert!(TAGS.contains(&tag), "{tag}");
            assert!(out.contains(tag), "{tag}");
        }
        assert_eq!(
            TAGS,
            [
                "[user]",
                "[assistant]",
                "[tool ",
                "[result]",
                "[result error]",
                "[summary]",
                "[subagent "
            ]
        );
    }

    #[test]
    fn system_reminders_and_hook_context_are_stripped() {
        let out = render_all(INJECTED, &closed());
        assert!(!out.contains("<system-reminder>") && !out.contains("</system-reminder>"));
        assert!(!out.contains("CLAUDE.md") && !out.contains("secret handshake"));
        assert!(!out.contains("remember the handshake"));
        assert!(!out.contains("# Context · ") && !out.contains("Briefing memory line"));
        assert!(!out.contains("Another briefing line"));
        // The real text stays.
        assert!(out.contains("[user] What does the parser do?"));
        assert!(out.contains("[user] Second question, with context before it."));
        assert!(out.contains("[user] Third question after a briefing block."));
        // Wrappers keep the command name and at most 300 characters of output.
        assert!(out.contains("[user] /review src/lib.rs"));
        assert!(!out.contains("<command-"));
        assert!(!out.contains("Caveat: do not respond"));
        let stdout = lines_with(&out, "[result] output line.");
        assert_eq!(stdout.len(), 1);
        assert_eq!(stdout[0].chars().count(), "[result] ".len() + 300);
        assert!(out.contains("[… 115 characters trimmed …]"), "415 - 300");
        // A task notification keeps its summary.
        assert!(out.contains("[user] (task notification) Background build finished"));
        assert!(!out.contains("<task-notification>") && !out.contains("<status>"));
    }

    #[test]
    fn images_binaries_and_thinking_never_stored() {
        let out = render_all(BINARY, &closed());
        assert!(out.contains("[image]"));
        assert_eq!(
            lines_with(&out, "[image]").len(),
            2,
            "a prompt and a result image"
        );
        assert!(longest_b64_run(&out) < 200, "{}", longest_b64_run(&out));
        assert!(out.contains("[binary removed]"));
        assert!(out.contains("Here is a dump: [binary removed] and then prose continues."));
        assert!(out.contains("[result] [binary removed]"));
        assert!(!out.contains("iVBORw0KGgo") && !out.contains('\u{0}'));
        assert!(!out.contains("PRIVATE-THINKING-TEXT"));

        let thinking = render_all(
            BINARY,
            &ReadOptions {
                include_thinking: true,
                ..closed()
            },
        );
        assert!(thinking.contains("PRIVATE-THINKING-TEXT about the image"));
    }

    #[test]
    fn trim_keeps_head_and_tail_at_char_boundaries() {
        let thai: String = "กขคงจฉชซฌญ".chars().cycle().take(3000).collect();
        let out = trim(&thai, 800);
        let head: String = thai.chars().take(500).collect();
        let tail: String = thai.chars().skip(2700).collect();
        assert!(out.starts_with(&head) && out.ends_with(&tail));
        assert!(out.contains("[… 2,200 characters trimmed …]"));
        let marker = out.find("[… ").unwrap();
        assert!(marker >= head.len(), "the marker comes after the head");
        assert!(out[marker..].len() > tail.len(), "and before the tail");
        // Short text and exact fits are untouched.
        assert_eq!(trim("short", 800), "short");
        let exact = "é".repeat(800);
        assert_eq!(trim(&exact, 800), exact);
        assert_ne!(trim(&"é".repeat(801), 800), "é".repeat(801));
        // Thousands are grouped, and a zero budget keeps only the marker.
        assert!(trim(&"x".repeat(1_234_567), 800).contains("[… 1,233,767 characters trimmed …]"));
        assert!(trim("abc", 0).contains("[… 3 characters trimmed …]"));
    }

    fn block_input(blocks: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut n = 0;
        for (role, text) in blocks {
            n += 1;
            let line = if *role == "user" {
                serde_json::json!({
                    "type": "user", "uuid": format!("u-{n}"), "isSidechain": false,
                    "timestamp": format!("2026-10-14T09:00:{n:02}.000Z"),
                    "message": {"role": "user", "content": text},
                })
            } else {
                serde_json::json!({
                    "type": "assistant", "uuid": format!("u-{n}"), "isSidechain": false,
                    "timestamp": format!("2026-10-14T09:00:{n:02}.000Z"),
                    "message": {"id": format!("m-{n}"), "role": "assistant",
                                "content": [{"type": "text", "text": text}]},
                })
            };
            out.extend(serde_json::to_vec(&line).unwrap());
            out.push(b'\n');
        }
        out
    }

    #[test]
    fn chunks_respect_chunk_chars_and_cut_at_blocks() {
        let paragraphs: Vec<String> = (0..20)
            .map(|i| format!("Paragraph {i:02} says something short and plain, then ends."))
            .collect();
        let long = paragraphs.join("\n\n");
        assert!(long.chars().count() > 1000);
        let input = block_input(&[
            ("user", "A short question."),
            ("assistant", "A short answer."),
            ("assistant", &long),
            ("assistant", "A closing remark."),
        ]);
        let opts = ReadOptions {
            chunk_chars: 500,
            ..closed()
        };
        let out = read(CLAUDE, &input, &opts);
        assert_eq!(out.turns.len(), 1);
        let chunks = &out.turns[0].chunks;
        assert!(chunks.len() >= 3);
        for c in chunks {
            assert!(c.text.chars().count() <= 500, "{}", c.text.chars().count());
            assert!(c.text.len() <= 32_768);
        }
        // Short blocks pack together; the long block starts a chunk of its own.
        assert!(chunks[0].text.contains("[user] A short question."));
        assert!(chunks[0].text.contains("[assistant] A short answer."));
        assert!(!chunks[0].text.contains("Paragraph"));
        let long_chunks: Vec<&str> = chunks
            .iter()
            .map(|c| c.text.as_str())
            .filter(|t| t.contains("Paragraph"))
            .collect();
        assert!(long_chunks.len() >= 3);
        assert!(long_chunks[0].starts_with("[assistant] Paragraph 00"));
        for (i, c) in long_chunks.iter().enumerate() {
            if i > 0 {
                assert!(c.starts_with("[assistant] (cont.) Paragraph"), "{c}");
            }
            let last = c.lines().last().unwrap();
            if i + 1 < long_chunks.len() {
                assert!(last.ends_with("ends."), "cut at a paragraph end: {last}");
            }
        }
        // Nothing is lost: every paragraph is there exactly once.
        let all: String = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for p in &paragraphs {
            assert_eq!(all.matches(p.as_str()).count(), 1, "{p}");
        }
        assert!(all.contains("[assistant] A closing remark."));

        // A block with no paragraph or sentence end is cut hard, still within the limit.
        let blob = "xy ".repeat(570);
        let input = block_input(&[("user", "Q."), ("assistant", &blob)]);
        let out = read(CLAUDE, &input, &opts);
        let chunks = &out.turns[0].chunks;
        assert!(chunks.len() >= 4);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 500));
        let xs: usize = chunks.iter().map(|c| c.text.matches('x').count()).sum();
        assert_eq!(xs, 570);
        assert!(chunks[2].text.starts_with("[assistant] (cont.) "));
    }

    #[test]
    fn chunks_stay_under_the_byte_limit_with_wide_characters() {
        // 30,000 Thai characters are 90,000 bytes; chunk_chars is far above the byte limit.
        let thai: String = "สวัสดีชาวโลก ".repeat(2500);
        let input = block_input(&[("user", "Q."), ("assistant", &thai)]);
        let opts = ReadOptions {
            chunk_chars: 1_000_000,
            ..closed()
        };
        let out = read(CLAUDE, &input, &opts);
        let chunks = &out.turns[0].chunks;
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| c.text.len() <= 32_768));
        assert!(
            chunks
                .iter()
                .all(|c| std::str::from_utf8(c.text.as_bytes()).is_ok())
        );
    }

    #[test]
    fn rendering_is_deterministic() {
        let opts = ReadOptions {
            chunk_chars: 300,
            ..closed()
        };
        for input in [TOOLS, INJECTED, BINARY, TURNS] {
            let a = read(CLAUDE, input, &opts);
            let b = read(CLAUDE, input, &opts);
            assert_eq!(a.turns, b.turns);
        }
        // A re-read starting at the turn's offset renders the turn byte for byte.
        let second = read(
            CLAUDE,
            TURNS,
            &ReadOptions {
                chunk_chars: 300,
                ..closed()
            },
        );
        let key = &second.turns[1].key;
        let marker = format!("\"uuid\":\"{key}\"");
        let pos = TURNS
            .windows(marker.len())
            .position(|w| w == marker.as_bytes())
            .unwrap();
        let start = TURNS[..pos]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1);
        let again = read(
            CLAUDE,
            &TURNS[start..],
            &ReadOptions {
                at_file_start: false,
                chunk_chars: 300,
                ..closed()
            },
        );
        assert_eq!(again.turns[0], second.turns[1]);
    }

    #[test]
    fn chunk_authored_at_is_its_first_record_time() {
        let input = block_input(&[
            ("user", "First prompt."),
            ("assistant", &"Sentence one is here. ".repeat(60)),
            ("assistant", "A last reply."),
        ]);
        let opts = ReadOptions {
            chunk_chars: 400,
            ..closed()
        };
        let out = read(CLAUDE, &input, &opts);
        let chunks = &out.turns[0].chunks;
        assert!(chunks.len() >= 3);
        assert_eq!(
            chunks[0].authored_at.as_deref(),
            Some("2026-10-14T09:00:01.000Z")
        );
        // The long block's chunks carry the long block's record time (its first record) ...
        assert_eq!(
            chunks[1].authored_at.as_deref(),
            Some("2026-10-14T09:00:02.000Z")
        );
        // ... and the last chunk starts with, or continues from, the closing records.
        let last = chunks.last().unwrap();
        assert!(last.text.contains("A last reply."));
        assert!(last.authored_at.is_some());
        assert_eq!(
            out.turns[0].started_at.as_deref(),
            Some("2026-10-14T09:00:01.000Z")
        );
    }

    #[test]
    fn a_compaction_summary_renders_as_summary() {
        const COMPACTION: &[u8] =
            include_bytes!("../../tests/fixtures/capture/claude/compaction.jsonl");
        let out = render_all(COMPACTION, &closed());
        assert!(out.contains("[summary] This session continues an earlier conversation."));
        assert!(out.contains("Summary: the parser was split."));
    }

    #[test]
    fn memory_tool_names_match_on_their_last_segment() {
        assert_eq!(MEMORY_TOOLS.len(), 8);
        for (name, want) in [
            ("mcp__alice__remember", true),
            ("mcp__alice__recall", true),
            (
                "mcp__plugin_mempalace_mempalace__mempalace_add_drawer",
                true,
            ),
            ("remember", true),
            ("mempalace_search", true),
            ("mcp__server-a__remember_not", false),
            ("mcp__alice__page_eval", false),
            ("Bash", false),
            ("context_free", false),
        ] {
            assert_eq!(memory_tool(name).is_some(), want, "{name}");
        }
        assert_eq!(memory_tool("mcp__alice__remember"), Some("remember"));
    }
}
