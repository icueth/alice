//! Distillation (SP2, spec §6): a run of captured chunks becomes extractive candidates, one Jev
//! choice question per candidate, and, from Jev's answers, the memories to write.
//!
//! Jev only chooses (spec §6.1). A memory is a passage of the session as it was written, with at
//! most a context line added here; Jev decides whether the passage holds lasting knowledge, and
//! of which kind. The module is pure: no I/O and no clock. The same input gives the same state
//! and questions byte for byte, so Jev's cache, which keys on the request body, answers a
//! repeated run.
//!
//! Chunks are parsed back by the rendering contract of `capture` ([`TAGS`]): a line that starts
//! with a tag is a block head, `<tag> (cont.)` at the start of a chunk continues a block the
//! renderer cut, and every other line belongs to the current block. A turn starts at a chunk
//! whose first line is a `[user]` prompt or a `[summary]`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;

use sha2::{Digest, Sha256};

use crate::capture::TAGS;
use crate::judge::jev::Question;
use crate::judge::{Choice, Distribution, verbal};
use crate::model::Kind;
use crate::search::lexical::in_unspaced_script;
use crate::settings::DistillSettings;

/// The most candidates, and so questions, in one request (spec R16).
pub const MAX_CANDIDATES: usize = 40;
/// The most characters of the request's state.
pub const STATE_CHARS: usize = 12_000;
/// A passage cut from a conclusion, a summary or a subagent report has at least this many
/// characters...
pub const PASSAGE_MIN: usize = 80;
/// ...and at most this many.
pub const PASSAGE_MAX: usize = 1_200;
/// A user prompt is a candidate when it has at least this many characters...
pub const USER_MIN: usize = 20;
/// ...and at most this many.
pub const USER_MAX: usize = 800;
/// The most characters of a memory's title.
pub const TITLE_MAX: usize = 120;
/// The most characters of a memory's passage (its context line comes on top).
pub const BODY_MAX: usize = 1_200;
/// A run covers at most this many turns of the main session...
pub const RUN_MAX_TURNS: usize = 60;
/// ...and at most this many characters of its chunks. A longer backlog is several runs.
pub const RUN_MAX_CHARS: usize = 120_000;
/// The most already-saved titles listed in the state.
pub const ALREADY_SAVED_MAX: usize = 20;
/// Two texts whose lower-cased character trigrams have a Jaccard similarity at or above this
/// say the same thing.
pub const OVERLAP: f64 = 0.6;

/// The options of every question (spec §6.4), ids and the descriptions Jev reads. `handoff` is
/// offered only for a passage with [`Candidate::handoff_ok`].
pub const OPTIONS: &[(&str, &str)] = &[
    (
        "decision",
        "a choice that was made, with its reason, that should guide later work",
    ),
    (
        "bugfix",
        "a bug that was found: what went wrong, why, and how it was fixed",
    ),
    (
        "howto",
        "a procedure or command sequence that worked and can be reused",
    ),
    (
        "warning",
        "a pitfall, limit or constraint to avoid in future work",
    ),
    (
        "preference_user",
        "how the user wants things done in every project (style, language, tools, workflow)",
    ),
    ("preference_project", "a convention for this project only"),
    (
        "fact",
        "a lasting fact about the project, its systems or its environment",
    ),
    (
        "handoff",
        "where the work stands: what is done and what comes next",
    ),
    (
        "already_saved",
        "the same knowledge as an item listed under \"Already saved in this session\", even when worded differently or with more detail",
    ),
    (
        "ephemeral",
        "a question, a request or thanks to the assistant, or a pick among choices it offered, with no lasting rule or fact in it; progress narration; a plan for the next minutes; a greeting; or a result that matters only right now",
    ),
    (
        "unclear",
        "cannot be understood without the surrounding conversation",
    ),
];

/// The first sentence of a prompt is quoted with at most this many characters.
const PROMPT_CHARS: usize = 200;
/// A user prompt in the state's excerpt.
const STATE_PROMPT_CHARS: usize = 300;
/// A turn's final assistant text in the state's excerpt.
const STATE_REPLY_CHARS: usize = 600;
/// A fenced code block of more lines than this is replaced by [`CODE`].
const CODE_LINES_MAX: usize = 10;
const CODE: &str = "[code]";
/// A conclusion's or report's body gains `Context: <prompt>` when the prompt has this many words,
const CONTEXT_MIN_WORDS: usize = 4;
/// or this many characters of a script written without spaces (Thai, CJK, …).
const CONTEXT_MIN_UNSPACED: usize = 12;
/// A subagent call's description is one line of at most this many characters (Task 3).
const CALL_CHARS: usize = 300;
const WITHHELD: &str = "[withheld:";
/// The memory tools whose `[tool alice <tool>] <title>` lines are things already saved.
const SAVE_TOOLS: &[&str] = &[
    "remember",
    "update_memory",
    "mempalace_add_drawer",
    "mempalace_update_drawer",
    "mempalace_diary_write",
];
/// Text that stands for something left out.
const PLACEHOLDERS: &[&str] = &[
    CODE,
    "[image]",
    "[document]",
    "[audio]",
    "[binary removed]",
    "…",
];

/// One captured chunk of the run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunChunk {
    pub id: String,
    pub ord: i64,
    pub text: String,
    /// The chunk belongs to one of the session's subagent sources, not to the session itself.
    pub subagent: bool,
    /// Whether the chunk is the first of a turn: `Some(true)` for a turn's first chunk,
    /// `Some(false)` for any other, `None` to infer it from the text (a chunk whose first line
    /// is a `[user]` prompt, not a command line, or a `[summary]`). The text rule misreads a
    /// mid-turn `[user]` line that starts a chunk, a withheld continuation (it has no
    /// `(cont.)`), a typed prompt that looks like a command, a turn whose first record renders
    /// nothing, and the boundary between two subagent sources.
    ///
    /// Task 12 fills it from `capture_turns.first_ord` of the chunk's source (`Some(true)` when
    /// a turn's `first_ord` is this `ord`), leaving out `tail:%` keys because a stored late tail
    /// continues the previous turn; and it sets `Some(true)` on the first chunk of each subagent
    /// source in the run, so one source's turn never takes in the next source's text.
    pub turn_start: Option<bool>,
}

/// One run of a source (spec §6.2): its chunks from `distill_ord` on, plus new chunks of its
/// subagent sources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunInput {
    pub uri: String,
    /// The source's project slug (the state's `Project:` line).
    pub project: String,
    /// Main chunks in any order (they are taken by `ord`), subagent chunks in source order.
    pub chunks: Vec<RunChunk>,
    /// The run ends with the session or a compaction, so `handoff` may be offered.
    pub closes_session: bool,
}

/// Where a candidate comes from, in priority order (spec §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CandidateSource {
    /// A compaction summary, cut at its headings and bullet groups.
    Summary,
    /// A turn's last assistant block, cut into passages.
    Conclusion,
    /// A user prompt of 20 to 800 characters.
    UserStatement,
    /// A subagent's final report: the `[subagent <type>] <report>` line of a call, or the last
    /// assistant block of a subagent source's turn.
    SubagentReport,
}

/// One passage Jev is asked about.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The question id, `c<n>`, numbered from 1 in [`Prepared::candidates`] order.
    pub qid: String,
    pub source: CandidateSource,
    /// The passage, as written in the session.
    pub text: String,
    /// The first sentence of the turn's prompt (at most 200 characters). Summaries and user
    /// statements have none.
    pub prompt: Option<String>,
    /// The chunks the passage came from, plus the prompt's chunk when the body gets the context
    /// line, in session order.
    pub chunk_ids: Vec<String>,
    /// `handoff` is offered: the passage is from the last two turns of a run that closes the
    /// session.
    pub handoff_ok: bool,
    /// Session order (main chunks by `ord`, then subagent chunks): [`decide`]'s tie-break.
    pub pos: usize,
}

/// A run made ready for Jev.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// By priority, then session order; at most [`MAX_CANDIDATES`].
    pub candidates: Vec<Candidate>,
    /// Every distinct title the run's memory-tool lines saved, in session order.
    pub already_saved: Vec<String>,
    /// The request's state, at most [`STATE_CHARS`] characters.
    pub state: String,
    /// One per candidate, in the same order.
    pub questions: Vec<Question>,
    /// The last main `ord` this run covers (the caller sets `distill_ord` to this plus one), or
    /// -1 when it covers no main chunk.
    pub consumed_ord: i64,
}

/// What [`decide`] keeps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DistillConfig {
    /// `distill.min_confidence`.
    pub min_confidence: f64,
    /// `distill.max_per_run`.
    pub max_per_run: usize,
}

impl Default for DistillConfig {
    fn default() -> Self {
        Self::from(&DistillSettings::default())
    }
}

impl From<&DistillSettings> for DistillConfig {
    fn from(s: &DistillSettings) -> Self {
        Self {
            min_confidence: s.min_confidence,
            max_per_run: s.max_per_run,
        }
    }
}

/// A memory to write through the `remember` path (spec §6.5).
#[derive(Debug, Clone, PartialEq)]
pub struct DistilledMemory {
    pub kind: Kind,
    /// `preference_user`: the memory goes to the `global` project, not the source's.
    pub to_global: bool,
    pub title: String,
    pub body: String,
    /// Provenance: the candidate's chunks.
    pub chunk_ids: Vec<String>,
    pub idempotency_key: String,
    /// The probability Jev gave the chosen option.
    pub p: f64,
}

/// Parses a run into candidates, the state and one question per candidate.
pub fn prepare(input: &RunInput) -> Prepared {
    let mut main: Vec<&RunChunk> = input.chunks.iter().filter(|c| !c.subagent).collect();
    main.sort_by_key(|c| c.ord);
    let covered = covered(&main);
    let consumed_ord = main[..covered].last().map_or(-1, |c| c.ord);
    let order: Vec<&RunChunk> = main[..covered]
        .iter()
        .copied()
        .chain(input.chunks.iter().filter(|c| c.subagent))
        .collect();
    let indexed: Vec<(usize, &RunChunk)> = order.iter().copied().enumerate().collect();
    let main_turns = parse(&indexed[..covered]);
    let sub_turns = parse(&indexed[covered..]);

    let mut already_saved: Vec<String> = Vec::new();
    for block in main_turns.iter().chain(&sub_turns).flat_map(|t| &t.blocks) {
        if let Some(title) = saved_title(block)
            && !already_saved.contains(&title)
        {
            already_saved.push(title);
        }
    }

    // `handoff` only when this run reaches the end of what the session has.
    let handoff_from =
        (input.closes_session && covered == main.len()).then(|| main_turns.len().saturating_sub(2));
    let mut drafts = Vec::new();
    for (i, turn) in main_turns.iter().enumerate() {
        let handoff_ok = handoff_from.is_some_and(|from| i >= from);
        turn_drafts(turn, false, handoff_ok, &mut drafts);
    }
    for turn in &sub_turns {
        turn_drafts(turn, true, false, &mut drafts);
    }

    let candidates: Vec<Candidate> = select(drafts, &already_saved)
        .into_iter()
        .enumerate()
        .map(|(i, d)| {
            let mut chunks = d.chunks;
            if context_line(d.source, d.prompt.as_deref()).is_some() {
                chunks.extend(d.prompt_chunks);
            }
            Candidate {
                qid: format!("c{}", i + 1),
                source: d.source,
                text: d.text,
                prompt: d.prompt,
                chunk_ids: chunks.into_iter().map(|k| order[k].id.clone()).collect(),
                handoff_ok: d.handoff_ok,
                pos: d.pos,
            }
        })
        .collect();
    let questions = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| question(i, c))
        .collect();
    let state = state(&input.project, &main_turns, &already_saved);
    Prepared {
        candidates,
        already_saved,
        state,
        questions,
        consumed_ord,
    }
}

/// Maps Jev's answers to memories (spec §6.4 and §6.5). A candidate is kept when its most
/// probable option is a knowledge option with at least `min_confidence`; at most `max_per_run`
/// are kept, by probability and then session order, with at most one `handoff`. The memories
/// come in session order.
pub fn decide(
    p: &Prepared,
    answers: &BTreeMap<String, Distribution>,
    uri: &str,
    cfg: &DistillConfig,
) -> Vec<DistilledMemory> {
    let mut kept: Vec<(&Candidate, Kind, bool, f64)> = Vec::new();
    for c in &p.candidates {
        let Some((best, prob)) = answers.get(&c.qid).and_then(Distribution::best) else {
            continue;
        };
        let Some((kind, to_global)) = knowledge(&best) else {
            continue;
        };
        // An option that was not offered is not an answer.
        if kind == Kind::Handoff && !c.handoff_ok {
            continue;
        }
        if prob >= cfg.min_confidence {
            kept.push((c, kind, to_global, prob));
        }
    }
    kept.sort_by(|a, b| b.3.total_cmp(&a.3).then(a.0.pos.cmp(&b.0.pos)));
    let mut out: Vec<(&Candidate, Kind, bool, f64)> = Vec::new();
    let mut handoff = false;
    for k in kept {
        if out.len() >= cfg.max_per_run {
            break;
        }
        if k.1 == Kind::Handoff {
            if handoff {
                continue;
            }
            handoff = true;
        }
        out.push(k);
    }
    out.sort_by_key(|k| k.0.pos);
    out.into_iter()
        .map(|(c, kind, to_global, prob)| DistilledMemory {
            kind,
            to_global,
            title: title_of(&c.text),
            body: body_of(c),
            chunk_ids: c.chunk_ids.clone(),
            idempotency_key: idempotency_key(uri, &c.text),
            p: prob,
        })
        .collect()
}

