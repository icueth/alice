//! Capture (SP2 spec §3): the pipeline that turns transcript bytes into raw-tier chunks, turn
//! keys and a cursor, each written exactly once.
//!
//! [`CaptureCtl::apply`] runs one pass of one source over bytes handed to it: a tenant upload
//! (`Service::capture_transcript`), or the local worker reading a transcript file
//! ([`CaptureWorker`], below). A pass:
//!
//! 1. checks the bytes' offset against the source's cursor: lower is refused with the cursor
//!    (the route's `409`), higher is a gap that is counted (spec §5.2);
//! 2. reads complete turns with [`alice_core::capture::read`];
//! 3. drops the turns whose key is stored already, in any source (spec §3.5), and applies the
//!    MemPalace rule (spec §9.2): while the source holds no captured turn, a turn that started
//!    at or before the end of an imported transcript of the session is held by the import, so
//!    its key is recorded as skipped and the cursor moves past it;
//! 4. asks the capture verdict for every identity of the source's project (slug, aliases,
//!    display name): `Wait` stores nothing and keeps the cursor, `Excluded` records the keys and
//!    moves the cursor, `Capture` goes on (plan decision P1);
//! 5. redacts every chunk with the project's redactor; in `block` mode a chunk with a secret
//!    becomes `<its first tag> [withheld: <detectors>]`;
//! 6. writes `CaptureSlice`s of at most 50 chunks / 32 KiB, one at a time, each awaited: a
//!    turn's key and the cursor after it go with the slice holding its last chunk, and the
//!    pass pauses 2 ms after each commit so a `remember` waits behind one slice at most.
//!
//! Passes over one source are serialised (ruling 6). `apply` blocks (reader pool and
//! `WriterHandle::submit_blocking`): call it from a blocking thread.

use crate::config::{CaptureVerdict, SettingsHandle};
use crate::redaction::Redactors;
use crate::writer::{
    CaptureChange, CaptureInit, CaptureSlice, INGEST_SLICE, INGEST_SLICE_BYTES, ProjectChoice,
    WriteOp, WriteOutcome, WriterHandle,
};
use alice_core::api::{CaptureHealth, CaptureSessionRow, CaptureStatus, TranscriptAnswer};
use alice_core::capture::{self, AgentKind, MAX_PASS_BYTES, ReadOptions, ReadStats, TAGS};
use alice_core::model::SourceType;
use alice_core::model::limits::MAX_CHUNK_BYTES;
use alice_core::redact::Redactor;
use alice_core::settings::{DistillProvider, ProjectTree, Settings};
use alice_core::store::ReaderPool;
use alice_core::store::capture as store;
use alice_core::store::projects::{self, DetectedForRead};
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_core::{AliceError, Result};
use axum::body::Bytes;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

/// The pause after each committed slice (spec §3.8), as the importer makes.
const SLICE_PAUSE: Duration = Duration::from_millis(2);
/// A subagent whose whole rendering is shorter than this many characters is dropped
/// (spec §3.4: prompt suggestions and other internal helpers also fire `SubagentStop`).
const MIN_SUBAGENT_CHARS: usize = 200;
/// Keys of turns that are not stored (an excluded project), per slice at most.
const SKIPPED_KEYS_PER_SLICE: usize = 500;
/// How many days of unrecognised-line counts are kept.
const UNKNOWN_DAYS: i64 = 7;
/// The largest byte offset a transcript can have: 2^53, exact in every JSON client and far
/// beyond any transcript. A larger offset is refused before anything is read, so no offset
/// arithmetic can overflow and no cursor is stored saturated (SQLite integers are signed).
pub const MAX_OFFSET: u64 = 1 << 53;

/// The events a transcript upload can be for (spec §5.2).
pub const UPLOAD_EVENTS: &[&str] = &[
    "session-start",
    "stop",
    "subagent-stop",
    "pre-compact",
    "session-end",
    "sweep",
];

/// One pass of one source: the transcript's bytes from `offset`, and how to read them.
#[derive(Debug, Clone)]
pub struct PassInput {
    pub agent: AgentKind,
    /// The session id (a subagent's: its main session's). Checked with
    /// [`alice_core::capture::valid_id`] by the caller.
    pub session: String,
    /// A subagent's id, for its own transcript (its own source, spec §3.4).
    pub subagent: Option<String>,
    /// What a new source's state starts with (`transcript_path`, `file_key` and `no_distill`).
    /// Its `agent`, `session_id` and a subagent's `parent_uri` are filled in from the fields
    /// above, whatever it says.
    pub init: CaptureInit,
    /// The project of a new source; an existing source keeps its own (spec §3.1).
    pub project: ProjectChoice,
    /// The transcript's bytes from `offset`. Only the first `MAX_PASS_BYTES` are read, and only
    /// complete lines are used.
    pub bytes: Bytes,
    /// The byte offset of `bytes[0]` in the transcript; at most [`MAX_OFFSET`] (a larger one
    /// is refused).
    pub offset: u64,
    /// `bytes` start at byte 0 of the transcript (records before the first prompt are a head
    /// turn, not the late tail of an earlier one).
    pub at_file_start: bool,
    /// The trailing turn is complete unless its last tool call has no result yet.
    pub close_trailing: bool,
    /// The trailing turn is complete, answered or not (a session end, a stale source).
    pub force_close: bool,
    /// The hook event behind the pass (`capture_state.last_event`).
    pub event: String,
    /// Mark the source ended after this pass.
    pub end: bool,
}

impl PassInput {
    /// The source's uri (spec §3.1).
    pub fn uri(&self) -> String {
        self.agent
            .source_uri(&self.session, self.subagent.as_deref())
    }
}

/// What one pass did.
#[derive(Debug, Clone, Default)]
pub struct PassReport {
    pub uri: String,
    /// The capture source, when it exists after the pass.
    pub source_id: Option<String>,
    /// `Some(cursor)`: the bytes start before the source's cursor, so nothing was read or
    /// written; the caller goes on from `cursor` (the route answers `409`).
    pub behind: Option<u64>,
    /// What the capture rules decided; `None` when the pass stopped before asking.
    pub verdict: Option<CaptureVerdict>,
    /// Why the whole pass was dropped: nothing was written. With subagents off `next_offset`
    /// moves past what was read; a subagent too short to keep leaves it at `offset`, so the
    /// file is judged whole again when it grows (never captured in part, as a gap).
    pub dropped: Option<&'static str>,
    /// The bytes started beyond the cursor: the cursor moved there and the gap was counted.
    pub gap: bool,
    /// The bytes handed to the reader: at most `MAX_PASS_BYTES`.
    pub bytes_read: u64,
    /// Where the next pass starts, as a byte offset in the transcript: the start of the first
    /// incomplete turn, or the end of the last complete line. Under `Wait`, `offset`.
    pub next_offset: u64,
    /// Turns written (their chunks, or their key alone when they rendered none).
    pub turns_stored: u64,
    /// Complete turns not written: stored already, excluded by the rules, or held by an
    /// imported transcript of the session (spec §9.2).
    pub turns_skipped: u64,
    /// Chunks the writer inserted.
    pub chunks_stored: u64,
    /// `bytes` are a whole pass without a line end: the caller skips that line.
    pub oversized_line: bool,
    pub stats: ReadStats,
    /// Writer operations sent (slices and state changes).
    pub writes: u64,
}

impl PassReport {
    /// The answer of an upload that was not behind the cursor.
    pub fn answer(&self) -> TranscriptAnswer {
        TranscriptAnswer {
            next_offset: self.next_offset,
            turns_stored: self.turns_stored,
            turns_skipped: self.turns_skipped,
            chunks_stored: self.chunks_stored,
            oversized_line: self.oversized_line,
        }
    }

    /// An upload the client asked not to capture (`no_capture=1`): nothing is read or written,
    /// and the client moves past the body's last complete line.
    pub fn not_captured(uri: String, offset: u64, body: &[u8]) -> Result<Self> {
        let lines = body.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
        Ok(PassReport {
            uri,
            next_offset: offset_plus(offset, lines as u64)?,
            ..Default::default()
        })
    }
}

/// Checks the ids of an upload (spec §3.1).
pub fn check_ids(session: &str, subagent: Option<&str>) -> Result<()> {
    if !capture::valid_id(session) {
        return Err(AliceError::invalid(
            "a session id is 1 to 128 letters, digits, '.', '_' or '-', starting with a letter or digit; send the agent's session id",
        ));
    }
    if subagent.is_some_and(|a| !capture::valid_id(a)) {
        return Err(AliceError::invalid(
            "a subagent id is 1 to 128 letters, digits, '.', '_' or '-', starting with a letter or digit; send the agent id the hook gave",
        ));
    }
    Ok(())
}

/// Checks an upload's offset: at most [`MAX_OFFSET`].
pub fn check_offset(offset: u64) -> Result<()> {
    if offset > MAX_OFFSET {
        return Err(AliceError::invalid(format!(
            "the offset {offset} is beyond any transcript (at most {MAX_OFFSET}); send the byte offset of the body's first byte in the file"
        )));
    }
    Ok(())
}

/// `offset + n`, refused when it would pass [`MAX_OFFSET`] (and so never overflows).
fn offset_plus(offset: u64, n: u64) -> Result<u64> {
    offset
        .checked_add(n)
        .filter(|&end| end <= MAX_OFFSET)
        .ok_or_else(|| {
            AliceError::invalid(format!(
                "the offset {offset} plus {n} bytes is beyond any transcript (at most {MAX_OFFSET}); send the byte offset of the body's first byte in the file"
            ))
        })
}

/// Checks an upload's event (spec §5.2).
pub fn check_event(event: &str) -> Result<()> {
    if UPLOAD_EVENTS.contains(&event) {
        Ok(())
    } else {
        Err(AliceError::invalid(format!(
            "unknown capture event \"{event}\"; use one of {}",
            UPLOAD_EVENTS.join(", ")
        )))
    }
}

/// `text` cut to at most `max` characters, at the end of a line: what is kept is whole lines
/// (the last one with its `\n`, unless the cut falls right before a `\n`). A first line longer
/// than `max` is cut at `max` characters. A text within `max` is returned whole.
pub(crate) fn cut_at_line_end(text: &str, max: usize) -> &str {
    // `end`: the byte index of character `max`; none when the text has no more than `max`.
    let Some((end, _)) = text.char_indices().nth(max) else {
        return text;
    };
    if text[end..].starts_with('\n') {
        return &text[..end];
    }
    match text[..end].rfind('\n') {
        Some(nl) => &text[..=nl],
        None => &text[..end],
    }
}

