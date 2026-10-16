//! Records and turns: the agent-independent half of a transcript reader (spec §3.3).
//!
//! An agent reader (`claude.rs`, later Codex) turns each line into a [`Record`]. [`group`] then
//! applies the turn rule, completeness and the late-record rule, and removes streaming
//! duplicates, so a renderer only ever sees finished turns.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::Value;

use super::turn_key;

/// One transcript line the reader understood.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Record {
    pub uuid: Option<String>,
    /// The API message id. Several records can share it (one response written as several lines).
    pub message_id: Option<String>,
    pub timestamp: Option<String>,
    pub kind: RecordKind,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// A user message with text (or a pasted image). It starts a turn unless its text is only
    /// harness wrappers (see [`Record::starts_turn`]); either way its blocks are the user's.
    Prompt,
    Assistant,
    /// A user message carrying tool results. Never starts a turn.
    ToolResults,
    /// A compaction summary. Always starts a turn of its own.
    Summary,
    /// A recognised line that is not conversation: bookkeeping, `system`, attachments, `isMeta`
    /// text and main-file sidechains. It has no blocks, except the [`Block::TooLarge`] stub of a
    /// line that was too long to parse. It never starts a turn, and [`group`] drops it when empty.
    Skip,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result's text: string content, or the text parts of array content joined by `\n`.
    /// Images (documents, audio) inside a result never become text: they follow the result as
    /// their own [`Block::Image`] (`Document`, `Audio`) blocks.
    ToolResult {
        tool_use_id: String,
        text: String,
        is_error: bool,
    },
    Image,
    Document,
    Audio,
    /// The stub of a line over `MAX_LINE_BYTES`: `[record skipped: too large]`.
    TooLarge,
}

/// Text that opens a harness wrapper rather than something the person typed. A user message
/// whose every text block starts with one of these does not start a turn (spec §3.3: "holding
/// only a command wrapper").
const WRAPPER_PREFIXES: &[&str] = &[
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
    "[Request interrupted by user",
];

/// The opening of a background-task notification written as a user message.
pub(crate) const TASK_NOTIFICATION: &str = "<task-notification>";

/// Alice's own session briefing, when a hook leaves it as plain user text (spec §3.3: injected
/// context is not captured).
pub(crate) const BRIEFING_PREFIX: &str = "# Context · ";

const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_CLOSE: &str = "</system-reminder>";

/// How far into each end of an oversized line [`Record::too_large`] looks for its id and time.
const STUB_SCAN_BYTES: usize = 64 << 10;

impl Record {
    /// Whether this record starts a turn (spec §3.3).
    pub fn starts_turn(&self) -> bool {
        match self.kind {
            RecordKind::Summary => true,
            RecordKind::Prompt => self.blocks.iter().any(|block| match block {
                Block::Text(text) => is_typed_text(text),
                Block::Image | Block::Document | Block::Audio => true,
                _ => false,
            }),
            _ => false,
        }
    }

    /// The person's own words, for [`super::CapturedTurn::prompt`]: set only when this record
    /// starts a turn as a prompt. Reminder spans, harness wrappers and briefings are removed. A
    /// task notification (or any prompt left with no words) falls back to its `<summary>`;
    /// without one there is no prompt.
    pub fn real_prompt(&self) -> Option<String> {
        if self.kind != RecordKind::Prompt || !self.starts_turn() {
            return None;
        }
        let mut summary = None;
        let mut parts: Vec<String> = Vec::new();
        for block in &self.blocks {
            let Block::Text(text) = block else { continue };
            let stripped = strip_system_reminders(text);
            let rest = stripped.trim();
            if rest.starts_with(TASK_NOTIFICATION) {
                summary = summary.or_else(|| tag_content(rest, "summary"));
            } else if is_typed_text(rest) {
                parts.push(rest.to_owned());
            }
        }
        let words = parts.join("\n");
        if words.trim().is_empty() {
            summary.filter(|s| !s.is_empty())
        } else {
            Some(words)
        }
    }

