//! Read-only queries for the MemPalace compatibility layer: the overview an old `status` tool
//! shows, drawer ids of every era resolved to what they name now, and the details a search hit
//! does not carry, and an agent's diary entries. Every query is bounded; nothing here writes.

use crate::error::Result;
use crate::model::Memory;
use crate::store::import::legacy_get;
use crate::store::memories::get_many;
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use std::collections::{BTreeMap, HashMap};

/// Rooms listed by [`overview`] (most memories first); a palace of thousands of one-off topics
/// must not make `status` large.
pub const MAX_ROOMS: usize = 200;

/// Wings (project aliases) listed by [`wing_aliases`].
pub const MAX_ALIASES: usize = 100;

/// Ids per `IN (...)` query of [`hit_details`].
const IN_BATCH: usize = 500;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overview {
    /// Live memories (everything but the trashed).
    pub memories: u64,
    pub chunks: u64,
    /// Project slug to memories plus chunks.
    pub wings: BTreeMap<String, u64>,
    /// Topic to memories, the top [`MAX_ROOMS`] by count.
    pub rooms: BTreeMap<String, u64>,
}

/// Two `GROUP BY` queries for the wings (memories and chunks per project) and one for the rooms.
pub fn overview(conn: &Connection) -> Result<Overview> {
    let mut out = Overview::default();
    let mut stmt = conn.prepare(
        "SELECT p.slug, count(*) FROM memories m JOIN projects p ON p.id = m.project_id
         WHERE m.status <> 'trashed' GROUP BY p.slug",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (slug, n) = row?;
        out.memories += n as u64;
        *out.wings.entry(slug).or_default() += n as u64;
    }
    let mut stmt = conn.prepare(
        "SELECT p.slug, count(*) FROM chunks c JOIN projects p ON p.id = c.project_id GROUP BY p.slug",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (slug, n) = row?;
        out.chunks += n as u64;
        *out.wings.entry(slug).or_default() += n as u64;
    }
    out.rooms = rooms(conn, None)?;
    Ok(out)
}