/// `distill:` and the first 32 hex digits of `sha256(uri ‖ 0x1f ‖ passage)`, so a re-run of the
/// same passage returns the first memory.
pub fn idempotency_key(uri: &str, passage: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(uri.as_bytes());
    hasher.update([0x1f]);
    hasher.update(passage.as_bytes());
    let hex = hex::encode(hasher.finalize());
    format!("distill:{}", &hex[..32])
}

/// The memory kind of a knowledge option, and whether it goes to `global`.
fn knowledge(option: &str) -> Option<(Kind, bool)> {
    Some(match option {
        "decision" => (Kind::Decision, false),
        "bugfix" => (Kind::Bugfix, false),
        "howto" => (Kind::Howto, false),
        "warning" => (Kind::Warning, false),
        "preference_user" => (Kind::Preference, true),
        "preference_project" => (Kind::Preference, false),
        "fact" => (Kind::Fact, false),
        "handoff" => (Kind::Handoff, false),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Parsing chunks back into turns and blocks.

/// A block's tag.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tag {
    User,
    Assistant,
    Tool(String),
    Result,
    ResultError,
    Summary,
    Subagent(String),
    /// A tag this module does not know, or text before any tag.
    Other,
}

/// One line of a block, and the chunks it came from (two when the renderer cut it).
#[derive(Debug, Clone)]
struct Line {
    text: String,
    chunks: Vec<usize>,
}

#[derive(Debug, Clone)]
struct Block {
    tag: Tag,
    lines: Vec<Line>,
}

impl Block {
    fn first(&self) -> &str {
        self.lines.first().map_or("", |l| l.text.as_str())
    }

    fn text(&self) -> String {
        lines_text(&self.lines).s
    }

    fn thinking(&self) -> bool {
        self.tag == Tag::Assistant && self.first().starts_with("(thinking)")
    }

    fn withheld(&self) -> bool {
        self.lines.iter().any(|l| l.text.contains(WITHHELD))
    }

    fn chunks(&self) -> BTreeSet<usize> {
        self.lines
            .iter()
            .flat_map(|l| l.chunks.iter().copied())
            .collect()
    }
}

/// How a turn's start is known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Start {
    /// From the text, or the start of a stream (no flag).
    #[default]
    Guessed,
    /// Flagged `Some(true)`: its first `[user]` block is the prompt record, whatever it looks like.
    Flagged,
    /// A stream that starts in the middle of a turn (flagged `Some(false)`): no prompt.
    Headless,
}

#[derive(Debug, Default)]
struct Turn {
    blocks: Vec<Block>,
    start: Start,
}

/// A head line's tag, whether it continues a block the renderer cut (`(cont.)`), and the text
/// after the tag. `None` for a body line.
fn parse_head(line: &str) -> Option<(Tag, bool, &str)> {
    let start = TAGS.iter().copied().find(|t| line.starts_with(t))?;
    let after = &line[start.len()..];
    let (tag, rest) = match start {
        "[user]" => (Tag::User, after),
        "[assistant]" => (Tag::Assistant, after),
        "[result]" => (Tag::Result, after),
        "[result error]" => (Tag::ResultError, after),
        "[summary]" => (Tag::Summary, after),
        "[tool " | "[subagent " => {
            let close = after.find(']')?;
            let name = after[..close].to_owned();
            let tag = if start == "[tool " {
                Tag::Tool(name)
            } else {
                Tag::Subagent(name)
            };
            (tag, &after[close + 1..])
        }
        _ => (Tag::Other, after),
    };
    // A head is its tag alone, or the tag, one space and the text.
    let rest = if rest.is_empty() {
        rest
    } else {
        rest.strip_prefix(' ')?
    };
    match rest.strip_prefix("(cont.)") {
        Some(r) if r.is_empty() || r.starts_with(' ') => {
            Some((tag, true, r.strip_prefix(' ').unwrap_or(r)))
        }
        _ => Some((tag, false, rest)),
    }
}

/// A chunk starts a turn when its first line is a `[user]` prompt (not a continuation, not a
/// command line, which never starts a turn) or a `[summary]`. A withheld chunk has lost its
/// `(cont.)`, so `[user] [withheld: …]` counts as a start.
fn starts_turn(chunk: &str) -> bool {
    let first = chunk.split('\n').next().unwrap_or("");
    match parse_head(first) {
        Some((Tag::Summary, false, _)) => true,
        Some((Tag::User, false, rest)) => !is_command(rest),
        _ => false,
    }
}

/// A slash command as a command wrapper renders it (`/review src/lib.rs`), not a path.
fn is_command(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("");
    first.len() > 1
        && first.len() <= 64
        && first.starts_with('/')
        && first[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
}

/// A body line as written: the renderer's neutralising space taken off a line that starts with
/// `[`, and a trim marker shown as `…`. The first line follows its tag and was never neutralised.
fn body_line(raw: &str, first: bool) -> String {
    let line = if first {
        raw
    } else {
        raw.strip_prefix(' ')
            .filter(|l| l.starts_with('['))
            .unwrap_or(raw)
    };
    let t = line.trim();
    if t.starts_with("[… ") && t.ends_with(" characters trimmed …]") {
        "…".to_owned()
    } else {
        line.to_owned()
    }
}

/// How a continuation joins the line the renderer cut it from, when it stays on that line: a cut
/// after a sentence end was a space, a cut in front of a combining mark was nothing. `None`
/// starts a new line (a paragraph or line cut, or a list item).
fn inline_join(prev: &str, next: &str) -> Option<&'static str> {
    let (a, b) = (prev.chars().next_back()?, next.chars().next()?);
    if is_mark(b) {
        return Some("");
    }
    let structural = bullet_len(next).is_some()
        || next.starts_with('#')
        || next.starts_with('|')
        || fence_of(next).is_some();
    if structural {
        None
    } else if matches!(a, '.' | '!' | '?' | ',' | ';') {
        Some(" ")
    } else if matches!(a, '。' | '！' | '？') {
        Some("")
    } else {
        None
    }
}

/// Whether a chunk starts a turn: its flag, else the text rule.
fn is_turn_start(chunk: &RunChunk) -> bool {
    chunk.turn_start.unwrap_or_else(|| starts_turn(&chunk.text))
}

/// Parses one stream of chunks (`(index, chunk)`, in order) into turns of blocks. A turn starts
/// where [`is_turn_start`] says so, and at the stream's first chunk.
fn parse(chunks: &[(usize, &RunChunk)]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for &(idx, chunk) in chunks {
        let start = match chunk.turn_start {
            Some(true) => Some(Start::Flagged),
            Some(false) => turns.is_empty().then_some(Start::Headless),
            None => (turns.is_empty() || starts_turn(&chunk.text)).then_some(Start::Guessed),
        };
        if let Some(start) = start {
            turns.push(Turn {
                blocks: Vec::new(),
                start,
            });
        }
        let turn = turns.last_mut().expect("a turn was pushed");
        for (i, raw) in chunk.text.split('\n').enumerate() {
            let Some((tag, cont, rest)) = parse_head(raw) else {
                let line = Line {
                    text: body_line(raw, false),
                    chunks: vec![idx],
                };
                match turn.blocks.last_mut() {
                    Some(block) => block.lines.push(line),
                    None => turn.blocks.push(Block {
                        tag: Tag::Other,
                        lines: vec![line],
                    }),
                }
                continue;
            };
            let text = body_line(rest, true);
            match turn.blocks.last_mut() {
                Some(prev) if cont && i == 0 && prev.tag == tag => {
                    let last = prev.lines.last_mut().expect("a block has a line");
                    match inline_join(&last.text, &text) {
                        Some(sep) => {
                            last.text.push_str(sep);
                            last.text.push_str(&text);
                            last.chunks.push(idx);
                        }
                        None => prev.lines.push(Line {
                            text,
                            chunks: vec![idx],
                        }),
                    }
                }
                _ => turn.blocks.push(Block {
                    tag,
                    lines: vec![Line {
                        text,
                        chunks: vec![idx],
                    }],
                }),
            }
        }
    }
    turns
}

/// How many of the main chunks (sorted by `ord`) the run covers: whole turns, at most
/// [`RUN_MAX_TURNS`] of them and [`RUN_MAX_CHARS`] characters. A first turn larger than a run
/// is covered chunk by chunk, at least one, so the cursor always moves.
fn covered(main: &[&RunChunk]) -> usize {
    let (mut turns, mut chars, mut end) = (0, 0, 0);
    while end < main.len() {
        let next = (end + 1..main.len())
            .find(|&k| is_turn_start(main[k]))
            .unwrap_or(main.len());
        let size: usize = main[end..next].iter().map(|c| c.text.chars().count()).sum();
        if turns == RUN_MAX_TURNS || (turns > 0 && chars + size > RUN_MAX_CHARS) {
            break;
        }
        if size > RUN_MAX_CHARS {
            let mut used = 0;
            for (k, chunk) in main.iter().enumerate().take(next).skip(end) {
                let n = chunk.text.chars().count();
                if k > end && used + n > RUN_MAX_CHARS {
                    return k;
                }
                used += n;
            }
            return next;
        }
        turns += 1;
        chars += size;
        end = next;
    }
    end
}

/// The turn's prompt: the first `[user]` block of real text among those it starts with (not a
/// command line, a task notification, a media placeholder or withheld text). The first block of
/// a flagged turn is the prompt record's text even when it looks like a command
/// (`/health returns 503`); a stream that starts in the middle of a turn has no prompt.
fn prompt_block(turn: &Turn) -> Option<&Block> {
    if turn.start == Start::Headless {
        return None;
    }
    turn.blocks
        .iter()
        .take_while(|b| b.tag == Tag::User)
        .enumerate()
        .find(|(i, b)| {
            let text = b.text();
            let t = text.trim();
            let typed = turn.start == Start::Flagged && *i == 0;
            !t.is_empty()
                && !b.withheld()
                && (typed || !is_command(t))
                && !t.starts_with("(task notification)")
                && !PLACEHOLDERS.contains(&t)
        })
        .map(|(_, b)| b)
}

/// The lines of the turn's last assistant block that is not thinking, together with the
/// assistant blocks right before it (one message cut into several text blocks). `None` when
/// there is none or it is withheld.
fn conclusion_lines(turn: &Turn) -> Option<Vec<Line>> {
    let blocks = &turn.blocks;
    let last = blocks
        .iter()
        .rposition(|b| b.tag == Tag::Assistant && !b.thinking())?;
    if blocks[last].withheld() {
        return None;
    }
    let mut first = last;
    while first > 0 && blocks[first - 1].tag == Tag::Assistant && !blocks[first - 1].withheld() {
        first -= 1;
    }
    let mut lines = Vec::new();
    for block in blocks[first..=last].iter().filter(|b| !b.thinking()) {
        if !lines.is_empty() {
            lines.push(Line {
                text: String::new(),
                chunks: Vec::new(),
            });
        }
        lines.extend(block.lines.iter().cloned());
    }
    Some(lines)
}

/// The turn's subagent reports. A call renders `[subagent <type>] <description>` (one line, at
/// most 300 characters) and its report a second `[subagent <type>]` block: a block that cannot
/// be a description, or that follows an open call of its type, is a report. An assistant block
/// closes every open call (its results came before it).
fn reports(turn: &Turn) -> Vec<&Block> {
    let mut open: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for block in &turn.blocks {
        match &block.tag {
            Tag::Assistant => open.clear(),
            Tag::Subagent(kind) => {
                let calls = open.entry(kind.as_str()).or_default();
                let long = block.lines.len() > 1 || block.first().chars().count() > CALL_CHARS;
                if long || *calls > 0 {
                    *calls = calls.saturating_sub(1);
                    out.push(block);
                } else {
                    *calls += 1;
                }
            }
            _ => {}
        }
    }
    out
}

/// The title a memory tool's line saved: `[tool alice remember] Use pnpm, not npm`.
fn saved_title(block: &Block) -> Option<String> {
    let Tag::Tool(name) = &block.tag else {
        return None;
    };
    let tool = name.strip_prefix("alice ")?;
    if !SAVE_TOOLS.contains(&tool) || block.withheld() {
        return None;
    }
    let title = block.first().trim();
    (!title.is_empty()).then(|| title.to_owned())
}

// ---------------------------------------------------------------------------------------------
// Candidates.

/// A candidate before selection.
struct Draft {
    source: CandidateSource,
    text: String,
    prompt: Option<String>,
    chunks: BTreeSet<usize>,
    prompt_chunks: BTreeSet<usize>,
    handoff_ok: bool,
    /// From a subagent source: its copy of a report loses to the main session's.
    from_subagent: bool,
    pos: usize,
}

/// The drafts of one turn: its prompt as a statement, its summaries, its subagent reports and
/// its conclusion. A subagent source's turn gives only reports: its prompt is the parent
/// agent's, not the user's, and its last assistant block is its final report.
fn turn_drafts(turn: &Turn, from_subagent: bool, handoff_ok: bool, out: &mut Vec<Draft>) {
    let prompt_block = prompt_block(turn);
    let prompt = prompt_block.and_then(|b| prompt_sentence(&b.text()));
    let prompt_chunks = prompt_block.map(Block::chunks).unwrap_or_default();
    let mut push = |source: CandidateSource, text: Text, prompt: Option<String>| {
        out.push(Draft {
            source,
            chunks: text.chunks(),
            text: text.s,
            prompt,
            prompt_chunks: prompt_chunks.clone(),
            handoff_ok,
            from_subagent,
            pos: out.len(),
        });
    };
    if !from_subagent {
        if let Some(block) = prompt_block {
            let text = trim_text(lines_text(&replace_code(&block.lines)));
            if (USER_MIN..=USER_MAX).contains(&text.chars()) {
                push(CandidateSource::UserStatement, text, None);
            }
        }
        for block in turn.blocks.iter().filter(|b| b.tag == Tag::Summary) {
            for text in passages(&block.lines) {
                push(CandidateSource::Summary, text, None);
            }
        }
    }
    for block in reports(turn) {
        for text in passages(&block.lines) {
            push(CandidateSource::SubagentReport, text, prompt.clone());
        }
    }
    if let Some(lines) = conclusion_lines(turn) {
        let source = if from_subagent {
            CandidateSource::SubagentReport
        } else {
            CandidateSource::Conclusion
        };
        for text in passages(&lines) {
            push(source, text, prompt.clone());
        }
    }
}

/// The local filters and the cap (spec §6.3): withheld and tool-like text goes, so does text
/// that repeats something already saved, or a candidate chosen before it; then at most
/// [`MAX_CANDIDATES`] by priority and newest first (the main session's copy first). The result
/// is in priority, then session, order.
fn select(drafts: Vec<Draft>, saved: &[String]) -> Vec<Draft> {
    let saved: Vec<Grams> = saved.iter().map(|s| trigrams(s)).collect();
    let mut pool: Vec<(Draft, Grams)> = drafts
        .into_iter()
        .filter(|d| !d.text.contains(WITHHELD) && !tool_like(&d.text))
        .map(|d| {
            let grams = trigrams(&d.text);
            (d, grams)
        })
        .filter(|(_, g)| !saved.iter().any(|s| jaccard(g, s) >= OVERLAP))
        .collect();
    pool.sort_by(|(a, _), (b, _)| {
        (a.source, a.from_subagent)
            .cmp(&(b.source, b.from_subagent))
            .then(b.pos.cmp(&a.pos))
    });
    let mut chosen: Vec<(Draft, Grams)> = Vec::new();
    for (draft, grams) in pool {
        if chosen.len() == MAX_CANDIDATES {
            break;
        }
        if chosen.iter().any(|(_, g)| jaccard(&grams, g) >= OVERLAP) {
            continue;
        }
        chosen.push((draft, grams));
    }
    let mut out: Vec<Draft> = chosen.into_iter().map(|(d, _)| d).collect();
    out.sort_by_key(|d| (d.source, d.pos));
    out
}

/// Mostly a tool line: most lines are block heads (a quoted transcript), or most of the text
/// is placeholders such as `[code]`.
fn tool_like(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let heads = lines
        .iter()
        .filter(|l| TAGS.iter().any(|t| l.starts_with(t)))
        .count();
    if heads * 2 > lines.len() {
        return true;
    }
    let solid = |s: &str| s.chars().filter(|c| !c.is_whitespace()).count();
    let mut prose = text.to_owned();
    for p in PLACEHOLDERS {
        prose = prose.replace(p, "");
    }
    solid(&prose) * 2 < solid(text)
}

type Grams = HashSet<[char; 3]>;

/// The character trigrams of the lower-cased text.
fn trigrams(text: &str) -> Grams {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
}

fn jaccard(a: &Grams, b: &Grams) -> f64 {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let shared = small.iter().filter(|g| large.contains(*g)).count();
    let union = a.len() + b.len() - shared;
    shared as f64 / union.max(1) as f64
}

/// Trigram Jaccard of two texts (lower-cased).
#[cfg(test)]
fn overlap(a: &str, b: &str) -> f64 {
    jaccard(&trigrams(a), &trigrams(b))
}

// ---------------------------------------------------------------------------------------------
// Passages.

/// Text, and the chunks each stretch of it came from.
#[derive(Debug, Clone, Default)]
struct Text {
    s: String,
    spans: Vec<(Range<usize>, Vec<usize>)>,
}

impl Text {
    fn chars(&self) -> usize {
        self.s.chars().count()
    }

    fn push(&mut self, sep: &str, line: &str, chunks: &[usize]) {
        if !self.s.is_empty() {
            self.s.push_str(sep);
        }
        let start = self.s.len();
        self.s.push_str(line);
        self.spans.push((start..self.s.len(), chunks.to_vec()));
    }

    fn append(&mut self, sep: &str, other: Text) {
        if !self.s.is_empty() {
            self.s.push_str(sep);
        }
        let base = self.s.len();
        self.s.push_str(&other.s);
        self.spans.extend(
            other
                .spans
                .into_iter()
                .map(|(r, c)| (r.start + base..r.end + base, c)),
        );
    }

    fn slice(&self, r: Range<usize>) -> Text {
        Text {
            s: self.s[r.clone()].to_owned(),
            spans: self
                .spans
                .iter()
                .filter(|(s, _)| s.start < r.end && r.start < s.end)
                .map(|(s, c)| {
                    (
                        s.start.max(r.start) - r.start..s.end.min(r.end) - r.start,
                        c.clone(),
                    )
                })
                .collect(),
        }
    }

    fn chunks(&self) -> BTreeSet<usize> {
        self.spans
            .iter()
            .filter(|(r, _)| !r.is_empty())
            .flat_map(|(_, c)| c.iter().copied())
            .collect()
    }
}

fn lines_text(lines: &[Line]) -> Text {
    let mut text = Text::default();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            text.s.push('\n');
        }
        let start = text.s.len();
        text.s.push_str(&line.text);
        text.spans.push((start..text.s.len(), line.chunks.clone()));
    }
    text
}