/// The capture pipeline of one daemon: shared by every pass, holding no thread.
pub struct CaptureCtl {
    readers: Arc<ReaderPool>,
    writer: WriterHandle,
    settings: Arc<SettingsHandle>,
    redactors: Arc<Redactors>,
    /// One pass per source at a time (ruling 6): a source's slices are sent one at a time, each
    /// built from the state the one before left.
    locks: SourceLocks,
    /// Unrecognised transcript lines per UTC day, the last [`UNKNOWN_DAYS`] days (in memory: a
    /// format-drift hint that starts from zero with the daemon).
    unknown: Mutex<VecDeque<(chrono::NaiveDate, u64)>>,
    /// Called when a pass makes its source due for distillation (wakes the distill worker).
    on_due: Mutex<Option<DueHook>>,
    /// Tests only (`ServiceOptions::capture_fail_after_slices`): slices left before a write
    /// fails as a kill between slices would; `None`: never.
    fail_after_slices: Mutex<Option<usize>>,
}

/// What [`CaptureCtl::set_on_due`] installs.
pub type DueHook = Arc<dyn Fn() + Send + Sync>;

/// A lock per source uri. An entry lives while a pass holds or waits for it.
#[derive(Default)]
struct SourceLocks(Mutex<HashMap<String, Arc<Mutex<()>>>>);

impl SourceLocks {
    /// Runs `f` while holding `uri`'s lock.
    fn run<T>(&self, uri: &str, f: impl FnOnce() -> T) -> T {
        let entry = SourceLock {
            locks: self,
            uri,
            lock: self.0.lock().entry(uri.to_owned()).or_default().clone(),
        };
        // Released before `entry` is dropped, which then forgets the lock if nobody wants it.
        let _held = entry.lock.lock();
        f()
    }
}

/// A pass's hold on its source's lock entry. Dropped (after the lock is released, also when
/// the pass panics), it removes the entry unless another pass holds or waits for it.
struct SourceLock<'a> {
    locks: &'a SourceLocks,
    uri: &'a str,
    lock: Arc<Mutex<()>>,
}

impl Drop for SourceLock<'_> {
    fn drop(&mut self) {
        let mut map = self.locks.0.lock();
        // The map's reference and this one: nobody else holds or waits for it, and nobody can
        // take it while the map is locked.
        if Arc::strong_count(&self.lock) == 2 {
            map.remove(self.uri);
        }
    }
}

/// A turn (or the late tail) a pass writes.
struct Unit {
    key: String,
    chunks: Vec<capture::RenderedChunk>,
    /// Where it ends, as a byte offset in the transcript: the next turn's start, or the end of
    /// what the read consumed.
    end: u64,
    /// A turn (`false`: a tail, which continues the previous turn).
    turn: bool,
    started_at: Option<String>,
    /// An imported transcript of the session holds it (spec §9.2): it has no chunks, and its key
    /// is recorded as skipped, like an excluded turn's.
    imported: bool,
}

impl CaptureCtl {
    pub fn new(
        readers: Arc<ReaderPool>,
        writer: WriterHandle,
        settings: Arc<SettingsHandle>,
        redactors: Arc<Redactors>,
    ) -> Arc<Self> {
        Arc::new(Self {
            readers,
            writer,
            settings,
            redactors,
            locks: SourceLocks::default(),
            unknown: Mutex::new(VecDeque::new()),
            on_due: Mutex::new(None),
            fail_after_slices: Mutex::new(None),
        })
    }

    /// Tests only: after `n` more slices, the next slice write fails (once), as a daemon killed
    /// between two slices of a pass would leave it.
    pub(crate) fn fail_after_slices(&self, n: Option<usize>) {
        *self.fail_after_slices.lock() = n;
    }

    /// Runs one pass (see the module docs). Blocks: call it from a blocking thread.
    pub fn apply(&self, input: PassInput) -> Result<PassReport> {
        let uri = input.uri();
        self.locks.run(&uri, || self.pass(&uri, input))
    }

    /// Unrecognised transcript lines read in the last 7 days.
    pub fn unrecognised_lines_7d(&self) -> u64 {
        let today = chrono::Utc::now().date_naive();
        self.unknown
            .lock()
            .iter()
            .filter(|(day, _)| (today - *day).num_days() < UNKNOWN_DAYS)
            .map(|(_, n)| n)
            .sum()
    }

    fn note_unknown(&self, n: u64) {
        if n == 0 {
            return;
        }
        let today = chrono::Utc::now().date_naive();
        let mut days = self.unknown.lock();
        match days.back_mut() {
            Some((day, count)) if *day == today => *count += n,
            _ => days.push_back((today, n)),
        }
        while days
            .front()
            .is_some_and(|(day, _)| (today - *day).num_days() >= UNKNOWN_DAYS)
        {
            days.pop_front();
        }
    }

    fn pass(&self, uri: &str, input: PassInput) -> Result<PassReport> {
        check_offset(input.offset)?;
        let mut report = PassReport {
            uri: uri.to_owned(),
            next_offset: input.offset,
            ..Default::default()
        };
        let state = self.readers.with(|c| store::by_uri(c, uri))?;
        report.source_id = state.as_ref().map(|s| s.source_id.clone());
        let cursor = state.as_ref().map_or(0, |s| s.byte_offset);
        if input.offset < cursor {
            report.behind = Some(cursor);
            report.next_offset = cursor;
            return Ok(report);
        }

        let settings = self.settings.get();
        let opts = ReadOptions {
            tool_output_chars: settings.capture.tool_output_chars,
            chunk_chars: settings.capture.chunk_chars,
            include_thinking: settings.capture.include_thinking,
            at_file_start: input.at_file_start,
            close_trailing: input.close_trailing,
            force_close: input.force_close,
        };
        let bytes = &input.bytes[..input.bytes.len().min(MAX_PASS_BYTES)];
        report.bytes_read = bytes.len() as u64;
        let out = capture::read(input.agent, bytes, &opts);
        report.stats = out.stats;
        self.note_unknown(out.stats.unknown);
        if out.oversized_line {
            report.oversized_line = true;
            return Ok(report);
        }
        let done = offset_plus(input.offset, out.consumed)?;

        let owner = state.as_ref().map(|s| s.project_id.clone());
        let identity = self
            .readers
            .with(|c| identity(c, owner.as_deref(), &input.project))?;
        let verdict = verdict(&self.settings, input.agent, &identity);
        if input.subagent.is_some() && !settings.capture.include_subagents {
            report.dropped = Some("subagent capture is off");
            report.turns_skipped = out.turns.len() as u64;
            report.next_offset = done;
            return Ok(report);
        }
        if let CaptureVerdict::Wait(reason) = &verdict {
            tracing::info!(
                uri,
                reason = reason.as_str(),
                "capture waits: the rules for this session's project are unknown right now"
            );
            report.verdict = Some(verdict);
            return Ok(report);
        }
        if input.subagent.is_some() && state.is_none() && input.offset == 0 && out.consumed > 0 {
            let chars: usize = out
                .turns
                .iter()
                .flat_map(|t| &t.chunks)
                .chain(&out.tail)
                .map(|c| c.text.chars().count())
                .sum();
            if chars < MIN_SUBAGENT_CHARS {
                // `next_offset` stays at `offset`: the file is judged whole when it grows.
                report.dropped = Some("the subagent's whole rendering is under 200 characters");
                report.turns_skipped = out.turns.len() as u64;
                return Ok(report);
            }
        }
        report.verdict = Some(verdict.clone());

        // Turn keys stored already, in any source, are skipped (spec §3.5).
        let keys: Vec<String> = out
            .tail_key
            .iter()
            .cloned()
            .chain(out.turns.iter().map(|t| t.key.clone()))
            .collect();
        let known = self
            .readers
            .with(|c| store::known_turns(c, input.agent.as_str(), &keys))?;
        // Spec §9.2: while the source holds no captured turn (its first capture may take several
        // uploads), an imported transcript of the session tells where capture continues.
        let imported_until = if state.as_ref().is_none_or(|s| s.next_ord == 0) {
            self.readers
                .with(|c| store::imported_session_end(c, &input.session))?
                .flatten()
                .and_then(|end| parse_time(&end))
        } else {
            None
        };
        let held = |at: Option<&str>| {
            imported_until.is_some_and(|until| at.and_then(parse_time).is_some_and(|t| t <= until))
        };
        // Where each turn ends in the transcript: where the next starts, the last at `done`.
        let ends: Vec<u64> = out
            .turns
            .iter()
            .skip(1)
            .map(|t| offset_plus(input.offset, t.start))
            .chain([Ok(done)])
            .collect::<Result<_>>()?;
        let tail_end = match out.turns.first() {
            Some(t) => offset_plus(input.offset, t.start)?,
            None => done,
        };
        let mut seen = HashSet::new();
        let mut units = Vec::new();
        if let Some(key) = out.tail_key.clone()
            && !known.contains(&key)
            && seen.insert(key.clone())
        {
            // The rest of a turn read by an earlier pass, timed by its first record.
            let imported = held(out.tail.iter().find_map(|c| c.authored_at.as_deref()));
            units.push(Unit {
                key,
                chunks: if imported { Vec::new() } else { out.tail },
                end: tail_end,
                turn: false,
                started_at: None,
                imported,
            });
        }
        for (turn, end) in out.turns.into_iter().zip(ends) {
            if known.contains(&turn.key) || !seen.insert(turn.key.clone()) {
                report.turns_skipped += 1;
                continue;
            }
            // A turn with no time is kept.
            let imported = held(turn.started_at.as_deref());
            if imported {
                report.turns_skipped += 1;
            }
            units.push(Unit {
                key: turn.key,
                chunks: if imported { Vec::new() } else { turn.chunks },
                end,
                turn: true,
                started_at: turn.started_at,
                imported,
            });
        }

        let base = self.base_slice(
            uri,
            &input,
            units
                .iter()
                .filter(|u| !u.imported)
                .find_map(|u| u.started_at.clone()),
        );
        if input.offset > cursor {
            let source_id = match &report.source_id {
                Some(id) => id.clone(),
                // A source the server never saw: register it, then move its cursor.
                None => {
                    report.writes += 1;
                    self.send(base.clone())?.0
                }
            };
            self.writer.submit_blocking(WriteOp::CaptureStateUpdate {
                source_id: source_id.clone(),
                change: CaptureChange::Gap {
                    byte_offset: input.offset,
                },
            })?;
            report.writes += 1;
            report.source_id = Some(source_id);
            report.gap = true;
            tracing::info!(
                uri,
                from = cursor,
                to = input.offset,
                "accepted a transcript upload from beyond the cursor (a gap)"
            );
        }

        let next_ord = state.as_ref().map_or(0, |s| s.next_ord);
        let slices = if matches!(verdict, CaptureVerdict::Excluded(_)) {
            report.turns_skipped += units.iter().filter(|u| u.turn && !u.imported).count() as u64;
            excluded_slices(&base, &input, units, done, next_ord)
        } else {
            let redactor = self.redactors.for_root(identity.tree.as_ref());
            redact(&redactor, uri, &mut units);
            report.turns_stored = units.iter().filter(|u| u.turn && !u.imported).count() as u64;
            stored_slices(&base, &input, units, done, next_ord)
        };
        let needed = |s: &CaptureSlice| {
            !s.chunks.is_empty()
                || !s.turn_keys.is_empty()
                || !s.skipped_keys.is_empty()
                || done > input.offset
                || input.end
                || (state.is_none() && input.event == "session-start")
                || (input.init.no_distill && !state.as_ref().is_some_and(|s| s.no_distill))
        };
        let expected: usize = slices
            .iter()
            .map(|s| s.turn_keys.len() + s.skipped_keys.len())
            .sum();
        let mut keys_new = 0;
        for slice in slices.into_iter().filter(needed) {
            let (source_id, inserted, new) = self.send(slice)?;
            report.source_id = Some(source_id);
            report.chunks_stored += inserted as u64;
            report.writes += 1;
            keys_new += new;
            std::thread::sleep(SLICE_PAUSE);
        }
        // Ruling 4: compared with the keys this pass found unknown. Fewer were new when another
        // pass (another source sharing the records) stored them first: a duplicate at worst.
        if keys_new < expected {
            tracing::debug!(
                uri,
                expected,
                new = keys_new,
                "another pass stored some of this pass's turns first"
            );
        }
        self.mark_due(&input, &report);
        report.next_offset = done;
        Ok(report)
    }

