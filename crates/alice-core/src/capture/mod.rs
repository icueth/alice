//! Capture: agent session transcripts read into turns (SP2, spec §3).
//!
//! [`read`] takes the bytes of one pass over a transcript, from the source's cursor, and returns
//! the complete turns it holds, the late tail of the previous turn, and how far the cursor may
//! move. It is pure: no I/O, no clock.
//!
//! Lines become records (`claude.rs`), records become turns (`record.rs`), and turns become text.
//! The rendering here is a placeholder (each turn's blocks joined by `\n`, one chunk per turn);
//! Task 3 replaces it with the rendering contract of spec §3.3.

mod claude;
mod record;

use sha2::{Digest, Sha256};

use record::{Block, GroupOptions, Located, Record, RecordKind};

/// A line longer than this is not parsed; it leaves `[record skipped: too large]` in its turn.
pub const MAX_LINE_BYTES: usize = 4 << 20;
/// One pass reads at most this much of a transcript.
pub const MAX_PASS_BYTES: usize = 8 << 20;

/// The most characters of a turn's prompt kept in [`CapturedTurn::prompt`], and of the first
/// text hashed by [`turn_key`].
const PROMPT_CHARS: usize = 200;

/// The agent that wrote a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude-code" => Some(AgentKind::ClaudeCode),
            "codex" => Some(AgentKind::Codex),
            _ => None,
        }
    }

    /// The raw-tier source uri of a session (spec §3.1). It never holds a path. Callers check
    /// both ids with [`valid_id`] first.
    pub fn source_uri(self, session: &str, subagent: Option<&str>) -> String {
        match subagent {
            Some(agent) => format!("{}:session:{session}:agent:{agent}", self.as_str()),
            None => format!("{}:session:{session}", self.as_str()),
        }
    }
}

/// A session or agent id is `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$` (spec §3.1).
pub fn valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=128).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// How to read one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOptions {
    /// `capture.tool_output_chars`: a tool result is trimmed to this many characters.
    pub tool_output_chars: usize,
    /// `capture.chunk_chars`: a rendered turn is cut into chunks of at most this many characters.
    pub chunk_chars: usize,
    /// `capture.include_thinking`: render `thinking` blocks.
    pub include_thinking: bool,
    /// The input starts at byte 0 of the file (records before the first turn are a head turn,
    /// not the late tail of an earlier one).
    pub at_file_start: bool,
    /// The trailing turn is complete: a settled `Stop`, `SubagentStop`, `PreCompact` or
    /// `SessionEnd`, or a file idle for the sweep (spec §3.3).
    pub close_trailing: bool,
}

impl Default for ReadOptions {
    /// The `capture.*` defaults (800, 2,000, no thinking), reading a file from its start and
    /// leaving the trailing turn open.
    fn default() -> Self {
        ReadOptions {
            tool_output_chars: 800,
            chunk_chars: 2000,
            include_thinking: false,
            at_file_start: true,
            close_trailing: false,
        }
    }
}

/// What one pass produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadOutcome {
    /// Complete turns, in file order.
    pub turns: Vec<CapturedTurn>,
    /// Continuation chunks of the source's previous turn: records found at a cursor > 0 before
    /// any turn starts (spec §3.3, late records). Empty at the file start.
    pub tail: Vec<RenderedChunk>,
    /// Bytes of the input that are done: up to the start of the first incomplete turn, or to the
    /// end of the last complete line. Always 0 or just after a `\n`.
    pub consumed: u64,
    /// The input is a whole pass (`MAX_PASS_BYTES`) without a single `\n`: one line is longer
    /// than a pass, and the caller must skip it.
    pub oversized_line: bool,
    pub stats: ReadStats,
}

/// One complete turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTurn {
    /// The turn key (spec §3.5); see [`turn_key`].
    pub key: String,
    /// The first timestamped record's time.
    pub started_at: Option<String>,
    /// The first sentence of the turn's prompt, at most 200 characters. `None` for a compaction
    /// summary or a head turn.
    pub prompt: Option<String>,
    pub chunks: Vec<RenderedChunk>,
}

/// One chunk of rendered text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedChunk {
    pub text: String,
    /// The time of the chunk's first timestamped record.
    pub authored_at: Option<String>,
}

