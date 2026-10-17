//! Capture state (SP2 spec §3.2, §3.5, §8.1): one `capture_state` row per captured transcript
//! source (its cursor, its file identity and its distillation state), and the turn keys that
//! make every turn stored exactly once across all sources. The writer is the only one that
//! changes these rows; everything here that writes is called inside its transaction.

use crate::error::Result;
use crate::ids::now_rfc3339;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Where a source's distillation stands (`capture_state.distill_state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistillState {
    #[default]
    Idle,
    Due,
    Done,
    WaitingBudget,
    WaitingJudge,
    SkippedCloud,
    SkippedExcluded,
    SkippedBackfill,
    Failed,
}

impl DistillState {
    pub const ALL: [DistillState; 9] = [
        DistillState::Idle,
        DistillState::Due,
        DistillState::Done,
        DistillState::WaitingBudget,
        DistillState::WaitingJudge,
        DistillState::SkippedCloud,
        DistillState::SkippedExcluded,
        DistillState::SkippedBackfill,
        DistillState::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DistillState::Idle => "idle",
            DistillState::Due => "due",
            DistillState::Done => "done",
            DistillState::WaitingBudget => "waiting_budget",
            DistillState::WaitingJudge => "waiting_judge",
            DistillState::SkippedCloud => "skipped_cloud",
            DistillState::SkippedExcluded => "skipped_excluded",
            DistillState::SkippedBackfill => "skipped_backfill",
            DistillState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<DistillState> {
        DistillState::ALL.into_iter().find(|d| d.as_str() == s)
    }
}

/// One capture source: its `capture_state` row with the source's uri and project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureState {
    pub source_id: String,
    pub uri: String,
    pub agent: String,
    pub session_id: String,
    pub parent_source_id: Option<String>,
    pub transcript_path: Option<String>,
    pub file_key: Option<String>,
    pub byte_offset: u64,
    pub next_ord: i64,
    pub last_event: Option<String>,
    pub last_capture_at: Option<String>,
    pub ended: bool,
    pub no_distill: bool,
    /// Times the cursor jumped forward over bytes never read (see [`record_gap`]).
    pub gaps: i64,
    pub distill_ord: i64,
    pub distill_state: DistillState,
    pub distill_attempts: i64,
    pub distill_next_try_at: Option<String>,
    pub project_id: String,
}

const SELECT_STATE: &str = "SELECT c.source_id, s.uri, c.agent, c.session_id, c.parent_source_id,
    c.transcript_path, c.file_key, c.byte_offset, c.next_ord, c.last_event, c.last_capture_at,
    c.ended, c.no_distill, c.gaps, c.distill_ord, c.distill_state, c.distill_attempts,
    c.distill_next_try_at, s.project_id
    FROM capture_state c JOIN sources s ON s.id = c.source_id";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CaptureState> {
    let state: String = r.get(15)?;
    Ok(CaptureState {
        source_id: r.get(0)?,
        uri: r.get(1)?,
        agent: r.get(2)?,
        session_id: r.get(3)?,
        parent_source_id: r.get(4)?,
        transcript_path: r.get(5)?,
        file_key: r.get(6)?,
        byte_offset: u64::try_from(r.get::<_, i64>(7)?).unwrap_or(0),
        next_ord: r.get(8)?,
        last_event: r.get(9)?,
        last_capture_at: r.get(10)?,
        ended: r.get(11)?,
        no_distill: r.get(12)?,
        gaps: r.get(13)?,
        distill_ord: r.get(14)?,
        distill_state: DistillState::parse(&state).unwrap_or_default(),
        distill_attempts: r.get(16)?,
        distill_next_try_at: r.get(17)?,
        project_id: r.get(18)?,
    })
}

