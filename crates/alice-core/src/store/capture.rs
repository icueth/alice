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
pub fn imported_session_end(conn: &Connection, session_id: &str) -> Result<Option<Option<String>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.ended_at FROM sources s
         WHERE s.session_id = ?1 AND s.type = 'session'
           AND s.uri NOT LIKE 'claude-code:session:%' AND s.uri NOT LIKE 'codex:session:%'
           AND NOT EXISTS (SELECT 1 FROM capture_state c WHERE c.source_id = s.id)",
    )?;
    let ends: Vec<Option<String>> = stmt
        .query_map([session_id], |r| r.get(0))?
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
}

/// Create the state of `source_id` if it has none. An existing one keeps its values, except
/// that a parent it lacked is filled in and `no_distill` only ever turns on (a client flag that
/// tightens).
pub fn ensure_state(conn: &Connection, source_id: &str, s: &NewState<'_>) -> Result<()> {
    let created = conn
        .prepare_cached(
            "INSERT OR IGNORE INTO capture_state
                (source_id, agent, session_id, parent_source_id, transcript_path, file_key, no_distill, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            source_id,
            s.agent,
            s.session_id,
            s.parent_source_id,
            s.transcript_path,
            s.file_key,
            s.no_distill,
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

pub fn set_ended(conn: &Connection, source_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE capture_state SET ended = 1 WHERE source_id = ?1",
        [source_id],
    )?)
}

/// A tenant client sent from beyond the cursor: the cursor moves there and the gap is counted.
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
            imported_session_end(&c, "s1").unwrap(),
            None,
            "captured, not imported"
        );
        assert_eq!(
            imported_session_end(&c, "s2").unwrap(),
            Some(Some("2026-10-15T09:00:00.000Z".into())),
            "the newest end of its imported sources"
        );
        assert_eq!(imported_session_end(&c, "s3").unwrap(), Some(None));
        assert_eq!(imported_session_end(&c, "s9").unwrap(), None);
        assert_eq!(
            imported_session_end(&c, "s4").unwrap(),
            Some(Some("2026-10-15T08:00:00.000Z".into())),
            "the latest as a time; one that does not parse loses to one that does"
        );
        assert_eq!(imported_session_end(&c, "s5").unwrap(), None);
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
}