fn trim_text(text: Text) -> Text {
    let start = text.s.len() - text.s.trim_start().len();
    let end = text.s.trim_end().len();
    if start >= end {
        return Text::default();
    }
    text.slice(start..end)
}

/// The opening of a code fence.
fn fence_of(t: &str) -> Option<&'static str> {
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Fenced code blocks of more than [`CODE_LINES_MAX`] lines become one `[code]` line.
fn replace_code(lines: &[Line]) -> Vec<Line> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let Some(fence) = fence_of(lines[i].text.trim_start()) else {
            out.push(lines[i].clone());
            i += 1;
            continue;
        };
        let close = (i + 1..lines.len()).find(|&j| lines[j].text.trim_start().starts_with(fence));
        let end = close.unwrap_or(lines.len() - 1);
        let inner = end - i - usize::from(close.is_some());
        if inner > CODE_LINES_MAX {
            let open = &lines[i].text;
            let indent = &open[..open.len() - open.trim_start().len()];
            let chunks: BTreeSet<usize> = lines[i..=end]
                .iter()
                .flat_map(|l| l.chunks.iter().copied())
                .collect();
            out.push(Line {
                text: format!("{indent}{CODE}"),
                chunks: chunks.into_iter().collect(),
            });
        } else {
            out.extend(lines[i..=end].iter().cloned());
        }
        i = end + 1;
    }
    out
}

/// The byte length of a list marker (`- `, `* `, `+ `, `• `, `1. `, `1) `) at the start of `t`.
fn bullet_len(t: &str) -> Option<usize> {
    ["- ", "* ", "+ ", "• "]
        .iter()
        .find(|m| t.starts_with(**m))
        .map(|m| m.len())
        .or_else(|| numbered_len(t))
}

fn numbered_len(t: &str) -> Option<usize> {
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &t[digits..];
    ((1..=3).contains(&digits) && (rest.starts_with(". ") || rest.starts_with(") ")))
        .then_some(digits + 2)
}

/// A heading: `# …`, a numbered section head such as `4. Errors and fixes:` (not indented), or
/// a bold line such as `**Key decisions**`.
fn is_heading(t: &str, indent: usize) -> bool {
    if let Some(rest) = t.strip_prefix('#') {
        let rest = rest.trim_start_matches('#');
        return rest.is_empty() || rest.starts_with(' ');
    }
    let short = t.chars().count() <= PASSAGE_MIN;
    let numbered = indent == 0 && numbered_len(t).is_some() && t.ends_with(':');
    let bold = t.len() > 4 && t.starts_with("**") && (t.ends_with("**") || t.ends_with("**:"));
    short && (numbered || bold)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Heading,
    Item,
    Para,
}

/// A heading, a list item (with its nested lines) or a paragraph.
#[derive(Debug)]
struct Unit {
    kind: UnitKind,
    text: Text,
    blank_before: bool,
    section: usize,
}

impl Unit {
    fn new(kind: UnitKind, blank_before: bool, section: usize) -> Self {
        Self {
            kind,
            text: Text::default(),
            blank_before,
            section,
        }
    }
}

/// Cuts lines into units. A blank line ends a unit, a heading starts a section, a list item at
/// the list's indentation starts a unit, and a kept code block is never cut.
fn units(lines: &[Line]) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut cur: Option<Unit> = None;
    let mut blank = false;
    let mut section = 0;
    let mut list_indent = 0;
    let mut fence: Option<&str> = None;
    for line in lines {
        let raw = line.text.trim_end();
        let t = raw.trim_start();
        if let Some(f) = fence {
            cur.get_or_insert_with(|| Unit::new(UnitKind::Para, blank, section))
                .text
                .push("\n", raw, &line.chunks);
            if t.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if t.is_empty() {
            out.extend(cur.take());
            blank = true;
            continue;
        }
        let indent = raw.len() - t.len();
        if let Some(f) = fence_of(t) {
            fence = Some(f);
            cur.get_or_insert_with(|| Unit::new(UnitKind::Para, blank, section))
                .text
                .push("\n", raw, &line.chunks);
            blank = false;
            continue;
        }
        if is_heading(t, indent) {
            out.extend(cur.take());
            section += 1;
            let mut heading = Unit::new(UnitKind::Heading, blank, section);
            heading.text.push("\n", t, &line.chunks);
            out.push(heading);
            blank = false;
            continue;
        }
        let item = bullet_len(t).is_some();
        let continues = match &cur {
            Some(u) if u.kind == UnitKind::Item => indent > list_indent,
            Some(u) => u.kind == UnitKind::Para && !item,
            None => false,
        };
        if !continues {
            out.extend(cur.take());
            let kind = if item {
                list_indent = indent;
                UnitKind::Item
            } else {
                UnitKind::Para
            };
            cur = Some(Unit::new(kind, blank, section));
        }
        cur.as_mut()
            .expect("set above")
            .text
            .push("\n", raw, &line.chunks);
        blank = false;
    }
    out.extend(cur);
    out
}

/// A heading line that is a plain paragraph: one line of at most [`PASSAGE_MIN`] characters that
/// does not end in `. ! ?` or their full-width forms, such as `Next steps:` (a Thai line, which has
/// no sentence mark, qualifies whatever it says). Followed by list items, it heads them as a
/// heading does.
fn is_lead_line(unit: &Unit) -> bool {
    let t = unit.text.s.trim();
    unit.kind == UnitKind::Para
        && !t.contains('\n')
        && t.chars().count() <= PASSAGE_MIN
        && !t.ends_with(['.', '!', '?', '。', '！', '？'])
}

/// Cuts a block's lines into passages of [`PASSAGE_MIN`] to [`PASSAGE_MAX`] characters (spec
/// §6.3): paragraphs and bullet groups, a heading leading the unit after it. A heading line
/// followed by list items keeps the whole group together (a handoff's done and next items stay
/// one passage). Short neighbours in a section join until they reach the minimum; a unit over the
/// maximum is cut at sentence ends, then spaces, then characters. Long code blocks become `[code]`
/// first.
fn passages(lines: &[Line]) -> Vec<Text> {
    let lines = replace_code(lines);
    let mut out: Vec<(Text, usize)> = Vec::new();
    let mut cur: Option<(Text, usize)> = None;
    let mut lead: Option<Text> = None;
    let mut units = units(&lines).into_iter().peekable();
    while let Some(unit) = units.next() {
        let heads_items = units
            .peek()
            .is_some_and(|n| n.kind == UnitKind::Item && n.section == unit.section);
        if unit.kind == UnitKind::Heading || (heads_items && is_lead_line(&unit)) {
            flush(&mut cur, &mut out);
            match &mut lead {
                Some(l) => l.append("\n", unit.text),
                None => lead = Some(unit.text),
            }
            continue;
        }
        let sep = if unit.blank_before { "\n\n" } else { "\n" };
        let unit_section = unit.section;
        let piece = match lead.take() {
            // A heading line leads its whole bullet group.
            Some(mut l) if unit.kind == UnitKind::Item => {
                l.append("\n", unit.text);
                while let Some(n) =
                    units.next_if(|n| n.kind == UnitKind::Item && n.section == unit_section)
                {
                    l.append(if n.blank_before { "\n\n" } else { "\n" }, n.text);
                }
                l
            }
            Some(mut l) => {
                l.append("\n", unit.text);
                l
            }
            None => unit.text,
        };
        if piece.chars() > PASSAGE_MAX {
            flush(&mut cur, &mut out);
            for r in cut_ranges(&piece.s, PASSAGE_MAX) {
                out.push((piece.slice(r), unit.section));
            }
            continue;
        }
        match &mut cur {
            Some((c, section))
                if *section == unit.section
                    && c.chars() < PASSAGE_MIN
                    && c.chars() + sep.len() + piece.chars() <= PASSAGE_MAX =>
            {
                c.append(sep, piece);
            }
            _ => {
                flush(&mut cur, &mut out);
                cur = Some((piece, unit.section));
            }
        }
    }
    flush(&mut cur, &mut out);
    out.into_iter()
        .map(|(t, _)| trim_text(t))
        .filter(|t| (PASSAGE_MIN..=PASSAGE_MAX).contains(&t.chars()))
        .collect()
}