fn many(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<CaptureState>> {
    let mut stmt = conn.prepare_cached(sql)?;
    Ok(stmt
        .query_map(p, row)?
        .collect::<std::result::Result<_, _>>()?)
}

/// A stored byte offset (SQLite integers are signed; a transcript is never 8 EiB long).
fn offset(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The capture source whose uri is `uri` (capture sources are of type `session`).
pub fn by_uri(conn: &Connection, uri: &str) -> Result<Option<CaptureState>> {
    Ok(conn
        .prepare_cached(&format!(
            "{SELECT_STATE} WHERE s.type = 'session' AND s.uri = ?1"
        ))?
        .query_row([uri], row)
        .optional()?)
}

pub fn by_source(conn: &Connection, source_id: &str) -> Result<Option<CaptureState>> {
    Ok(conn
        .prepare_cached(&format!("{SELECT_STATE} WHERE c.source_id = ?1"))?
        .query_row([source_id], row)
        .optional()?)
}

/// Bound parameters per `IN (...)` query, well under SQLite's limit.
const IN_BATCH: usize = 500;

/// Which of `keys` are stored for `agent`, in any source (spec §3.5).
pub fn known_turns(conn: &Connection, agent: &str, keys: &[String]) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for batch in keys.chunks(IN_BATCH) {
        let sql = format!(
            "SELECT turn_key FROM capture_turns WHERE agent = ? AND turn_key IN ({})",
            vec!["?"; batch.len()].join(",")
        );
        let mut stmt = conn.prepare(&sql)?;
        let params = std::iter::once(agent).chain(batch.iter().map(String::as_str));
        for k in stmt.query_map(params_from_iter(params), |r| r.get::<_, String>(0))? {
            out.insert(k?);
        }
    }
    Ok(out)
}

/// Sources not marked ended, oldest first.
pub fn open_sources(conn: &Connection) -> Result<Vec<CaptureState>> {
    many(
        conn,
        &format!("{SELECT_STATE} WHERE c.ended = 0 ORDER BY c.created_at, c.source_id"),
        [],
    )
}

/// Up to `limit` sources waiting to be distilled (`due`) whose next try has come at `now`
/// (none set: at once), the longest-waiting first.
pub fn due(conn: &Connection, now: &str, limit: usize) -> Result<Vec<CaptureState>> {
    many(
        conn,
        &format!(
            "{SELECT_STATE} WHERE c.distill_state = 'due'
                AND (c.distill_next_try_at IS NULL OR c.distill_next_try_at <= ?1)
             ORDER BY c.distill_next_try_at, c.created_at LIMIT ?2"
        ),
        params![now, limit as i64],
    )
}

/// Up to `limit` chunks of `source_id` from `ord` on, in order: `(id, ord, text)`.
pub fn chunks_from(
    conn: &Connection,
    source_id: &str,
    ord: i64,
    limit: usize,
) -> Result<Vec<(String, i64, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, ord, text FROM chunks WHERE source_id = ?1 AND ord >= ?2 ORDER BY ord LIMIT ?3",
    )?;
    Ok(stmt
        .query_map(params![source_id, ord, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?)
}

/// The session ids of every capture source, of every agent (rows of `capture_state` only; a
/// subagent's is its main session's). The MemPalace importer skips their transcripts (§9.2).
pub fn captured_session_ids(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT session_id FROM capture_state")?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

/// `Some(ended_at)` when a transcript of `session_id` exists that capture does not hold (an
/// imported one): a source of type `session` with that session id, no capture state and no
/// capture uri (spec §3.1). Inside, the latest `ended_at` of those sources, compared as times
/// (an importer may write an offset where capture writes `Z`; one that does not parse counts
/// only when none does), and `None` when none of them has one. `None` when there is no such
/// source (spec §9.2).
///
/// With `codex_rollouts` (a Codex session), a source named like one of the session's rollout
/// files, `rollout-<YYYY-MM-DDTHH-MM-SS>-<session_id>`, counts too: the importer files a
/// transcript under its file's stem, so a rollout MemPalace mined carries that name, never the
/// bare session id (Task 18b review, I2).
pub fn imported_session_end(
    conn: &Connection,
    session_id: &str,
    codex_rollouts: bool,
) -> Result<Option<Option<String>>> {
    // `rollout-` (8 characters), the time (19), `-` (the 28th), then the whole id: an id that
    // only ends another one never matches. The range keeps the scan on the index.
    let mut stmt = conn.prepare_cached(
        "SELECT s.ended_at FROM sources s
         WHERE (s.session_id = ?1
                OR (?2 AND s.session_id >= 'rollout-' AND s.session_id < 'rollout.'
                    AND length(s.session_id) = length(?1) + 28
                    AND substr(s.session_id, 28, 1) = '-' AND substr(s.session_id, 29) = ?1))
           AND s.type = 'session'
           AND s.uri NOT LIKE 'claude-code:session:%' AND s.uri NOT LIKE 'codex:session:%'
           AND NOT EXISTS (SELECT 1 FROM capture_state c WHERE c.source_id = s.id)",
    )?;
    let ends: Vec<Option<String>> = stmt
        .query_map(params![session_id, codex_rollouts], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if ends.is_empty() {
        return Ok(None);
    }
    let mut latest: Option<(Option<chrono::DateTime<chrono::FixedOffset>>, String)> = None;
    for end in ends.into_iter().flatten() {
        let at = chrono::DateTime::parse_from_rfc3339(end.trim()).ok();
        let later = match (&latest, at) {
            (None, _) => true,
            (Some((None, _)), at) => at.is_some(),
            (Some((Some(best), _)), Some(at)) => at > *best,
            (Some((Some(_), _)), None) => false,
        };
        if later {
            latest = Some((at, end));
        }
    }
    Ok(Some(latest.map(|(_, end)| end)))
}

/// The `limit` sources captured most recently (registered and not yet captured: by creation).
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<CaptureState>> {
    many(
        conn,
        &format!(
            "{SELECT_STATE} ORDER BY COALESCE(c.last_capture_at, c.created_at) DESC, c.source_id DESC
             LIMIT ?1"
        ),
        [limit as i64],
    )
}

/// The id of the capture source (type `session`) whose uri is `uri`.
pub fn source_id_by_uri(conn: &Connection, uri: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT id FROM sources WHERE type = 'session' AND uri = ?1")?
        .query_row([uri], |r| r.get(0))
        .optional()?)
}

/// What a capture source's state starts with.
#[derive(Debug, Clone, Default)]
pub struct NewState<'a> {
    pub agent: &'a str,
    pub session_id: &'a str,
    pub parent_source_id: Option<&'a str>,
    pub transcript_path: Option<&'a str>,
    pub file_key: Option<&'a str>,
    pub no_distill: bool,
    /// Where a new state's distillation starts: `idle`, or `skipped_backfill` for a source a
    /// backfill registers (spec §4.4). An existing state keeps its own.
    pub distill_state: DistillState,
}

/// Create the state of `source_id` if it has none. An existing one keeps its values, except
/// that a parent it lacked is filled in and `no_distill` only ever turns on (a client flag that
/// tightens).
pub fn ensure_state(conn: &Connection, source_id: &str, s: &NewState<'_>) -> Result<()> {
    let created = conn
        .prepare_cached(
            "INSERT OR IGNORE INTO capture_state
                (source_id, agent, session_id, parent_source_id, transcript_path, file_key, no_distill,
                 distill_state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?
        .execute(params![
            source_id,
            s.agent,
            s.session_id,
            s.parent_source_id,
            s.transcript_path,
            s.file_key,
            s.no_distill,
            s.distill_state.as_str(),
            now_rfc3339()
        ])?;
    if created == 0 && (s.parent_source_id.is_some() || s.no_distill) {
        conn.prepare_cached(
            "UPDATE capture_state SET parent_source_id = COALESCE(parent_source_id, ?2),
                no_distill = MAX(no_distill, ?3)
             WHERE source_id = ?1",
        )?
        .execute(params![source_id, s.parent_source_id, s.no_distill])?;
    }
    Ok(())
}

/// Record turn keys `(key, first_ord)` of `agent` for `source_id`. A key stored already (by
/// any source) keeps its row. Returns how many were new.
pub fn record_turns<'a>(
    conn: &Connection,
    agent: &str,
    source_id: &str,
    keys: impl IntoIterator<Item = (&'a str, i64)>,
) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO capture_turns (agent, turn_key, source_id, first_ord) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut new = 0;
    for (key, first_ord) in keys {
        new += stmt.execute(params![agent, key, source_id, first_ord])?;
    }
    Ok(new)
}

/// What one capture slice moves on a source's state.
#[derive(Debug, Clone, Default)]
pub struct Progress<'a> {
    /// `(byte_offset, next_ord)` after the slice's last complete turn. `next_ord` never moves
    /// back; `byte_offset` is set as given.
    pub cursor: Option<(u64, i64)>,
    pub last_event: Option<&'a str>,
    /// The session ended (never cleared here).
    pub ended: bool,
    /// The slice captured something (`last_capture_at` becomes now).
    pub captured: bool,
}

/// Move a source's state on after a slice. `next_ord` only grows: a stale slice committed late
/// must not hand a later pass ords that are taken (their chunks would be dropped by `INSERT OR
/// IGNORE` while their turn keys are recorded, and the turns lost). `byte_offset` is set as given,
/// since a replaced file resets it on purpose (`set_file_key`), and a turn read again is skipped
/// by its key.
pub fn advance(conn: &Connection, source_id: &str, p: &Progress<'_>) -> Result<()> {
    let (byte_offset, next_ord) = match p.cursor {
        Some((b, o)) => (Some(offset(b)), Some(o)),
        None => (None, None),
    };
    let captured_at = p.captured.then(now_rfc3339);
    conn.prepare_cached(
        "UPDATE capture_state SET byte_offset = COALESCE(?2, byte_offset),
            next_ord = MAX(next_ord, COALESCE(?3, next_ord)), last_event = COALESCE(?4, last_event),
            last_capture_at = COALESCE(?5, last_capture_at), ended = MAX(ended, ?6)
         WHERE source_id = ?1",
    )?
    .execute(params![
        source_id,
        byte_offset,
        next_ord,
        p.last_event,
        captured_at,
        p.ended
    ])?;
    Ok(())
}

/// The distillation state, its next try and its attempts. Returns the rows changed (0: no such
/// source).
pub fn set_distill(
    conn: &Connection,
    source_id: &str,
    state: DistillState,
    next_try_at: Option<&str>,
    attempts: i64,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET distill_state = ?2, distill_next_try_at = ?3, distill_attempts = ?4
         WHERE source_id = ?1",
        params![source_id, state.as_str(), next_try_at, attempts],
    )?)
}