/// Topic to live memories, of one project (by id) or of all; the top [`MAX_ROOMS`] by count.
pub fn rooms(conn: &Connection, project_id: Option<&str>) -> Result<BTreeMap<String, u64>> {
    let mut stmt = conn.prepare(
        "SELECT t.topic, count(*) AS n FROM memory_topics t JOIN memories m ON m.id = t.memory_id
         WHERE m.status <> 'trashed' AND (?1 IS NULL OR m.project_id = ?1)
         GROUP BY t.topic ORDER BY n DESC, t.topic LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![project_id, MAX_ROOMS as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (topic, n) = row?;
        out.insert(topic, n as u64);
    }
    Ok(out)
}

/// Alias to project slug (an old wing name and what it became), at most [`MAX_ALIASES`].
pub fn wing_aliases(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut stmt = conn.prepare(
        "SELECT a.alias, p.slug FROM project_aliases a JOIN projects p ON p.id = a.project_id
         WHERE a.alias <> p.slug ORDER BY a.alias LIMIT ?1",
    )?;
    let rows = stmt.query_map([MAX_ALIASES as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// What a drawer id names in Alice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatDrawer {
    /// A memory (its Alice id).
    Memory(String),
    Chunk {
        id: String,
        project_slug: String,
        text: String,
        source_uri: String,
        ord: i64,
        created_at: String,
    },
}

fn chunk_by(conn: &Connection, column: &str, value: &str) -> Result<Option<CompatDrawer>> {
    let sql = format!(
        "SELECT c.id, p.slug, c.text, s.uri, c.ord, coalesce(c.authored_at, c.created_at)
         FROM chunks c JOIN sources s ON s.id = c.source_id JOIN projects p ON p.id = c.project_id
         WHERE c.{column} = ?1 LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, [value], |r| {
            Ok(CompatDrawer::Chunk {
                id: r.get(0)?,
                project_slug: r.get(1)?,
                text: r.get(2)?,
                source_uri: r.get(3)?,
                ord: r.get(4)?,
                created_at: r.get(5)?,
            })
        })
        .optional()?)
}

/// An Alice memory id, a legacy id (a memory's own, or one `legacy_ids` points at a survivor),
/// or a chunk id (Alice's or a legacy one).
pub fn resolve_drawer(conn: &Connection, drawer_id: &str) -> Result<Option<CompatDrawer>> {
    let memory = |sql: &str, id: &str| -> Result<Option<String>> {
        Ok(conn.query_row(sql, [id], |r| r.get(0)).optional()?)
    };
    if let Some(id) = memory("SELECT id FROM memories WHERE id = ?1", drawer_id)? {
        return Ok(Some(CompatDrawer::Memory(id)));
    }
    if let Some(id) = memory(
        "SELECT id FROM memories WHERE legacy_id = ?1 ORDER BY seq LIMIT 1",
        drawer_id,
    )? {
        return Ok(Some(CompatDrawer::Memory(id)));
    }
    if let Some(t) = legacy_get(conn, drawer_id)? {
        match t.target_type.as_str() {
            "memory" => {
                if let Some(id) = memory("SELECT id FROM memories WHERE id = ?1", &t.target_id)? {
                    return Ok(Some(CompatDrawer::Memory(id)));
                }
            }
            "chunk" => {
                if let Some(c) = chunk_by(conn, "id", &t.target_id)? {
                    return Ok(Some(c));
                }
            }
            _ => {}
        }
    }
    if let Some(c) = chunk_by(conn, "id", drawer_id)? {
        return Ok(Some(c));
    }
    chunk_by(conn, "legacy_id", drawer_id)
}

/// What `Hit` does not carry (a hit has a project, a title and a body, not these).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HitDetail {
    /// A memory's topics, the first being its room; empty for a chunk.
    pub topics: Vec<String>,
    /// A chunk's source (a path or a session id, stored redacted).
    pub source_uri: Option<String>,
    pub ord: Option<i64>,
    /// The id the record had in MemPalace, when it was imported.
    pub legacy_id: Option<String>,
}

/// Details of memories and chunks by Alice id: one bounded query per tier and batch of ids.
pub fn hit_details(
    conn: &Connection,
    memory_ids: &[String],
    chunk_ids: &[String],
) -> Result<HashMap<String, HitDetail>> {
    let mut out: HashMap<String, HitDetail> = HashMap::new();
    for part in memory_ids.chunks(IN_BATCH) {
        let marks = vec!["?"; part.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, legacy_id FROM memories WHERE id IN ({marks})"
        ))?;
        for row in stmt.query_map(params_from_iter(part.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })? {
            let (id, legacy_id) = row?;
            out.entry(id).or_default().legacy_id = legacy_id;
        }
        // Insertion order: the room an import (or `remember`) listed first comes first.
        let mut stmt = conn.prepare(&format!(
            "SELECT memory_id, topic FROM memory_topics WHERE memory_id IN ({marks}) ORDER BY rowid"
        ))?;
        for row in stmt.query_map(params_from_iter(part.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (id, topic) = row?;
            out.entry(id).or_default().topics.push(topic);
        }
    }
    for part in chunk_ids.chunks(IN_BATCH) {
        let marks = vec!["?"; part.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT c.id, s.uri, c.ord, c.legacy_id FROM chunks c JOIN sources s ON s.id = c.source_id
             WHERE c.id IN ({marks})"
        ))?;
        for row in stmt.query_map(params_from_iter(part.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })? {
            let (id, uri, ord, legacy_id) = row?;
            out.insert(
                id,
                HitDetail {
                    topics: Vec::new(),
                    source_uri: Some(uri),
                    ord: Some(ord),
                    legacy_id,
                },
            );
        }
    }
    Ok(out)
}

/// An agent's live diary memories, newest first (`project` is a slug; `None` is every project).
/// At most `limit` rows.
pub fn recent_diaries(
    conn: &Connection,
    agent: &str,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT m.id FROM memories m JOIN projects p ON p.id = m.project_id
         WHERE m.kind = 'diary' AND m.status <> 'trashed' AND m.author_agent = ?1
           AND (?2 IS NULL OR p.slug = ?2)
         ORDER BY m.created_at DESC, m.seq DESC LIMIT ?3",
    )?;
    let ids: Vec<String> = stmt
        .query_map(rusqlite::params![agent, project, limit as i64], |r| {
            r.get(0)
        })?
        .collect::<std::result::Result<_, _>>()?;
    get_many(conn, &ids)
}

/// How many live diary memories [`recent_diaries`] could return for the same agent and project.
pub fn count_diaries(conn: &Connection, agent: &str, project: Option<&str>) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM memories m JOIN projects p ON p.id = m.project_id
         WHERE m.kind = 'diary' AND m.status <> 'trashed' AND m.author_agent = ?1
           AND (?2 IS NULL OR p.slug = ?2)",
        rusqlite::params![agent, project],
        |r| r.get::<_, i64>(0),
    )? as u64)
}