/// Ends the passage being built. One that is still short joins the passage before it when that
/// is in the same section and has room; otherwise it is dropped.
fn flush(cur: &mut Option<(Text, usize)>, out: &mut Vec<(Text, usize)>) {
    let Some((text, section)) = cur.take() else {
        return;
    };
    if text.chars() >= PASSAGE_MIN {
        out.push((text, section));
    } else if let Some((last, s)) = out.last_mut()
        && *s == section
        && last.chars() + 2 + text.chars() <= PASSAGE_MAX
    {
        last.append("\n\n", text);
    }
}

/// Byte ranges of `text` in pieces of at most `max` characters, each trimmed. When the rest is
/// only a little over `max`, the cut leaves it at least [`PASSAGE_MIN`] characters.
fn cut_ranges(text: &str, max: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    loop {
        let rest = &text[start..];
        start += rest.len() - rest.trim_start().len();
        let rest = &text[start..];
        if rest.is_empty() {
            break;
        }
        let total = rest.chars().count();
        if total <= max {
            out.push(start..start + rest.trim_end().len());
            break;
        }
        let target = if total < max + PASSAGE_MIN {
            total - PASSAGE_MIN
        } else {
            max
        };
        let cut = cut_point(rest, byte_of_char(rest, target.max(1)));
        out.push(start..start + rest[..cut].trim_end().len());
        start += cut;
    }
    out
}

/// Where to end a piece of `text` within its first `limit` bytes (a character boundary): after
/// the last sentence end or line break, else at the last space, else hard (never in front of a
/// combining mark). A cut in the first quarter is not used, so a piece is never a sliver.
fn cut_point(text: &str, limit: usize) -> usize {
    let floor = limit / 4;
    let (mut sentence, mut space) = (None, None);
    for (i, c) in text[..limit].char_indices() {
        let next = i + c.len_utf8();
        let then_space = text[next..].chars().next().is_none_or(char::is_whitespace);
        let ends = c == '\n'
            || matches!(c, '。' | '！' | '？')
            || (matches!(c, '.' | '!' | '?') && then_space);
        if ends && next > floor {
            sentence = Some(next);
        } else if c.is_whitespace() && i > floor {
            space = Some(i);
        }
    }
    sentence.or(space).unwrap_or_else(|| hard_cut(text, limit))
}

/// `limit`, moved back so the next piece does not start with a combining mark.
fn hard_cut(text: &str, limit: usize) -> usize {
    let mut cut = limit;
    while cut > 0 && text[cut..].chars().next().is_some_and(is_mark) {
        cut = text[..cut].char_indices().next_back().map_or(0, |(i, _)| i);
    }
    if cut == 0 { limit } else { cut }
}

/// The start of `text` within `max` characters, cut at the last space (or hard, never in front
/// of a combining mark, when the only space is in the first quarter).
fn cut_words(text: &str, max: usize) -> &str {
    if text.chars().count() <= max {
        return text;
    }
    let limit = byte_of_char(text, max);
    let cut = if text[limit..].starts_with(char::is_whitespace) {
        limit
    } else {
        text[..limit]
            .rfind(char::is_whitespace)
            .filter(|&i| i > limit / 4)
            .unwrap_or_else(|| hard_cut(text, limit))
    };
    text[..cut].trim_end()
}

/// The byte offset of the `n`th character (the end when there are fewer).
fn byte_of_char(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(i, _)| i)
}

/// A combining mark (Thai and Lao vowel and tone marks, Thai sara am, combining diacritics,
/// emoji modifiers): a piece never starts with one.
fn is_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}'
        | '\u{0E31}'
        | '\u{0E33}'..='\u{0E3A}'
        | '\u{0E47}'..='\u{0E4E}'
        | '\u{0EB1}'
        | '\u{0EB3}'..='\u{0EBC}'
        | '\u{0EC8}'..='\u{0ECD}'
        | '\u{200D}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{1F3FB}'..='\u{1F3FF}')
}

/// The text up to its first sentence end (`.`, `!` or `?` before a space or the end, `。`, `！`,
/// `？`) or line break, after a leading heading or list marker.
fn first_sentence(text: &str) -> &str {
    let text = strip_marker(text).trim();
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next = i + c.len_utf8();
        if c == '\n' {
            return text[..i].trim_end();
        }
        if matches!(c, '。' | '！' | '？')
            || (matches!(c, '.' | '!' | '?')
                && chars.peek().is_none_or(|&(_, n)| n.is_whitespace()))
        {
            return &text[..next];
        }
    }
    text
}

/// The first sentence of a prompt, at most [`PROMPT_CHARS`] characters.
fn prompt_sentence(text: &str) -> Option<String> {
    let sentence = cut_words(first_sentence(text), PROMPT_CHARS).trim();
    (!sentence.is_empty()).then(|| sentence.to_owned())
}

// ---------------------------------------------------------------------------------------------
// The request and the memories.

/// The state (spec §6.4): the project, an excerpt of the run (each prompt and each turn's final
/// assistant text, newest kept) and the titles already saved.
fn state(project: &str, main: &[Turn], saved: &[String]) -> String {
    let mut excerpt = Vec::new();
    for turn in main {
        if let Some(block) = prompt_block(turn) {
            excerpt.push(format!(
                "User: {}",
                one_line(&block.text(), STATE_PROMPT_CHARS)
            ));
        }
        if let Some(lines) = conclusion_lines(turn) {
            let reply = lines_text(&replace_code(&lines)).s;
            if !reply.trim().is_empty() {
                excerpt.push(format!(
                    "Assistant: {}",
                    one_line(&reply, STATE_REPLY_CHARS)
                ));
            }
        }
    }
    let head = format!("Project: {}\n", one_line(project, PROMPT_CHARS));
    let mut tail = String::new();
    if !saved.is_empty() {
        tail.push_str("\nAlready saved in this session:\n");
        for title in &saved[saved.len().saturating_sub(ALREADY_SAVED_MAX)..] {
            tail.push_str(&format!("- {}\n", one_line(title, PROMPT_CHARS)));
        }
    }
    const EXCERPT: &str = "\nSession excerpt:\n";
    let room = STATE_CHARS
        .saturating_sub(head.chars().count() + tail.chars().count() + EXCERPT.chars().count());
    let (mut used, mut from) = (0, excerpt.len());
    while from > 0 {
        let n = excerpt[from - 1].chars().count() + 1;
        if used + n > room {
            break;
        }
        used += n;
        from -= 1;
    }
    let mut state = head;
    if from < excerpt.len() {
        state.push_str(EXCERPT);
        for line in &excerpt[from..] {
            state.push_str(line);
            state.push('\n');
        }
    }
    state.push_str(&tail);
    state.truncate(byte_of_char(&state, STATE_CHARS));
    state
}

/// The text on one line, at most `max` characters (`…` when cut).
fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        line
    } else {
        format!("{}…", cut_words(&line, max.saturating_sub(1)))
    }
}

/// The question about candidate `i` (spec §6.4); its number is in words.
fn question(i: usize, c: &Candidate) -> Question {
    let n = verbal::number(i as u64 + 1);
    let asked = c
        .prompt
        .as_ref()
        .map(|p| format!(" (the user had asked: «{p}»)"))
        .unwrap_or_default();
    Question {
        id: c.qid.clone(),
        instructions: format!(
            "Passage {n} from this session{asked}: «{}». What lasting knowledge, if any, does it hold for future work?",
            c.text
        ),
        options: OPTIONS
            .iter()
            .filter(|(id, _)| *id != "handoff" || c.handoff_ok)
            .map(|(id, description)| Choice::new(id, description))
            .collect(),
    }
}

/// `Context: <prompt>` for a conclusion or a subagent report whose prompt has at least four
/// words, or at least twelve characters of a script written without spaces, so the memory stands
/// on its own.
fn context_line(source: CandidateSource, prompt: Option<&str>) -> Option<String> {
    if !matches!(
        source,
        CandidateSource::Conclusion | CandidateSource::SubagentReport
    ) {
        return None;
    }
    let prompt = prompt?;
    let words = prompt.split_whitespace().count();
    let unspaced = prompt.chars().filter(|&c| in_unspaced_script(c)).count();
    (words >= CONTEXT_MIN_WORDS || unspaced >= CONTEXT_MIN_UNSPACED)
        .then(|| format!("Context: {prompt}"))
}

/// The passage within [`BODY_MAX`] characters, and its context line.
fn body_of(c: &Candidate) -> String {
    let mut body = cut_words(&c.text, BODY_MAX).to_owned();
    if let Some(line) = context_line(c.source, c.prompt.as_deref()) {
        body.push_str("\n\n");
        body.push_str(&line);
    }
    body
}

/// The passage's first sentence within [`TITLE_MAX`] characters, cut at a word boundary.
/// Headings, placeholders and code (fence lines and what they enclose) are passed over, list
/// markers and bold marks dropped, and a line that ends with `:` runs on into the next. A
/// passage that is only code is titled by its first line of code.
fn title_of(text: &str) -> String {
    let mut prose: Vec<(bool, String)> = Vec::new();
    let mut code: Vec<(bool, String)> = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines().map(str::trim) {
        if let Some(f) = fence {
            if line.starts_with(f) {
                fence = None;
            } else if !line.is_empty() {
                code.push((false, line.to_owned()));
            }
            continue;
        }
        if let Some(f) = fence_of(line) {
            fence = Some(f);
            continue;
        }
        if line.is_empty() || PLACEHOLDERS.contains(&line) {
            continue;
        }
        let stripped = strip_markup(line);
        if !stripped.is_empty() {
            prose.push((line.starts_with('#'), stripped));
        }
    }
    let lines = if prose.is_empty() { code } else { prose };
    let pick = lines.iter().position(|(heading, _)| !heading).unwrap_or(0);
    let Some((_, first)) = lines.get(pick) else {
        return cut_words(text.trim(), TITLE_MAX).to_owned();
    };
    let mut line = first.clone();
    if line.ends_with(':')
        && let Some((_, next)) = lines.get(pick + 1)
    {
        line = format!("{line} {next}");
    }
    cut_words(first_sentence(&line), TITLE_MAX).to_owned()
}

/// A line without its heading marks, list marker and bold marks.
fn strip_markup(line: &str) -> String {
    strip_marker(line).replace("**", "").trim().to_owned()
}

/// `text` without a heading's `#`s or a list marker (`1. `, `- `, …) at its start.
fn strip_marker(text: &str) -> &str {
    let mut t = text.trim_start();
    if t.starts_with('#') {
        let rest = t.trim_start_matches('#');
        if rest.is_empty() || rest.starts_with(' ') {
            t = rest.trim_start();
        }
    }
    match bullet_len(t) {
        Some(n) => t[n..].trim_start(),
        None => t,
    }
}

#[cfg(test)]
mod tests {
    use super::CandidateSource as S;
    use super::*;
    use crate::capture::{AgentKind, ReadOptions, read};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    const URI: &str = "claude-code:session:S1";
    const TOOLS: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/tools.jsonl");