/// Chunks with an ord below `distill_ord` were distilled. It never moves back (a late answer to
/// an older run changes nothing).
pub fn set_distilled(conn: &Connection, source_id: &str, distill_ord: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET distill_ord = MAX(distill_ord, ?2) WHERE source_id = ?1",
        params![source_id, distill_ord],
    )?)
}

/// A source a backfill registered goes on live (Task 18b review, I1): a hook's pass is about to
/// capture new turns into it. Its chunks below `from_ord` (the backfilled history) count as
/// distilled and it is `idle` again, so only what is captured from now on is distilled, never
/// the backfill as a burst. Its subagents that are still `skipped_backfill` are moved past their
/// chunks the same way (a subagent's chunks are distilled with its session's). Nothing changes
/// unless the source is `skipped_backfill`. Returns the rows of the source changed (0: it is not
/// backfilled, or there is no such source).
pub fn revive_backfilled(conn: &Connection, source_id: &str, from_ord: i64) -> Result<usize> {
    let revived = conn.execute(
        "UPDATE capture_state SET distill_state = 'idle', distill_ord = MAX(distill_ord, ?2),
            distill_next_try_at = NULL, distill_attempts = 0
         WHERE source_id = ?1 AND distill_state = 'skipped_backfill'",
        params![source_id, from_ord],
    )?;
    if revived > 0 {
        conn.execute(
            "UPDATE capture_state SET distill_state = 'idle', distill_ord = MAX(distill_ord, next_ord),
                distill_next_try_at = NULL, distill_attempts = 0
             WHERE parent_source_id = ?1 AND distill_state = 'skipped_backfill'",
            [source_id],
        )?;
    }
    Ok(revived)
}

pub fn set_ended(conn: &Connection, source_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET ended = 1 WHERE source_id = ?1",
        [source_id],
    )?)
}

/// The stale end makes the session due (spec §6.2, §10; Task 11b), as a session end does: main
/// source `source_id` becomes `due` when it holds chunks not distilled yet (its own, or a
/// subagent's that is not skipped for its project's rules) and it or a subagent captured
/// something at `recent_since` or later. An older one is distilled only when asked, so turning
/// distillation on makes no backlog. A source due already keeps its next try, and a backfilled
/// one waits to be asked. Returns the rows changed (0 or 1).
pub fn due_after_stale_end(
    conn: &Connection,
    source_id: &str,
    recent_since: &str,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET distill_state = 'due', distill_next_try_at = NULL, distill_attempts = 0
         WHERE source_id = ?1 AND parent_source_id IS NULL
           AND distill_state NOT IN ('due', 'skipped_backfill')
           AND (COALESCE(last_capture_at, created_at) >= ?2
                OR EXISTS (SELECT 1 FROM capture_state k
                           WHERE k.parent_source_id = ?1 AND k.last_capture_at >= ?2))
           AND (next_ord > distill_ord
                OR EXISTS (SELECT 1 FROM capture_state k
                           WHERE k.parent_source_id = ?1 AND k.next_ord > k.distill_ord
                             AND k.distill_state NOT IN ('skipped_cloud', 'skipped_excluded')))",
        params![source_id, recent_since],
    )?)
}

/// A pass starts beyond the cursor: the cursor moves there and the gap is counted. On a tenant,
/// a client sent from beyond the cursor (it skipped a line, or the server's copy is older than
/// the client's); on a local daemon, the worker skipped a line longer than a pass. Bytes in a
/// gap are never read.
pub fn record_gap(conn: &Connection, source_id: &str, byte_offset: u64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET byte_offset = ?2, gaps = gaps + 1 WHERE source_id = ?1",
        params![source_id, offset(byte_offset)],
    )?)
}

/// The local file's identity changed (or was first seen): the new key and where to read from.
pub fn set_file_key(
    conn: &Connection,
    source_id: &str,
    file_key: &str,
    byte_offset: u64,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET file_key = ?2, byte_offset = ?3 WHERE source_id = ?1",
        params![source_id, file_key, offset(byte_offset)],
    )?)
}

// ---------------------------------------------------------------------------------------------
// What the distill worker reads (SP2 Task 12).

/// The subagent sources of `source_id` (those whose parent it is), the oldest first.
pub fn children(conn: &Connection, source_id: &str) -> Result<Vec<CaptureState>> {
    many(
        conn,
        &format!("{SELECT_STATE} WHERE c.parent_source_id = ?1 ORDER BY c.created_at, c.source_id"),
        [source_id],
    )
}

/// The main sources (no parent) of session `session_id`, of every agent, the oldest first.
pub fn sessions(conn: &Connection, session_id: &str) -> Result<Vec<CaptureState>> {
    many(
        conn,
        &format!(
            "{SELECT_STATE} WHERE c.session_id = ?1 AND c.parent_source_id IS NULL
             ORDER BY c.created_at, c.source_id"
        ),
        [session_id],
    )
}