    /// A slice of this pass with no chunk, key or cursor yet.
    fn base_slice(&self, uri: &str, input: &PassInput, started_at: Option<String>) -> CaptureSlice {
        let agent = input.agent.as_str().to_owned();
        CaptureSlice {
            source: SourceInput {
                source_type: SourceType::Session,
                uri: uri.to_owned(),
                agent: Some(agent.clone()),
                session_id: Some(input.session.clone()),
                started_at,
                ..Default::default()
            },
            project: input.project.clone(),
            init: CaptureInit {
                agent,
                session_id: input.session.clone(),
                parent_uri: input
                    .subagent
                    .as_ref()
                    .map(|_| input.agent.source_uri(&input.session, None)),
                ..input.init.clone()
            },
            chunks: Vec::new(),
            turn_keys: Vec::new(),
            skipped_keys: Vec::new(),
            cursor: None,
            last_event: Some(input.event.clone()),
            ended: false,
            record_event: false,
        }
    }

    /// Writes one slice and waits for it: `(source_id, chunks inserted, keys new)`.
    fn send(&self, slice: CaptureSlice) -> Result<(String, usize, usize)> {
        {
            let mut left = self.fail_after_slices.lock();
            match left.as_mut() {
                Some(0) => {
                    *left = None;
                    return Err(AliceError::Other(
                        "the capture pass stopped between two slices (test hook)".into(),
                    ));
                }
                Some(n) => *n -= 1,
                None => {}
            }
        }
        match self
            .writer
            .submit_blocking(WriteOp::CaptureSlice(Box::new(slice)))?
        {
            WriteOutcome::Captured {
                source_id,
                inserted,
                keys_new,
            } => Ok((source_id, inserted, keys_new)),
            other => Err(AliceError::Other(format!(
                "unexpected writer outcome {}",
                other.name()
            ))),
        }
    }
}

/// An RFC 3339 time (`Z` or an offset), compared as an instant; `None` when it does not parse.
fn parse_time(t: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(t.trim()).ok()
}

/// The slices of an excluded project (spec §3.6): the keys are recorded with no chunk, and the
/// cursor moves past the turns.
fn excluded_slices(
    base: &CaptureSlice,
    input: &PassInput,
    units: Vec<Unit>,
    done: u64,
    next_ord: i64,
) -> Vec<CaptureSlice> {
    let mut slices = Vec::new();
    let mut units = units.into_iter().peekable();
    loop {
        let mut slice = base.clone();
        let mut end = input.offset;
        for unit in units.by_ref().take(SKIPPED_KEYS_PER_SLICE) {
            slice.skipped_keys.push(unit.key);
            end = unit.end;
        }
        let last = units.peek().is_none();
        slice.cursor = Some((if last { done } else { end }, next_ord));
        slice.ended = last && input.end;
        slices.push(slice);
        if last {
            return slices;
        }
    }
}

/// The slices of a captured pass: chunks in order from `next_ord`, at most `INGEST_SLICE`
/// chunks and `INGEST_SLICE_BYTES` per slice. A unit's key goes with the slice holding its last
/// chunk; every slice carries the cursor after the last unit it completes (or the one before,
/// so the ords of a turn cut between slices are not given away), and the last one the cursor
/// after the pass. An imported unit's key is a skipped key of the slice it falls in (at most
/// `SKIPPED_KEYS_PER_SLICE` per slice), so the cursor never passes it before its key is kept.
fn stored_slices(
    base: &CaptureSlice,
    input: &PassInput,
    units: Vec<Unit>,
    done: u64,
    mut next_ord: i64,
) -> Vec<CaptureSlice> {
    let mut slices = Vec::new();
    let mut slice = base.clone();
    let mut bytes = 0usize;
    let mut complete = (input.offset, next_ord);
    for unit in units {
        if unit.imported {
            if slice.skipped_keys.len() >= SKIPPED_KEYS_PER_SLICE {
                slice.cursor = Some(complete);
                slices.push(std::mem::replace(&mut slice, base.clone()));
                bytes = 0;
            }
            slice.skipped_keys.push(unit.key);
            complete = (unit.end, next_ord);
            continue;
        }
        let first_ord = next_ord;
        for chunk in unit.chunks {
            let len = chunk.text.len();
            if !slice.chunks.is_empty()
                && (slice.chunks.len() >= INGEST_SLICE || bytes + len > INGEST_SLICE_BYTES)
            {
                slice.cursor = Some(complete);
                slices.push(std::mem::replace(&mut slice, base.clone()));
                bytes = 0;
            }
            slice.chunks.push(ChunkInput {
                ord: next_ord,
                text: chunk.text,
                authored_at: chunk.authored_at,
                legacy_id: None,
            });
            bytes += len;
            next_ord += 1;
        }
        slice.turn_keys.push((unit.key, first_ord));
        complete = (unit.end, next_ord);
    }
    slice.cursor = Some((done, next_ord));
    slice.ended = input.end;
    slices.push(slice);
    // Ruling 5: the pass's `source.capture` event goes with its last slice that stores chunks.
    if let Some(last) = slices.iter_mut().rev().find(|s| !s.chunks.is_empty()) {
        last.record_event = true;
    }
    slices
}

/// Redacts every chunk with `r`, the chunks of a unit as consecutive pieces (a private key cut
/// between chunks is redacted in each). In `block` mode a chunk with a secret is withheld whole.
fn redact(r: &Redactor, uri: &str, units: &mut [Unit]) {
    let (mut redacted, mut withheld) = (0usize, 0usize);
    for unit in units {
        let mut open = false;
        for chunk in &mut unit.chunks {
            let out = r.apply_piece(&chunk.text, &mut open);
            if out.found.is_empty() {
                continue;
            }
            if r.blocks() {
                chunk.text = withheld_stub(r, &chunk.text, &out.found);
                withheld += 1;
            } else {
                chunk.text = fit(out.text);
                redacted += 1;
            }
        }
    }
    if redacted > 0 {
        tracing::info!(
            uri,
            chunks = redacted,
            "redacted secrets in captured chunks before storing them"
        );
    }
    if withheld > 0 {
        tracing::info!(
            uri,
            chunks = withheld,
            "withheld captured chunks that held a secret (privacy.redact_mode = block)"
        );
    }
}

/// Ruling 2: a withheld chunk keeps the rendering contract. It is its first tag and the
/// detectors, as `[assistant] [withheld: github_token]`, never a bare `[withheld: …]` line.
fn withheld_stub(r: &Redactor, text: &str, found: &[String]) -> String {
    let tag = first_tag(text);
    // A tag is a tool or subagent name; one that itself looks like a secret is not kept.
    let tag = if r.apply(tag).found.is_empty() {
        tag
    } else {
        "[result]"
    };
    format!("{tag} [withheld: {}]", found.join(", "))
}

/// The tag a rendered chunk starts with (`[assistant]`, `[tool Bash]`, `[subagent Explore]`).
fn first_tag(text: &str) -> &str {
    for tag in TAGS {
        if !text.starts_with(tag) {
            continue;
        }
        if tag.ends_with(']') {
            return tag;
        }
        // `[tool <name>]` and `[subagent <type>]` run to the first `]` of the line.
        let line = text.lines().next().unwrap_or("");
        if let Some(close) = line.find(']') {
            return &text[..=close];
        }
    }
    "[result]"
}

