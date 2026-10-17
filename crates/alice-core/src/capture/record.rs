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
    /// The line's uuid. A line too large to parse has the one found near its ends, else a
    /// stand-in made from its bytes (see [`Record::too_large`]).
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
    /// the stub can still key a head turn and date its chunk. When no uuid is found there
    /// (Claude Code writes it mid-line, between two copies of a large tool result), the stub
    /// takes a stand-in ([`stand_in_uuid`]): made from the `sourceToolAssistantUUID` at the
    /// line's end when there is one, which a resumed or forked copy of the line keeps, else from
    /// the line's bytes. Each such line keys its own turn or tail, and the same line read again
    /// keys the same. A shared fallback key would make every later stub's tail look stored
    /// already.
    pub fn too_large(line: &[u8]) -> Record {
        Record {
            uuid: scan_string_field(line, "uuid")
                .filter(|v| super::valid_id(v))
                .or_else(|| Some(stand_in_uuid(line))),
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
    string_value(found)
}

/// The string value at the start of `found` (the bytes after `"key":"`), up to its closing
/// quote. A value with an escape or over 128 bytes is not taken.
fn string_value(found: &[u8]) -> Option<String> {
    let end = found.iter().position(|&b| b == b'"')?;
    let value = &found[..end];
    if value.is_empty() || value.len() > 128 || value.contains(&b'\\') {
        return None;
    }
    std::str::from_utf8(value).ok().map(str::to_owned)
}

/// The field of a Claude Code tool-result line that names the assistant record whose tool call
/// it answers. Claude Code writes it at the line's end, and a resumed or forked session's copy
/// of the line keeps it (its `sessionId`, also near the end, changes).
const SOURCE_TOOL_ASSISTANT: &str = "sourceToolAssistantUUID";

/// The stand-in uuid of a line too large to parse that shows none: `too-large-` and 32 hex
/// digits of a SHA-256. A valid id, so it is the stub's turn key.
///
/// - When the line's last `STUB_SCAN_BYTES` name a valid `sourceToolAssistantUUID` (the last
///   one there, which is the line's own field rather than text inside its content), the hash
///   is of that name: every copy of the line keys alike, so a resumed or forked session stores
///   the line's tail once (Task 18b).
/// - Otherwise it is of the line's length and its first and last `STUB_SCAN_BYTES` (the bytes
///   the scan looked at).
fn stand_in_uuid(line: &[u8]) -> String {
    let head = &line[..line.len().min(STUB_SCAN_BYTES)];
    let tail = &line[line.len().saturating_sub(STUB_SCAN_BYTES)..];
    let bytes = match scan_tail_field(tail, SOURCE_TOOL_ASSISTANT).filter(|v| super::valid_id(v)) {
        Some(source) => format!("{SOURCE_TOOL_ASSISTANT}\x1f{source}").into_bytes(),
        None => {
            let mut bytes = Vec::with_capacity(8 + head.len() + tail.len());
            bytes.extend((line.len() as u64).to_le_bytes());
            bytes.extend(head);
            bytes.extend(tail);
            bytes
        }
    };
    let mut hash = crate::normalize::sha256_hex(&bytes);
    hash.truncate(32);
    format!("too-large-{hash}")
}

/// `"key":"value"`, the last one in `tail`, as [`scan_string_field`] reads it, but never from
/// the line's head.
fn scan_tail_field(tail: &[u8], key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let needle = needle.as_bytes();
    string_value(&tail[rfind(tail, needle)? + needle.len()..])
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
    /// Close the trailing turn even with an unanswered `tool_use`.
    pub force_close: bool,
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
/// - Every turn but the last is complete. The last is complete when `close_trailing` and its
///   last `tool_use` has a result, or when `force_close`.
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
    // A trailing turn whose last `tool_use` has no result yet is still running: its result would
    // otherwise arrive later as a tail, apart from its call.
    let awaiting_result = opts.close_trailing
        && !opts.force_close
        && last_tool_use_unanswered(turns.last().map_or(&head[..], |t| &t.records[..]));
    let close = opts.force_close
        || (opts.close_trailing && !awaiting_result)
        || (opts.full_pass && !progress);
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

/// Whether the last `tool_use` of `records` has no `tool_result` after it. A call with no id
/// cannot be matched and does not count.
fn last_tool_use_unanswered(records: &[Record]) -> bool {
    let mut last: Option<&str> = None;
    for block in records.iter().flat_map(|r| &r.blocks) {
        match block {
            Block::ToolUse { id, .. } if !id.is_empty() => last = Some(id),
            Block::ToolResult { tool_use_id, .. } if last == Some(tool_use_id.as_str()) => {
                last = None;
            }
            _ => {}
        }
    }
    last.is_some()
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

        // Neither is found: no time, and a stand-in uuid made from the line's bytes.
        let bare = Record::too_large(&vec![b'x'; 1000]);
        assert_eq!(bare.timestamp, None);
        assert!(
            bare.uuid
                .as_deref()
                .is_some_and(|u| u.starts_with("too-large-") && u.len() == 42),
            "{:?}",
            bare.uuid
        );
    }

    /// Review fix F4: a line too large to parse whose uuid is out of reach of the scan (Claude
    /// Code writes it mid-line, between two copies of a large tool result) still keys its own
    /// turn or tail: different lines get different keys, the same line read again the same.
    #[test]
    fn an_oversized_line_without_a_uuid_keys_by_its_bytes() {
        let line = |session: &str, middle: usize| {
            let mut l = br#"{"type":"user","message":{"content":""#.to_vec();
            l.extend(std::iter::repeat_n(b'x', 200_000));
            l.extend(br#""},"uuid":"u-mid","timestamp":"2026-10-14T09:00:00.000Z","echo":""#);
            l.extend(std::iter::repeat_n(b'y', middle));
            l.extend(format!(r#"","sessionId":"{session}"}}"#).into_bytes());
            l
        };
        let key = |l: &[u8]| Record::too_large(l).turn_key();
        let (a, b) = (line("s-1", 100_000), line("s-2", 100_000));
        assert_eq!(a.len(), b.len());
        let stub = Record::too_large(&a);
        assert_eq!(stub.timestamp, None, "the time is out of reach too");
        assert_eq!(stub.blocks, [Block::TooLarge]);
        assert_ne!(key(&a), key(&b), "two lines: two keys");
        assert_eq!(
            key(&a),
            key(&a.clone()),
            "the same line read again: the same key"
        );
        // Only the length differs (the windows at both ends are the same): another key.
        assert_ne!(key(&a), key(&line("s-1", 100_001)));
        // Never the constant key of a keyless record with no text.
        for l in [&a, &b] {
            assert_ne!(key(l), turn_key(None, None, ""));
            assert!(super::super::valid_id(&key(l)), "{}", key(l));
        }
        let bare = |n| key(&vec![b'x'; n]);
        assert_ne!(bare(1000), bare(1001));
        assert_eq!(bare(1000), bare(1000));
    }

    /// Task 18b (6b review): a resumed or forked session copies an oversized tool-result line
    /// under its own `sessionId`, which sits in the window the stand-in hashes. When the tail
    /// window names the assistant record whose tool call this answers
    /// (`sourceToolAssistantUUID`, the same in every copy), the stand-in is made from that, so
    /// both copies key alike and the tail is stored once. A line without it keeps the byte hash.
    #[test]
    fn copies_of_an_oversized_tool_result_under_other_sessions_key_alike() {
        let line = |session: &str, source: Option<&str>| {
            let mut l =
                format!(r#"{{"type":"user","sessionId":"{session}","message":{{"content":""#)
                    .into_bytes();
            l.extend(std::iter::repeat_n(b'x', 200_000));
            l.extend(
                br#""},"uuid":"u-mid","timestamp":"2026-10-14T09:00:00.000Z","toolUseResult":""#,
            );
            l.extend(std::iter::repeat_n(b'y', 100_000));
            l.extend(format!(r#"","sessionId":"{session}""#).into_bytes());
            if let Some(s) = source {
                l.extend(format!(r#","sourceToolAssistantUUID":"{s}""#).into_bytes());
            }
            l.push(b'}');
            l
        };
        let key = |l: &[u8]| Record::too_large(l).turn_key();
        let assistant = "8c1e5a90-3f2b-4d7e-9a61-0b2c4d6e8f10";
        let (a, b) = (line("s-1", Some(assistant)), line("s-2", Some(assistant)));
        assert_ne!(a, b, "the copies differ in their session ids");
        assert_eq!(key(&a), key(&b), "two copies of one line: one key");
        assert!(key(&a).starts_with("too-large-") && key(&a).len() == 42);
        assert!(super::super::valid_id(&key(&a)));
        // Another tool call's result: another key.
        let other = line("s-1", Some("0f9e8d7c-6b5a-4938-8271-605f4e3d2c1b"));
        assert_ne!(key(&a), key(&other));
        // Without it, the stand-in is the byte hash, as before: the copies key apart.
        let (c, d) = (line("s-1", None), line("s-2", None));
        assert_ne!(key(&c), key(&d));
        assert_ne!(key(&a), key(&c));
        // Only the tail window counts: a name far from the end (inside the content) is not used.
        let mut early = line("s-1", None);
        early.splice(
            40..40,
            format!(r#""sourceToolAssistantUUID":"{assistant}","#).into_bytes(),
        );
        let mut early2 = line("s-2", None);
        early2.splice(
            40..40,
            format!(r#""sourceToolAssistantUUID":"{assistant}","#).into_bytes(),
        );
        assert_ne!(key(&early), key(&early2), "a head-window name is not taken");
        // A value that is no valid id is not taken either.
        let (e, f) = (
            line("s-1", Some("not an id!")),
            line("s-2", Some("not an id!")),
        );
        assert_ne!(key(&e), key(&f));
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
                force_close: false,
                full_pass: false,
            },
        );
        assert_eq!(grouped.turns.len(), 1);
        assert_eq!(grouped.turns[0].key, "p");
        assert_eq!(grouped.turns[0].records.len(), 1);
        assert_eq!(grouped.resume, None);
    }
}
