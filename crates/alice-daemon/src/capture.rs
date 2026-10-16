//! Capture (SP2 spec §3): the pipeline that turns transcript bytes into raw-tier chunks, turn
//! keys and a cursor, each written exactly once.
//!
//! [`CaptureCtl::apply`] runs one pass of one source over bytes handed to it: a tenant upload
//! today (`Service::capture_transcript`), the local file reader of Task 6b later. A pass:
//!
//! 1. checks the bytes' offset against the source's cursor: lower is refused with the cursor
//!    (the route's `409`), higher is a gap that is counted (spec §5.2);
//! 2. reads complete turns with [`alice_core::capture::read`];
//! 3. drops the turns whose key is stored already, in any source (spec §3.5), and applies the
//!    MemPalace rule (a no-op until Task 18);
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
    /// Complete turns not written: stored already, or excluded by the rules.
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
}

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
        })
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
        let imported_until = mempalace_imported_until(&input);
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
            units.push(Unit {
                key,
                chunks: out.tail,
                end: tail_end,
                turn: false,
                started_at: None,
            });
        }
        for (turn, end) in out.turns.into_iter().zip(ends) {
            let imported = imported_until.as_deref().is_some_and(|until| {
                turn.started_at
                    .as_deref()
                    .is_some_and(|started| started <= until)
            });
            if known.contains(&turn.key) || imported || !seen.insert(turn.key.clone()) {
                report.turns_skipped += 1;
                continue;
            }
            units.push(Unit {
                key: turn.key,
                chunks: turn.chunks,
                end,
                turn: true,
                started_at: turn.started_at,
            });
        }

        let base = self.base_slice(uri, &input, units.iter().find_map(|u| u.started_at.clone()));
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
            report.turns_skipped += units.iter().filter(|u| u.turn).count() as u64;
            excluded_slices(&base, &input, units, done, next_ord)
        } else {
            let redactor = self.redactors.for_root(identity.tree.as_ref());
            redact(&redactor, uri, &mut units);
            report.turns_stored = units.iter().filter(|u| u.turn).count() as u64;
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

/// The end of an imported transcript of the session (MemPalace, spec §9.2): turns that started
/// at or before it were captured by the import. Wired in Task 18; until then no turn is
/// skipped by it.
fn mempalace_imported_until(_input: &PassInput) -> Option<String> {
    None
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
/// after the pass.
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

#[cfg(test)]
mod tests {
    use super::*;

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
