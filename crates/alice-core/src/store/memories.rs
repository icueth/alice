//! Knowledge-tier memories.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::{Author, Kind, Memory, Origin, Status, limits};
use crate::normalize::{content_hash, derive_title, nfc, normalize_topic};
use crate::store::fts;
use crate::store::projects::{self, ResolvedProject};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::hash_map::Entry;

#[derive(Debug, Clone, Default)]
pub struct NewMemoryInput {
    pub content: String,
    pub kind: Option<Kind>,
    pub title: Option<String>,
    pub topics: Vec<String>,
    pub importance: Option<u8>,
    pub supersedes: Vec<String>,
    pub idempotency_key: Option<String>,
    pub author: Author,
    pub legacy_id: Option<String>,
    /// Import only: keep the original creation time.
    pub created_at: Option<String>,
    /// The project is `global` only because nothing named one (no project, no working
    /// directory): the judge may route the memory to a project later.
    pub project_defaulted: bool,
    /// How the memory came to exist. Internal: never taken from a request.
    pub origin: Origin,
    /// The ids of the raw chunks the memory was distilled from, recorded with it in
    /// `provenance` (an id with no chunk is not recorded). Internal: never taken from a request.
    pub provenance: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PreparedMemory {
    pub id: String,
    pub project_id: String,
    pub project_slug: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub topics: Vec<String>,
    pub importance: u8,
    pub content_hash: String,
    pub author: Author,
    pub idempotency_key: Option<String>,
    pub supersedes: Vec<String>,
    pub created_at: String,
    pub hlc: String,
    pub legacy_id: Option<String>,
    /// The writer gave a kind (0 in the database when `kind` defaulted to `note`: the judge may
    /// classify it).
    pub kind_explicit: bool,
    pub project_defaulted: bool,
    pub origin: Origin,
    /// Chunk ids, without blanks or repeats.
    pub provenance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    New { id: String, seq: i64 },
    Duplicate { id: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    #[default]
    Edit,
    Supersede,
    Archive,
    Forget,
    Restore,
    /// Move to another project (`project`). Not offered over MCP: the schema leaves it out.
    #[schemars(skip)]
    Move,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MemoryUpdate {
    pub action: UpdateAction,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub topics: Option<Vec<String>>,
    #[serde(default)]
    pub importance: Option<u8>,
    /// For `supersede`: the id of the memory that replaces this one.
    #[serde(default)]
    pub superseded_by: Option<String>,
    /// For `move`: the slug (or alias) of the project the memory goes to. Not offered over MCP.
    #[serde(default)]
    #[schemars(skip)]
    pub project: Option<String>,
    /// For `move`: the memory's text as the destination's privacy rules leave it, set by the
    /// daemon after it screened the text (never by a caller: not part of any request). The
    /// writer refuses the move if the memory changed since, and stores this text with it.
    #[serde(skip)]
    #[schemars(skip)]
    pub screened: Option<ScreenedMove>,
}

/// A memory's text screened with the privacy rules of the project it is moving to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenedMove {
    /// `content_hash` and `updated_at` of the memory that was screened: topics and importance
    /// are not in the hash.
    pub content_hash: String,
    pub updated_at: String,
    pub title: String,
    pub body: String,
    pub topics: Vec<String>,
}

fn normalize_topics(raw: &[String]) -> Result<Vec<String>> {
    if raw.len() > limits::MAX_TOPICS {
        return Err(AliceError::invalid(format!(
            "too many topics ({}); use at most {} short tags",
            raw.len(),
            limits::MAX_TOPICS
        )));
    }
    let mut out: Vec<String> = raw.iter().filter_map(|t| normalize_topic(t)).collect();
    out.sort();
    out.dedup();
    Ok(out)
}

fn check_importance(i: Option<u8>) -> Result<u8> {
    match i.unwrap_or(3) {
        v @ 1..=5 => Ok(v),
        v => Err(AliceError::invalid(format!(
            "importance {v} is out of range; use 1 to 5 (3 is normal)"
        ))),
    }
}

/// A given title is trimmed and NFC-normalised; a missing or blank one becomes the body's first line.
fn resolve_title(given: Option<&str>, body: &str) -> Result<String> {
    match given.map(|t| nfc(t.trim())).filter(|t| !t.is_empty()) {
        Some(t) if t.chars().count() > limits::MAX_TITLE_CHARS => {
            Err(AliceError::invalid(format!(
                "title is longer than {} characters; shorten it",
                limits::MAX_TITLE_CHARS
            )))
        }
        Some(t) => Ok(t),
        None => Ok(derive_title(body)),
    }
}

fn check_content_len(what: &str, text: &str) -> Result<()> {
    if text.len() > limits::MAX_CONTENT_BYTES {
        return Err(AliceError::invalid(format!(
            "{what} is {} bytes; the limit is {} bytes. Save a shorter summary or split it into several memories",
            text.len(),
            limits::MAX_CONTENT_BYTES
        )));
    }
    Ok(())
}

/// Validate and normalise a new memory. Nothing is written.
pub fn prepare(
    input: NewMemoryInput,
    project: &ResolvedProject,
    hlc: String,
) -> Result<PreparedMemory> {
    let content = nfc(input.content.trim());
    if content.is_empty() {
        return Err(AliceError::invalid(
            "content is empty; send the text to remember",
        ));
    }
    check_content_len("content", &content)?;
    let title = resolve_title(input.title.as_deref(), &content)?;
    let kind = input.kind.unwrap_or(Kind::Note);
    let topics = normalize_topics(&input.topics)?;
    let importance = check_importance(input.importance)?;
    let hash = content_hash(&project.slug, kind, &title, &content);
    let mut provenance: Vec<String> = Vec::with_capacity(input.provenance.len());
    for c in input.provenance {
        let c = c.trim();
        if !c.is_empty() && !provenance.iter().any(|p| p == c) {
            provenance.push(c.to_string());
        }
    }
    Ok(PreparedMemory {
        id: new_id(),
        project_id: project.id.clone(),
        project_slug: project.slug.clone(),
        kind,
        title,
        body: content,
        topics,
        importance,
        content_hash: hash,
        author: input.author,
        idempotency_key: input.idempotency_key.filter(|k| !k.is_empty()),
        supersedes: input.supersedes,
        created_at: input.created_at.unwrap_or_else(now_rfc3339),
        hlc,
        legacy_id: input.legacy_id,
        kind_explicit: input.kind.is_some(),
        project_defaulted: input.project_defaulted,
        origin: input.origin,
        provenance,
    })
}

pub fn seq_of(conn: &Connection, id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT seq FROM memories WHERE id = ?1 OR legacy_id = ?1 ORDER BY seq LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// The id of the memory `id` names (its id or a legacy id, as `seq_of` resolves it).
pub fn id_of(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM memories WHERE id = ?1 OR legacy_id = ?1 ORDER BY seq LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn set_status_by_seq(conn: &Connection, seq: i64, status: Status) -> Result<()> {
    set_status(conn, seq, status, None)
}

/// A `pending` memory becomes `active`: the end of its write path, not a change to it (undo's
/// comparison holds the two states equal too). `updated_at` and `hlc` stay as the write left
/// them, so a move screened while the memory was pending is not refused as an edit.
pub fn activate_pending(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute(
        "UPDATE memories SET status = 'active' WHERE seq = ?1 AND status = 'pending'",
        [seq],
    )?;
    Ok(())
}

/// Status change; `hlc` stamps it for sync when the change comes from a user or agent action.
fn set_status(conn: &Connection, seq: i64, status: Status, hlc: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE memories SET status = ?1, updated_at = ?2, hlc = COALESCE(?3, hlc) WHERE seq = ?4",
        params![status.as_str(), now_rfc3339(), hlc, seq],
    )?;
    Ok(())
}

/// The dedup index (`project_id`, `content_hash`) skips trashed rows, so a row that edits its
/// text or leaves the trash must not take the content of another live memory.
pub(crate) fn ensure_unique_content(
    conn: &Connection,
    seq: i64,
    project_id: &str,
    hash: &str,
) -> Result<()> {
    let other: Option<String> = conn
        .query_row(
            "SELECT id FROM memories WHERE project_id = ?1 AND content_hash = ?2 AND status <> 'trashed' AND seq <> ?3",
            params![project_id, hash, seq],
            |r| r.get(0),
        )
        .optional()?;
    match other {
        Some(other) => Err(AliceError::invalid(format!(
            "memory {other} already holds this text in the project; use or update that memory instead"
        ))),
        None => Ok(()),
    }
}

/// Add a link `from` → `to` of type `kind` (`supersedes`, `duplicate_of`, `contradicts`, ...),
/// made by `by`. An existing identical link is left as it is.
pub fn link(conn: &Connection, from: &str, to: &str, kind: &str, by: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO links (from_id, to_id, type, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from, to, kind, by, now_rfc3339()],
    )?;
    Ok(())
}

/// An explicit memory beats a distilled one, exact copies included (SP2 final review M1): when
/// `id` is a distilled memory that exact dedupe found (any status but trashed: active, pending,
/// superseded or archived) and its exact copy at hand is not distilled, the memory becomes the
/// copy's: its origin, author and `kind_explicit`, with `updated_at` and `hlc` moved. Its status
/// stays as it is. A distilled copy changes nothing. No history row is recorded here (the writer
/// records events).
fn promote_distilled(
    conn: &Connection,
    id: &str,
    origin: Origin,
    author: &Author,
    kind_explicit: bool,
    hlc: &str,
) -> Result<()> {
    if origin == Origin::Distill {
        return Ok(());
    }
    conn.execute(
        "UPDATE memories SET origin = ?2, author_agent = ?3, author_model = ?4, author_session = ?5,
            kind_explicit = ?6, updated_at = ?7, hlc = ?8
         WHERE id = ?1 AND origin = 'distill'",
        params![
            id,
            origin.as_str(),
            author.agent,
            author.model,
            author.session,
            kind_explicit,
            now_rfc3339(),
            hlc
        ],
    )?;
    Ok(())
}

/// Insert a prepared memory (status `pending`) with its topics, provenance and FTS rows.
/// Exact duplicates and repeated idempotency keys return the existing id and write nothing,
/// except that an explicit copy of a distilled memory makes it explicit (`promote_distilled`).
/// Each id in `supersedes` gets a `supersedes` link and becomes `superseded`; unknown ids are
/// skipped and trashed memories keep their status. Call inside a transaction (the writer does).
pub fn insert(conn: &Connection, m: &PreparedMemory) -> Result<InsertOutcome> {
    if let Some(key) = &m.idempotency_key
        && let Some(id) = conn
            .query_row(
                "SELECT id FROM memories WHERE idempotency_key = ?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
    {
        return Ok(InsertOutcome::Duplicate { id });
    }
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM memories WHERE project_id = ?1 AND content_hash = ?2 AND status <> 'trashed'",
            params![m.project_id, m.content_hash],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        promote_distilled(conn, &id, m.origin, &m.author, m.kind_explicit, &m.hlc)?;
        return Ok(InsertOutcome::Duplicate { id });
    }
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO memories (id, project_id, kind, title, body, importance, status, content_hash,
            author_agent, author_model, author_session, idempotency_key, created_at, updated_at, hlc, legacy_id,
            kind_explicit, project_defaulted, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            m.id,
            m.project_id,
            m.kind.as_str(),
            m.title,
            m.body,
            m.importance,
            m.content_hash,
            m.author.agent,
            m.author.model,
            m.author.session,
            m.idempotency_key,
            m.created_at,
            now,
            m.hlc,
            m.legacy_id,
            m.kind_explicit,
            m.project_defaulted,
            m.origin.as_str()
        ],
    )?;
    let seq = conn.last_insert_rowid();
    for t in &m.topics {
        conn.execute(
            "INSERT OR IGNORE INTO memory_topics (memory_id, topic) VALUES (?1, ?2)",
            params![m.id, t],
        )?;
    }
    // Only chunks that exist: a provenance row must lead to text.
    for chunk in &m.provenance {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO provenance (memory_id, chunk_id) SELECT ?1, id FROM chunks WHERE id = ?2",
        )?
        .execute(params![m.id, chunk])?;
    }
    fts::index_memory(conn, seq, &m.title, &m.body, &m.topics)?;
    let by = m.author.agent.clone().unwrap_or_else(|| "user".into());
    for old in &m.supersedes {
        let Some(old_seq) = seq_of(conn, old)?.filter(|s| *s != seq) else {
            continue;
        };
        let (old_id, old_status): (String, String) = conn.query_row(
            "SELECT id, status FROM memories WHERE seq = ?1",
            [old_seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // A forgotten memory stays in the trash: reviving it could clash with live content.
        if old_status != Status::Trashed.as_str() {
            set_status(conn, old_seq, Status::Superseded, Some(&m.hlc))?;
        }
        link(conn, &m.id, &old_id, "supersedes", &by)?;
    }
    Ok(InsertOutcome::New {
        id: m.id.clone(),
        seq,
    })
}

const SELECT_MEMORY: &str = "SELECT m.seq, m.id, p.slug, m.kind, m.title, m.body, m.importance, m.status,
    m.author_agent, m.author_model, m.author_session, m.created_at, m.updated_at, m.access_count, m.legacy_id,
    m.origin
    FROM memories m JOIN projects p ON p.id = m.project_id";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, Memory)> {
    let kind: String = r.get(3)?;
    let status: String = r.get(7)?;
    let origin: String = r.get(15)?;
    Ok((
        r.get(0)?,
        Memory {
            id: r.get(1)?,
            project: r.get(2)?,
            kind: Kind::parse(&kind).unwrap_or(Kind::Note),
            title: r.get(4)?,
            body: r.get(5)?,
            topics: Vec::new(),
            importance: r.get(6)?,
            status: Status::parse(&status).unwrap_or(Status::Active),
            author: Author {
                agent: r.get(8)?,
                model: r.get(9)?,
                session: r.get(10)?,
            },
            created_at: r.get(11)?,
            updated_at: r.get(12)?,
            access_count: r.get(13)?,
            legacy_id: r.get(14)?,
            origin: Origin::parse(&origin).unwrap_or_default(),
        },
    ))
}

fn load_topics(conn: &Connection, m: &mut Memory) -> Result<()> {
    let mut stmt =
        conn.prepare_cached("SELECT topic FROM memory_topics WHERE memory_id = ?1 ORDER BY topic")?;
    m.topics = stmt
        .query_map([&m.id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(())
}

/// Fetch by id or legacy id, in the order requested. Unknown ids are skipped.
pub fn get_many(conn: &Connection, ids: &[String]) -> Result<Vec<Memory>> {
    let sql = format!("{SELECT_MEMORY} WHERE m.id = ?1 OR m.legacy_id = ?1 ORDER BY m.seq LIMIT 1");
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some((_, mut m)) = conn.query_row(&sql, [id], read_row).optional()? {
            load_topics(conn, &mut m)?;
            out.push(m);
        }
    }
    Ok(out)
}

/// SQL that holds for every memory but a distilled one whose judge stage is not over: recall and
/// the briefing show only the memories it holds for (SP2 spec §6.6). `table` qualifies the
/// columns: `"m."`, or `""` for the bare table.
pub fn judged_sql(table: &str) -> String {
    format!("NOT ({table}origin = 'distill' AND {table}judge_state <> 'done')")
}

/// Bound parameters per `IN (...)` query, well under SQLite's limit of 32,766.
pub(crate) const IN_BATCH: usize = 4_096;

/// Fetch by internal sequence numbers (search results). Order follows the input (first
/// occurrence); unknown seqs are skipped. A recall passes up to ~2,000 seqs here, so the rows
/// and their topics are read with one `IN` query each (per 4,096 seqs) and nothing is quadratic.
pub fn get_by_seqs(conn: &Connection, seqs: &[i64]) -> Result<Vec<(i64, Memory)>> {
    let mut first_pos: HashMap<i64, usize> = HashMap::with_capacity(seqs.len());
    let mut unique: Vec<i64> = Vec::with_capacity(seqs.len());
    for (i, &s) in seqs.iter().enumerate() {
        if let Entry::Vacant(e) = first_pos.entry(s) {
            e.insert(i);
            unique.push(s);
        }
    }
    let mut rows: Vec<(i64, Memory)> = Vec::with_capacity(unique.len());
    for batch in unique.chunks(IN_BATCH) {
        let sql = format!(
            "{SELECT_MEMORY} WHERE m.seq IN ({})",
            vec!["?"; batch.len()].join(",")
        );
        let mut stmt = conn.prepare(&sql)?;
        for row in stmt.query_map(params_from_iter(batch.iter()), read_row)? {
            rows.push(row?);
        }
    }
    rows.sort_by_key(|(s, _)| first_pos[s]);

    let mut topics: HashMap<String, Vec<String>> = HashMap::new();
    for batch in rows.chunks(IN_BATCH) {
        let sql = format!(
            "SELECT memory_id, topic FROM memory_topics WHERE memory_id IN ({}) ORDER BY memory_id, topic",
            vec!["?"; batch.len()].join(",")
        );
        let mut stmt = conn.prepare(&sql)?;
        let found = stmt.query_map(params_from_iter(batch.iter().map(|(_, m)| &m.id)), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in found {
            let (id, topic) = row?;
            topics.entry(id).or_default().push(topic);
        }
    }
    for (_, m) in rows.iter_mut() {
        m.topics = topics.remove(&m.id).unwrap_or_default();
    }
    Ok(rows)
}

pub fn touch_access(conn: &Connection, seqs: &[i64]) -> Result<()> {
    let now = now_rfc3339();
    let mut stmt = conn.prepare_cached(
        "UPDATE memories SET access_count = access_count + 1, last_accessed_at = ?1 WHERE seq = ?2",
    )?;
    for s in seqs {
        stmt.execute(params![now, s])?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved,
    /// The target project already holds this text in a live memory (its id).
    DuplicateOf(String),
}

/// `others` become trashed duplicates of `survivor`: each gets a `duplicate_of` link to it and
/// the survivor gains their provenance. The survivor itself and unknown ids are skipped, and so
/// is an id already trashed. Returns the ids that were merged. (Provenance is not part of an
/// event's snapshot: undoing a merge restores status and links, and the provenance rows copied
/// to the survivor stay there, which is harmless.)
pub fn merge_into(
    conn: &Connection,
    survivor_id: &str,
    others: &[String],
    hlc: &str,
) -> Result<Vec<String>> {
    let survivor_seq = seq_of(conn, survivor_id)?.ok_or_else(|| {
        AliceError::NotFound(format!("memory {survivor_id} (survivor); check the id"))
    })?;
    let (survivor, survivor_status): (String, String) = conn.query_row(
        "SELECT id, status FROM memories WHERE seq = ?1",
        [survivor_seq],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if survivor_status == Status::Trashed.as_str() {
        return Err(AliceError::invalid(format!(
            "memory {survivor} is in the trash and cannot be the survivor of a merge; restore it first or pick another"
        )));
    }
    let mut merged = Vec::new();
    for o in others {
        let Some(seq) = seq_of(conn, o)?.filter(|s| *s != survivor_seq) else {
            continue;
        };
        let (id, status): (String, String) = conn.query_row(
            "SELECT id, status FROM memories WHERE seq = ?1",
            [seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if status == Status::Trashed.as_str() || merged.contains(&id) {
            continue;
        }
        set_status(conn, seq, Status::Trashed, Some(hlc))?;
        link(conn, &id, &survivor, "duplicate_of", "merge")?;
        conn.execute(
            "INSERT OR IGNORE INTO provenance (memory_id, chunk_id) SELECT ?1, chunk_id FROM provenance WHERE memory_id = ?2",
            params![survivor, id],
        )?;
        merged.push(id);
    }
    Ok(merged)
}

/// Move a memory to another project, unless that project already holds the same text.
pub fn move_to_project(
    conn: &Connection,
    id: &str,
    project_id: &str,
    hlc: &str,
) -> Result<MoveOutcome> {
    let seq = seq_of(conn, id)?.ok_or_else(|| AliceError::NotFound(format!("memory {id}")))?;
    let (kind, title, body): (String, String, String) = conn.query_row(
        "SELECT kind, title, body FROM memories WHERE seq = ?1",
        [seq],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let slug: String = conn
        .query_row(
            "SELECT slug FROM projects WHERE id = ?1",
            [project_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| AliceError::NotFound(format!("project {project_id}")))?;
    let hash = content_hash(
        &slug,
        Kind::parse(&kind).unwrap_or(Kind::Note),
        &title,
        &body,
    );
    if let Some(other) = conn
        .query_row(
            "SELECT id FROM memories WHERE project_id = ?1 AND content_hash = ?2 AND status <> 'trashed' AND seq <> ?3",
            params![project_id, hash, seq],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        // The mover's owner takes over an exact distilled copy (the caller merges into it).
        let (origin, agent, model, session, kind_explicit): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            bool,
        ) = conn.query_row(
            "SELECT origin, author_agent, author_model, author_session, kind_explicit
             FROM memories WHERE seq = ?1",
            [seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        promote_distilled(
            conn,
            &other,
            Origin::parse(&origin).unwrap_or_default(),
            &Author {
                agent,
                model,
                session,
            },
            kind_explicit,
            hlc,
        )?;
        return Ok(MoveOutcome::DuplicateOf(other));
    }
    conn.execute(
        "UPDATE memories SET project_id = ?1, content_hash = ?2, updated_at = ?3, hlc = ?4, project_defaulted = 0 WHERE seq = ?5",
        params![project_id, hash, now_rfc3339(), hlc, seq],
    )?;
    Ok(MoveOutcome::Moved)
}

/// Apply an edit or status change and return the updated memory.
/// Everything is validated before the first write.
pub fn apply_update(conn: &Connection, id: &str, u: &MemoryUpdate, hlc: String) -> Result<Memory> {
    let seq = seq_of(conn, id)?.ok_or_else(|| {
        AliceError::NotFound(format!(
            "memory {id}; check the id returned by recall or remember"
        ))
    })?;
    let (_, current) = get_by_seqs(conn, &[seq])?
        .pop()
        .ok_or_else(|| AliceError::NotFound(format!("memory {id}")))?;
    let (project_id, stored_hash): (String, String) = conn.query_row(
        "SELECT project_id, content_hash FROM memories WHERE seq = ?1",
        [seq],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let status_change = |target: Status| -> Result<()> {
        if current.status == Status::Trashed && target != Status::Trashed {
            ensure_unique_content(conn, seq, &project_id, &stored_hash)?;
        }
        set_status(conn, seq, target, Some(&hlc))
    };
    match u.action {
        UpdateAction::Edit => {
            let body = match u.body.as_deref() {
                Some(b) => nfc(b.trim()),
                None => current.body.clone(),
            };
            if body.is_empty() {
                return Err(AliceError::invalid(
                    "body cannot be empty; use action \"forget\" to remove a memory",
                ));
            }
            check_content_len("body", &body)?;
            let title = match u.title.as_deref() {
                Some(t) => resolve_title(Some(t), &body)?,
                // A title that was derived from the old body follows the new one.
                None if current.title == derive_title(&current.body) => derive_title(&body),
                None => current.title.clone(),
            };
            let kind = u.kind.unwrap_or(current.kind);
            let importance = match u.importance {
                Some(i) => check_importance(Some(i))?,
                None => current.importance,
            };
            let new_topics = u.topics.as_deref().map(normalize_topics).transpose()?;
            let hash = content_hash(&current.project, kind, &title, &body);
            if current.status != Status::Trashed {
                ensure_unique_content(conn, seq, &project_id, &hash)?;
            }
            let text_changed = title != current.title || body != current.body;
            conn.execute(
                "UPDATE memories SET title=?1, body=?2, kind=?3, importance=?4, content_hash=?5, updated_at=?6, hlc=?7,
                    embed_state = CASE WHEN ?8 THEN 'pending' ELSE embed_state END,
                    kind_explicit = CASE WHEN ?10 THEN 1 ELSE kind_explicit END WHERE seq=?9",
                params![title, body, kind.as_str(), importance, hash, now_rfc3339(), hlc, text_changed, seq, u.kind.is_some()],
            )?;
            if let Some(topics) = &new_topics {
                conn.execute(
                    "DELETE FROM memory_topics WHERE memory_id = ?1",
                    [&current.id],
                )?;
                for topic in topics {
                    conn.execute(
                        "INSERT INTO memory_topics (memory_id, topic) VALUES (?1, ?2)",
                        params![current.id, topic],
                    )?;
                }
            }
            let topics = new_topics.as_ref().unwrap_or(&current.topics);
            fts::unindex_memory(conn, seq)?;
            fts::index_memory(conn, seq, &title, &body, topics)?;
        }
        UpdateAction::Supersede => {
            let by = u.superseded_by.as_deref().ok_or_else(|| {
                AliceError::invalid(
                    "supersede needs superseded_by: the id of the memory that replaces this one",
                )
            })?;
            let by_seq = seq_of(conn, by)?.ok_or_else(|| {
                AliceError::NotFound(format!(
                    "memory {by} (superseded_by); pass the id of the newer memory"
                ))
            })?;
            if by_seq == seq {
                return Err(AliceError::invalid(
                    "a memory cannot supersede itself; pass the id of the newer memory in superseded_by",
                ));
            }
            let by_id: String =
                conn.query_row("SELECT id FROM memories WHERE seq = ?1", [by_seq], |r| {
                    r.get(0)
                })?;
            status_change(Status::Superseded)?;
            link(conn, &by_id, &current.id, "supersedes", "user")?;
        }
        UpdateAction::Archive => status_change(Status::Archived)?,
        UpdateAction::Forget => status_change(Status::Trashed)?,
        UpdateAction::Restore => {
            status_change(Status::Active)?;
            // Restored by hand, it is no longer a merged duplicate: `get` stops redirecting.
            conn.execute(
                "DELETE FROM links WHERE from_id = ?1 AND type = 'duplicate_of'",
                [&current.id],
            )?;
        }
        UpdateAction::Move => {
            let raw = u.project.as_deref().ok_or_else(|| {
                AliceError::invalid("move needs project: the slug of the project to move it to")
            })?;
            let slug = projects::move_slug(raw)?;
            let target = projects::resolve_move_target(conn, &slug)?.ok_or_else(|| {
                AliceError::NotFound(format!("project {slug}; moving into it creates it"))
            })?;
            match move_to_project(conn, &current.id, &target.id, &hlc)? {
                MoveOutcome::Moved => {}
                MoveOutcome::DuplicateOf(other) => {
                    return Err(AliceError::invalid(format!(
                        "project {} already holds the same text as memory {other}; nothing was moved",
                        target.slug
                    )));
                }
            }
        }
    }
    let (_, updated) = get_by_seqs(conn, &[seq])?
        .pop()
        .ok_or_else(|| AliceError::NotFound(id.to_string()))?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Kind, Status, limits};
    use crate::project::ProjectRef;
    use crate::store::{open_writer, projects};

    fn setup() -> (
        tempfile::TempDir,
        rusqlite::Connection,
        projects::ResolvedProject,
    ) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(
            &c,
            &ProjectRef {
                key: "k".into(),
                slug: "app".into(),
                name: "app".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        (d, c, p)
    }

    fn input(content: &str) -> NewMemoryInput {
        NewMemoryInput {
            content: content.into(),
            ..Default::default()
        }
    }

    /// Provenance names chunks that exist: an id with no chunk behind it is not recorded, and
    /// blanks and repeats are dropped before the insert.
    #[test]
    fn provenance_records_only_chunks_that_exist() {
        let (_d, c, p) = setup();
        c.execute(
            "INSERT INTO sources (id, project_id, type, uri, created_at)
             VALUES ('S1', ?1, 'session', 'claude-code:session:s1', '2026-10-16T00:00:00.000Z')",
            [&p.id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at)
             VALUES ('C1', 'S1', ?1, 0, 'turn', 'h', '2026-10-16T00:00:00.000Z')",
            [&p.id],
        )
        .unwrap();
        let mut i = input("We distil only what Jev chose");
        i.origin = Origin::Distill;
        i.provenance = vec!["C1".into(), " C1 ".into(), "".into(), "C-MISSING".into()];
        let m = prepare(i, &p, "h".into()).unwrap();
        assert_eq!(m.provenance, ["C1", "C-MISSING"]);
        let InsertOutcome::New { id, .. } = insert(&c, &m).unwrap() else {
            panic!("expected new")
        };
        let rows: Vec<String> = c
            .prepare("SELECT chunk_id FROM provenance WHERE memory_id = ?1")
            .unwrap()
            .query_map([&id], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows, ["C1"], "a missing chunk id is not recorded");
        assert_eq!(
            get_many(&c, std::slice::from_ref(&id)).unwrap()[0].origin,
            Origin::Distill
        );
    }

    /// `(origin, author agent, model, session, kind_explicit, updated_at, hlc)` of a memory.
    type Owner = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
        String,
        String,
    );

    fn owner(c: &rusqlite::Connection, id: &str) -> Owner {
        c.query_row(
            "SELECT origin, author_agent, author_model, author_session, kind_explicit, updated_at, hlc
             FROM memories WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap()
    }

    fn distilled_input(content: &str) -> NewMemoryInput {
        let mut i = input(content);
        i.kind = Some(Kind::Note);
        i.origin = Origin::Distill;
        i.author = Author {
            agent: Some("claude-code".into()),
            session: Some("S1".into()),
            model: None,
        };
        i
    }

    fn explicit_input(content: &str) -> NewMemoryInput {
        let mut i = input(content);
        i.author = Author {
            agent: Some("codex".into()),
            model: Some("gpt-x".into()),
            session: Some("S2".into()),
        };
        i
    }

    /// SP2 final M1: an explicit memory beats a distilled one, exact copies included. An
    /// explicit write of the text a live distilled memory holds returns that memory as the
    /// duplicate and makes it the writer's: origin, author and `kind_explicit` become the
    /// newcomer's, and `updated_at` and `hlc` move. A distilled write of an explicit memory's
    /// text, or of another distilled memory's, changes nothing.
    #[test]
    fn an_explicit_copy_promotes_a_distilled_memory_and_not_the_other_way() {
        let (_d, c, p) = setup();
        let text = "Deploys run blue green on Fridays";
        let InsertOutcome::New { id, .. } = insert(
            &c,
            &prepare(distilled_input(text), &p, "h1".into()).unwrap(),
        )
        .unwrap() else {
            panic!("expected new")
        };
        let before = owner(&c, &id);
        assert_eq!((before.0.as_str(), before.4), ("distill", true));
        // A distilled copy of a distilled memory: nothing changes.
        let again = insert(
            &c,
            &prepare(distilled_input(text), &p, "h2".into()).unwrap(),
        )
        .unwrap();
        assert_eq!(again, InsertOutcome::Duplicate { id: id.clone() });
        assert_eq!(owner(&c, &id), before);
        std::thread::sleep(std::time::Duration::from_millis(5));
        // The same text and kind, written by an agent that gave no kind (`note` by default).
        let out = insert(&c, &prepare(explicit_input(text), &p, "h3".into()).unwrap()).unwrap();
        assert_eq!(out, InsertOutcome::Duplicate { id: id.clone() });
        let after = owner(&c, &id);
        assert_eq!(
            (
                after.0.as_str(),
                after.1.as_deref(),
                after.2.as_deref(),
                after.3.as_deref(),
                after.4,
                after.6.as_str()
            ),
            (
                "agent",
                Some("codex"),
                Some("gpt-x"),
                Some("S2"),
                false,
                "h3"
            )
        );
        assert_ne!(after.5, before.5, "updated_at moves");
        let rows: i64 = c
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        // The other way round: a distilled copy of an explicit memory changes nothing.
        let InsertOutcome::New { id: mine, .. } = insert(
            &c,
            &prepare(
                explicit_input("Releases are tagged from main"),
                &p,
                "h4".into(),
            )
            .unwrap(),
        )
        .unwrap() else {
            panic!("expected new")
        };
        let kept = owner(&c, &mine);
        let copy = insert(
            &c,
            &prepare(
                distilled_input("Releases are tagged from main"),
                &p,
                "h5".into(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(copy, InsertOutcome::Duplicate { id: mine.clone() });
        assert_eq!(owner(&c, &mine), kept);
    }

    /// SP2 final M1 on the move path: an explicit memory moved into a project that holds its
    /// exact distilled copy makes that copy the writer's (the mover then merges into it); a
    /// distilled memory moved onto an explicit copy changes nothing.
    #[test]
    fn moving_an_explicit_memory_onto_a_distilled_copy_promotes_the_copy() {
        let (_d, c, app) = setup();
        let lib = projects::upsert_project(
            &c,
            &ProjectRef {
                key: "k/lib".into(),
                slug: "lib".into(),
                name: "lib".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        let new_in = |p: &projects::ResolvedProject, i: NewMemoryInput, hlc: &str| -> String {
            match insert(&c, &prepare(i, p, hlc.into()).unwrap()).unwrap() {
                InsertOutcome::New { id, .. } => id,
                other => panic!("expected new, got {other:?}"),
            }
        };
        let text = "The cache is write-through";
        let copy = new_in(&lib, distilled_input(text), "h1");
        let mut mine = explicit_input(text);
        mine.kind = Some(Kind::Note);
        let mine = new_in(&app, mine, "h2");
        assert_eq!(
            move_to_project(&c, &mine, &lib.id, "h3").unwrap(),
            MoveOutcome::DuplicateOf(copy.clone())
        );
        let promoted = owner(&c, &copy);
        assert_eq!(
            (
                promoted.0.as_str(),
                promoted.1.as_deref(),
                promoted.2.as_deref(),
                promoted.3.as_deref(),
                promoted.4,
                promoted.6.as_str()
            ),
            (
                "agent",
                Some("codex"),
                Some("gpt-x"),
                Some("S2"),
                true,
                "h3"
            )
        );
        // The other way round: nothing changes.
        let text = "The queue is at-least-once";
        let mut theirs = explicit_input(text);
        theirs.kind = Some(Kind::Note);
        let theirs = new_in(&lib, theirs, "h4");
        let kept = owner(&c, &theirs);
        let auto = new_in(&app, distilled_input(text), "h5");
        assert_eq!(
            move_to_project(&c, &auto, &lib.id, "h6").unwrap(),
            MoveOutcome::DuplicateOf(theirs.clone())
        );
        assert_eq!(owner(&c, &theirs), kept);
    }

    #[test]
    fn insert_then_get_round_trips() {
        let (_d, c, p) = setup();
        let mut i = input("Use WAL mode\nbecause readers must not block the writer");
        i.kind = Some(Kind::Decision);
        i.topics = vec!["SQLite".into(), " storage ".into()];
        i.author = Author {
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let m = prepare(i, &p, "h".into()).unwrap();
        let InsertOutcome::New { id, .. } = insert(&c, &m).unwrap() else {
            panic!("expected new")
        };
        let got = &get_many(&c, std::slice::from_ref(&id)).unwrap()[0];
        assert_eq!(got.title, "Use WAL mode");
        assert_eq!(got.kind, Kind::Decision);
        assert_eq!(got.status, Status::Pending);
        assert_eq!(
            got.topics,
            vec!["sqlite".to_string(), "storage".to_string()]
        );
        assert_eq!(got.project, "app");
        assert_eq!(got.author.agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn the_judge_flags_record_whether_the_kind_and_the_project_were_given() {
        let (_d, c, p) = setup();
        let flags = |c: &rusqlite::Connection, id: &str| -> (i64, i64) {
            c.query_row(
                "SELECT kind_explicit, project_defaulted FROM memories WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let mut given = input("a decision with a kind");
        given.kind = Some(Kind::Decision);
        let mut defaulted = input("a note without a kind");
        defaulted.project_defaulted = true;
        let ids: Vec<String> = [given, defaulted]
            .into_iter()
            .map(
                |i| match insert(&c, &prepare(i, &p, "h".into()).unwrap()).unwrap() {
                    InsertOutcome::New { id, .. } => id,
                    other => panic!("{other:?}"),
                },
            )
            .collect();
        assert_eq!(
            flags(&c, &ids[0]),
            (1, 0),
            "an explicit kind, a known project"
        );
        assert_eq!(
            flags(&c, &ids[1]),
            (0, 1),
            "no kind given, no project known"
        );
    }

    #[test]
    fn a_chosen_kind_or_project_is_never_the_judges_to_change() {
        let (_d, c, p) = setup();
        let mut i = input("kind and project were not given");
        i.project_defaulted = true;
        let InsertOutcome::New { id, seq } =
            insert(&c, &prepare(i, &p, "h".into()).unwrap()).unwrap()
        else {
            panic!()
        };
        let flags = |c: &rusqlite::Connection| -> (i64, i64) {
            c.query_row(
                "SELECT kind_explicit, project_defaulted FROM memories WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(flags(&c), (0, 1));
        // An edit that does not name a kind leaves the flag alone; one that does makes it explicit.
        let edit = |kind| MemoryUpdate {
            action: UpdateAction::Edit,
            importance: Some(4),
            kind,
            ..Default::default()
        };
        apply_update(&c, &id, &edit(None), "h".into()).unwrap();
        assert_eq!(flags(&c), (0, 1));
        apply_update(&c, &id, &edit(Some(Kind::Decision)), "h".into()).unwrap();
        assert_eq!(flags(&c), (1, 1));
        // Moving it anywhere is a choice about the project.
        let other = projects::upsert_project(
            &c,
            &ProjectRef {
                key: "k2".into(),
                slug: "other".into(),
                name: "other".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        move_to_project(&c, &id, &other.id, "h").unwrap();
        assert_eq!(flags(&c), (1, 0));
    }

    #[test]
    fn exact_duplicates_and_idempotency_keys_return_existing_id() {
        let (_d, c, p) = setup();
        let a = insert(&c, &prepare(input("same  text"), &p, "h".into()).unwrap()).unwrap();
        let b = insert(&c, &prepare(input("same text"), &p, "h".into()).unwrap()).unwrap();
        let InsertOutcome::New { id, .. } = a else {
            panic!()
        };
        assert_eq!(b, InsertOutcome::Duplicate { id: id.clone() });

        let mut k1 = input("first wording");
        k1.idempotency_key = Some("retry-1".into());
        let mut k2 = input("second wording");
        k2.idempotency_key = Some("retry-1".into());
        let InsertOutcome::New { id: kid, .. } =
            insert(&c, &prepare(k1, &p, "h".into()).unwrap()).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            insert(&c, &prepare(k2, &p, "h".into()).unwrap()).unwrap(),
            InsertOutcome::Duplicate { id: kid }
        );
    }

    #[test]
    fn oversized_and_invalid_inputs_are_rejected_with_the_limit() {
        let (_d, _c, p) = setup();
        let big = "x".repeat(limits::MAX_CONTENT_BYTES + 1);
        let err = prepare(input(&big), &p, "h".into())
            .unwrap_err()
            .to_string();
        assert!(err.contains("65536"), "{err}");
        assert!(prepare(input("   "), &p, "h".into()).is_err());
        let mut bad = input("ok");
        bad.importance = Some(9);
        assert!(
            prepare(bad, &p, "h".into())
                .unwrap_err()
                .to_string()
                .contains("1 to 5")
        );
        let mut many = input("ok");
        many.topics = (0..25).map(|i| format!("t{i}")).collect();
        assert!(
            prepare(many, &p, "h".into())
                .unwrap_err()
                .to_string()
                .contains("20")
        );
    }

    #[test]
    fn supersede_on_insert_and_update_actions() {
        let (_d, c, p) = setup();
        let InsertOutcome::New { id: old, .. } =
            insert(&c, &prepare(input("port is 8080"), &p, "h".into()).unwrap()).unwrap()
        else {
            panic!()
        };
        let mut newer = input("port is 9090");
        newer.supersedes = vec![old.clone()];
        let InsertOutcome::New { id: new_id, .. } =
            insert(&c, &prepare(newer, &p, "h".into()).unwrap()).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            get_many(&c, std::slice::from_ref(&old)).unwrap()[0].status,
            Status::Superseded
        );
        let link: i64 = c
            .query_row(
                "SELECT count(*) FROM links WHERE from_id=?1 AND to_id=?2 AND type='supersedes'",
                [&new_id, &old],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(link, 1);

        let edited = apply_update(
            &c,
            &new_id,
            &MemoryUpdate {
                action: UpdateAction::Edit,
                body: Some("port is 9091".into()),
                ..Default::default()
            },
            "h2".into(),
        )
        .unwrap();
        assert_eq!(edited.body, "port is 9091");
        let forgotten = apply_update(
            &c,
            &new_id,
            &MemoryUpdate {
                action: UpdateAction::Forget,
                ..Default::default()
            },
            "h3".into(),
        )
        .unwrap();
        assert_eq!(forgotten.status, Status::Trashed);
        let restored = apply_update(
            &c,
            &new_id,
            &MemoryUpdate {
                action: UpdateAction::Restore,
                ..Default::default()
            },
            "h4".into(),
        )
        .unwrap();
        assert_eq!(restored.status, Status::Active);
        assert!(apply_update(&c, "NOPE", &MemoryUpdate::default(), "h".into()).is_err());
    }

    #[test]
    fn fts_rows_follow_edits() {
        let (_d, c, p) = setup();
        let InsertOutcome::New { id, seq } = insert(
            &c,
            &prepare(input("HostLinkReconnect retries"), &p, "h".into()).unwrap(),
        )
        .unwrap() else {
            panic!()
        };
        let hits = |q: &str| -> i64 {
            c.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hits("\"reconnect\""), 1);
        apply_update(
            &c,
            &id,
            &MemoryUpdate {
                action: UpdateAction::Edit,
                body: Some("totally different".into()),
                title: Some("other".into()),
                ..Default::default()
            },
            "h".into(),
        )
        .unwrap();
        assert_eq!(hits("\"reconnect\""), 0);
        assert_eq!(hits("\"different\""), 1);
        let _ = seq;
    }

    fn count(c: &rusqlite::Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn add(
        c: &rusqlite::Connection,
        p: &projects::ResolvedProject,
        content: &str,
    ) -> (String, i64) {
        let InsertOutcome::New { id, seq } =
            insert(c, &prepare(input(content), p, "h".into()).unwrap()).unwrap()
        else {
            panic!("expected a new memory for {content:?}")
        };
        (id, seq)
    }

    fn edit(body: Option<&str>, title: Option<&str>) -> MemoryUpdate {
        MemoryUpdate {
            action: UpdateAction::Edit,
            body: body.map(Into::into),
            title: title.map(Into::into),
            ..Default::default()
        }
    }

    fn action(a: UpdateAction) -> MemoryUpdate {
        MemoryUpdate {
            action: a,
            ..Default::default()
        }
    }

    fn invalid_text(r: Result<Memory>) -> String {
        match r {
            Err(AliceError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn oversized_remember_is_an_invalid_error_naming_the_limit_and_writes_nothing() {
        let (_d, c, p) = setup();
        let big = "ข้อมูล ".repeat(200 * 1024 / "ข้อมูล ".len());
        match prepare(input(&big), &p, "h".into()) {
            Err(AliceError::Invalid(msg)) => {
                assert!(msg.contains("65536") && msg.contains("split"), "{msg}")
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
        let mut long_title = input("ok");
        long_title.title = Some("t".repeat(limits::MAX_TITLE_CHARS + 1));
        match prepare(long_title, &p, "h".into()) {
            Err(AliceError::Invalid(msg)) => assert!(msg.contains("200"), "{msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert_eq!(count(&c, "SELECT count(*) FROM memories"), 0);
    }

    #[test]
    fn edits_enforce_the_limits_before_writing_anything() {
        let (_d, c, p) = setup();
        let (id, _) = add(&c, &p, "original body");
        let too_long = "t".repeat(limits::MAX_TITLE_CHARS + 1);
        assert!(
            invalid_text(apply_update(
                &c,
                &id,
                &edit(None, Some(&too_long)),
                "h".into()
            ))
            .contains("200")
        );
        let big = "x".repeat(limits::MAX_CONTENT_BYTES + 1);
        assert!(
            invalid_text(apply_update(&c, &id, &edit(Some(&big), None), "h".into()))
                .contains("65536")
        );
        let mut many = edit(Some("changed body"), None);
        many.topics = Some((0..25).map(|i| format!("t{i}")).collect());
        assert!(invalid_text(apply_update(&c, &id, &many, "h".into())).contains("20"));
        let got = &get_many(&c, std::slice::from_ref(&id)).unwrap()[0];
        assert_eq!(
            (got.title.as_str(), got.body.as_str()),
            ("original body", "original body"),
            "a rejected edit changes nothing"
        );
        assert_eq!(
            count(
                &c,
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH '\"changed\"'"
            ),
            0
        );

        let blank = apply_update(
            &c,
            &id,
            &edit(Some("new first line\nmore"), Some("   ")),
            "h".into(),
        )
        .unwrap();
        assert_eq!(
            blank.title, "new first line",
            "a blank title falls back to the first line"
        );
    }

    #[test]
    fn a_body_edit_refreshes_a_derived_title_but_keeps_a_chosen_one() {
        let (_d, c, p) = setup();
        let (derived, _) = add(&c, &p, "port is 8080");
        let got =
            apply_update(&c, &derived, &edit(Some("port is 9091"), None), "h".into()).unwrap();
        assert_eq!(got.title, "port is 9091");

        let mut chosen = input("listen on 8080");
        chosen.title = Some("API port".into());
        let InsertOutcome::New { id, .. } =
            insert(&c, &prepare(chosen, &p, "h".into()).unwrap()).unwrap()
        else {
            panic!()
        };
        let got = apply_update(&c, &id, &edit(Some("listen on 9091"), None), "h".into()).unwrap();
        assert_eq!(got.title, "API port");
    }

    #[test]
    fn changes_that_would_duplicate_a_live_memory_name_it() {
        let (_d, c, p) = setup();
        let (alpha, _) = add(&c, &p, "alpha");
        let (beta, _) = add(&c, &p, "beta");
        let msg = invalid_text(apply_update(
            &c,
            &beta,
            &edit(Some("alpha"), Some("alpha")),
            "h".into(),
        ));
        assert!(msg.contains(&alpha), "{msg}");
        assert_eq!(
            get_many(&c, std::slice::from_ref(&beta)).unwrap()[0].body,
            "beta"
        );

        // Trashed rows are outside the dedup index, so the same text can be saved again;
        // restoring the trashed one must then explain the clash instead of failing in SQLite.
        let (gamma, _) = add(&c, &p, "gamma");
        apply_update(&c, &gamma, &action(UpdateAction::Forget), "h".into()).unwrap();
        let (gamma2, _) = add(&c, &p, "gamma");
        for a in [UpdateAction::Restore, UpdateAction::Archive] {
            let msg = invalid_text(apply_update(&c, &gamma, &action(a), "h".into()));
            assert!(msg.contains(&gamma2), "{a:?}: {msg}");
        }
        assert_eq!(get_many(&c, &[gamma]).unwrap()[0].status, Status::Trashed);
    }

    #[test]
    fn status_changes_record_the_hlc_for_sync() {
        let (_d, c, p) = setup();
        let hlc_of = |id: &str| -> String {
            c.query_row("SELECT hlc FROM memories WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        let (id, _) = add(&c, &p, "will be archived");
        apply_update(
            &c,
            &id,
            &action(UpdateAction::Archive),
            "0000000000009-0000-dev".into(),
        )
        .unwrap();
        assert_eq!(hlc_of(&id), "0000000000009-0000-dev");

        let (old, _) = add(&c, &p, "port is 1");
        let mut newer = input("port is 2");
        newer.supersedes = vec![old.clone()];
        insert(
            &c,
            &prepare(newer, &p, "0000000000010-0000-dev".into()).unwrap(),
        )
        .unwrap();
        assert_eq!(hlc_of(&old), "0000000000010-0000-dev");
    }

    #[test]
    fn a_memory_cannot_supersede_itself() {
        let (_d, c, p) = setup();
        let (id, _) = add(&c, &p, "self");
        let own = MemoryUpdate {
            action: UpdateAction::Supersede,
            superseded_by: Some(id.clone()),
            ..Default::default()
        };
        assert!(invalid_text(apply_update(&c, &id, &own, "h".into())).contains("itself"));
        assert_eq!(get_many(&c, &[id]).unwrap()[0].status, Status::Pending);
    }

    #[test]
    fn superseding_a_trashed_memory_leaves_it_in_the_trash() {
        let (_d, c, p) = setup();
        // Forget a memory, then save the same text again as its replacement.
        let (gone, _) = add(&c, &p, "port is 8080");
        apply_update(&c, &gone, &action(UpdateAction::Forget), "h".into()).unwrap();
        let mut again = input("port is 8080");
        again.supersedes = vec![gone.clone()];
        let InsertOutcome::New { id: new_id, .. } =
            insert(&c, &prepare(again, &p, "h".into()).unwrap()).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            get_many(&c, std::slice::from_ref(&gone)).unwrap()[0].status,
            Status::Trashed
        );
        let link: i64 = c
            .query_row(
                "SELECT count(*) FROM links WHERE from_id=?1 AND to_id=?2 AND type='supersedes'",
                [&new_id, &gone],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(link, 1);
    }

    #[test]
    fn reads_follow_the_requested_order_and_resolve_legacy_ids() {
        let (_d, c, p) = setup();
        let (a, sa) = add(&c, &p, "first");
        let mut legacy = input("second");
        legacy.legacy_id = Some("drawer_42".into());
        let InsertOutcome::New { id: b, seq: sb } =
            insert(&c, &prepare(legacy, &p, "h".into()).unwrap()).unwrap()
        else {
            panic!()
        };
        let seqs: Vec<i64> = get_by_seqs(&c, &[sb, 999, sa])
            .unwrap()
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(seqs, vec![sb, sa]);
        let ids: Vec<String> = get_many(&c, &["drawer_42".into(), "missing".into(), a.clone()])
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec![b.clone(), a.clone()]);
        assert_eq!(seq_of(&c, "drawer_42").unwrap(), Some(sb));
        assert_eq!(id_of(&c, "drawer_42").unwrap(), Some(b.clone()));
        assert_eq!(id_of(&c, &a).unwrap(), Some(a));
        assert_eq!(id_of(&c, "missing").unwrap(), None);
        touch_access(&c, &[sb, sb]).unwrap();
        assert_eq!(get_many(&c, &[b]).unwrap()[0].access_count, 2);
    }

    /// Search hands over thousands of seqs, repeated and unknown ones included: every known
    /// memory comes back once, at its first position, with its own topics.
    #[test]
    fn get_by_seqs_keeps_first_order_and_topics_across_batches() {
        let (_d, c, p) = setup();
        let mut seqs = Vec::new();
        for i in 0..(IN_BATCH + 50) {
            let mut m = input(&format!("memory number {i}"));
            if i % 3 == 0 {
                m.topics = vec![format!("t{i}"), "shared".into()];
            }
            let InsertOutcome::New { seq, .. } =
                insert(&c, &prepare(m, &p, "h".into()).unwrap()).unwrap()
            else {
                panic!()
            };
            seqs.push(seq);
        }
        // Newest first, every seq twice, plus seqs that do not exist.
        let mut asked: Vec<i64> = seqs.iter().rev().copied().collect();
        asked.extend(seqs.iter().copied());
        asked.insert(7, 1_000_000);
        asked.push(-5);
        let got = get_by_seqs(&c, &asked).unwrap();
        let want: Vec<i64> = seqs.iter().rev().copied().collect();
        assert_eq!(got.iter().map(|(s, _)| *s).collect::<Vec<_>>(), want);
        for (seq, m) in &got {
            let i = seqs.iter().position(|s| s == seq).unwrap();
            assert_eq!(m.body, format!("memory number {i}"));
            if i % 3 == 0 {
                let mut expected = vec![format!("t{i}"), "shared".to_string()];
                expected.sort();
                assert_eq!(m.topics, expected, "seq {seq}");
            } else {
                assert!(m.topics.is_empty(), "seq {seq}: {:?}", m.topics);
            }
        }
        assert!(get_by_seqs(&c, &[]).unwrap().is_empty());
        assert!(get_by_seqs(&c, &[1_000_000]).unwrap().is_empty());
    }

    #[test]
    fn merge_trashes_the_others_with_links_and_moves_provenance() {
        let (_d, c, p) = setup();
        let ins = |t: &str| match insert(&c, &prepare(input(t), &p, "h".into()).unwrap()).unwrap() {
            InsertOutcome::New { id, .. } => id,
            o => panic!("{o:?}"),
        };
        let (s, a, b) = (ins("survivor text"), ins("dup one"), ins("dup two"));
        c.execute(
            "INSERT INTO provenance (memory_id, chunk_id) VALUES (?1, 'CH1')",
            [&a],
        )
        .unwrap();
        let merged = merge_into(&c, &s, &[a.clone(), b.clone(), s.clone()], "h2").unwrap();
        assert_eq!(
            merged,
            vec![a.clone(), b.clone()],
            "the survivor is never merged into itself"
        );
        for id in [&a, &b] {
            let st: String = c
                .query_row("SELECT status FROM memories WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(st, "trashed");
            let n: i64 = c.query_row("SELECT count(*) FROM links WHERE from_id = ?1 AND to_id = ?2 AND type = 'duplicate_of'", [id, &s], |r| r.get(0)).unwrap();
            assert_eq!(n, 1);
        }
        let prov: i64 = c
            .query_row(
                "SELECT count(*) FROM provenance WHERE memory_id = ?1 AND chunk_id = 'CH1'",
                [&s],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(prov, 1);
        // Merging again changes nothing; a trashed survivor is refused.
        assert!(
            merge_into(&c, &s, std::slice::from_ref(&a), "h3")
                .unwrap()
                .is_empty()
        );
        let err = merge_into(&c, &a, std::slice::from_ref(&s), "h4")
            .unwrap_err()
            .to_string();
        assert!(err.contains("trash"), "{err}");
        assert!(matches!(
            merge_into(&c, "nope", &[s], "h5"),
            Err(AliceError::NotFound(_))
        ));
    }

    #[test]
    fn moving_to_a_project_that_holds_the_same_text_reports_the_duplicate() {
        let (_d, c, p) = setup();
        let other = crate::store::projects::upsert_project(
            &c,
            &crate::project::ProjectRef {
                key: "k2".into(),
                slug: "other".into(),
                name: "other".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        let m1 = prepare(input("same text"), &p, "h".into()).unwrap();
        let InsertOutcome::New { id: a, .. } = insert(&c, &m1).unwrap() else {
            panic!()
        };
        let m2 = prepare(input("same text"), &other, "h".into()).unwrap();
        let InsertOutcome::New { id: b, .. } = insert(&c, &m2).unwrap() else {
            panic!()
        };
        assert_eq!(
            move_to_project(&c, &a, &other.id, "h3").unwrap(),
            MoveOutcome::DuplicateOf(b)
        );
        let m3 = prepare(input("unique text"), &p, "h".into()).unwrap();
        let InsertOutcome::New { id: u, .. } = insert(&c, &m3).unwrap() else {
            panic!()
        };
        assert_eq!(
            move_to_project(&c, &u, &other.id, "h4").unwrap(),
            MoveOutcome::Moved
        );
        let slug: String = c.query_row("SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1", [&u], |r| r.get(0)).unwrap();
        assert_eq!(slug, "other");
    }
}