    /// One record of a scripted Claude Code session.
    enum Step<'a> {
        User(&'a str),
        Say(&'a str),
        Think(&'a str),
        Bash(&'a str, &'a str),
        Agent(&'a str, &'a str),
        Compact(&'a str),
        Command(&'a str, &'a str),
    }
    use Step::*;

    /// The JSONL lines Claude Code writes for a session.
    struct Jsonl {
        out: String,
        n: usize,
        agent: Option<String>,
    }

    impl Jsonl {
        fn line(&mut self, kind: &str, message: Value, extra: Value) {
            self.n += 1;
            let n = self.n;
            let mut v = json!({
                "type": kind,
                "uuid": format!("rec-{n}"),
                "timestamp": format!(
                    "2026-10-14T{:02}:{:02}:{:02}.000Z",
                    9 + n / 3600,
                    n / 60 % 60,
                    n % 60
                ),
                "sessionId": "S1",
                "message": message,
            });
            if let Some(agent) = &self.agent {
                v["agentId"] = json!(agent);
                v["isSidechain"] = json!(true);
            }
            if let Value::Object(extra) = extra {
                for (k, x) in extra {
                    v[k] = x;
                }
            }
            self.out.push_str(&v.to_string());
            self.out.push('\n');
        }

        fn user(&mut self, content: Value, extra: Value) {
            self.line("user", json!({"role": "user", "content": content}), extra);
        }

        fn assistant(&mut self, block: Value) {
            let id = format!("msg-{}", self.n + 1);
            self.line(
                "assistant",
                json!({"id": id, "role": "assistant", "content": [block]}),
                Value::Null,
            );
        }

        fn tool(&mut self, name: &str, input: Value, result: &str) {
            let id = format!("toolu-{}", self.n + 1);
            self.assistant(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
            self.user(
                json!([{"type": "tool_result", "tool_use_id": id, "content": result}]),
                Value::Null,
            );
        }
    }

    /// A session as JSONL; `agent` makes it a subagent's own file.
    fn jsonl(steps: &[Step], agent: Option<&str>) -> Vec<u8> {
        let mut j = Jsonl {
            out: String::new(),
            n: 0,
            agent: agent.map(str::to_owned),
        };
        for step in steps {
            match *step {
                User(t) => j.user(json!(t), Value::Null),
                Say(t) => j.assistant(json!({"type": "text", "text": t})),
                Think(t) => j.assistant(json!({"type": "thinking", "thinking": t, "signature": "c2ln"})),
                Bash(cmd, out) => j.tool("Bash", json!({"command": cmd}), out),
                Agent(desc, report) => j.tool(
                    "Task",
                    json!({"subagent_type": "general-purpose", "description": desc, "prompt": "Do the task."}),
                    report,
                ),
                Compact(t) => j.user(json!(t), json!({"isCompactSummary": true})),
                Command(name, args) => j.user(
                    json!(format!(
                        "<command-name>{name}</command-name>\n<command-args>{args}</command-args>"
                    )),
                    Value::Null,
                ),
            }
        }
        j.out.into_bytes()
    }

    fn opts(chunk_chars: usize) -> ReadOptions {
        ReadOptions {
            chunk_chars,
            close_trailing: true,
            force_close: true,
            ..ReadOptions::default()
        }
    }

    /// The chunk texts Task 3's renderer makes of a session (default options).
    fn render(steps: &[Step]) -> Vec<String> {
        render_with(steps, None, &opts(2000))
    }

    fn render_with(steps: &[Step], agent: Option<&str>, opts: &ReadOptions) -> Vec<String> {
        render_bytes(&jsonl(steps, agent), opts)
    }

    fn render_bytes(input: &[u8], opts: &ReadOptions) -> Vec<String> {
        read(AgentKind::ClaudeCode, input, opts)
            .turns
            .into_iter()
            .flat_map(|t| t.chunks)
            .map(|c| c.text)
            .collect()
    }

    /// A run over main chunks (ids `m<ord>`) and one subagent source's chunks (ids `s<ord>`),
    /// with no turn-start flags (the text rule).
    fn run(main: Vec<String>, subs: Vec<String>, closes_session: bool) -> RunInput {
        let mut chunks: Vec<RunChunk> = main
            .into_iter()
            .enumerate()
            .map(|(i, text)| RunChunk {
                id: format!("m{i}"),
                ord: i as i64,
                text,
                subagent: false,
                turn_start: None,
            })
            .collect();
        chunks.extend(subs.into_iter().enumerate().map(|(i, text)| RunChunk {
            id: format!("s{i}"),
            ord: i as i64,
            text,
            subagent: true,
            turn_start: None,
        }));
        RunInput {
            uri: URI.into(),
            project: "alice".into(),
            chunks,
            closes_session,
        }
    }

    /// Chunk texts with exact turn-start flags: the first chunk of each turn the reader found.
    fn render_flagged(
        steps: &[Step],
        agent: Option<&str>,
        opts: &ReadOptions,
    ) -> Vec<(String, bool)> {
        read(AgentKind::ClaudeCode, &jsonl(steps, agent), opts)
            .turns
            .into_iter()
            .flat_map(|t| {
                t.chunks
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| (c.text, i == 0))
            })
            .collect()
    }

    /// A run as Task 12 builds it: main chunks (ids `m<ord>`, ords from 0) and subagent sources
    /// one after another (ids `s<k>-<ord>`, ords from each source's first ord), the first chunk
    /// of each source a turn start. Without `flags` every flag is `None`.
    fn flagged_run(
        main: &[(String, bool)],
        subs: &[(i64, Vec<(String, bool)>)],
        flags: bool,
    ) -> RunInput {
        let flag = |start: bool| flags.then_some(start);
        let mut chunks: Vec<RunChunk> = main
            .iter()
            .enumerate()
            .map(|(i, (text, start))| RunChunk {
                id: format!("m{i}"),
                ord: i as i64,
                text: text.clone(),
                subagent: false,
                turn_start: flag(*start),
            })
            .collect();
        for (k, (first_ord, source)) in subs.iter().enumerate() {
            chunks.extend(source.iter().enumerate().map(|(i, (text, start))| {
                let ord = first_ord + i as i64;
                RunChunk {
                    id: format!("s{k}-{ord}"),
                    ord,
                    text: text.clone(),
                    subagent: true,
                    turn_start: flag(*start || i == 0),
                }
            }));
        }
        RunInput {
            uri: URI.into(),
            project: "alice".into(),
            chunks,
            closes_session: false,
        }
    }

    fn sources(p: &Prepared) -> Vec<CandidateSource> {
        p.candidates.iter().map(|c| c.source).collect()
    }

    fn of(p: &Prepared, source: CandidateSource) -> Vec<&Candidate> {
        p.candidates.iter().filter(|c| c.source == source).collect()
    }

    fn next(x: &mut u64) -> usize {
        *x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*x >> 33) as usize
    }

    /// `n` pseudo-words of random syllables, so generated passages share few trigrams.
    fn words(seed: u64, n: usize) -> String {
        const C: [&str; 20] = [
            "b", "d", "f", "g", "h", "j", "k", "l", "m", "n", "p", "r", "s", "t", "v", "w", "z",
            "ch", "sh", "th",
        ];
        const V: [&str; 7] = ["a", "e", "i", "o", "u", "ai", "ou"];
        const F: [&str; 7] = ["", "", "n", "r", "s", "l", "x"];
        let mut x = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let syllables = 2 + next(&mut x) % 2;
            let mut w = String::new();
            for _ in 0..syllables {
                w.push_str(C[next(&mut x) % C.len()]);
                w.push_str(V[next(&mut x) % V.len()]);
                w.push_str(F[next(&mut x) % F.len()]);
            }
            out.push(w);
        }
        out.join(" ")
    }

    /// `n` Thai pseudo-words (consonants with vowel and tone marks), joined by `sep`.
    fn thai(seed: u64, n: usize, sep: &str) -> String {
        const C: [&str; 16] = [
            "ก", "ข", "ค", "ง", "จ", "ช", "ด", "ต", "ท", "น", "บ", "ป", "พ", "ม", "ร", "ส",
        ];
        const M: [&str; 8] = ["", "ั", "ิ", "ี", "ุ", "่", "้", "ำ"];
        const F: [&str; 6] = ["", "", "น", "ง", "ม", "ก"];
        let mut x = seed.wrapping_add(0x5851_f42d_4c95_7f2d);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let syllables = 1 + next(&mut x) % 3;
            let mut w = String::new();
            for _ in 0..syllables {
                w.push_str(C[next(&mut x) % C.len()]);
                w.push_str(M[next(&mut x) % M.len()]);
                w.push_str(F[next(&mut x) % F.len()]);
            }
            out.push(w);
        }
        out.join(sep)
    }

    fn answer(id: &str, p: f64) -> Distribution {
        Distribution::from_pairs([(id.to_owned(), p), ("ephemeral".to_owned(), 1.0 - p)])
    }

    fn all_answer(p: &Prepared, id: &str, prob: f64) -> BTreeMap<String, Distribution> {
        p.candidates
            .iter()
            .map(|c| (c.qid.clone(), answer(id, prob)))
            .collect()
    }

    /// A run of `n` turns, each a short prompt (no statement) and a distinct conclusion.
    fn conclusions_run(n: usize, closes_session: bool) -> Prepared {
        let prompts: Vec<String> = (0..n).map(|i| format!("Go {i}.")).collect();
        let replies: Vec<String> = (0..n)
            .map(|i| format!("Reply {i}: {}.", words(400 + i as u64, 16)))
            .collect();
        let steps: Vec<Step> = (0..n)
            .flat_map(|i| [User(&prompts[i]), Say(&replies[i])])
            .collect();
        prepare(&run(render(&steps), vec![], closes_session))
    }

    #[test]
    fn candidates_come_in_priority_order() {
        let summary = "Summary of the earlier work:\n\n## Key decisions\n- We chose SQLite WAL mode for the store, because readers must never block the single writer thread.\n- The embedder runs on its own thread, so a slow model never stalls the request path of the daemon.\n\n## Errors and fixes\n- The lease test was flaky: the renewal raced the real clock, so the test now injects a fake clock and the race is gone.";
        let main = render(&[
            Compact(summary),
            User("Please make the ingest batch size configurable through the settings file."),
            Say("Let me look at the writer first."),
            Bash(
                "rg INGEST_SLICE",
                "src/writer.rs:12: const INGEST_SLICE: usize = 50;",
            ),
            Say(
                "The batch size is now read from `ingest.batch_size` in the settings file, with the old constant as its default. Values outside 1 to 500 are refused with a message that names the setting.",
            ),
            User("Now survey the capture module and report what it does in detail."),
            Agent(
                "Survey capture",
                "Report: the capture module reads transcripts incrementally, renders each turn as tagged text and cuts it into chunks of at most two thousand characters.",
            ),
            Say(
                "The survey is done. Capture keeps one cursor per transcript file and never stores a turn twice, because each turn key is recorded with its chunks.",
            ),
        ]);
        assert!(main[0].starts_with("[summary] "), "{main:#?}");
        let p = prepare(&run(main, vec![], false));
        assert_eq!(
            sources(&p),
            [
                S::Summary,
                S::Summary,
                S::Conclusion,
                S::Conclusion,
                S::UserStatement,
                S::UserStatement,
                S::SubagentReport
            ],
            "{:#?}",
            p.candidates
        );
        let texts: Vec<&str> = p.candidates.iter().map(|c| c.text.as_str()).collect();
        // The summary is cut at its headings, each heading keeping its whole bullet group.
        assert!(texts[0].starts_with("## Key decisions\n- We chose SQLite WAL"));
        assert!(texts[0].ends_with("\n- The embedder runs on its own thread, so a slow model never stalls the request path of the daemon."));
        assert!(texts[1].starts_with("## Errors and fixes\n- The lease test was flaky"));
        // A turn's conclusion is its last assistant block, not the narration before a tool.
        assert!(texts[2].starts_with("The batch size is now read"));
        assert!(texts[3].starts_with("The survey is done."));
        assert!(!texts.iter().any(|t| t.contains("look at the writer first")));
        assert!(texts[4].starts_with("Please make the ingest batch size"));
        assert!(texts[5].starts_with("Now survey the capture module"));
        assert!(texts[6].starts_with("Report: the capture module reads transcripts"));
        // Ids, questions and prompts line up.
        assert_eq!(p.questions.len(), p.candidates.len());
        for (i, (c, q)) in p.candidates.iter().zip(&p.questions).enumerate() {
            assert_eq!(c.qid, format!("c{}", i + 1));
            assert_eq!(q.id, c.qid);
        }
        assert_eq!(
            p.candidates[2].prompt.as_deref(),
            Some("Please make the ingest batch size configurable through the settings file.")
        );
        assert_eq!(
            p.candidates[6].prompt.as_deref(),
            Some("Now survey the capture module and report what it does in detail.")
        );
        assert!(p.candidates[0].prompt.is_none() && p.candidates[4].prompt.is_none());
    }