/// Line counts of one pass. `lines` counts every complete line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    pub lines: u64,
    /// JSON objects of a shape the reader does not recognise (an unknown `type`, or a
    /// conversation line with no usable content). Known line types that carry no conversation
    /// are not counted.
    pub unknown: u64,
    /// Lines that are not UTF-8, not JSON, or not a JSON object.
    pub invalid: u64,
    /// Lines over [`MAX_LINE_BYTES`].
    pub too_large: u64,
}

/// The turn key: the uuid when present, else the first 32 hex digits of
/// `sha256(timestamp ‖ "\x1f" ‖ the first 200 characters of first_text)` (spec §3.5).
pub fn turn_key(first_uuid: Option<&str>, timestamp: Option<&str>, first_text: &str) -> String {
    if let Some(uuid) = first_uuid.filter(|u| !u.is_empty()) {
        return uuid.to_owned();
    }
    let end = first_text
        .char_indices()
        .nth(PROMPT_CHARS)
        .map_or(first_text.len(), |(i, _)| i);
    let mut hasher = Sha256::new();
    hasher.update(timestamp.unwrap_or("").as_bytes());
    hasher.update([0x1f]);
    hasher.update(&first_text.as_bytes()[..end]);
    let mut key = hex::encode(hasher.finalize());
    key.truncate(32);
    key
}