    /// This record's turn key (spec §3.5): its uuid, else a hash of its time and first text.
    pub fn turn_key(&self) -> String {
        turn_key(
            self.uuid.as_deref(),
            self.timestamp.as_deref(),
            self.key_text(),
        )
    }

    fn key_text(&self) -> &str {
        self.blocks
            .iter()
            .find_map(|block| match block {
                Block::Text(text) | Block::Thinking(text) => Some(text.as_str()),
                Block::ToolResult { text, .. } => Some(text.as_str()),
                Block::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .unwrap_or("")
    }

    /// The stub left by a line too long to parse. The line is not parsed; its `uuid` and
    /// `timestamp` are looked up as plain `"key":"value"` text near both ends (best effort), so
    /// the stub can still key a head turn and date its chunk.
    pub fn too_large(line: &[u8]) -> Record {
        Record {
            uuid: scan_string_field(line, "uuid").filter(|v| super::valid_id(v)),
            message_id: None,
            timestamp: scan_string_field(line, "timestamp").filter(|v| {
                v.len() <= 40
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b":.+-".contains(&b))
            }),
            kind: RecordKind::Skip,
            blocks: vec![Block::TooLarge],
        }
    }
}

/// A text block that counts as typed: something left after removing reminders, and not a
/// harness wrapper.
fn is_typed_text(text: &str) -> bool {
    let stripped = strip_system_reminders(text);
    let rest = stripped.trim_start();
    !rest.is_empty()
        && !rest.starts_with(BRIEFING_PREFIX)
        && !WRAPPER_PREFIXES.iter().any(|p| rest.starts_with(p))
}

/// The trimmed text between `<tag>` and `</tag>`, the first time it appears.
pub(crate) fn tag_content(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let from = text.find(&open)? + open.len();
    let len = text[from..].find(&format!("</{tag}>"))?;
    Some(text[from..from + len].trim().to_owned())
}

/// Removes every `<system-reminder>…</system-reminder>` span (injected context, spec §3.3). An
/// unclosed reminder runs to the end of the text.
pub(crate) fn strip_system_reminders(text: &str) -> Cow<'_, str> {
    if !text.contains(REMINDER_OPEN) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(REMINDER_OPEN) {
        out.push_str(&rest[..open]);
        let after = &rest[open + REMINDER_OPEN.len()..];
        match after.find(REMINDER_CLOSE) {
            Some(close) => rest = &after[close + REMINDER_CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// `"key":"value"` found as text near the end of `line`, then near its start. In a Claude Code
/// line the top-level `uuid` and `timestamp` follow `message`, so the end is tried first. A
/// value with an escape or over 128 bytes is not taken.
fn scan_string_field(line: &[u8], key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let needle = needle.as_bytes();
    let tail = &line[line.len().saturating_sub(STUB_SCAN_BYTES)..];
    let head = &line[..line.len().min(STUB_SCAN_BYTES)];
    let found = rfind(tail, needle)
        .map(|at| &tail[at + needle.len()..])
        .or_else(|| find(head, needle).map(|at| &head[at + needle.len()..]))?;
    let end = found.iter().position(|&b| b == b'"')?;
    let value = &found[..end];
    if value.is_empty() || value.len() > 128 || value.contains(&b'\\') {
        return None;
    }
    std::str::from_utf8(value).ok().map(str::to_owned)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// A record and the byte offset of its line in the pass input.
#[derive(Debug, Clone)]
pub(crate) struct Located {
    pub start: usize,
    pub record: Record,
}

/// A complete turn, ready to render.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawTurn {
    pub key: String,
    /// The byte offset of the turn's first line in the pass input.
    pub start: usize,
    pub records: Vec<Record>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GroupOptions {
    /// The input starts at byte 0 of the file.
    pub at_file_start: bool,
    /// The last turn is complete (a settled `Stop`-class event, or an idle sweep).
    pub close_trailing: bool,
    /// The input is a whole pass (`MAX_PASS_BYTES`), so more of the file follows.
    pub full_pass: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Grouped {
    /// Complete turns, in file order. A head turn (records before the first turn at the file
    /// start) comes first, keyed by its first record.
    pub turns: Vec<RawTurn>,
    /// Records before the first turn at a cursor > 0: the late tail of the source's previous
    /// turn. Empty unless complete.
    pub tail: Vec<Record>,
    /// Where the next pass starts (an offset into the input), when something is pending: the
    /// start of the trailing turn, or 0 for a head or tail that waits. `None` means everything
    /// up to the last complete line is done.
    pub resume: Option<usize>,
}

/// Applies the turn rule, completeness and the late-record rule of spec §3.3.
///
/// - A turn starts at a record for which [`Record::starts_turn`] holds and runs until the next
///   one. Records that carry no blocks are dropped.
/// - Every turn but the last is complete. The last is complete when `close_trailing`.
/// - Records before the first turn form the head (at the file start: a turn of its own) or the
///   tail (at a cursor). They are complete when any turn follows them, or when `close_trailing`.
/// - At the file start nothing is consumed until something is complete, so a later pass still
///   reads the head from byte 0.
/// - A full pass that would otherwise make no progress is closed, so a turn longer than one pass
///   cannot pin the cursor; the rest of that turn arrives as the next pass's tail.
pub(crate) fn group(records: Vec<Located>, opts: GroupOptions) -> Grouped {
    let mut head: Vec<Record> = Vec::new();
    let mut turns: Vec<RawTurn> = Vec::new();
    for Located { start, record } in records {
        if record.starts_turn() {
            turns.push(RawTurn {
                key: record.turn_key(),
                start,
                records: vec![record],
            });
        } else if record.blocks.is_empty() {
            continue;
        } else if let Some(turn) = turns.last_mut() {
            turn.records.push(record);
        } else {
            head.push(record);
        }
    }

    let progress = turns.iter().any(|t| t.start > 0);
    let close = opts.close_trailing || (opts.full_pass && !progress);
    let head_complete = close || !turns.is_empty();
    let pending = if close { None } else { turns.pop() };

    let mut out = Grouped::default();
    if opts.at_file_start {
        if !head_complete {
            out.resume = Some(0);
        } else if head.is_empty() && out.turns.is_empty() && turns.is_empty() && !opts.full_pass {
            // Nothing to emit: keep the cursor at 0, so content that arrives later, before any
            // prompt, still becomes a keyed head turn instead of a keyless tail.
            out.resume = Some(0);
        } else if let Some(first) = head.first() {
            out.turns.push(RawTurn {
                key: first.turn_key(),
                start: 0,
                records: head,
            });
        }
    } else if !head.is_empty() {
        if head_complete {
            out.tail = head;
        } else {
            out.resume = Some(0);
        }
    }
    out.turns.extend(turns);
    if let Some(turn) = pending {
        out.resume.get_or_insert(turn.start);
    }

    for turn in &mut out.turns {
        drop_streaming_duplicates(&mut turn.records);
    }
    drop_streaming_duplicates(&mut out.tail);
    out
}

/// Spec §3.3: within a turn, a block identical to one already seen under the same `message.id`
/// is skipped. Records left with nothing are removed.
fn drop_streaming_duplicates(records: &mut Vec<Record>) {
    // The blocks kept so far under each message id.
    let mut kept_by_id: HashMap<String, Vec<Block>> = HashMap::new();
    for record in records.iter_mut() {
        let Some(id) = record.message_id.clone() else {
            continue;
        };
        let kept = kept_by_id.entry(id).or_default();
        record.blocks.retain(|block| {
            let seen = kept.contains(block);
            if !seen {
                kept.push(block.clone());
            }
            !seen
        });
    }
    records.retain(|r| !r.blocks.is_empty() || r.starts_turn());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: RecordKind, uuid: &str, blocks: Vec<Block>) -> Record {
        Record {
            uuid: Some(uuid.to_owned()),
            message_id: None,
            timestamp: None,
            kind,
            blocks,
        }
    }

    fn text(s: &str) -> Block {
        Block::Text(s.to_owned())
    }

    #[test]
    fn wrappers_reminders_and_images_decide_a_prompt() {
        let prompt = |b: Vec<Block>| rec(RecordKind::Prompt, "u", b).starts_turn();
        assert!(prompt(vec![text("Fix the bug.")]));
        assert!(prompt(vec![Block::Image]), "a pasted image is input");
        assert!(!prompt(vec![text("<command-name>/clear</command-name>")]));
        assert!(!prompt(vec![text(
            "  <local-command-stdout>ok</local-command-stdout>"
        )]));
        assert!(!prompt(vec![text(
            "[Request interrupted by user for tool use]"
        )]));
        assert!(!prompt(vec![text(
            "<system-reminder>ctx</system-reminder>\n"
        )]));
        assert!(prompt(vec![text(
            "<system-reminder>ctx</system-reminder>Real text."
        )]));
        assert!(!prompt(vec![text("")]));
        assert!(rec(RecordKind::Summary, "s", vec![]).starts_turn());
        assert!(!rec(RecordKind::ToolResults, "t", vec![text("Fix it.")]).starts_turn());
    }

    #[test]
    fn reminder_spans_are_removed() {
        assert_eq!(
            strip_system_reminders("a<system-reminder>x</system-reminder>b"),
            "ab"
        );
        assert_eq!(strip_system_reminders("a<system-reminder>x"), "a");
        assert_eq!(
            strip_system_reminders(
                "<system-reminder>1</system-reminder>a<system-reminder>2</system-reminder>"
            ),
            "a"
        );
        let r = rec(
            RecordKind::Prompt,
            "u",
            vec![text("<system-reminder>ctx</system-reminder>  Do it. Now.")],
        );
        assert_eq!(r.real_prompt().as_deref(), Some("Do it. Now."));
    }

    #[test]
    fn an_oversized_line_keeps_its_id_and_time() {
        let mut line =
            br#"{"parentUuid":"p-1","message":{"content":[{"type":"text","text":""#.to_vec();
        line.extend(std::iter::repeat_n(b'x', 200_000));
        line.extend(br#""}]},"uuid":"u-big","timestamp":"2026-10-14T09:00:00.000Z"}"#);
        let stub = Record::too_large(&line);
        assert_eq!(stub.uuid.as_deref(), Some("u-big"));
        assert_eq!(stub.timestamp.as_deref(), Some("2026-10-14T09:00:00.000Z"));
        assert_eq!(stub.blocks, [Block::TooLarge]);
        assert!(!stub.starts_turn());

        let bare = Record::too_large(&vec![b'x'; 1000]);
        assert_eq!((bare.uuid, bare.timestamp), (None, None));
    }

    #[test]
    fn duplicates_are_per_message_id() {
        let mut a = rec(RecordKind::Assistant, "a", vec![text("same"), text("same")]);
        a.message_id = Some("m1".into());
        let mut b = rec(RecordKind::Assistant, "b", vec![text("same"), text("new")]);
        b.message_id = Some("m1".into());
        let mut c = rec(RecordKind::Assistant, "c", vec![text("same")]);
        c.message_id = Some("m1".into());
        let mut records = vec![a, b, c];
        drop_streaming_duplicates(&mut records);
        assert_eq!(records.len(), 2, "the record left empty is removed");
        assert_eq!(records[0].blocks, [text("same")]);
        assert_eq!(records[1].blocks, [text("new")]);
    }

    #[test]
    fn records_without_blocks_never_reach_a_turn() {
        let located = |start, record| Located { start, record };
        let records = vec![
            located(0, rec(RecordKind::Skip, "bookkeeping", vec![])),
            located(10, rec(RecordKind::Prompt, "p", vec![text("Go.")])),
            located(20, rec(RecordKind::Skip, "meta", vec![])),
        ];
        let grouped = group(
            records,
            GroupOptions {
                at_file_start: true,
                close_trailing: true,
                full_pass: false,
            },
        );
        assert_eq!(grouped.turns.len(), 1);
        assert_eq!(grouped.turns[0].key, "p");
        assert_eq!(grouped.turns[0].records.len(), 1);
        assert_eq!(grouped.resume, None);
    }
}