/// `text` cut to the chunk limit at a character boundary (a redaction marker can be longer
/// than the secret it replaces).
fn fit(mut text: String) -> String {
    if text.len() > MAX_CHUNK_BYTES {
        let mut end = MAX_CHUNK_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Every name the project of a pass is known by, and its working tree.
#[derive(Debug, Default)]
struct Identity {
    names: Vec<String>,
    tree: Option<ProjectTree>,
}

/// The project of a pass: the source's own (`owner`, its project id) when the source exists,
/// else the one `choice` names. A detected project the store does not hold yet is known by
/// what was detected; one it holds (or an import placeholder a write would adopt) adds its
/// stored names.
fn identity(conn: &Connection, owner: Option<&str>, choice: &ProjectChoice) -> Result<Identity> {
    if let Some(id) = owner {
        return stored_identity(conn, id);
    }
    match choice {
        ProjectChoice::Fallback => stored_identity(conn, projects::GLOBAL_ID),
        ProjectChoice::Slug(s) => match projects::resolve_slug(conn, s)? {
            Some(p) => {
                let mut i = stored_identity(conn, &p.id)?;
                i.names.push(s.clone());
                Ok(i)
            }
            None => Ok(Identity {
                names: vec![s.clone()],
                tree: None,
            }),
        },
        ProjectChoice::Detected(r) => {
            let mut i = match projects::detected_for_read(conn, r)? {
                DetectedForRead::Stored(p) | DetectedForRead::Placeholder(p) => {
                    stored_identity(conn, &p.id)?
                }
                DetectedForRead::Ambiguous(_) | DetectedForRead::Unstored => Identity::default(),
            };
            i.names.extend([r.slug.clone(), r.name.clone()]);
            // The working tree in use, keyed by the project.
            if let Some(tree) = r.tree() {
                i.tree = Some(tree);
            }
            Ok(i)
        }
    }
}

fn stored_identity(conn: &Connection, project_id: &str) -> Result<Identity> {
    let p = projects::get_by_id(conn, project_id)?;
    let tree = projects::tree_of(conn, project_id)?;
    let mut names = vec![p.slug, p.name];
    let mut stmt =
        conn.prepare_cached("SELECT alias FROM project_aliases WHERE project_id = ?1")?;
    for alias in stmt.query_map([project_id], |r| r.get::<_, String>(0))? {
        names.push(alias?);
    }
    Ok(Identity { names, tree })
}

/// Ruling 3: the verdict asked for every identity of the project. Any `Wait` wins (the rules
/// are unknown, and moving on would lose the turns for good), then any `Excluded`.
fn verdict(settings: &SettingsHandle, agent: AgentKind, identity: &Identity) -> CaptureVerdict {
    let mut names: Vec<&str> = Vec::new();
    for n in identity.names.iter().map(|n| n.trim()) {
        if !n.is_empty() && !names.contains(&n) {
            names.push(n);
        }
    }
    if names.is_empty() {
        names.push("");
    }
    let mut excluded = None;
    for name in names {
        match settings.capture_verdict(agent, name, identity.tree.as_ref()) {
            CaptureVerdict::Wait(reason) => return CaptureVerdict::Wait(reason),
            CaptureVerdict::Excluded(reason) => {
                excluded.get_or_insert(reason);
            }
            CaptureVerdict::Capture => {}
        }
    }
    excluded.map_or(CaptureVerdict::Capture, CaptureVerdict::Excluded)
}

/// Server tenants keep no path of the person's machine (spec §9.4): an adopted database's
/// transcript paths and file identities are cleared. Idempotent; run at a tenant's start.
pub(crate) fn forget_local_paths(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET transcript_path = NULL, file_key = NULL
         WHERE transcript_path IS NOT NULL OR file_key IS NOT NULL",
        [],
    )?)
}

// ---------------------------------------------------------------------------------------------
// Distillation (SP2 Task 12, spec §6.2).

impl CaptureCtl {
    /// Installs what a pass calls after it makes its source due (the distill worker's wake-up).
    pub fn set_on_due(&self, hook: DueHook) {
        *self.on_due.lock() = Some(hook);
    }

    /// Spec §6.2: a pass for a session end or a compaction makes its (main) source due for
    /// distillation, when distillation is on and the source holds chunks not distilled yet (its
    /// own, or a subagent's that is not skipped for its own project's rules). A source due
    /// already keeps its next try, and a backfilled one is distilled only when asked (`alice
    /// capture distill`). The capture is written either way: a failure here is logged, and the
    /// distill worker's quiet check finds the source later.
    fn mark_due(&self, input: &PassInput, report: &PassReport) {
        if input.subagent.is_some()
            || !matches!(input.event.as_str(), "session-end" | "pre-compact")
            || self.settings.get().distill.provider == DistillProvider::Off
        {
            return;
        }
        let Some(source_id) = report.source_id.as_deref() else {
            return;
        };
        let pending = self.readers.with(|c| {
            let Some(st) = store::by_source(c, source_id)? else {
                return Ok(None);
            };
            if matches!(
                st.distill_state,
                store::DistillState::Due | store::DistillState::SkippedBackfill
            ) {
                return Ok(None);
            }
            let undistilled = st.next_ord > st.distill_ord
                || store::children(c, source_id)?.iter().any(|k| {
                    k.next_ord > k.distill_ord
                        && !matches!(
                            k.distill_state,
                            store::DistillState::SkippedCloud
                                | store::DistillState::SkippedExcluded
                        )
                });
            Ok(undistilled.then_some(st))
        });
        let st = match pending {
            Ok(Some(st)) => st,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(uri = report.uri, error = %e, "cannot tell whether a session is due for distillation; the distill worker looks again later");
                return;
            }
        };
        let marked = self.writer.submit_blocking(WriteOp::CaptureStateUpdate {
            source_id: st.source_id,
            change: CaptureChange::Distill {
                state: store::DistillState::Due,
                next_try_at: None,
                attempts: 0,
            },
        });
        if let Err(e) = marked {
            tracing::warn!(uri = report.uri, error = %e, "cannot mark a session due for distillation; the distill worker looks again later");
            return;
        }
        let hook = self.on_due.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// The capture verdict for a stored source's project, asked for every name the project is known
/// by (ruling 3), and its working tree: the distill worker's "excluded" check.
pub(crate) fn source_verdict(
    readers: &ReaderPool,
    settings: &SettingsHandle,
    agent: AgentKind,
    project_id: &str,
) -> Result<(CaptureVerdict, Option<ProjectTree>)> {
    let identity = readers.with(|c| stored_identity(c, project_id))?;
    let v = verdict(settings, agent, &identity);
    Ok((v, identity.tree))
}

fn distill_provider(s: &Settings) -> &'static str {
    match s.distill.provider {
        DistillProvider::Off => "off",
        DistillProvider::Jev => "jev",
    }
}

/// Until distillation exists: `off`, or `ready` when a provider is set.
fn distill_state(s: &Settings) -> &'static str {
    match s.distill.provider {
        DistillProvider::Off => "off",
        DistillProvider::Jev => "ready",
    }
}

/// The capture line of the health; `None` while capture never ran.
pub(crate) fn health(
    conn: &Connection,
    settings: &Settings,
    unrecognised_lines_7d: u64,
) -> Result<Option<Box<CaptureHealth>>> {
    let count = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<u64> {
        Ok(conn.query_row(sql, p, |r| r.get::<_, i64>(0))? as u64)
    };
    let sources = count("SELECT count(*) FROM capture_state", &[])?;
    if sources == 0 && unrecognised_lines_7d == 0 {
        return Ok(None);
    }
    let since = (chrono::Utc::now() - chrono::Duration::hours(24))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    // A chunk stored in the last day belongs to a source that captured in the last day, so
    // only those sources' chunks are looked at.
    Ok(Some(Box::new(CaptureHealth {
        sessions_24h: count(
            "SELECT count(*) FROM capture_state WHERE last_capture_at >= ?1",
            &[&since],
        )?,
        chunks_24h: count(
            "SELECT count(*) FROM capture_state c JOIN chunks ch ON ch.source_id = c.source_id
             WHERE c.last_capture_at >= ?1 AND ch.created_at >= ?1",
            &[&since],
        )?,
        last_capture_at: conn.query_row(
            "SELECT max(last_capture_at) FROM capture_state",
            [],
            |r| r.get(0),
        )?,
        open_sessions: count("SELECT count(*) FROM capture_state WHERE ended = 0", &[])?,
        distill_provider: distill_provider(settings).into(),
        distill_state: distill_state(settings).into(),
        distill_due: count(
            "SELECT count(*) FROM capture_state WHERE distill_state = 'due'",
            &[],
        )?,
        distilled_24h: 0,
        distill_dropped_24h: 0,
        distill_warning: None,
        unrecognised_lines_7d,
    })))
}

/// `GET /api/v1/capture/status`: the 50 sessions captured most recently.
pub(crate) fn status(
    conn: &Connection,
    tenant: bool,
    settings: &Settings,
) -> Result<CaptureStatus> {
    let mut sessions = Vec::new();
    for s in store::recent(conn, 50)? {
        let project = projects::get_by_id(conn, &s.project_id)
            .map(|p| p.slug)
            .unwrap_or_default();
        let chunks: i64 = conn.query_row(
            "SELECT count(*) FROM chunks WHERE source_id = ?1",
            [&s.source_id],
            |r| r.get(0),
        )?;
        sessions.push(CaptureSessionRow {
            uri: s.uri,
            project,
            chunks: chunks as u64,
            byte_offset: s.byte_offset,
            last_capture_at: s.last_capture_at,
            ended: s.ended,
            distill_state: s.distill_state.as_str().into(),
            gaps: s.gaps,
        });
    }
    Ok(CaptureStatus {
        tenant,
        enabled: settings.capture.enabled,
        distill_provider: distill_provider(settings).into(),
        distill_state: distill_state(settings).into(),
        sessions,
    })
}

// ---- Local capture (Task 6b; spec §4.2, §10): the worker that reads transcript files ----

pub use local::{
    CaptureWorker, Close, Job, LocalCaptureConfig, LocalCaptureCounts, NOTIFY_EVENTS,
    TENANT_NOTIFY_REFUSAL, find_rollout,
};