/// The ords at which turns of `source_id` start, within `from..=to`: the `first_ord` of its
/// stored turn keys. Late tails (`tail:…` keys) are left out: a stored tail continues the turn
/// before it.
pub fn turn_starts(conn: &Connection, source_id: &str, from: i64, to: i64) -> Result<HashSet<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT first_ord FROM capture_turns
         WHERE source_id = ?1 AND turn_key NOT LIKE 'tail:%' AND first_ord BETWEEN ?2 AND ?3",
    )?;
    Ok(stmt
        .query_map(params![source_id, from, to], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

/// Up to `limit` sources in `state`, the longest-waiting first; with `now`, only those whose
/// next try has come (none set: at once).
pub fn in_state(
    conn: &Connection,
    state: DistillState,
    now: Option<&str>,
    limit: usize,
) -> Result<Vec<CaptureState>> {
    many(
        conn,
        &format!(
            "{SELECT_STATE} WHERE c.distill_state = ?1
                AND (?2 IS NULL OR c.distill_next_try_at IS NULL OR c.distill_next_try_at <= ?2)
             ORDER BY c.distill_next_try_at, c.created_at, c.source_id LIMIT ?3"
        ),
        params![state.as_str(), now, limit as i64],
    )
}

/// Up to `limit` main sources (no parent) that are `idle` or `done`, hold chunks not distilled
/// yet (their own, or a subagent's that is not skipped), have been quiet since `quiet_since`
/// (neither they nor a subagent of theirs captured anything later), and captured something at
/// `recent_since` or later (an older backlog is distilled only when asked). `(source_id,
/// agent)`, the most recently captured first (spec §6.2; Task 12 fix round 1).
pub fn quiet_undistilled(
    conn: &Connection,
    quiet_since: &str,
    recent_since: &str,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.source_id, c.agent FROM capture_state c
         WHERE c.parent_source_id IS NULL
           AND c.distill_state IN ('idle', 'done')
           AND COALESCE(c.last_capture_at, c.created_at) <= ?1
           AND NOT EXISTS (SELECT 1 FROM capture_state k
                           WHERE k.parent_source_id = c.source_id AND k.last_capture_at > ?1)
           AND (COALESCE(c.last_capture_at, c.created_at) >= ?2
                OR EXISTS (SELECT 1 FROM capture_state k
                           WHERE k.parent_source_id = c.source_id AND k.last_capture_at >= ?2))
           AND (c.next_ord > c.distill_ord
                OR EXISTS (SELECT 1 FROM capture_state k
                           WHERE k.parent_source_id = c.source_id AND k.next_ord > k.distill_ord
                             AND k.distill_state NOT IN ('skipped_cloud', 'skipped_excluded')))
         ORDER BY COALESCE(c.last_capture_at, c.created_at) DESC, c.source_id DESC LIMIT ?3",
    )?;
    Ok(stmt
        .query_map(params![quiet_since, recent_since, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<std::result::Result<_, _>>()?)
}

/// Up to `limit` sources waiting to be distilled (`due`) whose next try has come at `now` (none
/// set: at once), the most recently captured first (Task 12 fix round 1: a backlog is drained
/// newest first).
pub fn due_newest(conn: &Connection, now: &str, limit: usize) -> Result<Vec<CaptureState>> {
    many(
        conn,
        &format!(
            "{SELECT_STATE} WHERE c.distill_state = 'due'
                AND (c.distill_next_try_at IS NULL OR c.distill_next_try_at <= ?1)
             ORDER BY COALESCE(c.last_capture_at, c.created_at) DESC, c.source_id DESC LIMIT ?2"
        ),
        params![now, limit as i64],
    )
}

/// Chunks of `source_id` with `from <= ord < until`, at most `limit`, in order: `(id, ord,
/// text)`. `until` is a cursor's `next_ord`: chunks at or past it belong to a turn whose last
/// slice is not written yet.
pub fn chunks_between(
    conn: &Connection,
    source_id: &str,
    from: i64,
    until: i64,
    limit: usize,
) -> Result<Vec<(String, i64, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, ord, text FROM chunks WHERE source_id = ?1 AND ord >= ?2 AND ord < ?3
         ORDER BY ord LIMIT ?4",
    )?;
    Ok(stmt
        .query_map(params![source_id, from, until, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?)
}

/// Up to `limit` sources not marked ended that captured nothing since `before` (one that never
/// captured: registered before it), the stalest first.
pub fn stale_open(conn: &Connection, before: &str, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT source_id FROM capture_state
         WHERE ended = 0 AND COALESCE(last_capture_at, created_at) <= ?1
         ORDER BY COALESCE(last_capture_at, created_at), source_id LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![before, limit as i64], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::projects::GLOBAL_ID;

    fn conn() -> (tempfile::TempDir, Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        (d, c)
    }

    fn source(c: &Connection, id: &str, uri: &str, session: Option<&str>, ended_at: Option<&str>) {
        c.execute(
            "INSERT INTO sources (id, project_id, type, uri, session_id, ended_at, created_at)
             VALUES (?1, ?2, 'session', ?3, ?4, ?5, '2026-10-16T00:00:00.000Z')",
            params![id, GLOBAL_ID, uri, session, ended_at],
        )
        .unwrap();
    }

    fn state(c: &Connection, id: &str, session: &str) {
        ensure_state(
            c,
            id,
            &NewState {
                agent: "claude-code",
                session_id: session,
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn distill_state_strings_round_trip_and_match_serde() {
        for d in DistillState::ALL {
            assert_eq!(DistillState::parse(d.as_str()), Some(d));
            assert_eq!(
                serde_json::to_value(d).unwrap(),
                serde_json::Value::String(d.as_str().into())
            );
        }
        assert_eq!(DistillState::parse("busy"), None);
    }

    /// An existing state keeps what it has: a second `ensure_state` only fills in a missing
    /// parent and turns `no_distill` on, never off.
    #[test]
    fn ensure_state_creates_once_and_only_tightens() {
        let (_d, c) = conn();
        source(&c, "S1", "claude-code:session:s1", Some("s1"), None);
        ensure_state(
            &c,
            "S1",
            &NewState {
                agent: "claude-code",
                session_id: "s1",
                transcript_path: Some("/t/a.jsonl"),
                ..Default::default()
            },
        )
        .unwrap();
        ensure_state(
            &c,
            "S1",
            &NewState {
                agent: "codex",
                session_id: "other",
                parent_source_id: Some("P1"),
                transcript_path: Some("/t/b.jsonl"),
                no_distill: true,
                ..Default::default()
            },
        )
        .unwrap();
        let st = by_source(&c, "S1").unwrap().unwrap();
        assert_eq!(
            (
                st.agent.as_str(),
                st.session_id.as_str(),
                st.transcript_path.as_deref()
            ),
            ("claude-code", "s1", Some("/t/a.jsonl"))
        );
        assert_eq!(st.parent_source_id.as_deref(), Some("P1"));
        assert!(st.no_distill);
        ensure_state(
            &c,
            "S1",
            &NewState {
                agent: "claude-code",
                session_id: "s1",
                parent_source_id: Some("P2"),
                no_distill: false,
                ..Default::default()
            },
        )
        .unwrap();
        let st = by_source(&c, "S1").unwrap().unwrap();
        assert_eq!(st.parent_source_id.as_deref(), Some("P1"));
        assert!(st.no_distill, "no_distill never turns off");

        // Task 18b: a backfill's first state is `skipped_backfill`; an existing one keeps its own.
        let backfilled = NewState {
            agent: "claude-code",
            session_id: "s1",
            distill_state: DistillState::SkippedBackfill,
            ..Default::default()
        };
        ensure_state(&c, "S1", &backfilled).unwrap();
        assert_eq!(
            by_source(&c, "S1").unwrap().unwrap().distill_state,
            DistillState::Idle
        );
        source(&c, "S2", "claude-code:session:s2", Some("s2"), None);
        ensure_state(
            &c,
            "S2",
            &NewState {
                session_id: "s2",
                ..backfilled
            },
        )
        .unwrap();
        assert_eq!(
            by_source(&c, "S2").unwrap().unwrap().distill_state,
            DistillState::SkippedBackfill
        );
    }

    #[test]
    fn chunks_from_reads_in_order_from_an_ord() {
        let (_d, c) = conn();
        source(&c, "S1", "claude-code:session:s1", Some("s1"), None);
        for ord in [2, 0, 1, 3] {
            c.execute(
                "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at)
                 VALUES (?1, 'S1', ?2, ?3, ?4, 'h', '2026-10-16T00:00:00.000Z')",
                params![format!("C{ord}"), GLOBAL_ID, ord, format!("text {ord}")],
            )
            .unwrap();
        }
        let got = chunks_from(&c, "S1", 1, 2).unwrap();
        assert_eq!(
            got,
            vec![
                ("C1".to_string(), 1, "text 1".to_string()),
                ("C2".to_string(), 2, "text 2".to_string())
            ]
        );
        assert!(chunks_from(&c, "S1", 4, 10).unwrap().is_empty());
    }

    /// Spec §9.2: an imported transcript of a session (a source with that session id and no
    /// capture state) tells where capture continues; a captured one does not count.
    #[test]
    fn imported_session_end_and_captured_session_ids() {
        let (_d, c) = conn();
        source(
            &c,
            "CAP",
            "claude-code:session:s1",
            Some("s1"),
            Some("2026-10-16T09:00:00.000Z"),
        );
        state(&c, "CAP", "s1");
        source(
            &c,
            "IMP1",
            "file:///palace/s2-a.jsonl",
            Some("s2"),
            Some("2026-10-15T08:00:00.000Z"),
        );
        source(
            &c,
            "IMP2",
            "file:///palace/s2-b.jsonl",
            Some("s2"),
            Some("2026-10-15T09:00:00.000Z"),
        );
        source(&c, "IMP3", "file:///palace/s2-c.jsonl", Some("s2"), None);
        source(&c, "IMP4", "file:///palace/s3.jsonl", Some("s3"), None);
        // Compared as times: 10:00+07:00 is 03:00Z, before 08:00Z (as text it sorts after).
        source(
            &c,
            "IMP5",
            "file:///palace/s4-a.jsonl",
            Some("s4"),
            Some("2026-10-15T10:00:00+07:00"),
        );
        source(
            &c,
            "IMP6",
            "file:///palace/s4-b.jsonl",
            Some("s4"),
            Some("2026-10-15T08:00:00.000Z"),
        );
        source(
            &c,
            "IMP7",
            "file:///palace/s4-c.jsonl",
            Some("s4"),
            Some("soon"),
        );
        // Not transcripts capture would continue: a capture uri without its state (it is the
        // source capture is about to hold), and a file source carrying the session id.
        source(
            &c,
            "CAP2",
            "claude-code:session:s5",
            Some("s5"),
            Some("2026-10-15T08:00:00.000Z"),
        );
        c.execute(
            "INSERT INTO sources (id, project_id, type, uri, session_id, ended_at, created_at)
             VALUES ('DOC', ?1, 'file', 'file:///notes/s5.md', 's5', '2026-10-15T08:00:00.000Z',
                     '2026-10-16T00:00:00.000Z')",
            [GLOBAL_ID],
        )
        .unwrap();
        assert_eq!(
            imported_session_end(&c, "s1", false).unwrap(),
            None,
            "captured, not imported"
        );
        assert_eq!(
            imported_session_end(&c, "s2", false).unwrap(),
            Some(Some("2026-10-15T09:00:00.000Z".into())),
            "the newest end of its imported sources"
        );
        assert_eq!(imported_session_end(&c, "s3", false).unwrap(), Some(None));
        assert_eq!(imported_session_end(&c, "s9", false).unwrap(), None);
        assert_eq!(
            imported_session_end(&c, "s4", false).unwrap(),
            Some(Some("2026-10-15T08:00:00.000Z".into())),
            "the latest as a time; one that does not parse loses to one that does"
        );
        assert_eq!(imported_session_end(&c, "s5", false).unwrap(), None);
        // Capture sources of every agent, and nothing else.
        source(&c, "CX", "codex:session:cx1", Some("cx1"), None);
        ensure_state(
            &c,
            "CX",
            &NewState {
                agent: "codex",
                session_id: "cx1",
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            captured_session_ids(&c).unwrap(),
            HashSet::from(["s1".to_string(), "cx1".to_string()])
        );
    }

    /// Task 18b review, I2: MemPalace files a mined Codex rollout under its file's stem,
    /// `rollout-<time>-<session id>`, so a Codex session asks for that name as well as its id. A
    /// Claude Code session does not, and a name whose id only ends the session's never matches.
    #[test]
    fn a_codex_session_finds_its_rollouts_imported_under_their_names() {
        let (_d, c) = conn();
        let id = "0199e4c1-5b2a-7d10-8e3f-4a5b6c7d8e9f";
        let imported = |src: &str, session: String, end: &str| {
            source(
                &c,
                src,
                &format!("file:///h/.codex/sessions/2026/10/03/{session}.jsonl"),
                Some(session.as_str()),
                Some(end),
            );
        };
        imported(
            "R1",
            format!("rollout-2026-10-03T01-57-34-{id}"),
            "2026-10-03T02:00:00.000Z",
        );
        // A later rollout file of the same session (a resumed one).
        imported(
            "R2",
            format!("rollout-2026-10-04T09-00-00-{id}"),
            "2026-10-04T09:30:00.000Z",
        );
        // Not this session's: another id that ends with this one, a name without the time, and
        // another session's rollout.
        imported(
            "X1",
            format!("rollout-2026-10-05T01-00-00-x-{id}"),
            "2026-10-05T01:00:00.000Z",
        );
        imported("X2", format!("rollout-{id}"), "2026-10-06T01:00:00.000Z");
        imported(
            "X3",
            "rollout-2026-10-07T01-00-00-0199e4c1-0000-7000-8000-000000000000".into(),
            "2026-10-07T01:00:00.000Z",
        );
        assert_eq!(
            imported_session_end(&c, id, true).unwrap(),
            Some(Some("2026-10-04T09:30:00.000Z".into())),
            "the newest end of the session's rollouts"
        );
        assert_eq!(
            imported_session_end(&c, id, false).unwrap(),
            None,
            "only a Codex session asks for rollout names"
        );
        // The bare id still counts for a Codex session.
        imported("B1", id.to_string(), "2026-10-08T00:00:00.000Z");
        assert_eq!(
            imported_session_end(&c, id, true).unwrap(),
            Some(Some("2026-10-08T00:00:00.000Z".into()))
        );
        assert_eq!(
            imported_session_end(&c, id, false).unwrap(),
            Some(Some("2026-10-08T00:00:00.000Z".into()))
        );
    }

    /// `recent` lists by the last capture (a source registered but not captured yet by when it
    /// was registered), newest first; `due` lists due sources whose next try has come, the
    /// longest-waiting first.
    #[test]
    fn recent_and_due_order() {
        let (_d, c) = conn();
        for (id, session) in [("A", "a"), ("B", "b"), ("C", "c")] {
            source(
                &c,
                id,
                &format!("claude-code:session:{session}"),
                Some(session),
                None,
            );
            state(&c, id, session);
        }
        let set = |id: &str, col: &str, v: &str| {
            c.execute(
                &format!("UPDATE capture_state SET {col} = ?2 WHERE source_id = ?1"),
                params![id, v],
            )
            .unwrap();
        };
        set("A", "created_at", "2026-10-16T01:00:00.000Z");
        set("B", "created_at", "2026-10-16T02:00:00.000Z");
        set("C", "created_at", "2026-10-16T03:00:00.000Z");
        set("A", "last_capture_at", "2026-10-16T05:00:00.000Z");
        let ids = |v: Vec<CaptureState>| v.into_iter().map(|s| s.source_id).collect::<Vec<_>>();
        assert_eq!(ids(recent(&c, 10).unwrap()), ["A", "C", "B"]);
        assert_eq!(ids(recent(&c, 1).unwrap()), ["A"]);

        set_distill(
            &c,
            "A",
            DistillState::Due,
            Some("2026-10-16T07:00:00.000Z"),
            1,
        )
        .unwrap();
        set_distill(&c, "B", DistillState::Due, None, 0).unwrap();
        set_distill(
            &c,
            "C",
            DistillState::Due,
            Some("2026-10-16T06:00:00.000Z"),
            1,
        )
        .unwrap();
        assert_eq!(
            ids(due(&c, "2026-10-16T06:30:00.000Z", 10).unwrap()),
            ["B", "C"]
        );
        assert_eq!(
            ids(due(&c, "2026-10-16T08:00:00.000Z", 10).unwrap()),
            ["B", "C", "A"]
        );
        assert_eq!(ids(due(&c, "2026-10-16T08:00:00.000Z", 1).unwrap()), ["B"]);
        set_distill(&c, "B", DistillState::Done, None, 0).unwrap();
        assert_eq!(
            ids(due(&c, "2026-10-16T08:00:00.000Z", 10).unwrap()),
            ["C", "A"]
        );
    }

    /// `distill_ord` never moves back, and `known_turns` reads more keys than one `IN` batch.
    #[test]
    fn distilled_ord_is_monotonic_and_known_turns_batches() {
        let (_d, c) = conn();
        source(&c, "S1", "claude-code:session:s1", Some("s1"), None);
        state(&c, "S1", "s1");
        set_distilled(&c, "S1", 7).unwrap();
        set_distilled(&c, "S1", 3).unwrap();
        assert_eq!(by_source(&c, "S1").unwrap().unwrap().distill_ord, 7);
        let keys: Vec<String> = (0..1_200).map(|i| format!("k{i}")).collect();
        let stored = record_turns(
            &c,
            "claude-code",
            "S1",
            keys.iter().step_by(2).map(|k| (k.as_str(), 0)),
        )
        .unwrap();
        assert_eq!(stored, 600);
        let again = record_turns(&c, "claude-code", "S2", [("k0", 5)]).unwrap();
        assert_eq!(again, 0, "a stored key keeps its first source");
        let known = known_turns(&c, "claude-code", &keys).unwrap();
        assert_eq!(known.len(), 600);
        assert!(known.contains("k1198") && !known.contains("k1199"));
    }

    /// Task 18b review, I1: a backfilled source that goes on live is `idle` again from where the
    /// live part starts, its backfilled subagents are moved past their chunks, and a source that
    /// is not `skipped_backfill` (a live one, or one asked for meanwhile) is left alone.
    #[test]
    fn a_backfilled_source_revives_from_where_it_goes_on() {
        let (_d, c) = conn();
        let st = |id: &str| by_source(&c, id).unwrap().unwrap();
        let backfilled = |id: &str, uri: &str, next_ord: i64| {
            source(&c, id, uri, Some("m1"), None);
            ensure_state(
                &c,
                id,
                &NewState {
                    agent: "claude-code",
                    session_id: "m1",
                    distill_state: DistillState::SkippedBackfill,
                    ..Default::default()
                },
            )
            .unwrap();
            c.execute(
                "UPDATE capture_state SET next_ord = ?2, distill_attempts = 2,
                    distill_next_try_at = '2026-10-16T00:00:00.000Z' WHERE source_id = ?1",
                params![id, next_ord],
            )
            .unwrap();
        };
        backfilled("M1", "claude-code:session:m1", 12);
        backfilled("K1", "claude-code:session:m1:agent:k1", 5);
        backfilled("K2", "claude-code:session:m1:agent:k2", 7);
        // A subagent that ran live already: its chunks are the live part's.
        source(
            &c,
            "K3",
            "claude-code:session:m1:agent:k3",
            Some("m1"),
            None,
        );
        state(&c, "K3", "m1");
        c.execute(
            "UPDATE capture_state SET next_ord = 4 WHERE source_id = 'K3'",
            [],
        )
        .unwrap();
        for k in ["K1", "K2", "K3"] {
            c.execute(
                "UPDATE capture_state SET parent_source_id = 'M1' WHERE source_id = ?1",
                [k],
            )
            .unwrap();
        }
        // K2 went on live first: revived by its own pass, from its own cursor.
        assert_eq!(revive_backfilled(&c, "K2", 6).unwrap(), 1);

        assert_eq!(revive_backfilled(&c, "M1", 12).unwrap(), 1);
        let m1 = st("M1");
        assert_eq!(
            (m1.distill_state, m1.distill_ord, m1.distill_attempts),
            (DistillState::Idle, 12, 0)
        );
        assert_eq!(m1.distill_next_try_at, None);
        let k1 = st("K1");
        assert_eq!(
            (k1.distill_state, k1.distill_ord),
            (DistillState::Idle, 5),
            "a backfilled subagent is past its chunks"
        );
        assert_eq!(
            (st("K2").distill_state, st("K2").distill_ord),
            (DistillState::Idle, 6),
            "a subagent revived by its own pass keeps its live part"
        );
        assert_eq!(
            (st("K3").distill_state, st("K3").distill_ord),
            (DistillState::Idle, 0),
            "a live subagent keeps every chunk"
        );

        // Not backfilled (any more): nothing changes, its subagents neither.
        set_distill(&c, "M1", DistillState::Due, None, 0).unwrap();
        c.execute(
            "UPDATE capture_state SET distill_state = 'skipped_backfill', distill_ord = 0
             WHERE source_id = 'K1'",
            [],
        )
        .unwrap();
        assert_eq!(revive_backfilled(&c, "M1", 20).unwrap(), 0);
        assert_eq!(
            (st("M1").distill_state, st("M1").distill_ord),
            (DistillState::Due, 12)
        );
        assert_eq!(
            (st("K1").distill_state, st("K1").distill_ord),
            (DistillState::SkippedBackfill, 0)
        );
        assert_eq!(revive_backfilled(&c, "NOPE", 1).unwrap(), 0);
    }

    /// SP2 Task 12: what the distill worker reads. Subagents by parent, main sources by
    /// session, turn starts without late tails, sources by state and next try, quiet sources
    /// with undistilled chunks (their own or a subagent's), and stale open sources.
    #[test]
    fn distill_worker_reads() {
        let (_d, c) = conn();
        let set = |id: &str, col: &str, v: &dyn rusqlite::ToSql| {
            c.execute(
                &format!("UPDATE capture_state SET {col} = ?2 WHERE source_id = ?1"),
                params![id, v],
            )
            .unwrap();
        };
        for (id, uri, session) in [
            ("M1", "claude-code:session:m1", "m1"),
            ("M2", "claude-code:session:m2", "m2"),
            ("M3", "claude-code:session:m3", "m3"),
            ("K1", "claude-code:session:m1:agent:k1", "m1"),
            ("K2", "claude-code:session:m1:agent:k2", "m1"),
        ] {
            source(&c, id, uri, Some(session), None);
            state(&c, id, session);
        }
        for (id, created) in [
            ("M1", "2026-10-16T01:00:00.000Z"),
            ("M2", "2026-10-16T02:00:00.000Z"),
            ("M3", "2026-10-16T03:00:00.000Z"),
            ("K2", "2026-10-16T04:00:00.000Z"),
            ("K1", "2026-10-16T05:00:00.000Z"),
        ] {
            set(id, "created_at", &created);
        }
        set("K1", "parent_source_id", &"M1");
        set("K2", "parent_source_id", &"M1");
        let ids = |v: Vec<CaptureState>| v.into_iter().map(|s| s.source_id).collect::<Vec<_>>();
        assert_eq!(
            ids(children(&c, "M1").unwrap()),
            ["K2", "K1"],
            "oldest first"
        );
        assert!(children(&c, "M2").unwrap().is_empty());
        assert_eq!(
            ids(sessions(&c, "m1").unwrap()),
            ["M1"],
            "main sources only"
        );

        record_turns(
            &c,
            "claude-code",
            "M1",
            [("a", 0), ("b", 3), ("tail:c", 5), ("d", 9)],
        )
        .unwrap();
        record_turns(&c, "claude-code", "M2", [("e", 4)]).unwrap();
        assert_eq!(
            turn_starts(&c, "M1", 0, 8).unwrap(),
            HashSet::from([0, 3]),
            "in range, no tail, no other source"
        );

        set_distill(
            &c,
            "M2",
            DistillState::SkippedCloud,
            Some("2026-10-16T06:00:00.000Z"),
            0,
        )
        .unwrap();
        set_distill(&c, "M3", DistillState::SkippedCloud, None, 0).unwrap();
        assert_eq!(
            ids(in_state(&c, DistillState::SkippedCloud, None, 10).unwrap()),
            ["M3", "M2"]
        );
        assert_eq!(
            ids(in_state(
                &c,
                DistillState::SkippedCloud,
                Some("2026-10-16T05:00:00.000Z"),
                10
            )
            .unwrap()),
            ["M3"],
            "a next try still to come waits"
        );

        // Quiet: M1 has undistilled chunks of its own; M2 is skipped_cloud; M3 has nothing.
        set_distill(&c, "M2", DistillState::Idle, None, 0).unwrap();
        set_distill(&c, "M3", DistillState::Done, None, 0).unwrap();
        set("M1", "next_ord", &4);
        set("M1", "last_capture_at", &"2026-10-16T07:00:00.000Z");
        set("M2", "last_capture_at", &"2026-10-16T07:00:00.000Z");
        set("M3", "last_capture_at", &"2026-10-16T07:00:00.000Z");
        let quiet = |at: &str| -> Vec<String> {
            quiet_undistilled(&c, at, "2026-10-10T00:00:00.000Z", 10)
                .unwrap()
                .into_iter()
                .map(|(id, agent)| {
                    assert_eq!(agent, "claude-code");
                    id
                })
                .collect()
        };
        assert_eq!(quiet("2026-10-16T08:00:00.000Z"), ["M1"]);
        assert!(
            quiet("2026-10-16T06:00:00.000Z").is_empty(),
            "not quiet yet"
        );
        // A subagent that captured later keeps the session from being quiet.
        set("K1", "last_capture_at", &"2026-10-16T09:00:00.000Z");
        assert!(quiet("2026-10-16T08:00:00.000Z").is_empty());
        assert_eq!(quiet("2026-10-16T10:00:00.000Z"), ["M1"]);
        // Distilled to the end: only a subagent's undistilled chunks make it due again.
        set_distilled(&c, "M1", 4).unwrap();
        assert!(quiet("2026-10-16T10:00:00.000Z").is_empty());
        set("K2", "next_ord", &2);
        assert_eq!(quiet("2026-10-16T10:00:00.000Z"), ["M1"]);
        // A skipped subagent's chunks do not count.
        set_distill(&c, "K2", DistillState::SkippedCloud, None, 0).unwrap();
        assert!(quiet("2026-10-16T10:00:00.000Z").is_empty());
        set_distill(&c, "K2", DistillState::Idle, None, 0).unwrap();
        // Captured before `recent_since` (a week back, say): left for `alice capture distill`.
        assert!(
            quiet_undistilled(
                &c,
                "2026-10-16T10:00:00.000Z",
                "2026-10-16T09:30:00.000Z",
                10
            )
            .unwrap()
            .is_empty(),
            "neither M1 (07:00) nor its subagents (K1 09:00) captured since 09:30"
        );
        assert_eq!(
            quiet_undistilled(
                &c,
                "2026-10-16T10:00:00.000Z",
                "2026-10-16T08:30:00.000Z",
                10
            )
            .unwrap()
            .len(),
            1,
            "K1 captured at 09:00, so the session is recent"
        );
        // A due source is not quiet-triggered again.
        set_distill(&c, "M1", DistillState::Due, None, 0).unwrap();
        assert!(quiet("2026-10-16T10:00:00.000Z").is_empty());

        // Stale: open sources by their last capture (or registration).
        set_ended(&c, "M3").unwrap();
        assert_eq!(
            stale_open(&c, "2026-10-16T07:30:00.000Z", 10).unwrap(),
            ["K2", "M1", "M2"],
            "K2 never captured (registered 04:00); M1 and M2 at 07:00; M3 ended; K1 later"
        );
        assert_eq!(
            stale_open(&c, "2026-10-16T04:30:00.000Z", 10).unwrap(),
            ["K2"]
        );
    }

    /// Task 11b (c), spec §6.2 and §10: the stale end makes a main source due when it holds
    /// chunks not distilled yet (its own, or a subagent's that is not skipped) and it or a
    /// subagent captured at `recent_since` or later. A due source keeps its next try, a
    /// backfilled one waits to be asked, and a subagent is never marked itself.
    #[test]
    fn the_stale_end_makes_a_recent_main_source_due() {
        let (_d, c) = conn();
        let set = |id: &str, col: &str, v: &dyn rusqlite::ToSql| {
            c.execute(
                &format!("UPDATE capture_state SET {col} = ?2 WHERE source_id = ?1"),
                params![id, v],
            )
            .unwrap();
        };
        for (id, uri, session) in [
            ("R", "claude-code:session:r", "r"),
            ("O", "claude-code:session:o", "o"),
            ("K", "claude-code:session:o:agent:k", "o"),
        ] {
            source(&c, id, uri, Some(session), None);
            state(&c, id, session);
            set(id, "created_at", &"2026-09-01T00:00:00.000Z");
        }
        set("K", "parent_source_id", &"O");
        let since = "2026-10-08T00:00:00.000Z";
        let st = |id: &str| by_source(&c, id).unwrap().unwrap();
        let due = |id: &str| due_after_stale_end(&c, id, since).unwrap();

        // Captured two days before the end, three chunks undistilled: due, at once.
        set("R", "next_ord", &3);
        set("R", "last_capture_at", &"2026-10-14T00:00:00.000Z");
        set_distill(
            &c,
            "R",
            DistillState::Failed,
            Some("2026-10-20T00:00:00.000Z"),
            5,
        )
        .unwrap();
        assert_eq!(due("R"), 1);
        let r = st("R");
        assert_eq!(
            (r.distill_state, r.distill_next_try_at, r.distill_attempts),
            (DistillState::Due, None, 0)
        );
        // Due already: it keeps its next try.
        set_distill(
            &c,
            "R",
            DistillState::Due,
            Some("2026-10-20T00:00:00.000Z"),
            2,
        )
        .unwrap();
        assert_eq!(due("R"), 0);
        assert_eq!(st("R").distill_attempts, 2);
        // Backfilled: it waits to be asked.
        set_distill(&c, "R", DistillState::SkippedBackfill, None, 0).unwrap();
        assert_eq!(due("R"), 0);
        // Distilled to the end: nothing to do.
        set_distill(&c, "R", DistillState::Done, None, 0).unwrap();
        set_distilled(&c, "R", 3).unwrap();
        assert_eq!(due("R"), 0);

        // Captured a month before: not marked, so turning distillation on makes no backlog.
        set("O", "next_ord", &3);
        set("O", "last_capture_at", &"2026-09-16T00:00:00.000Z");
        assert_eq!(due("O"), 0);
        assert_eq!(st("O").distill_state, DistillState::Idle);
        // A subagent that captured within the week makes the session recent; the subagent itself
        // is never marked.
        set("K", "last_capture_at", &"2026-10-14T00:00:00.000Z");
        assert_eq!(due("K"), 0);
        assert_eq!(due("O"), 1);
        assert_eq!(st("K").distill_state, DistillState::Idle);
        // Only a subagent's undistilled chunks: due, unless the subagent is skipped.
        set_distilled(&c, "O", 3).unwrap();
        set_distill(&c, "O", DistillState::Idle, None, 0).unwrap();
        assert_eq!(due("O"), 0, "nothing undistilled");
        set("K", "next_ord", &2);
        set_distill(&c, "K", DistillState::SkippedExcluded, None, 0).unwrap();
        assert_eq!(due("O"), 0, "a skipped subagent's chunks do not count");
        set_distill(&c, "K", DistillState::Idle, None, 0).unwrap();
        assert_eq!(due("O"), 1);
        assert_eq!(due("NOPE"), 0);
    }

    /// Task 12 fix round 1: due sources newest capture first, and chunks bounded by a cursor.
    #[test]
    fn due_newest_first_and_chunks_below_a_cursor() {
        let (_d, c) = conn();
        for (id, session, at) in [
            ("A", "a", "2026-10-16T01:00:00.000Z"),
            ("B", "b", "2026-10-16T03:00:00.000Z"),
            ("C", "c", "2026-10-16T02:00:00.000Z"),
        ] {
            source(
                &c,
                id,
                &format!("claude-code:session:{session}"),
                Some(session),
                None,
            );
            state(&c, id, session);
            c.execute(
                "UPDATE capture_state SET last_capture_at = ?2 WHERE source_id = ?1",
                params![id, at],
            )
            .unwrap();
            set_distill(&c, id, DistillState::Due, None, 0).unwrap();
        }
        set_distill(
            &c,
            "C",
            DistillState::Due,
            Some("2026-10-16T09:00:00.000Z"),
            1,
        )
        .unwrap();
        let ids = |v: Vec<CaptureState>| v.into_iter().map(|s| s.source_id).collect::<Vec<_>>();
        assert_eq!(
            ids(due_newest(&c, "2026-10-16T08:00:00.000Z", 10).unwrap()),
            ["B", "A"]
        );
        assert_eq!(
            ids(due_newest(&c, "2026-10-16T10:00:00.000Z", 10).unwrap()),
            ["B", "C", "A"]
        );
        for ord in 0..5 {
            c.execute(
                "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at)
                 VALUES (?1, 'A', ?2, ?3, 't', 'h', '2026-10-16T00:00:00.000Z')",
                params![format!("A{ord}"), GLOBAL_ID, ord],
            )
            .unwrap();
        }
        let ords = |v: Vec<(String, i64, String)>| v.into_iter().map(|r| r.1).collect::<Vec<_>>();
        assert_eq!(ords(chunks_between(&c, "A", 1, 4, 10).unwrap()), [1, 2, 3]);
        assert_eq!(ords(chunks_between(&c, "A", 1, 4, 2).unwrap()), [1, 2]);
        assert!(chunks_between(&c, "A", 3, 3, 10).unwrap().is_empty());
    }
}