/// Reads one pass of a transcript. `input` holds the file's bytes from the cursor; only its first
/// [`MAX_PASS_BYTES`] are read, and only complete lines (ending in `\n`) are used.
pub fn read(agent: AgentKind, input: &[u8], opts: &ReadOptions) -> ReadOutcome {
    let mut out = ReadOutcome::default();
    let parse: fn(&[u8]) -> claude::LineResult = match agent {
        AgentKind::ClaudeCode => claude::parse_line,
        // The Codex rollout reader is Task 17. Until then nothing is read or consumed.
        AgentKind::Codex => return out,
    };
    let full_pass = input.len() >= MAX_PASS_BYTES;
    let input = &input[..input.len().min(MAX_PASS_BYTES)];

    let mut records = Vec::new();
    let mut pos = 0;
    while let Some(newline) = input[pos..].iter().position(|&b| b == b'\n') {
        let start = pos;
        let line = &input[start..start + newline];
        pos = start + newline + 1;
        out.stats.lines += 1;
        if line.len() > MAX_LINE_BYTES {
            out.stats.too_large += 1;
            records.push(Located {
                start,
                record: Record::too_large(line),
            });
            continue;
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        match parse(line) {
            claude::LineResult::Record(record) => records.push(Located { start, record }),
            claude::LineResult::Unknown => out.stats.unknown += 1,
            claude::LineResult::Invalid => out.stats.invalid += 1,
        }
    }
    if pos == 0 {
        // Not one complete line. In a whole pass that is a line longer than the pass.
        out.oversized_line = full_pass;
        return out;
    }

    let grouped = record::group(
        records,
        GroupOptions {
            at_file_start: opts.at_file_start,
            close_trailing: opts.close_trailing,
            full_pass,
        },
    );
    out.consumed = grouped.resume.unwrap_or(pos) as u64;
    out.turns = grouped
        .turns
        .into_iter()
        .map(|turn| CapturedTurn {
            started_at: turn.records.iter().find_map(|r| r.timestamp.clone()),
            prompt: turn
                .records
                .first()
                .filter(|r| r.kind == RecordKind::Prompt)
                .and_then(|r| first_sentence(&r.prompt_text(), PROMPT_CHARS)),
            chunks: render_placeholder(&turn.records, opts),
            key: turn.key,
        })
        .collect();
    out.tail = render_placeholder(&grouped.tail, opts);
    out
}

/// The text up to the first sentence end (`.`, `!` or `?` followed by a space or the end), the
/// first line break, or `max_chars` characters, whichever comes first.
fn first_sentence(text: &str, max_chars: usize) -> Option<String> {
    let text = text.trim();
    let mut end = text.len();
    let mut chars = text.char_indices().peekable();
    let mut count = 0;
    while let Some((i, c)) = chars.next() {
        if count == max_chars || c == '\n' {
            end = i;
            break;
        }
        count += 1;
        if matches!(c, '.' | '!' | '?' | '。' | '！' | '？')
            && chars.peek().is_none_or(|&(_, next)| next.is_whitespace())
        {
            end = i + c.len_utf8();
            break;
        }
    }
    let sentence = text[..end].trim();
    (!sentence.is_empty()).then(|| sentence.to_owned())
}

/// Task 2's stand-in renderer: one chunk holding every block, joined by `\n`. Task 3 replaces it.
fn render_placeholder(records: &[Record], opts: &ReadOptions) -> Vec<RenderedChunk> {
    let lines: Vec<String> = records
        .iter()
        .flat_map(|r| &r.blocks)
        .filter_map(|block| match block {
            Block::Text(text) => Some(text.clone()),
            Block::Thinking(text) => opts.include_thinking.then(|| text.clone()),
            Block::ToolUse { id, name, input } => Some(format!("{name} {id} {input}")),
            Block::ToolResult {
                tool_use_id,
                text,
                is_error,
            } => Some(format!(
                "{tool_use_id}{}: {text}",
                if *is_error { " error" } else { "" }
            )),
            Block::Image => Some("[image]".to_owned()),
            Block::Document => Some("[document]".to_owned()),
            Block::Audio => Some("[audio]".to_owned()),
            Block::TooLarge => Some("[record skipped: too large]".to_owned()),
        })
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    vec![RenderedChunk {
        text: lines.join("\n"),
        authored_at: records.iter().find_map(|r| r.timestamp.clone()),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    const TURNS: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/turns.jsonl");
    const COMPACTION: &[u8] =
        include_bytes!("../../tests/fixtures/capture/claude/compaction.jsonl");
    const CONTINUATION: &[u8] =
        include_bytes!("../../tests/fixtures/capture/claude/continuation.jsonl");
    const LATE_TAIL: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/late_tail.jsonl");
    const ODD_LINES: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/odd_lines.jsonl");
    const SAMPLE_1: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/sample-1.jsonl");
    const SAMPLE_2: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/sample-2.jsonl");
    const SAMPLE_AGENT: &[u8] =
        include_bytes!("../../tests/fixtures/capture/claude/sample-1-agent-1.jsonl");

    const CLAUDE: AgentKind = AgentKind::ClaudeCode;

    fn opts(at_file_start: bool, close_trailing: bool) -> ReadOptions {
        ReadOptions {
            at_file_start,
            close_trailing,
            ..ReadOptions::default()
        }
    }

    /// The uuid-shaped ids the hand-written fixtures use.
    fn id(prefix: &str, n: u64) -> String {
        format!("{prefix}000000-0000-4000-8000-{n:012}")
    }

    fn keys(out: &ReadOutcome) -> Vec<String> {
        out.turns.iter().map(|t| t.key.clone()).collect()
    }

    /// Every rendered character of a read, turns and tail.
    fn all_text(out: &ReadOutcome) -> String {
        let mut s = String::new();
        for chunk in out.turns.iter().flat_map(|t| &t.chunks).chain(&out.tail) {
            s.push_str(&chunk.text);
            s.push('\n');
        }
        s
    }

    fn turn_text(turn: &CapturedTurn) -> String {
        turn.chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The byte offset of the start of the first line containing `needle`.
    fn line_start(input: &[u8], needle: &str) -> u64 {
        let pos = input
            .windows(needle.len())
            .position(|w| w == needle.as_bytes())
            .expect("needle in input");
        input[..pos]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1) as u64
    }

    fn line(v: serde_json::Value) -> Vec<u8> {
        let mut out = serde_json::to_vec(&v).unwrap();
        out.push(b'\n');
        out
    }

    fn prompt_line(uuid: &str, text: &str) -> Vec<u8> {
        line(json!({
            "type": "user", "uuid": uuid, "parentUuid": null, "isSidechain": false,
            "timestamp": "2026-10-14T09:00:00.000Z",
            "message": {"role": "user", "content": text},
        }))
    }

    fn assistant_line(uuid: &str, message_id: &str, text: &str) -> Vec<u8> {
        line(json!({
            "type": "assistant", "uuid": uuid, "parentUuid": null, "isSidechain": false,
            "timestamp": "2026-10-14T09:00:01.000Z",
            "message": {"id": message_id, "type": "message", "role": "assistant",
                        "content": [{"type": "text", "text": text}], "stop_reason": "end_turn"},
        }))
    }

    fn assert_on_line_start(input: &[u8], out: &ReadOutcome, what: &str) {
        let c = out.consumed as usize;
        assert!(c <= input.len(), "{what}: consumed {c} > {}", input.len());
        assert!(
            c == 0 || input[c - 1] == b'\n',
            "{what}: consumed {c} is not a line start"
        );
    }

    #[test]
    fn turns_start_at_real_prompts_only() {
        let out = read(CLAUDE, TURNS, &opts(true, true));
        assert_eq!(keys(&out), [id("a1", 1), id("a1", 7), id("a1", 14)]);
        assert_eq!(
            out.turns[0].prompt.as_deref(),
            Some("Please list the files in the project.")
        );
        assert_eq!(
            out.turns[0].started_at.as_deref(),
            Some("2026-10-14T09:00:01.000Z")
        );
        assert_eq!(out.turns[2].prompt.as_deref(), Some("Thanks!"));
        let text = all_text(&out);
        assert!(
            !text.contains("Skill body injected"),
            "isMeta text is not captured"
        );
        assert!(
            !text.contains("Sidechain prompt text"),
            "a main-file sidechain is not captured"
        );
        assert!(
            text.contains("hello world"),
            "tool results stay inside their turn"
        );
        assert!(turn_text(&out.turns[1]).contains("a.txt holds a greeting"));
        assert_eq!(out.stats.lines, 20);
        assert_eq!(
            (out.stats.unknown, out.stats.invalid, out.stats.too_large),
            (0, 0, 0)
        );
        assert!(out.tail.is_empty());
    }

    #[test]
    fn trailing_turn_waits_unless_closed() {
        let open = read(CLAUDE, TURNS, &opts(true, false));
        assert_eq!(keys(&open), [id("a1", 1), id("a1", 7)]);
        assert_eq!(open.consumed, line_start(TURNS, "Thanks! That is all."));
        assert!(!open.oversized_line);

        let closed = read(CLAUDE, TURNS, &opts(true, true));
        assert_eq!(closed.turns.len(), 3);
        assert_eq!(closed.consumed, TURNS.len() as u64);
    }

    #[test]
    fn compaction_summary_is_its_own_turn() {
        let out = read(CLAUDE, COMPACTION, &opts(true, true));
        assert_eq!(keys(&out), [id("c1", 1), id("c1", 5), id("c1", 8)]);
        let summary = &out.turns[1];
        assert_eq!(summary.prompt, None, "a summary is not a prompt");
        assert_eq!(
            summary.started_at.as_deref(),
            Some("2026-10-14T10:00:02.900Z")
        );
        let text = turn_text(summary);
        assert!(text.contains("Summary: the parser was split."));
        assert!(
            text.contains("Picking up after the compaction."),
            "the turn runs to the next prompt"
        );
        let all = all_text(&out);
        assert!(
            !all.contains("Conversation compacted"),
            "compact_boundary yields nothing"
        );
        assert!(!all.contains("Notice text."), "system lines are dropped");
        assert_eq!((out.stats.unknown, out.stats.invalid), (0, 0));
    }

    #[test]
    fn late_records_at_a_cursor_become_the_tail() {
        let out = read(CLAUDE, LATE_TAIL, &opts(false, true));
        assert_eq!(out.tail.len(), 1);
        assert!(out.tail[0].text.contains("test result: ok"));
        assert_eq!(
            out.tail[0].authored_at.as_deref(),
            Some("2026-10-14T12:00:01.000Z")
        );
        assert_eq!(keys(&out), [id("a7", 3)]);

        // A later turn completes the tail even while the last turn waits.
        let open = read(CLAUDE, LATE_TAIL, &opts(false, false));
        assert_eq!(open.tail.len(), 1);
        assert!(open.turns.is_empty());
        assert_eq!(open.consumed, line_start(LATE_TAIL, "Ship it."));

        // Without a later turn the tail waits too.
        let head_only = &LATE_TAIL[..line_start(LATE_TAIL, "Ship it.") as usize];
        let waiting = read(CLAUDE, head_only, &opts(false, false));
        assert!(waiting.tail.is_empty() && waiting.turns.is_empty());
        assert_eq!(waiting.consumed, 0);

        let head = read(CLAUDE, LATE_TAIL, &opts(true, true));
        assert!(head.tail.is_empty());
        assert_eq!(keys(&head), [id("a7", 1), id("a7", 3)]);
        assert_eq!(head.turns[0].prompt, None);
        assert!(turn_text(&head.turns[0]).contains("test result: ok"));
    }

    #[test]
    fn continuation_head_is_keyed_by_its_first_record() {
        let out = read(CLAUDE, CONTINUATION, &opts(true, true));
        assert_eq!(keys(&out), [id("e1", 1), id("e1", 3), id("e1", 5)]);
        assert_eq!(
            out.turns[0].started_at.as_deref(),
            Some("2026-10-14T11:00:01.000Z")
        );
        assert_eq!((out.stats.unknown, out.stats.invalid), (0, 0));

        // Bookkeeping alone at the file start keeps the cursor at 0, so the head rule still
        // applies on the next pass.
        let preamble = &CONTINUATION[..line_start(CONTINUATION, "msg_cont_a") as usize];
        let at_start = read(CLAUDE, preamble, &opts(true, false));
        assert!(at_start.turns.is_empty() && at_start.tail.is_empty());
        assert_eq!(at_start.consumed, 0);
        // At a cursor, lines that carry nothing are simply consumed.
        let at_cursor = read(CLAUDE, preamble, &opts(false, false));
        assert!(at_cursor.turns.is_empty() && at_cursor.tail.is_empty());
        assert_eq!(at_cursor.consumed, preamble.len() as u64);
    }

    #[test]
    fn streaming_duplicates_render_once() {
        let said = "The same block, written twice.";
        let mut input = prompt_line("p-1", "Say it once.");
        input.extend(assistant_line("a-1", "msg_dup", said));
        input.extend(assistant_line("a-2", "msg_dup", said));
        let out = read(CLAUDE, &input, &opts(true, true));
        assert_eq!(out.turns.len(), 1);
        assert_eq!(turn_text(&out.turns[0]).matches(said).count(), 1);

        // The same text under another message.id is not a duplicate.
        input.extend(assistant_line("a-3", "msg_other", said));
        let out = read(CLAUDE, &input, &opts(true, true));
        assert_eq!(turn_text(&out.turns[0]).matches(said).count(), 2);
    }

    #[test]
    fn reader_survives_arbitrary_json_lines() {
        let out = read(CLAUDE, ODD_LINES, &opts(true, true));
        assert_eq!(out.stats.lines, 12);
        // Lines 3, 4, 5, 7, 8, 9, 10 and 12 carry no text.
        assert_eq!(out.stats.unknown + out.stats.invalid, 8);
        assert_eq!((out.stats.unknown, out.stats.invalid), (5, 3));
        let fallback = turn_key(
            None,
            Some("2026-10-14T13:00:05.000Z"),
            "A prompt without a uuid.",
        );
        assert_eq!(keys(&out), [id("0d", 1), fallback]);
        let text = all_text(&out);
        assert!(text.contains("String content from the assistant."));
        assert!(text.contains("Readable after the noise."));
        assert_eq!(out.consumed, ODD_LINES.len() as u64);

        // Each odd line alone, and some worse ones, in every mode.
        let deep = format!("{}{}\n", "[".repeat(100_000), "]".repeat(100_000));
        let mut cases: Vec<Vec<u8>> = ODD_LINES
            .split_inclusive(|&b| b == b'\n')
            .map(<[u8]>::to_vec)
            .collect();
        cases.extend(
            [
                "{not json\n",
                "\n",
                "   \n",
                "{}\n",
                "{\"type\":\"user\"}\n",
                "{\"type\":\"user\",\"message\":{\"content\":{\"a\":1}}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\"}]}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":7}]}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\"}]}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":5}]}}\n",
                "{\"type\":\"system\"}\n",
                "{\"type\":\"user\",\"uuid\":5,\"timestamp\":[],\"isMeta\":\"yes\",\"message\":{\"content\":\"x\"}}\n",
                deep.as_str(),
            ]
            .map(|s| s.as_bytes().to_vec()),
        );
        for case in &cases {
            for (start, close) in [(true, true), (true, false), (false, true), (false, false)] {
                let out = read(CLAUDE, case, &opts(start, close));
                assert_on_line_start(case, &out, &String::from_utf8_lossy(case));
            }
        }
    }

    #[test]
    fn cursor_only_ever_lands_on_a_line_start() {
        for cut in 0..=TURNS.len() {
            let input = &TURNS[..cut];
            for close in [false, true] {
                let out = read(CLAUDE, input, &opts(true, close));
                assert_on_line_start(input, &out, &format!("cut {cut} close {close}"));
            }
        }

        // A complete line with invalid UTF-8 is counted and skipped.
        let third = line_start(TURNS, "Thanks! That is all.") as usize;
        let mut input = TURNS[..third].to_vec();
        input
            .extend_from_slice(b"{\"type\":\"user\",\"message\":{\"content\":\"bad \xff\xfe\"}}\n");
        input.extend_from_slice(&TURNS[third..]);
        let out = read(CLAUDE, &input, &opts(true, true));
        assert_eq!(out.stats.invalid, 1);
        assert_eq!(out.turns.len(), 3);
        assert_eq!(out.consumed, input.len() as u64);
    }

    #[test]
    fn a_line_over_four_mebibytes_leaves_a_stub() {
        let big = "x".repeat(5 << 20);
        let mut input = prompt_line("p-big", "Take a screenshot.");
        input.extend(assistant_line("a-big", "msg_big", &big));
        input.extend(assistant_line(
            "a-after",
            "msg_after",
            "After the big line.",
        ));
        assert!(input.len() > MAX_LINE_BYTES);
        let out = read(CLAUDE, &input, &opts(true, true));
        assert_eq!(keys(&out), ["p-big"]);
        let text = turn_text(&out.turns[0]);
        assert!(text.contains("[record skipped: too large]"));
        assert!(text.contains("After the big line."));
        assert!(!text.contains("xxxxxxxx"));
        assert_eq!(out.stats.too_large, 1);
        assert_eq!(out.consumed, input.len() as u64);
    }

    #[test]
    fn no_newline_in_a_full_pass_is_an_oversized_line() {
        let input = vec![b'{'; MAX_PASS_BYTES];
        for close in [false, true] {
            let out = read(CLAUDE, &input, &opts(false, close));
            assert!(out.oversized_line);
            assert_eq!(out.consumed, 0);
            assert!(out.turns.is_empty() && out.tail.is_empty());
        }
        // A short pass without a newline is only an unfinished line.
        let out = read(CLAUDE, &input[..1000], &opts(false, true));
        assert!(!out.oversized_line);
        assert_eq!(out.consumed, 0);
    }

    #[test]
    fn a_full_pass_without_progress_closes_its_turn() {
        // One prompt, then more than a pass of assistant lines: the pass would otherwise
        // re-read the same 8 MiB until the turn ends.
        let mut file = prompt_line("p-long", "Write a long answer.");
        let mut n = 0;
        while file.len() <= MAX_PASS_BYTES + (1 << 20) {
            n += 1;
            let text = format!("part {n} {}", "lorem ipsum ".repeat(80_000));
            file.extend(assistant_line(
                &format!("a-{n}"),
                &format!("msg_{n}"),
                &text,
            ));
        }
        let pass = &file[..MAX_PASS_BYTES];
        let out = read(CLAUDE, pass, &opts(true, false));
        assert_eq!(keys(&out), ["p-long"]);
        assert_on_line_start(pass, &out, "full pass");
        let consumed = out.consumed as usize;
        assert!(consumed > 0 && consumed < pass.len());
        assert_eq!(
            consumed,
            pass.iter().rposition(|&b| b == b'\n').unwrap() + 1
        );

        // The rest of the turn arrives as the late tail.
        let rest = read(CLAUDE, &file[consumed..], &opts(false, true));
        assert!(rest.turns.is_empty());
        assert_eq!(rest.tail.len(), 1);
        assert!(rest.tail[0].text.contains(&format!("part {n} ")));
    }

    #[test]
    fn the_samples_read_without_unknown_lines() {
        // The turns are the typed prompts, the task notifications and the compaction summary;
        // the `api-request-blob` copies of the same messages add none.
        let expected: [(&str, &[u8], &[&str]); 2] = [
            (
                "sample-1",
                SAMPLE_1,
                &[
                    "b1ce2ce0-0df0-50ab-4bc2-5775ffa309f7",
                    "3e1cd44a-0005-f3f7-48d6-d3cfca51d32a",
                    "68ca8071-075d-58bb-27ae-0123de95f0dc",
                    "830929b2-3b94-28ca-07a6-94845cad4f59",
                ],
            ),
            (
                "sample-2",
                SAMPLE_2,
                &[
                    "d48218ac-3702-2477-61c0-34128d33b075",
                    "a3548d02-5ee2-ecb8-0b68-0d4367c80410",
                    "5869264f-4cbe-5c5b-d9bb-849fc0e0a617",
                ],
            ),
        ];
        for (name, sample, turn_keys) in expected {
            let out = read(CLAUDE, sample, &opts(true, true));
            assert_eq!(out.stats.unknown, 0, "{name}");
            assert_eq!(out.stats.invalid, 0, "{name}");
            assert!(!out.turns.is_empty(), "{name}");
            assert_eq!(keys(&out), turn_keys, "{name}");
            assert_eq!(out.consumed, sample.len() as u64, "{name}");
            assert!(!all_text(&out).contains("\"type\":\"base64\""), "{name}");
        }
    }

    #[test]
    fn a_subagent_files_own_prompt_starts_a_turn() {
        // Every line of a subagent file is a sidechain carrying `agentId`.
        let out = read(CLAUDE, SAMPLE_AGENT, &opts(true, true));
        assert_eq!((out.stats.unknown, out.stats.invalid), (0, 0));
        assert_eq!(out.turns[0].key, "0b204206-70cf-d21b-30a5-9e1854da2b43");
        assert!(out.turns[0].prompt.is_some());
    }

    #[test]
    fn codex_reads_nothing_until_its_reader_exists() {
        let out = read(AgentKind::Codex, TURNS, &opts(true, true));
        assert!(out.turns.is_empty() && out.tail.is_empty());
        assert_eq!(out.consumed, 0);
    }

    #[test]
    fn agent_kinds_ids_and_uris() {
        for kind in [AgentKind::ClaudeCode, AgentKind::Codex] {
            assert_eq!(AgentKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(AgentKind::ClaudeCode.as_str(), "claude-code");
        assert_eq!(AgentKind::Codex.as_str(), "codex");
        assert_eq!(AgentKind::parse("claude"), None);
        assert_eq!(
            AgentKind::ClaudeCode.source_uri("s1", None),
            "claude-code:session:s1"
        );
        assert_eq!(
            AgentKind::ClaudeCode.source_uri("s1", Some("a1")),
            "claude-code:session:s1:agent:a1"
        );
        assert_eq!(AgentKind::Codex.source_uri("s1", None), "codex:session:s1");

        for ok in [
            "a",
            "Z9",
            "889f0759-f327-4038-722c-82218ecb7c01",
            "a.b_c-d",
            &"a".repeat(128),
        ] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            ".a",
            "_a",
            "a/b",
            "a b",
            "a:b",
            "é",
            "a\n",
            &"a".repeat(129),
        ] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn turn_key_prefers_the_uuid_then_hashes() {
        assert_eq!(turn_key(Some("u-1"), Some("t"), "x"), "u-1");
        let ts = "2026-10-14T09:00:00.000Z";
        let key = turn_key(None, Some(ts), "hello");
        let expected = hex::encode(Sha256::digest(format!("{ts}\x1fhello").as_bytes()));
        assert_eq!(key, expected[..32]);
        assert_eq!(
            turn_key(Some(""), Some(ts), "hello"),
            key,
            "an empty uuid is absent"
        );
        let no_ts = hex::encode(Sha256::digest("\x1fhello".as_bytes()));
        assert_eq!(turn_key(None, None, "hello"), no_ts[..32]);
        // Only the first 200 characters count.
        assert_eq!(
            turn_key(None, Some(ts), &"é".repeat(300)),
            turn_key(None, Some(ts), &"é".repeat(200))
        );
        assert_ne!(
            turn_key(None, Some(ts), &"é".repeat(199)),
            turn_key(None, Some(ts), &"é".repeat(200))
        );
    }
}