/// Local capture: a notify queues a pass of its session's transcript, and one low-priority
/// thread, `alice-capture`, reads the file from the source's cursor and hands the bytes to
/// [`CaptureCtl::apply`], the pipeline uploads go through too.
///
/// - **Queue.** One job per source waits in a map: the newest event wins, a `session-end` is
///   never lost and no close is weakened. A `stop`-class event waits the settle delay first.
/// - **Close** (ruling 1). `stop`, `subagent-stop`, `pre-compact` and a quiet sweep close the
///   trailing turn unless its last tool call waits for a result; `session-end` and a stale
///   sweep close it whatever it waits for. A pass that stops short of the end of the file
///   leaves its last turn open, as a capped upload does.
/// - **The file** must be a regular file named `*.jsonl`, opened read-only (a FIFO cannot hold
///   the thread). Its identity, `dev:inode:sha256(first line)`, is kept with the source: a
///   changed identity, or a file shorter than the cursor, puts the cursor back to 0, and the
///   turn keys skip what is stored (spec §3.2).
/// - **A line longer than a pass** (ruling 2): its end is found by reading on, and the next
///   pass starts `MAX_LINE_BYTES + 1` bytes before that end, as a skip from beyond the cursor
///   (a gap, as a tenant client's skip of the line is). The reader keeps those last bytes as one
///   line too large to parse: the stub `[result] [record skipped: too large]`, with the line's
///   uuid and time when they sit near its end (Claude Code writes them there).
/// - **The sweep**, every `sweep_every` and at start, passes the open sources whose transcript
///   changed since their last pass, or whose idle time asks for a stronger close: quiet for
///   `quiet` closes the trailing turn, idle for `stale` makes a final pass and ends the source.
/// - **Codex** (ruling 6): a null transcript path is looked for under `$CODEX_HOME/sessions`.
///
/// A server tenant runs no worker: it never opens a client's files (spec §5.6).
mod local {
    use super::{CaptureCtl, PassInput, check_ids};
    use crate::config::CaptureVerdict;
    use crate::service::ProjectResolver;
    use crate::writer::{CaptureChange, CaptureInit, WriteOp, WriterHandle};
    use alice_core::api::CaptureNotify;
    use alice_core::capture::{self, AgentKind, MAX_LINE_BYTES, MAX_PASS_BYTES};
    use alice_core::store::ReaderPool;
    use alice_core::store::capture as store;
    use alice_core::{AliceError, Result};
    use axum::body::Bytes;
    use parking_lot::Mutex;
    use sha2::{Digest, Sha256};
    use std::collections::hash_map::Entry;
    use std::collections::{HashMap, HashSet};
    use std::fs::{File, Metadata};
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant, SystemTime};

    /// The events a notify can be for (spec §4.1; `backfill`: §4.4).
    pub const NOTIFY_EVENTS: &[&str] = &[
        "session-start",
        "stop",
        "subagent-stop",
        "pre-compact",
        "session-end",
        "sweep",
        "backfill",
    ];

    /// A server tenant's answer to a notify (spec §4.2).
    pub const TENANT_NOTIFY_REFUSAL: &str =
        "this server cannot read files on your machine; use the capture script (docs/capture.md)";

    /// How much of a transcript's first line its identity hashes (a line can be megabytes).
    const KEY_LINE_BYTES: u64 = 64 << 10;
    /// What is read at a time while looking for the end of a line longer than a pass.
    const SCAN_BLOCK: usize = 1 << 20;
    /// Passes one job runs before the other sources' jobs go first (64 MiB of transcript).
    const PASSES_PER_TURN: usize = 8;
    /// Date folders of `$CODEX_HOME/sessions` searched for a rollout (spec §4.2).
    const ROLLOUT_DAYS: usize = 7;

    /// How a pass treats the transcript's trailing turn (ruling 1). Ordered: a stronger close
    /// is never replaced by a weaker one.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Close {
        /// Only turns another turn follows are complete (`session-start`).
        #[default]
        Open,
        /// `close_trailing`: complete unless its last tool call has no result yet (`stop`,
        /// `subagent-stop`, `pre-compact`, a quiet sweep).
        Trailing,
        /// `force_close`: complete whatever it waits for (`session-end`, a stale sweep).
        Force,
    }

    /// The next pass of one source, waiting in the worker's queue.
    #[derive(Debug, Clone)]
    pub struct Job {
        pub agent: AgentKind,
        pub session: String,
        pub subagent: Option<String>,
        /// The transcript; `None` for a Codex session whose hook gave no path (its rollout is
        /// searched for by the session id).
        pub path: Option<PathBuf>,
        /// The session's working directory: a new source's project.
        pub cwd: Option<String>,
        /// The event behind the pass (`capture_state.last_event`).
        pub event: String,
        pub close: Close,
        /// Mark the source ended after the pass that reaches the end of the file.
        pub end: bool,
        /// Decide the close when the pass runs, from how long the transcript has been idle
        /// (a sweep): quiet closes the trailing turn, stale forces it and ends the source.
        pub by_age: bool,
        /// The settle delay: the pass waits until then.
        pub not_before: Instant,
    }

    impl Job {
        pub fn uri(&self) -> String {
            self.agent
                .source_uri(&self.session, self.subagent.as_deref())
        }

        /// Coalesces `newer`, a later notification of the same source: its event wins unless
        /// this one ends the session, and closes, ends and waits only grow.
        fn merge(&mut self, newer: Job) {
            if !self.end || newer.end {
                self.event = newer.event;
            }
            self.close = self.close.max(newer.close);
            self.end |= newer.end;
            self.by_age |= newer.by_age;
            self.not_before = self.not_before.max(newer.not_before);
            if newer.path.is_some() {
                self.path = newer.path;
            }
            if newer.cwd.is_some() {
                self.cwd = newer.cwd;
            }
        }
    }

    /// Checks a notification (spec §4.1) and makes its job; `settle` delays a `stop`-class
    /// event.
    fn notify_job(n: CaptureNotify, settle: Duration) -> Result<Job> {
        let agent = AgentKind::parse(&n.agent).ok_or_else(|| {
            AliceError::invalid(format!(
                "unknown agent \"{}\"; use claude-code or codex",
                n.agent
            ))
        })?;
        if !NOTIFY_EVENTS.contains(&n.event.as_str()) {
            return Err(AliceError::invalid(format!(
                "unknown capture event \"{}\"; use one of {}",
                n.event,
                NOTIFY_EVENTS.join(", ")
            )));
        }
        let absolute = |field: &str, p: &str| -> Result<PathBuf> {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                Ok(p)
            } else {
                Err(AliceError::invalid(format!(
                    "{field} must be an absolute path; send the path the hook gave"
                )))
            }
        };
        let main = match n.transcript_path.trim() {
            "" if agent == AgentKind::Codex => None,
            "" => {
                return Err(AliceError::invalid(
                    "transcript_path is empty; send the transcript path the hook gave",
                ));
            }
            p => Some(absolute("transcript_path", p)?),
        };
        let own = n
            .agent_transcript_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let (subagent, path) = match own {
            Some(p) => {
                let Some(id) = n
                    .agent_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                else {
                    return Err(AliceError::invalid(
                        "agent_transcript_path needs the subagent's agent_id; send both, as the SubagentStop hook gives them",
                    ));
                };
                (
                    Some(id.to_owned()),
                    Some(absolute("agent_transcript_path", p)?),
                )
            }
            None => (None, main),
        };
        check_ids(&n.session_id, subagent.as_deref())?;
        let (close, end, by_age, settles) = match n.event.as_str() {
            "session-start" => (Close::Open, false, false, false),
            "session-end" => (Close::Force, true, false, true),
            "sweep" | "backfill" => (Close::Open, false, true, false),
            _ => (Close::Trailing, false, false, true),
        };
        Ok(Job {
            agent,
            session: n.session_id,
            subagent,
            path,
            cwd: n.cwd.filter(|c| !c.trim().is_empty()),
            event: n.event,
            close,
            end,
            by_age,
            not_before: Instant::now() + if settles { settle } else { Duration::ZERO },
        })
    }

    /// The worker's timings (`ServiceOptions`; tests shorten them) and Codex's home.
    #[derive(Debug, Clone)]
    pub struct LocalCaptureConfig {
        /// A pass caused by `stop`, `subagent-stop`, `pre-compact` or `session-end` waits this
        /// long first, so the agent has finished writing the turn.
        pub settle: Duration,
        /// How often the open sources are looked at (and once at start).
        pub sweep_every: Duration,
        /// A transcript idle this long has its trailing turn closed.
        pub quiet: Duration,
        /// A transcript idle this long gets a final pass, and its source ends.
        pub stale: Duration,
        /// `$CODEX_HOME`: where a Codex session's rollout is looked for when its hook gave no
        /// path. `None`: never looked for.
        pub codex_home: Option<PathBuf>,
    }

    /// What the worker counted since the daemon started (for `alice doctor`).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct LocalCaptureCounts {
        /// Passes run, each over at most `MAX_PASS_BYTES` of one transcript.
        pub passes: u64,
        /// Codex sessions whose hook gave no transcript path and whose rollout was not found.
        pub rollouts_not_found: u64,
        /// Compressed Codex rollouts (`*.jsonl.zst`), skipped unread (spec §10).
        pub rollouts_compressed: u64,
    }

    /// What a source's last finished job saw. The sweep passes the source again when its
    /// transcript changed since, or when its idle time now asks for a stronger close.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Seen {
        len: u64,
        mtime: Option<SystemTime>,
        close: Close,
    }

    /// How a job ended.
    enum Outcome {
        /// The transcript was read as far as it can be now.
        Done(Seen),
        /// The file cannot be read, or the worker is stopping: the next event or sweep tries
        /// again.
        Retry,
        /// The job is back in the queue: more is left (behind the other sources' jobs), or the
        /// project's rules are unknown for now (a sweep period later, so a `session-end` is
        /// not lost and a source never stored is tried again too).
        Requeued,
    }

    /// The local capture worker: its queue and its thread.
    pub struct CaptureWorker {
        ctl: Arc<CaptureCtl>,
        readers: Arc<ReaderPool>,
        writer: WriterHandle,
        resolver: ProjectResolver,
        cfg: LocalCaptureConfig,
        /// The jobs waiting, one per source uri.
        pending: Mutex<HashMap<String, Job>>,
        /// bounded(1): notifications that arrive together wake the thread once.
        wake_tx: crossbeam_channel::Sender<()>,
        wake_rx: crossbeam_channel::Receiver<()>,
        stop: AtomicBool,
        thread: Mutex<Option<JoinHandle<()>>>,
        passes: AtomicU64,
        rollouts_not_found: AtomicU64,
        rollouts_compressed: AtomicU64,
    }

    impl CaptureWorker {
        pub fn new(
            ctl: Arc<CaptureCtl>,
            readers: Arc<ReaderPool>,
            writer: WriterHandle,
            resolver: ProjectResolver,
            cfg: LocalCaptureConfig,
        ) -> Arc<Self> {
            let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
            Arc::new(Self {
                ctl,
                readers,
                writer,
                resolver,
                cfg,
                pending: Mutex::new(HashMap::new()),
                wake_tx,
                wake_rx,
                stop: AtomicBool::new(false),
                thread: Mutex::new(None),
                passes: AtomicU64::new(0),
                rollouts_not_found: AtomicU64::new(0),
                rollouts_compressed: AtomicU64::new(0),
            })
        }

        /// Starts the thread `alice-capture`, which sweeps at once.
        pub fn spawn(self: &Arc<Self>) {
            let me = self.clone();
            let handle = std::thread::Builder::new()
                .name("alice-capture".into())
                .spawn(move || me.run())
                .expect("spawn capture thread");
            *self.thread.lock() = Some(handle);
        }

        /// Checks a notification and queues its pass; the file is read later (spec §4.2).
        pub fn queue(&self, n: CaptureNotify) -> Result<()> {
            self.notify(notify_job(n, self.cfg.settle)?);
            Ok(())
        }

        /// Queues `job`, coalesced with the one waiting for its source.
        pub fn notify(&self, job: Job) {
            match self.pending.lock().entry(job.uri()) {
                Entry::Occupied(mut e) => e.get_mut().merge(job),
                Entry::Vacant(e) => {
                    e.insert(job);
                }
            }
            let _ = self.wake_tx.try_send(());
        }

        pub fn counts(&self) -> LocalCaptureCounts {
            LocalCaptureCounts {
                passes: self.passes.load(Ordering::Relaxed),
                rollouts_not_found: self.rollouts_not_found.load(Ordering::Relaxed),
                rollouts_compressed: self.rollouts_compressed.load(Ordering::Relaxed),
            }
        }

        /// Asks the thread to stop after its current pass, without waiting.
        pub fn signal_stop(&self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = self.wake_tx.try_send(());
        }

        /// Stops the thread, waiting up to `wait` for its current pass to end. A pass that does
        /// not end in time is left; its writes fail once the writer has stopped.
        pub fn stop(&self, wait: Duration) {
            self.signal_stop();
            let Some(handle) = self.thread.lock().take() else {
                return;
            };
            let waited = Instant::now();
            while !handle.is_finished() && waited.elapsed() < wait {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !handle.is_finished() {
                tracing::warn!("a capture pass is still running; stopping without it");
            } else if handle.join().is_err() {
                tracing::error!("the capture worker panicked");
            }
        }

        fn stopping(&self) -> bool {
            self.stop.load(Ordering::Relaxed)
        }

        fn run(&self) {
            crate::maintenance::lower_priority();
            let mut seen: HashMap<String, Seen> = HashMap::new();
            let mut next_sweep = Instant::now();
            while !self.stopping() {
                if Instant::now() >= next_sweep {
                    if let Err(e) = self.sweep(&mut seen) {
                        tracing::warn!(error = %e, "the capture sweep failed; it runs again later");
                    }
                    next_sweep = Instant::now() + self.cfg.sweep_every;
                }
                while Instant::now() < next_sweep {
                    let Some(job) = self.take_due() else { break };
                    if self.stopping() {
                        return;
                    }
                    // A pass that panics (a transcript no one foresaw) costs its own source a
                    // retry, not every session's capture for the rest of the daemon's life.
                    let uri = job.uri();
                    let run = std::panic::AssertUnwindSafe(|| self.run_job(job, &mut seen));
                    if std::panic::catch_unwind(run).is_err() {
                        seen.remove(&uri);
                        tracing::error!(
                            uri,
                            "a capture pass panicked; the next event or sweep tries it again"
                        );
                    }
                }
                let due = self.pending.lock().values().map(|j| j.not_before).min();
                let until = due.map_or(next_sweep, |d| d.min(next_sweep));
                let _ = self
                    .wake_rx
                    .recv_timeout(until.saturating_duration_since(Instant::now()));
            }
        }

        /// The waiting job that is due first, if one is due.
        fn take_due(&self) -> Option<Job> {
            let now = Instant::now();
            let mut pending = self.pending.lock();
            let uri = pending
                .iter()
                .filter(|(_, j)| j.not_before <= now)
                .min_by_key(|(_, j)| j.not_before)
                .map(|(uri, _)| uri.clone())?;
            pending.remove(&uri)
        }

        /// Puts `job` back behind the others; a notification that came meanwhile is merged
        /// into it as the newer one.
        fn requeue(&self, job: Job) {
            match self.pending.lock().entry(job.uri()) {
                Entry::Occupied(mut e) => {
                    let newer = e.insert(job);
                    e.get_mut().merge(newer);
                }
                Entry::Vacant(e) => {
                    e.insert(job);
                }
            }
            let _ = self.wake_tx.try_send(());
        }

        /// `quiet` and `stale` applied to a transcript last changed at `mtime`.
        fn close_by_age(&self, mtime: Option<SystemTime>, now: SystemTime) -> Close {
            let idle = mtime
                .and_then(|m| now.duration_since(m).ok())
                .unwrap_or_default();
            if idle >= self.cfg.stale {
                Close::Force
            } else if idle >= self.cfg.quiet {
                Close::Trailing
            } else {
                Close::Open
            }
        }

        /// Queues a pass of each open source whose transcript changed since its last pass, or
        /// whose idle time asks for a stronger close (a stale one always: its final pass ends
        /// it). A source with no transcript path (an upload's) or whose file is gone is left
        /// to its age (Task 12).
        fn sweep(&self, seen: &mut HashMap<String, Seen>) -> Result<()> {
            let open = self.readers.with(store::open_sources)?;
            let uris: HashSet<&str> = open.iter().map(|s| s.uri.as_str()).collect();
            seen.retain(|uri, _| uris.contains(uri.as_str()));
            let now = SystemTime::now();
            for s in &open {
                let Some(path) = s.transcript_path.as_deref() else {
                    continue;
                };
                let Some(agent) = AgentKind::parse(&s.agent) else {
                    continue;
                };
                // Ids never hold ':', so the subagent's is what follows `:agent:`.
                let subagent = s.uri.split_once(":agent:").map(|(_, a)| a.to_owned());
                if agent.source_uri(&s.session_id, subagent.as_deref()) != s.uri
                    || self.pending.lock().contains_key(&s.uri)
                {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let (len, mtime) = (meta.len(), meta.modified().ok());
                let close = self.close_by_age(mtime, now);
                let last = seen.get(&s.uri);
                let changed = last.is_none_or(|l| l.len != len || l.mtime != mtime);
                let wanted = close == Close::Force
                    || (len != s.byte_offset && (changed || last.is_some_and(|l| l.close < close)));
                if wanted {
                    self.notify(Job {
                        agent,
                        session: s.session_id.clone(),
                        subagent,
                        path: Some(PathBuf::from(path)),
                        cwd: None,
                        event: "sweep".into(),
                        close: Close::Open,
                        end: false,
                        by_age: true,
                        not_before: Instant::now(),
                    });
                }
            }
            Ok(())
        }

        fn run_job(&self, job: Job, seen: &mut HashMap<String, Seen>) {
            let uri = job.uri();
            let Some(path) = self.transcript(&job) else {
                return;
            };
            match self.passes(&uri, &job, &path) {
                Ok(Outcome::Done(s)) => {
                    seen.insert(uri, s);
                }
                Ok(Outcome::Retry | Outcome::Requeued) => {
                    seen.remove(&uri);
                }
                Err(e) => {
                    seen.remove(&uri);
                    tracing::warn!(uri, error = %e, "a capture pass failed; the next event or sweep tries again");
                }
            }
        }

        /// The transcript `job` reads: its path, or the rollout of a Codex session whose hook
        /// gave none. `None` (counted, for a Codex rollout) when there is none to read.
        fn transcript(&self, job: &Job) -> Option<PathBuf> {
            let path = match &job.path {
                Some(p) => p.clone(),
                None => {
                    let found = self
                        .cfg
                        .codex_home
                        .as_deref()
                        .map(|home| search_rollouts(home, &job.session))
                        .unwrap_or_default();
                    match found.path {
                        Some(p) => p,
                        None if found.compressed => {
                            self.rollouts_compressed.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                session = job.session.as_str(),
                                "this Codex session's rollout is compressed (.jsonl.zst); it is not captured"
                            );
                            return None;
                        }
                        None => {
                            self.rollouts_not_found.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                session = job.session.as_str(),
                                "no rollout of this Codex session was found in the newest date folders of CODEX_HOME/sessions; it is not captured"
                            );
                            return None;
                        }
                    }
                }
            };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".jsonl") {
                return Some(path);
            }
            if name.ends_with(".jsonl.zst") {
                self.rollouts_compressed.fetch_add(1, Ordering::Relaxed);
            }
            tracing::info!(
                path = %path.display(),
                "a capture notify names a file that is not a .jsonl transcript; it is not read"
            );
            None
        }

        /// Up to `PASSES_PER_TURN` passes over `path`, each from the source's cursor.
        fn passes(&self, uri: &str, job: &Job, path: &Path) -> Result<Outcome> {
            // A new source's project; an existing one keeps its own (spec §3.1). Git runs at
            // most once a minute per directory.
            let project = self.resolver.choose(None, job.cwd.as_deref());
            // Where the next pass starts, past a line longer than a pass.
            let mut skip_to = None;
            for _ in 0..PASSES_PER_TURN {
                if self.stopping() {
                    return Ok(Outcome::Retry);
                }
                let (mut file, meta) = match open_transcript(path) {
                    Ok(f) => f,
                    // A new session's transcript is written with its first message, after
                    // SessionStart: the source is registered by the first pass that finds it.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        tracing::debug!(uri, "the transcript does not exist yet");
                        return Ok(Outcome::Retry);
                    }
                    Err(e) => {
                        tracing::info!(uri, error = %e, "the transcript cannot be read; the next event or sweep tries again");
                        return Ok(Outcome::Retry);
                    }
                };
                let (len, mtime) = (meta.len(), meta.modified().ok());
                let key = file_key(&mut file, &meta)?;
                let state = self.readers.with(|c| store::by_uri(c, uri))?;
                let mut cursor = state.as_ref().map_or(0, |s| s.byte_offset);
                if let Some(s) = &state
                    && (s.file_key.as_deref() != Some(key.as_str()) || len < cursor)
                {
                    // Replaced or cut short (spec §3.2): read again from 0, and the turn keys
                    // skip what is stored. A source that never had a key gets one where it is.
                    let from = if s.file_key.is_none() && len >= cursor {
                        cursor
                    } else {
                        0
                    };
                    self.writer.submit_blocking(WriteOp::CaptureStateUpdate {
                        source_id: s.source_id.clone(),
                        change: CaptureChange::FileKey {
                            file_key: key.clone(),
                            byte_offset: from,
                        },
                    })?;
                    if from < cursor {
                        tracing::info!(
                            uri,
                            "the transcript was replaced or cut short; it is read again from the start, and stored turns are skipped"
                        );
                    }
                    cursor = from;
                    skip_to = None;
                }
                let close = if job.by_age {
                    job.close.max(self.close_by_age(mtime, SystemTime::now()))
                } else {
                    job.close
                };
                let end = job.end || (job.by_age && close == Close::Force);
                let offset = skip_to
                    .take()
                    .filter(|&o| o > cursor && o < len)
                    .unwrap_or(cursor);
                let want = len.saturating_sub(offset).min(MAX_PASS_BYTES as u64);
                let bytes = read_at(&mut file, offset, want)?;
                // A pass short of the end of the file leaves its last turn open: it may go on
                // past what was read (as a capped upload's does).
                let more = offset + (bytes.len() as u64) < len;
                let report = self.ctl.apply(PassInput {
                    agent: job.agent,
                    session: job.session.clone(),
                    subagent: job.subagent.clone(),
                    init: CaptureInit {
                        transcript_path: Some(path.to_string_lossy().into_owned()),
                        file_key: Some(key),
                        ..Default::default()
                    },
                    project: project.clone(),
                    bytes: Bytes::from(bytes),
                    offset,
                    at_file_start: offset == 0,
                    close_trailing: !more && close >= Close::Trailing,
                    force_close: !more && close == Close::Force,
                    event: job.event.clone(),
                    end: !more && end,
                });
                self.passes.fetch_add(1, Ordering::Relaxed);
                let report = report?;
                if report.behind.is_some() {
                    // Another pass moved the cursor meanwhile: go on from there.
                    continue;
                }
                if report.oversized_line {
                    // Ruling 2: the next pass starts `MAX_LINE_BYTES + 1` bytes before the
                    // line's end, which the reader keeps as one line too large to parse (the
                    // stub). Its end is at least a pass past `offset`, so this is past it.
                    match self.line_end(&mut file, offset + MAX_PASS_BYTES as u64, len)? {
                        Some(nl) => {
                            skip_to = Some(nl - MAX_LINE_BYTES as u64 - 1);
                            continue;
                        }
                        // The line has no end yet: when the file grows.
                        None => return Ok(Outcome::Done(Seen { len, mtime, close })),
                    }
                }
                if matches!(report.verdict, Some(CaptureVerdict::Wait(_))) {
                    self.requeue(Job {
                        not_before: Instant::now() + self.cfg.sweep_every,
                        ..job.clone()
                    });
                    return Ok(Outcome::Requeued);
                }
                if !more || report.dropped.is_some() || report.next_offset <= offset {
                    return Ok(Outcome::Done(Seen { len, mtime, close }));
                }
            }
            if self.stopping() {
                return Ok(Outcome::Retry);
            }
            self.requeue(Job {
                not_before: Instant::now(),
                ..job.clone()
            });
            Ok(Outcome::Requeued)
        }

        /// The offset of the `\n` that ends the line running through `from`, or `None` when
        /// the file ends first (or the worker is stopping).
        fn line_end(&self, file: &mut File, from: u64, len: u64) -> Result<Option<u64>> {
            file.seek(SeekFrom::Start(from))?;
            let mut buf = vec![0u8; SCAN_BLOCK];
            let mut at = from;
            while at < len && !self.stopping() {
                let want = SCAN_BLOCK.min((len - at) as usize);
                let n = file.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                if let Some(i) = buf[..n].iter().position(|&b| b == b'\n') {
                    return Ok(Some(at + i as u64));
                }
                at += n as u64;
            }
            Ok(None)
        }
    }

    /// Opens a transcript read-only; it must be a regular file. Non-blocking, so a FIFO named
    /// like a transcript cannot hold the thread at `open`.
    fn open_transcript(path: &Path) -> std::io::Result<(File, Metadata)> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok((file, meta))
    }

    /// The file's identity (spec §3.2): `dev:inode:sha256(first line)`, the first line without
    /// its `\n` and hashed over its first `KEY_LINE_BYTES` at most.
    fn file_key(file: &mut File, meta: &Metadata) -> std::io::Result<String> {
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = {
            let _ = meta;
            (0u64, 0u64)
        };
        let head = read_at(file, 0, KEY_LINE_BYTES)?;
        let line = head
            .iter()
            .position(|&b| b == b'\n')
            .map_or(&head[..], |nl| &head[..nl]);
        Ok(format!("{dev}:{ino}:{}", hex::encode(Sha256::digest(line))))
    }

    /// At most `n` bytes of `file` from `offset`.
    fn read_at(file: &mut File, offset: u64, n: u64) -> std::io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::with_capacity(n as usize);
        file.by_ref().take(n).read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// What a search for a Codex rollout found.
    #[derive(Debug, Default)]
    struct Rollout {
        path: Option<PathBuf>,
        /// A compressed rollout (`*.jsonl.zst`) names the session, and no plain one does.
        compressed: bool,
    }

    /// Ruling 6: the rollout of the Codex session `session_id`, for a hook that gave no
    /// transcript path. It is `rollout-*-<session_id>.jsonl`, the whole id at the end of the
    /// name (an id that is part of another's never matches), in one of the newest 7
    /// `sessions/YYYY/MM/DD` folders under `codex_home`, the newest folder first. A compressed
    /// rollout (`rollout-*-<session_id>.jsonl.zst`) is not read.
    pub fn find_rollout(codex_home: &Path, session_id: &str) -> Option<PathBuf> {
        search_rollouts(codex_home, session_id).path
    }

    fn search_rollouts(codex_home: &Path, session_id: &str) -> Rollout {
        let mut out = Rollout::default();
        if !capture::valid_id(session_id) {
            return out;
        }
        let plain = format!("-{session_id}.jsonl");
        let compressed = format!("{plain}.zst");
        // `rollout-`, then the rest ending `-<id>.jsonl[.zst]`: the two never share the `-`.
        let rollout_of = |name: &str, end: &str| {
            name.len() >= "rollout-".len() + end.len()
                && name.starts_with("rollout-")
                && name.ends_with(end)
        };
        for dir in newest_days(&codex_home.join("sessions"), ROLLOUT_DAYS) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut found: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| rollout_of(n.as_str(), &plain) || rollout_of(n.as_str(), &compressed))
                .collect();
            // The newest first: a rollout's name starts with its time.
            found.sort_unstable_by(|a, b| b.cmp(a));
            for name in found {
                if rollout_of(&name, &plain) {
                    out.path = Some(dir.join(name));
                    return out;
                }
                out.compressed = true;
            }
        }
        out
    }

    /// The `YYYY/MM/DD` folders under `sessions`, newest first, at most `n`.
    fn newest_days(sessions: &Path, n: usize) -> Vec<PathBuf> {
        let named = |dir: &Path, digits: usize| -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|s| s.len() == digits && s.bytes().all(|b| b.is_ascii_digit()))
                .collect()
        };
        let mut days = Vec::new();
        for y in named(sessions, 4) {
            let year = sessions.join(&y);
            for m in named(&year, 2) {
                let month = year.join(&m);
                for d in named(&month, 2) {
                    days.push((y.clone(), m.clone(), d));
                }
            }
        }
        days.sort_unstable_by(|a, b| b.cmp(a));
        days.truncate(n);
        days.into_iter()
            .map(|(y, m, d)| sessions.join(y).join(m).join(d))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn job(event: &str) -> Job {
            notify_job(
                CaptureNotify {
                    agent: "claude-code".into(),
                    event: event.into(),
                    session_id: "s1".into(),
                    transcript_path: "/t/s1.jsonl".into(),
                    ..Default::default()
                },
                Duration::from_millis(1500),
            )
            .unwrap()
        }

        /// The newest event wins, a `session-end` is never lost and no close is weakened.
        #[test]
        fn notifications_coalesce_and_a_session_end_is_never_lost() {
            let mut j = job("stop");
            assert_eq!((j.close, j.end, j.by_age), (Close::Trailing, false, false));
            let settled = j.not_before;
            j.merge(job("session-start"));
            assert_eq!(j.event, "session-start");
            assert_eq!(j.close, Close::Trailing, "the stop's close is kept");
            assert!(j.not_before >= settled, "the settle delay is kept");

            j.merge(job("session-end"));
            assert_eq!(
                (j.event.as_str(), j.close, j.end),
                ("session-end", Close::Force, true)
            );
            j.merge(job("stop"));
            j.merge(job("sweep"));
            assert_eq!(
                (j.event.as_str(), j.close, j.end, j.by_age),
                ("session-end", Close::Force, true, true),
                "a later event never loses the end"
            );

            let start = job("session-start");
            assert!(start.not_before <= Instant::now(), "no settle delay");
            assert_eq!(start.close, Close::Open);
            assert!(job("backfill").by_age && job("sweep").by_age);
            let end = job("session-end");
            assert!(end.not_before > Instant::now(), "session-end settles too");
        }

        #[test]
        fn a_transcripts_identity_is_its_inode_and_first_line() {
            let d = tempfile::tempdir().unwrap();
            let path = d.path().join("k.jsonl");
            let key = |p: &Path| {
                let (mut f, meta) = open_transcript(p).unwrap();
                file_key(&mut f, &meta).unwrap()
            };
            std::fs::write(&path, b"{\"first\":1}\n{\"second\":2}\n").unwrap();
            let k1 = key(&path);
            let parts: Vec<&str> = k1.split(':').collect();
            assert_eq!(parts.len(), 3);
            assert_eq!(
                parts[2],
                hex::encode(Sha256::digest(b"{\"first\":1}")),
                "the first line, without its newline"
            );
            // Appending, or rewriting in place with the same first line: the same identity.
            std::fs::write(&path, b"{\"first\":1}\n{\"other\":3}\n{\"more\":4}\n").unwrap();
            assert_eq!(key(&path), k1);
            // Another first line: another identity.
            std::fs::write(&path, b"{\"first\":0}\n").unwrap();
            assert_ne!(key(&path), k1);
            // Replaced by another file with the same content: another inode.
            let k2 = key(&path);
            let tmp = d.path().join("k.tmp");
            std::fs::write(&tmp, b"{\"first\":0}\n").unwrap();
            std::fs::rename(&tmp, &path).unwrap();
            if cfg!(unix) {
                assert_ne!(key(&path), k2);
            }
            // Not a regular file: refused.
            assert!(open_transcript(d.path()).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_briefing_is_cut_at_a_line_end() {
        // Within the limit (and exactly at it): whole.
        assert_eq!(cut_at_line_end("ab\ncd\n", 6), "ab\ncd\n");
        assert_eq!(cut_at_line_end("ab\ncd\n", 100), "ab\ncd\n");
        // The cut falls inside a line: back to the end of the line before.
        assert_eq!(cut_at_line_end("ab\ncd\nef\n", 7), "ab\ncd\n");
        assert_eq!(cut_at_line_end("ab\ncd\nef\n", 4), "ab\n");
        // The cut falls right before a line end: the lines before it, whole.
        assert_eq!(cut_at_line_end("ab\ncd\nef\n", 5), "ab\ncd");
        // A first line longer than the limit: cut at the limit.
        assert_eq!(cut_at_line_end("abcdef\ngh", 3), "abc");
        // Characters, not bytes: Thai is three bytes a character.
        let thai = "ร้านค้า\nเปิด\n";
        assert_eq!(cut_at_line_end(thai, 9), "ร้านค้า\n");
        assert_eq!(cut_at_line_end(thai, 3), "ร้า");
        let long = "x\n".repeat(6_000);
        let cut = cut_at_line_end(&long, alice_core::api::BRIEFING_MAX_CHARS);
        assert_eq!(cut.chars().count(), alice_core::api::BRIEFING_MAX_CHARS);
        assert!(cut.ends_with('\n'));
    }

    #[test]
    fn a_withheld_chunk_keeps_its_first_tag() {
        let r = Redactor::builtin();
        let found = vec!["github_token".to_string()];
        let stub = |text: &str| withheld_stub(&r, text, &found);
        assert_eq!(
            stub("[assistant] use the token\n[tool Bash] git push"),
            "[assistant] [withheld: github_token]"
        );
        assert_eq!(
            stub("[tool Bash] curl -H token\n[result] ok"),
            "[tool Bash] [withheld: github_token]"
        );
        assert_eq!(
            stub("[result error] denied"),
            "[result error] [withheld: github_token]"
        );
        assert_eq!(
            stub("[subagent Explore] (cont.) the report"),
            "[subagent Explore] [withheld: github_token]"
        );
        assert_eq!(
            stub("[user] (cont.) more"),
            "[user] [withheld: github_token]"
        );
        assert_eq!(stub("no tag at all"), "[result] [withheld: github_token]");
    }

    /// Passes over one source never overlap; passes over two do; the map forgets a lock nobody
    /// wants, also after a pass that panicked.
    #[test]
    fn source_locks_serialise_one_source_and_clean_up() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let locks = Arc::new(SourceLocks::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let (locks, inside, most) = (locks.clone(), inside.clone(), most.clone());
                std::thread::spawn(move || {
                    let uri = if i % 2 == 0 { "a" } else { "b" };
                    for _ in 0..20 {
                        locks.run(uri, || {
                            let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                            most.fetch_max(now, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_micros(200));
                            inside.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(
            most.load(Ordering::SeqCst),
            2,
            "one pass per source, two sources"
        );
        assert!(locks.0.lock().is_empty());

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            locks.run("c", || panic!("a pass failed"))
        }));
        assert!(panicked.is_err());
        assert!(
            locks.0.lock().is_empty(),
            "a panicking pass leaves no entry"
        );
        assert_eq!(
            locks.run("c", || 7),
            7,
            "and the source can be passed again"
        );
    }

    #[test]
    fn offsets_stay_within_max_offset_and_never_overflow() {
        assert!(check_offset(MAX_OFFSET).is_ok());
        assert!(check_offset(MAX_OFFSET + 1).is_err());
        assert!(check_offset(u64::MAX).is_err());
        assert_eq!(offset_plus(10, 5).unwrap(), 15);
        assert_eq!(offset_plus(MAX_OFFSET - 5, 5).unwrap(), MAX_OFFSET);
        assert!(offset_plus(MAX_OFFSET, 1).is_err());
        assert!(
            offset_plus(u64::MAX, 1).is_err(),
            "an overflow is refused, not wrapped"
        );
        assert!(PassReport::not_captured("u".into(), u64::MAX, b"x\n").is_err());
        assert_eq!(
            PassReport::not_captured("u".into(), 7, b"a\nb\nhalf")
                .unwrap()
                .next_offset,
            11
        );
    }

    #[test]
    fn fit_cuts_at_a_character_boundary() {
        let text = "é".repeat(MAX_CHUNK_BYTES);
        let cut = fit(text);
        assert!(cut.len() <= MAX_CHUNK_BYTES && cut.len() >= MAX_CHUNK_BYTES - 1);
        assert_eq!(fit("short".into()), "short");
    }

    fn unit(key: &str, sizes: &[usize], end: u64) -> Unit {
        Unit {
            key: key.into(),
            chunks: sizes
                .iter()
                .map(|&n| capture::RenderedChunk {
                    text: "x".repeat(n),
                    authored_at: None,
                })
                .collect(),
            end,
            turn: true,
            started_at: None,
            imported: false,
        }
    }

    fn imported(key: &str, end: u64) -> Unit {
        Unit {
            imported: true,
            ..unit(key, &[], end)
        }
    }

    fn input(offset: u64, end: bool) -> PassInput {
        PassInput {
            agent: AgentKind::ClaudeCode,
            session: "s".into(),
            subagent: None,
            init: CaptureInit::default(),
            project: ProjectChoice::Fallback,
            bytes: Bytes::new(),
            offset,
            at_file_start: offset == 0,
            close_trailing: true,
            force_close: false,
            event: "stop".into(),
            end,
        }
    }

    fn base() -> CaptureSlice {
        CaptureSlice {
            source: SourceInput::default(),
            project: ProjectChoice::Fallback,
            init: CaptureInit::default(),
            chunks: Vec::new(),
            turn_keys: Vec::new(),
            skipped_keys: Vec::new(),
            cursor: None,
            last_event: None,
            ended: false,
            record_event: false,
        }
    }

    /// Keys and cursors go with each turn's last slice; a turn cut between slices hands no ord
    /// away early; only the last storing slice records the event; the last slice ends the
    /// session.
    #[test]
    fn slices_carry_keys_and_cursors_with_each_turns_last_chunk() {
        let big = INGEST_SLICE_BYTES / 2 + 1;
        let units = vec![
            unit("a", &[10, 10], 1100),
            unit("b", &[big, big, big], 1200),
            unit("empty", &[], 1220),
            unit("c", &[10], 1300),
        ];
        let slices = stored_slices(&base(), &input(1000, true), units, 1350, 7);
        /// `(ords, turn keys, cursor, record_event, ended)` of a slice.
        type Shape = (Vec<i64>, Vec<(String, i64)>, Option<(u64, i64)>, bool, bool);
        let shape: Vec<Shape> = slices
            .iter()
            .map(|s| {
                (
                    s.chunks.iter().map(|c| c.ord).collect(),
                    s.turn_keys.clone(),
                    s.cursor,
                    s.record_event,
                    s.ended,
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (
                    vec![7, 8, 9],
                    vec![("a".into(), 7)],
                    Some((1100, 9)),
                    false,
                    false
                ),
                (vec![10], vec![], Some((1100, 9)), false, false),
                (
                    vec![11, 12],
                    vec![("b".into(), 9), ("empty".into(), 12), ("c".into(), 12)],
                    Some((1350, 13)),
                    true,
                    true
                ),
            ]
        );
        for s in &slices {
            assert!(s.chunks.len() <= INGEST_SLICE);
            assert!(s.chunks.iter().map(|c| c.text.len()).sum::<usize>() <= INGEST_SLICE_BYTES);
        }
        // Fifty chunks at most per slice.
        let many = vec![unit("m", &[1; 120], 10)];
        let slices = stored_slices(&base(), &input(0, false), many, 10, 0);
        assert_eq!(
            slices.iter().map(|s| s.chunks.len()).collect::<Vec<_>>(),
            [50, 50, 20]
        );
        assert!(slices.iter().all(|s| !s.ended));
    }

    /// Spec §9.2: an imported turn's key is a skipped key of the slice it falls in, with the
    /// cursor after it and no ord taken; at most `SKIPPED_KEYS_PER_SLICE` per slice.
    #[test]
    fn imported_units_are_skipped_keys_in_order_with_the_cursor() {
        let big = INGEST_SLICE_BYTES / 2 + 1;
        let units = vec![
            imported("old-1", 1100),
            unit("new-1", &[big, big], 1200),
            imported("old-2", 1250),
            unit("new-2", &[10], 1300),
        ];
        let slices = stored_slices(&base(), &input(1000, false), units, 1300, 3);
        /// `(ords, turn keys, skipped keys, cursor)` of a slice.
        type Shape = (
            Vec<i64>,
            Vec<(String, i64)>,
            Vec<String>,
            Option<(u64, i64)>,
        );
        let shape: Vec<Shape> = slices
            .iter()
            .map(|s| {
                (
                    s.chunks.iter().map(|c| c.ord).collect(),
                    s.turn_keys.clone(),
                    s.skipped_keys.clone(),
                    s.cursor,
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (vec![3], vec![], vec!["old-1".into()], Some((1100, 3))),
                (
                    vec![4, 5],
                    vec![("new-1".into(), 3), ("new-2".into(), 5)],
                    vec!["old-2".into()],
                    Some((1300, 6))
                ),
            ]
        );

        // Only imported units: no chunk, no ord, and the keys in slices of 500.
        let units: Vec<Unit> = (0..1_200)
            .map(|i| imported(&format!("k{i}"), 2000 + i))
            .collect();
        let slices = stored_slices(&base(), &input(1000, true), units, 3300, 0);
        assert_eq!(
            slices
                .iter()
                .map(|s| (s.skipped_keys.len(), s.cursor, s.chunks.len(), s.ended))
                .collect::<Vec<_>>(),
            vec![
                (500, Some((2499, 0)), 0, false),
                (500, Some((2999, 0)), 0, false),
                (200, Some((3300, 0)), 0, true),
            ]
        );
        assert!(slices.iter().all(|s| s.turn_keys.is_empty()));
    }

    /// Times are compared as instants: an offset and `Z` agree, and text that is not a time
    /// does not parse (the turn is then kept).
    #[test]
    fn imported_ends_compare_as_times() {
        let t = |s: &str| parse_time(s).unwrap();
        assert_eq!(
            t("2026-10-16T07:00:04+07:00"),
            t("2026-10-16T00:00:04.000Z")
        );
        assert!(t("2026-10-16T00:00:05.000Z") > t("2026-10-16T07:00:04+07:00"));
        assert!(t(" 2026-10-16T00:00:01Z ") < t("2026-10-16T00:00:04.000Z"));
        for bad in ["", "soon", "2026-10-16", "2026-10-16T00:00:04"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn excluded_slices_record_keys_and_move_the_cursor() {
        let units: Vec<Unit> = (0..1_200)
            .map(|i| unit(&format!("k{i}"), &[10], 50 + i + 1))
            .collect();
        let slices = excluded_slices(&base(), &input(50, false), units, 1_300, 4);
        assert_eq!(
            slices
                .iter()
                .map(|s| s.skipped_keys.len())
                .collect::<Vec<_>>(),
            [500, 500, 200]
        );
        assert!(
            slices
                .iter()
                .all(|s| s.chunks.is_empty() && s.turn_keys.is_empty())
        );
        assert_eq!(slices[0].cursor, Some((550, 4)));
        assert_eq!(
            slices[2].cursor,
            Some((1_300, 4)),
            "the last one: after the pass"
        );
        let none = excluded_slices(&base(), &input(50, true), Vec::new(), 80, 4);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].cursor, Some((80, 4)));
        assert!(none[0].ended);
    }
}