    #[test]
    fn conclusions_are_split_into_passages_within_bounds() {
        let long_para = (0..30)
            .map(|i| format!("{}.", words(i, 12)))
            .collect::<Vec<_>>()
            .join(" ");
        // Eleven lines of code go; ten stay.
        let long_code = (0..11)
            .map(|i| format!("    let line_{i} = compute({i});"))
            .collect::<Vec<_>>()
            .join("\n");
        let short_code = (0..9)
            .map(|i| format!("cargo test -p alice-core step_{i}"))
            .chain(["cargo fmt --all --check".to_owned()])
            .collect::<Vec<_>>()
            .join("\n");
        let reply = format!(
            "Here is what changed.\n\n{long_para}\n\nThe new loop:\n\n```rust\n{long_code}\n```\n\nRun these to check it:\n\n```sh\n{short_code}\n```\n\nShort note.\n\n- {} keeps the renewal window apart from the grace period.\n- Second bullet.\n- {} is injected into the lease manager by every test.",
            words(100, 6),
            words(101, 6)
        );
        let main = render(&[User("Explain the change to the lease loop."), Say(&reply)]);
        assert!(
            main.iter().any(|c| c.starts_with("[assistant] (cont.) ")),
            "the reply spans chunks: {main:#?}"
        );
        let p = prepare(&run(main, vec![], false));
        let conclusions = of(&p, S::Conclusion);
        assert!(conclusions.len() >= 4, "{conclusions:#?}");
        for c in &conclusions {
            let n = c.text.chars().count();
            assert!((PASSAGE_MIN..=PASSAGE_MAX).contains(&n), "{n}: {}", c.text);
        }
        let all = conclusions
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("[code]") && !all.contains("let line_"),
            "{all}"
        );
        assert!(
            all.contains(&format!("```sh\n{short_code}\n```")),
            "a code block of 10 lines or fewer stays"
        );
        for i in 0..30 {
            assert!(all.contains(&words(i, 12)), "sentence {i} is kept");
        }
        // The short pieces join their neighbours instead of being lost.
        let joined = |a: &str, b: &str| {
            conclusions
                .iter()
                .any(|c| c.text.contains(a) && c.text.contains(b))
        };
        assert!(joined("Short note.", &words(100, 6)), "{all}");
        assert!(joined("- Second bullet.", &words(101, 6)), "{all}");
    }

    /// Task 13 tuning: a heading line keeps its whole bullet group in one passage, so a handoff
    /// holds what is done and what comes next together. A heading line is a heading, or one line
    /// of at most 80 characters that does not end in `. ! ?` or their full-width forms (with or
    /// without a colon; a Thai line has no sentence mark); a sentence before a list does not head
    /// it.
    #[test]
    fn a_heading_line_keeps_its_bullet_group_together() {
        let [a, b, c, d, e] = [500, 501, 502, 503, 504].map(|s| words(s, 12));
        let conclusions = |reply: &str| -> Vec<String> {
            let main = render(&[User("Wrap up the work for today."), Say(reply)]);
            of(&prepare(&run(main, vec![], true)), S::Conclusion)
                .iter()
                .map(|c| c.text.clone())
                .collect()
        };
        let texts = conclusions(&format!(
            "Status for the next session\n- Done: {a}.\n- Next: {b}.\n- Open: {c}.\n\nThat is all for the list.\n- {d}.\n- {e}."
        ));
        assert!(
            texts.contains(&format!(
                "Status for the next session\n- Done: {a}.\n- Next: {b}.\n- Open: {c}."
            )),
            "{texts:#?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains(&d) && t.contains(&e)),
            "a sentence does not head the list after it: {texts:#?}"
        );

        // A Thai line has no sentence mark: it heads the list after it.
        let thai = "งานที่ค้างอยู่ตอนนี้";
        let texts = conclusions(&format!("{thai}\n- {a}.\n- {b}."));
        assert!(
            texts.contains(&format!("{thai}\n- {a}.\n- {b}.")),
            "{texts:#?}"
        );

        // The bound: a line of 80 characters heads its list, one of 81 does not.
        for (n, leads) in [(80, true), (81, false)] {
            let line = format!("Heading {}", "x".repeat(n - 8));
            assert_eq!(line.chars().count(), n);
            let texts = conclusions(&format!("{line}\n- {a}.\n- {b}."));
            assert_eq!(
                texts.iter().any(|t| t.contains(&line) && t.contains(&a)),
                leads,
                "a line of {n} characters: {texts:#?}"
            );
            if leads {
                assert!(
                    texts.contains(&format!("{line}\n- {a}.\n- {b}.")),
                    "{texts:#?}"
                );
            }
        }

        let summary = format!("## Pending tasks\n- {a}.\n- {b}.\n\n## Current work\n{c}.");
        let p = prepare(&run(render(&[Compact(&summary)]), vec![], false));
        let texts: Vec<&str> = of(&p, S::Summary).iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                format!("## Pending tasks\n- {a}.\n- {b}."),
                format!("## Current work\n{c}.")
            ],
            "a heading leads its bullet group, or the paragraph after it"
        );
    }

    #[test]
    fn already_saved_lines_feed_the_state_and_filter_candidates() {
        let mut main = render_bytes(TOOLS, &opts(2000));
        assert!(
            main.iter()
                .any(|c| c.contains("[tool alice remember] Use pnpm, not npm"))
        );
        main.extend(render(&[
            User("Use pnpm, not npm please"),
            Say("Noted."),
            User("Keep the changelog in English and Thai, and update both in the same commit."),
            Say("Understood."),
        ]));
        let p = prepare(&run(main, vec![], false));
        assert_eq!(p.already_saved, ["Use pnpm, not npm"]);
        assert!(
            p.state
                .contains("Already saved in this session:\n- Use pnpm, not npm"),
            "{}",
            p.state
        );
        assert!(overlap("Use pnpm, not npm please", "Use pnpm, not npm") >= OVERLAP);
        let statements: Vec<&str> = of(&p, S::UserStatement)
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            !statements.iter().any(|t| t.contains("pnpm")),
            "{statements:?}"
        );
        assert!(statements.contains(
            &"Keep the changelog in English and Thai, and update both in the same commit."
        ));
        assert!(
            !p.questions
                .iter()
                .any(|q| q.instructions.contains("not npm please"))
        );
        // A search is not a save.
        assert!(
            !p.already_saved
                .iter()
                .any(|s| s.contains("package manager choice"))
        );
    }

    #[test]
    fn withheld_and_tool_only_text_is_never_a_candidate() {
        // privacy.redact_mode = block: the daemon replaces a chunk that held a secret by its
        // first tag and the detectors.
        let withhold = |chunks: Vec<String>, tag: &str| -> Vec<String> {
            chunks
                .into_iter()
                .map(|c| {
                    if c.starts_with(tag) {
                        format!("{tag} [withheld: github_token]")
                    } else {
                        c
                    }
                })
                .collect()
        };
        let mut main = withhold(
            render_with(
                &[
                    User("Deploy the service with the new token, then tell me what happened."),
                    Say(
                        "The deploy finished and the service answered its health check on the first try after the token was rotated, so nothing else needs to change in the deploy script or in the token rotation job.",
                    ),
                ],
                None,
                &opts(200),
            ),
            "[assistant]",
        );
        main.extend(withhold(
            render(&[
                User("Here is the key I want you to use for the staging deploy today."),
                Say(
                    "The staging deploy now reads the key from the environment, and the key itself is never written to the repository or to the logs.",
                ),
            ]),
            "[user]",
        ));
        let build_log = "error[E0425]: cannot find value `lease_deadline` in this scope, see the renewal module for where it moved\n".repeat(4);
        main.extend(render(&[
            User("Run the build and show me the errors."),
            Bash("cargo build", &build_log),
        ]));
        main.extend(render(&[
            User("Clean the build directory and rebuild everything from scratch."),
            Command("/review", "src/lib.rs"),
            Say(
                "[tool Bash] rm -rf build\n[result] removed 120 files from the build directory and from the cache\n[tool Bash] cargo build --release --locked --all-targets",
            ),
        ]));
        main.extend(render_with(
            &[
                User("What is left to do on the lease work?"),
                Say(
                    "Two things are left: the renewal metric and the docs page that explains the lease settings to operators.",
                ),
                Bash("git status --short", "M src/lease.rs"),
                Think(
                    "The benchmark numbers are not ready, so I will not mention them now and will wait for the next run.",
                ),
            ],
            None,
            &ReadOptions {
                include_thinking: true,
                ..opts(2000)
            },
        ));
        main.extend(render(&[
            User("Why did the lease test fail?"),
            Say(
                "The lease test failed because the renewal deadline came from the wall clock while the test moved a fake clock forward.",
            ),
        ]));
        assert!(
            main.iter()
                .any(|c| c == "[assistant] [withheld: github_token]")
        );
        assert!(main.iter().any(|c| c == "[user] [withheld: github_token]"));
        assert!(
            main.iter()
                .any(|c| c.contains("\n[user] /review src/lib.rs"))
        );
        assert!(main.iter().any(|c| c.contains("[assistant] (thinking) ")));

        let p = prepare(&run(main, vec![], false));
        for c in &p.candidates {
            for never in [
                "[withheld:",
                "health check",
                "staging deploy",
                "error[E0425]",
                "rm -rf",
                "cargo build",
                "/review",
                "benchmark numbers",
            ] {
                assert!(!c.text.contains(never), "{never}: {}", c.text);
            }
        }
        assert!(!p.state.contains("withheld") && !p.state.contains("health check"));
        assert!(!p.state.contains("benchmark numbers"));
        let conclusions: Vec<&str> = of(&p, S::Conclusion)
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(conclusions.len(), 2, "{conclusions:#?}");
        assert!(conclusions[0].starts_with("Two things are left"));
        assert!(conclusions[1].starts_with("The lease test failed"));
    }

    #[test]
    fn at_most_forty_questions_and_a_bounded_state() {
        let prompts: Vec<String> = (0..100)
            .map(|i| format!("Turn {i}: {}", words(i, 6)))
            .collect();
        let replies: Vec<String> = (0..100)
            .map(|i| format!("{}.", words(1_000 + i, 90)))
            .collect();
        let steps: Vec<Step> = (0..100)
            .flat_map(|i| [User(&prompts[i]), Say(&replies[i])])
            .collect();
        let main = render(&steps);
        assert_eq!(main.len(), 100, "one chunk per turn");
        let p = prepare(&run(main, vec![], false));
        assert_eq!(p.questions.len(), MAX_CANDIDATES);
        assert_eq!(p.candidates.len(), MAX_CANDIDATES);
        let n = p.state.chars().count();
        assert!(n <= STATE_CHARS && n > STATE_CHARS - 1_000, "{n}");
        assert!(p.state.starts_with("Project: alice\n"), "{}", p.state);
        // The run covers the oldest 60 turns; their newest conclusions are asked about.
        assert_eq!(p.consumed_ord, RUN_MAX_TURNS as i64 - 1);
        assert!(p.candidates.iter().all(|c| c.source == S::Conclusion));
        assert_eq!(p.candidates[0].text, replies[20]);
        assert_eq!(p.candidates[39].text, replies[59]);
        // The state keeps the newest turns.
        assert!(p.state.contains(&prompts[59]) && !p.state.contains(&prompts[0]));
        assert!(!p.state.contains(&prompts[60]));
    }

    #[test]
    fn handoff_is_offered_only_at_the_end_of_a_closing_run() {
        let prompts: Vec<String> = (0..4)
            .map(|i| format!("Step {i}: {}", words(200 + i, 5)))
            .collect();
        let replies: Vec<String> = (0..4)
            .map(|i| format!("Step {i} done: {}.", words(300 + i, 20)))
            .collect();
        let steps: Vec<Step> = (0..4)
            .flat_map(|i| [User(&prompts[i]), Say(&replies[i])])
            .collect();
        let main = render(&steps);
        let offered = |q: &Question| q.options.iter().any(|o| o.id == "handoff");

        let closing = prepare(&run(main.clone(), vec![], true));
        assert_eq!(closing.candidates.len(), 8);
        for (c, q) in closing.candidates.iter().zip(&closing.questions) {
            let late = c.text.starts_with("Step 2") || c.text.starts_with("Step 3");
            assert_eq!(c.handoff_ok, late, "{}", c.text);
            assert_eq!(offered(q), late);
            assert_eq!(q.options.len(), if late { 11 } else { 10 });
        }

        let open = prepare(&run(main, vec![], false));
        assert_eq!(open.candidates.len(), 8);
        assert!(open.candidates.iter().all(|c| !c.handoff_ok));
        assert!(!open.questions.iter().any(offered));
    }

    #[test]
    fn decide_keeps_confident_knowledge_up_to_the_cap() {
        let p = conclusions_run(16, false);
        assert_eq!(p.candidates.len(), 16);
        assert!(p.candidates.iter().all(|c| c.source == S::Conclusion));
        let cfg = DistillConfig::default();
        assert_eq!((cfg.min_confidence, cfg.max_per_run), (0.7, 8));

        let mut answers = BTreeMap::new();
        for i in 0..12 {
            answers.insert(
                format!("c{}", i + 1),
                answer("decision", 0.80 + 0.01 * i as f64),
            );
        }
        answers.insert("c13".into(), answer("ephemeral", 0.95));
        answers.insert("c14".into(), answer("unclear", 0.9));
        answers.insert("c15".into(), answer("already_saved", 0.99));
        answers.insert("c16".into(), answer("fact", 0.65));
        let kept = decide(&p, &answers, URI, &cfg);
        assert_eq!(kept.len(), 8);
        assert!(
            kept.iter()
                .all(|m| m.kind == Kind::Decision && !m.to_global)
        );
        // The eight most probable (c5 to c12), in session order.
        let got: Vec<&str> = kept.iter().map(|m| m.body.as_str()).collect();
        let want: Vec<&str> = p.candidates[4..12]
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(got, want);
        for (m, i) in kept.iter().zip(4..12) {
            assert!((m.p - (0.80 + 0.01 * f64::from(i))).abs() < 1e-9, "{}", m.p);
        }

        // Equal answers: session order breaks the tie.
        let tied: BTreeMap<String, Distribution> = (1..=12)
            .map(|i| (format!("c{i}"), answer("decision", 0.9)))
            .collect();
        let got: Vec<String> = decide(&p, &tied, URI, &cfg)
            .into_iter()
            .map(|m| m.body)
            .collect();
        let want: Vec<String> = p.candidates[..8].iter().map(|c| c.text.clone()).collect();
        assert_eq!(got, want);

        // A fact at 0.65 is dropped below 0.7 even with room under the cap, and kept below 0.6.
        let few = BTreeMap::from([
            ("c1".to_owned(), answer("decision", 0.9)),
            ("c16".to_owned(), answer("fact", 0.65)),
        ]);
        let kept = decide(&p, &few, URI, &cfg);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, Kind::Decision);
        let low = DistillConfig {
            min_confidence: 0.6,
            max_per_run: 8,
        };
        let kept = decide(&p, &few, URI, &low);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].kind, Kind::Fact);
    }

    #[test]
    fn preference_user_goes_to_global_and_one_handoff_at_most() {
        let p = conclusions_run(5, true);
        let ok: Vec<bool> = p.candidates.iter().map(|c| c.handoff_ok).collect();
        assert_eq!(ok, [false, false, false, true, true]);
        let answers = BTreeMap::from([
            ("c1".to_owned(), answer("preference_user", 0.9)),
            ("c2".to_owned(), answer("preference_project", 0.85)),
            // Not offered for this passage: dropped.
            ("c3".to_owned(), answer("handoff", 0.97)),
            ("c4".to_owned(), answer("handoff", 0.8)),
            ("c5".to_owned(), answer("handoff", 0.9)),
        ]);
        let kept = decide(&p, &answers, URI, &DistillConfig::default());
        let got: Vec<(Kind, bool, &str)> = kept
            .iter()
            .map(|m| (m.kind, m.to_global, m.body.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Kind::Preference, true, p.candidates[0].text.as_str()),
                (Kind::Preference, false, p.candidates[1].text.as_str()),
                (Kind::Handoff, false, p.candidates[4].text.as_str()),
            ]
        );
    }

    #[test]
    fn bodies_titles_and_keys_follow_the_spec() {
        let reply = "The lease test failed now and then because the renewal deadline was computed from the wall clock while the test advanced a fake clock, so the two drifted apart under load. The lease manager now takes the clock from its caller.";
        let report = "Report: the lease is renewed every ten seconds by its holder, and a lease that misses three renewals in a row is taken over by the next worker in line.";
        let main = render_with(
            &[
                User("please fix the flaky lease test"),
                Bash(
                    "cargo test -p alice-core lease",
                    "test lease::renews ... FAILED",
                ),
                Say(reply),
                User("fix the test"),
                Say(
                    "The second test is fixed as well: it waited on a channel that nobody closed, so it now closes the channel when the worker stops.",
                ),
                User("Write down how the lease works and report back to me."),
                Agent("Explain the lease", report),
            ],
            None,
            &opts(200),
        );
        assert!(main[0].starts_with("[user] please fix the flaky lease test"));
        assert!(main[1].starts_with("[assistant] The lease test failed"));
        assert!(main[2].starts_with("[assistant] (cont.) The lease manager"));
        assert!(main[4].starts_with("[user] Write down how the lease works"));
        assert!(main[5].starts_with("[subagent general-purpose] Report: the lease"));
        let p = prepare(&run(main, vec![], false));
        let cfg = DistillConfig {
            min_confidence: 0.7,
            max_per_run: 50,
        };
        let answers = all_answer(&p, "decision", 0.9);
        let kept = decide(&p, &answers, URI, &cfg);
        assert_eq!(kept.len(), p.candidates.len());
        let find = |start: &str| -> (&Candidate, &DistilledMemory) {
            let c = p
                .candidates
                .iter()
                .find(|c| c.text.starts_with(start))
                .unwrap_or_else(|| panic!("{start}: {:#?}", p.candidates));
            let key = idempotency_key(URI, &c.text);
            (c, kept.iter().find(|m| m.idempotency_key == key).unwrap())
        };

        let (c, m) = find("The lease test failed");
        assert_eq!(c.source, S::Conclusion);
        assert_eq!(c.text, reply, "the renderer's cut is joined back");
        assert_eq!(
            m.body,
            format!("{reply}\n\nContext: please fix the flaky lease test")
        );
        assert!(m.title.chars().count() <= TITLE_MAX, "{}", m.title);
        assert!(m.title.chars().count() > 100, "{}", m.title);
        assert!(
            reply.starts_with(&m.title) && reply[m.title.len()..].starts_with(' '),
            "cut at a word boundary: {}",
            m.title
        );
        // Provenance: the reply's chunks and the prompt's, because the context line is used.
        assert_eq!(c.chunk_ids, ["m0", "m1", "m2"]);
        assert_eq!(m.chunk_ids, c.chunk_ids);
        let mut h = Sha256::new();
        h.update(URI.as_bytes());
        h.update([0x1f]);
        h.update(reply.as_bytes());
        let hex = hex::encode(h.finalize());
        assert_eq!(m.idempotency_key, format!("distill:{}", &hex[..32]));
        assert_eq!(m.idempotency_key.len(), "distill:".len() + 32);
        let again = decide(&p, &answers, URI, &cfg);
        assert_eq!(again, kept, "stable across calls");

        // A prompt under four words adds no context line; a statement never has one.
        let (_, m) = find("The second test is fixed");
        assert!(!m.body.contains("Context:"), "{}", m.body);
        let (c, m) = find("please fix the flaky lease test");
        assert_eq!(c.source, S::UserStatement);
        assert_eq!(m.body, "please fix the flaky lease test");
        assert_eq!(m.chunk_ids, ["m0"]);

        // A subagent report gets the context line too.
        let (c, m) = find("Report: the lease is renewed");
        assert_eq!(c.source, S::SubagentReport);
        assert_eq!(
            m.body,
            format!("{report}\n\nContext: Write down how the lease works and report back to me.")
        );
        assert_eq!(m.chunk_ids, ["m4", "m5"]);
        assert_eq!(
            m.title,
            "Report: the lease is renewed every ten seconds by its holder, and a lease that misses three renewals in a row is taken"
        );
    }

    #[test]
    fn thai_passages_are_cut_at_character_boundaries() {
        let spaced = thai(1, 330, " ");
        let solid = thai(2, 260, "");
        assert!(spaced.chars().count() > 1_300, "{}", spaced.chars().count());
        assert!(solid.chars().count() > 1_300 && !solid.contains(' '));
        let reply = format!("{spaced}\n\n{solid}");
        let prompt = "ช่วยอธิบายว่าทำไมการทดสอบสัญญาเช่าถึงล้มเหลวเป็นบางครั้ง";
        let short = thai(3, 400, " ")
            .chars()
            .take(700)
            .collect::<String>()
            .trim()
            .to_owned();
        let long: String = thai(4, 400, " ").chars().take(900).collect();
        assert!(
            short.len() > USER_MAX,
            "more bytes than the limit, fewer characters"
        );
        // The renderer cuts this one hard in front of a tone mark (after 1,988 characters).
        let head: String = thai(5, 700, "").chars().take(1_987).collect();
        assert_eq!(head.chars().count(), 1_987);
        let glued = format!("{head}ก\u{0E48}{}", thai(6, 150, ""));
        // A hard cut at 1,200 characters would put a mark first in the next piece.
        let marks = format!("ฮ{}{}", "ก\u{0E48}".repeat(600), thai(7, 60, ""));
        let resume = thai(8, 40, " ");
        assert!(is_mark(marks.chars().nth(PASSAGE_MAX).unwrap()));
        let main = render(&[
            User(prompt),
            Say(&reply),
            User(&short),
            Say("รับทราบ"),
            User(&long),
            Say("รับทราบ"),
            User("ข้อความยาวที่ไม่มีช่องว่างเลย"),
            Say(&glued),
            User("ข้อความที่มีวรรณยุกต์ทุกตัว"),
            Say(&marks),
            User("ทำต่อ"),
            Say(&resume),
        ]);
        assert!(
            main.iter().any(|c| c
                .strip_prefix("[assistant] (cont.) ")
                .is_some_and(|rest| rest.starts_with(is_mark))),
            "the renderer cut in front of a mark"
        );
        let p = prepare(&run(main, vec![], false));

        let conclusions = of(&p, S::Conclusion);
        assert!(conclusions.len() >= 5, "{conclusions:#?}");
        let flat = |s: &str| s.replace('\n', "");
        for c in &conclusions {
            let n = c.text.chars().count();
            assert!((PASSAGE_MIN..=PASSAGE_MAX).contains(&n), "{n}");
            assert!(
                [&reply, &glued, &marks, &resume]
                    .iter()
                    .any(|whole| flat(whole).contains(&flat(&c.text))),
                "a passage is a slice of a reply: {}",
                c.text
            );
            for line in c.text.lines() {
                assert!(
                    !line.starts_with(is_mark),
                    "a line starts with a mark: {line}"
                );
            }
        }
        assert!(
            conclusions.iter().any(|c| !c.text.contains(' ')
                && glued.contains(&c.text)
                && c.text.contains("ก\u{0E48}")),
            "the spaceless text is cut too"
        );
        // The renderer's cut is glued back: the passage over it is a plain slice.
        let over_cut = format!("{}ก\u{0E48}", &head[head.len() - 30..]);
        assert!(
            conclusions.iter().any(|c| c.text.contains(&over_cut)),
            "{conclusions:#?}"
        );
        let cut = conclusions
            .iter()
            .find(|c| c.text.starts_with('ฮ'))
            .unwrap();
        assert_eq!(cut.text.chars().count(), PASSAGE_MAX - 1);
        assert!(marks.starts_with(&cut.text));

        let kept = decide(
            &p,
            &all_answer(&p, "fact", 0.9),
            URI,
            &DistillConfig {
                min_confidence: 0.7,
                max_per_run: 50,
            },
        );
        assert_eq!(kept.len(), p.candidates.len());
        for c in &p.candidates {
            let key = idempotency_key(URI, &c.text);
            let m = kept.iter().find(|m| m.idempotency_key == key).unwrap();
            assert!(!m.title.is_empty() && m.title.chars().count() <= TITLE_MAX);
            assert!(c.text.starts_with(&m.title), "{}", m.title);
            assert!(
                !c.text[m.title.len()..].starts_with(is_mark),
                "the title does not end in front of a mark: {}",
                m.title
            );
            assert!(m.body.starts_with(&c.text));
            // An unspaced prompt of 12 or more code points counts as four words; ทำต่อ (five
            // code points) does not.
            if c.source == S::Conclusion && c.prompt.as_deref() == Some(prompt) {
                assert_eq!(m.body, format!("{}\n\nContext: {prompt}", c.text));
            }
            if c.prompt.as_deref() == Some("ทำต่อ") {
                assert_eq!(m.body, c.text, "no context line");
            }
        }
        assert!(
            p.candidates
                .iter()
                .any(|c| c.source == S::Conclusion && c.prompt.as_deref() == Some(prompt))
        );
        assert!(
            p.candidates
                .iter()
                .any(|c| c.prompt.as_deref() == Some("ทำต่อ"))
        );

        // Statements count characters, not bytes.
        let statements: Vec<&str> = of(&p, S::UserStatement)
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(statements.contains(&short.as_str()));
        assert!(statements.contains(&prompt));
        assert!(!statements.contains(&long.as_str()));
        assert!(
            p.questions
                .iter()
                .any(|q| q.instructions.contains(&format!("«{prompt}»")))
        );
    }

    #[test]
    fn questions_follow_the_spec_wording() {
        let ids: Vec<&str> = OPTIONS.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "decision",
                "bugfix",
                "howto",
                "warning",
                "preference_user",
                "preference_project",
                "fact",
                "handoff",
                "already_saved",
                "ephemeral",
                "unclear"
            ]
        );
        let described = |id: &str| OPTIONS.iter().find(|(i, _)| *i == id).unwrap().1;
        for (id, text) in [
            (
                "decision",
                "a choice that was made, with its reason, that should guide later work",
            ),
            (
                "bugfix",
                "a bug that was found: what went wrong, why, and how it was fixed",
            ),
            (
                "howto",
                "a procedure or command sequence that worked and can be reused",
            ),
            (
                "warning",
                "a pitfall, limit or constraint to avoid in future work",
            ),
            (
                "preference_user",
                "how the user wants things done in every project (style, language, tools, workflow)",
            ),
            ("preference_project", "a convention for this project only"),
            (
                "fact",
                "a lasting fact about the project, its systems or its environment",
            ),
            (
                "handoff",
                "where the work stands: what is done and what comes next",
            ),
            (
                "already_saved",
                "the same knowledge as an item listed under \"Already saved in this session\", even when worded differently or with more detail",
            ),
            (
                "ephemeral",
                "a question, a request or thanks to the assistant, or a pick among choices it offered, with no lasting rule or fact in it; progress narration; a plan for the next minutes; a greeting; or a result that matters only right now",
            ),
            (
                "unclear",
                "cannot be understood without the surrounding conversation",
            ),
        ] {
            assert_eq!(described(id), text);
        }

        let main = render(&[
            User("Why does the writer batch its slices?"),
            Say(
                "The writer batches at most fifty chunks per slice, so one big ingest never holds the write lock for long and remember stays fast.",
            ),
        ]);
        let p = prepare(&run(main.clone(), vec![], false));
        assert_eq!(sources(&p), [S::Conclusion, S::UserStatement]);
        assert_eq!(
            p.questions[0].instructions,
            format!(
                "Passage one from this session (the user had asked: «Why does the writer batch its slices?»): «{}». What lasting knowledge, if any, does it hold for future work?",
                p.candidates[0].text
            )
        );
        assert_eq!(
            p.questions[1].instructions,
            "Passage two from this session: «Why does the writer batch its slices?». What lasting knowledge, if any, does it hold for future work?"
        );
        let options: Vec<(&str, &str)> = p.questions[0]
            .options
            .iter()
            .map(|o| (o.id.as_str(), o.description.as_str()))
            .collect();
        let want: Vec<(&str, &str)> = OPTIONS
            .iter()
            .copied()
            .filter(|(id, _)| *id != "handoff")
            .collect();
        assert_eq!(options, want);

        // The same run gives the same request, byte for byte (Jev's cache keys on it).
        let again = prepare(&run(main, vec![], false));
        assert_eq!(again.state, p.state);
        assert_eq!(
            format!("{:?}", again.questions),
            format!("{:?}", p.questions)
        );
        assert_eq!(again.candidates, p.candidates);
    }

    #[test]
    fn a_long_backlog_is_cut_at_a_turn_boundary() {
        // RUN_MAX_CHARS: 50 turns of about 2,900 characters, each in two chunks.
        let prompts: Vec<String> = (0..50).map(|i| format!("Go {i}.")).collect();
        let replies: Vec<String> = (0..50)
            .map(|i| format!("{}.", words(500 + i, 380)))
            .collect();
        let steps: Vec<Step> = (0..50)
            .flat_map(|i| [User(&prompts[i]), Say(&replies[i])])
            .collect();
        let main = render(&steps);
        assert!(main.len() > 50, "turns span chunks");
        // The chunks of whole turns that fit in RUN_MAX_CHARS.
        let mut fit = 0;
        let mut used = 0;
        let mut turns = 0;
        while fit < main.len() {
            let end = (fit + 1..main.len())
                .find(|&k| main[k].starts_with("[user] "))
                .unwrap_or(main.len());
            let size: usize = main[fit..end].iter().map(|c| c.chars().count()).sum();
            if used + size > RUN_MAX_CHARS {
                break;
            }
            used += size;
            fit = end;
            turns += 1;
        }
        assert!(turns < RUN_MAX_TURNS && fit < main.len(), "{turns}");
        let p = prepare(&run(main, vec![], true));
        assert_eq!(p.consumed_ord, fit as i64 - 1);
        assert!(
            p.candidates.iter().all(|c| !c.handoff_ok),
            "a run cut short does not end the session"
        );

        // One turn larger than a run still moves the cursor, chunk by chunk.
        let huge = words(9, 20_000);
        let main = render(&[User("Go on."), Say(&huge), User("Next turn.")]);
        let n = main.len();
        let p = prepare(&run(main.clone(), vec![], false));
        let covered = usize::try_from(p.consumed_ord).unwrap() + 1;
        assert!(covered >= 1 && covered < n - 1, "{covered} of {n}");
        let size = |k: usize| -> usize { main[..k].iter().map(|c| c.chars().count()).sum() };
        assert!(size(covered) <= RUN_MAX_CHARS && size(covered + 1) > RUN_MAX_CHARS);

        // A run with no main chunk leaves the cursor where it is.
        let sub = render_with(
            &[User("Survey the writer."), Say(&replies[0])],
            Some("agent-1"),
            &opts(2000),
        );
        assert_eq!(prepare(&run(vec![], sub, true)).consumed_ord, -1);
    }

    #[test]
    fn a_subagent_source_gives_only_its_final_report() {
        let report = "Report: the writer holds one connection, batches at most fifty chunks per slice and never makes a network call, so a slow embedder cannot stall it.";
        let main = render(&[
            User("Find out how the writer batches its work."),
            Agent("Survey the writer", report),
            Say("Done."),
        ]);
        let sub = render_with(
            &[
                User(
                    "Survey the writer module of the daemon and report how it batches its writes.",
                ),
                Say("I will read the writer first and then look at how the slices are cut."),
                Bash(
                    "rg INGEST_SLICE crates",
                    "crates/alice-daemon/src/writer.rs:40",
                ),
                Say(report),
            ],
            Some("agent-1"),
            &opts(2000),
        );
        let main_len = main.len();
        let p = prepare(&run(main, sub, false));
        let reports = of(&p, S::SubagentReport);
        assert_eq!(
            reports.len(),
            1,
            "the parent's copy and the subagent's own final text are one candidate: {reports:#?}"
        );
        assert_eq!(reports[0].text, report);
        assert_eq!(
            reports[0].prompt.as_deref(),
            Some("Find out how the writer batches its work."),
            "the main session's copy wins"
        );
        assert!(reports[0].chunk_ids.iter().all(|id| id.starts_with('m')));
        assert!(
            !p.candidates
                .iter()
                .any(|c| c.text.contains("Survey the writer module")),
            "a subagent's prompt is not the user's statement"
        );
        assert!(
            !p.candidates
                .iter()
                .any(|c| c.text.contains("read the writer first"))
        );
        assert!(!p.state.contains("Survey the writer module"));
        assert_eq!(p.consumed_ord, main_len as i64 - 1);
    }

    #[test]
    fn a_command_line_never_starts_a_turn() {
        let first = render(&[
            User("Go."),
            Say(
                "The first part is done: the renewal loop now runs on its own thread and never blocks the writer.",
            ),
        ]);
        let prompt = "Now finish the second part of the lease work, the metric and the docs page, and tell me when both are done.";
        let second = render_with(
            &[
                User(prompt),
                Command("/review", "src/lease.rs"),
                Say(
                    "Both are done: the lease metric counts renewals per minute and the docs page explains every lease setting.",
                ),
            ],
            None,
            &opts(128),
        );
        assert!(
            second
                .iter()
                .any(|c| c.starts_with("[user] /review src/lease.rs")),
            "the command line starts a chunk: {second:#?}"
        );
        let p = prepare(&run([first, second].concat(), vec![], true));
        let conclusions = of(&p, S::Conclusion);
        assert_eq!(conclusions.len(), 2, "{conclusions:#?}");
        assert!(
            conclusions.iter().all(|c| c.handoff_ok),
            "two turns, both among the last two"
        );
        assert_eq!(conclusions[1].prompt.as_deref(), Some(prompt));
    }

    #[test]
    fn trim_markers_and_neutralised_lines_read_as_written() {
        let report = format!(
            "Report: {}.\n\nSee the links below:\n[the docs](https://example.com/docs) explain the renewal window in more detail than this report can.\n\n{}.",
            words(600, 40),
            words(601, 400)
        );
        let main = render(&[
            User("Survey the lease module and report back."),
            Agent("Survey the lease", &report),
            Say("Done."),
        ]);
        assert!(
            main.iter()
                .any(|c| c.contains("\n [the docs](https://example.com/docs)"))
        );
        assert!(main.iter().any(|c| c.contains(" characters trimmed …]")));
        let p = prepare(&run(main, vec![], false));
        let reports = of(&p, S::SubagentReport);
        assert!(
            reports.iter().any(|c| c
                .text
                .starts_with("See the links below:\n[the docs](https://example.com/docs) explain")),
            "{reports:#?}"
        );
        assert!(
            reports
                .iter()
                .all(|c| !c.text.contains("characters trimmed"))
        );
        assert!(
            reports.iter().any(|c| c.text.ends_with("\n…")),
            "the cut report ends with an ellipsis: {reports:#?}"
        );
    }

    #[test]
    fn explicit_turn_starts_fix_what_the_text_rule_misreads() {
        let conclusions_of = |p: &Prepared| -> Vec<(String, Option<String>)> {
            of(p, S::Conclusion)
                .iter()
                .map(|c| (c.text.clone(), c.prompt.clone()))
                .collect()
        };

        // 1. A withheld continuation of the prompt: the stub has no `(cont.)`.
        let first = "Please fix the flaky lease test in the renewal module.";
        let prompt = format!("{first} {}. {}", words(700, 14), words(701, 10));
        let reply = format!("The lease test is fixed: {}.", words(702, 15));
        let mut chunks = render_flagged(&[User(&prompt), Say(&reply)], None, &opts(200));
        assert!(chunks[0].1 && chunks[0].0.starts_with("[user] Please fix"));
        assert!(
            !chunks[1].1 && chunks[1].0.starts_with("[user] (cont.) "),
            "{chunks:#?}"
        );
        assert!(
            chunks[2..]
                .iter()
                .all(|c| !c.1 && c.0.starts_with("[assistant]"))
        );
        // A run that starts in the middle of the prompt has no prompt to quote; today's rule
        // quotes the fragment.
        let fragment = prompt_sentence(&words(701, 10)).unwrap();
        let mid = prepare(&flagged_run(&chunks[1..], &[], true));
        assert_eq!(conclusions_of(&mid), [(reply.clone(), None)]);
        let mid = prepare(&flagged_run(&chunks[1..], &[], false));
        assert_eq!(conclusions_of(&mid), [(reply.clone(), Some(fragment))]);
        chunks[1].0 = "[user] [withheld: github_token]".into();
        let fixed = prepare(&flagged_run(&chunks, &[], true));
        assert_eq!(
            conclusions_of(&fixed),
            [(reply.clone(), Some(first.into()))]
        );
        // Without flags (today's rule) the stub reads as a turn with no prompt.
        let guessed = prepare(&flagged_run(&chunks, &[], false));
        assert_eq!(conclusions_of(&guessed), [(reply.clone(), None)]);

        // 2. A typed prompt that looks like a command.
        let check = "Check the writer first, please.";
        let first_reply = format!("The writer is fine: {}.", words(703, 14));
        let typed = "/health returns 503 after the deploy, find out why it fails.";
        let second_reply = format!("The health route failed because {}.", words(704, 14));
        let chunks = render_flagged(
            &[
                User(check),
                Say(&first_reply),
                User(typed),
                Say(&second_reply),
            ],
            None,
            &opts(2000),
        );
        assert_eq!(
            chunks.iter().filter(|c| c.1).count(),
            2,
            "the reader sees two turns"
        );
        assert!(chunks[1].0.starts_with("[user] /health returns 503"));
        let fixed = prepare(&flagged_run(&chunks, &[], true));
        assert_eq!(
            conclusions_of(&fixed),
            [
                (first_reply.clone(), Some(check.into())),
                (second_reply.clone(), Some(typed.into()))
            ]
        );
        assert!(of(&fixed, S::UserStatement).iter().any(|c| c.text == typed));
        let guessed = prepare(&flagged_run(&chunks, &[], false));
        assert_eq!(
            conclusions_of(&guessed),
            [(second_reply.clone(), Some(check.into()))],
            "today's rule reads the two turns as one"
        );

        // 3. Two subagent sources; B's run starts in the middle of its turn, at a continuation.
        let report_a = format!("Report A: {}.", words(705, 16));
        let a = render_flagged(
            &[User("Survey the writer and report back."), Say(&report_a)],
            Some("agent-a"),
            &opts(2000),
        );
        let narration = format!("I will read the module first: {}.", words(706, 40));
        let report_b = format!("Report B: {}.", words(707, 16));
        let b_all = render_flagged(
            &[
                User("Survey the reader and report back."),
                Say(&narration),
                Bash("rg Reader", "src/reader.rs:1"),
                Say(&report_b),
            ],
            Some("agent-b"),
            &opts(200),
        );
        let from = b_all
            .iter()
            .position(|c| c.0.starts_with("[assistant] (cont.) "))
            .unwrap();
        let b = b_all[from..].to_vec();
        let main = render_flagged(
            &[
                User("Survey both modules with two agents."),
                Say("Both surveys are back."),
            ],
            None,
            &opts(2000),
        );
        let subs = [(0, a), (100 + from as i64, b)];
        let reports = |p: &Prepared| -> Vec<String> {
            of(p, S::SubagentReport)
                .iter()
                .map(|c| c.text.clone())
                .collect()
        };
        let fixed = prepare(&flagged_run(&main, &subs, true));
        assert_eq!(reports(&fixed), [report_a.clone(), report_b.clone()]);
        assert!(
            !fixed
                .candidates
                .iter()
                .any(|c| c.text.contains("read the module first")),
            "B's continuation is not glued into A's report"
        );
        let guessed = prepare(&flagged_run(&main, &subs, false));
        assert_eq!(
            reports(&guessed),
            [report_b],
            "today's rule lets A's turn take in B's text"
        );

        // Where the text rule is right, exact flags change nothing.
        const TURNS: &[u8] = include_bytes!("../../tests/fixtures/capture/claude/turns.jsonl");
        for input in [TOOLS, TURNS] {
            let chunks: Vec<(String, bool)> = read(AgentKind::ClaudeCode, input, &opts(2000))
                .turns
                .into_iter()
                .flat_map(|t| {
                    t.chunks
                        .into_iter()
                        .enumerate()
                        .map(|(i, c)| (c.text, i == 0))
                })
                .collect();
            let with = prepare(&flagged_run(&chunks, &[], true));
            let without = prepare(&flagged_run(&chunks, &[], false));
            assert_eq!(with.candidates, without.candidates);
            assert_eq!(with.state, without.state);
            assert_eq!(with.consumed_ord, without.consumed_ord);
        }

        // `covered` counts turns by the flags too.
        let steps: Vec<RunChunk> = (0..=RUN_MAX_TURNS)
            .map(|i| RunChunk {
                id: format!("m{i}"),
                ord: i as i64,
                text: format!("[assistant] Step {i} is done."),
                subagent: false,
                turn_start: Some(true),
            })
            .collect();
        let refs: Vec<&RunChunk> = steps.iter().collect();
        assert_eq!(covered(&refs), RUN_MAX_TURNS);
        let unflagged: Vec<RunChunk> = steps
            .iter()
            .cloned()
            .map(|c| RunChunk {
                turn_start: None,
                ..c
            })
            .collect();
        let refs: Vec<&RunChunk> = unflagged.iter().collect();
        assert_eq!(covered(&refs), RUN_MAX_TURNS + 1, "one guessed turn");
    }

    #[test]
    fn titles_skip_code_fences_and_prompts_drop_list_markers() {
        assert_eq!(
            title_of(
                "```sh\ncargo test -p alice-core --lib\n```\nThese two commands are what CI runs before every push."
            ),
            "These two commands are what CI runs before every push."
        );
        assert_eq!(
            title_of("Run these to check it:\n\n```sh\ncargo test -p alice-core\n```"),
            "Run these to check it:"
        );
        assert_eq!(
            title_of("```sh\ncargo test -p alice-core --lib\ncargo fmt --all --check\n```"),
            "cargo test -p alice-core --lib",
            "a passage of code alone is titled by its first line"
        );
        for p in [
            "1. Fix the lease test in the renewal module. 2. Then update the docs.",
            "- Fix the lease test in the renewal module. And more.",
            "# Fix the lease test in the renewal module. Thanks",
        ] {
            assert_eq!(
                prompt_sentence(p).as_deref(),
                Some("Fix the lease test in the renewal module."),
                "{p}"
            );
        }

        let reply = format!(
            "```sh\ncargo test -p alice-core lease\n```\nThe lease test passes again: {}.",
            words(800, 12)
        );
        let main = render(&[
            User("1. Fix the lease test in the renewal module. 2. Then update the docs page."),
            Say(&reply),
        ]);
        let p = prepare(&run(main, vec![], false));
        let c = of(&p, S::Conclusion)[0].clone();
        assert_eq!(c.text, reply);
        assert_eq!(
            c.prompt.as_deref(),
            Some("Fix the lease test in the renewal module.")
        );
        let kept = decide(
            &p,
            &all_answer(&p, "howto", 0.9),
            URI,
            &DistillConfig::default(),
        );
        let key = idempotency_key(URI, &c.text);
        let m = kept.iter().find(|m| m.idempotency_key == key).unwrap();
        assert!(
            m.title.starts_with("The lease test passes again: "),
            "{}",
            m.title
        );
        assert!(
            m.body
                .ends_with("\n\nContext: Fix the lease test in the renewal module.")
        );
    }
}
