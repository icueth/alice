//! Merging one project into another (`alice projects merge <from> <into>`).
//!
//! Everything of `from` becomes part of `into`: its memories, sources and chunks change project
//! (facts have no project of their own: they follow the memory they were learned from), and the
//! project row goes. Its slug, its key and the aliases it had keep resolving, to `into`, so later
//! writes and imports that name `from` land in `into`.
//!
//! The merge is one event (`project.merge`, target type `project`, target id: the id of `from`).
//! Its before-state holds what undo needs to bring the project row and its names back; the ids
//! of the rows that moved are in `project_merge_rows`, under the merge's id, because there can
//! be too many for an event payload. Undo moves back exactly those rows that are still in `into`.
//! Nothing that carries a project is stored outside the rows moved here: the full-text indexes
//! and the vectors are keyed by row, not by project, and a search filters on the row's project.

use crate::error::{AliceError, Result};
use crate::ids::new_id;
use crate::model::Kind;
use crate::normalize::content_hash;
use crate::store::projects::{self, GLOBAL_ID, ResolvedProject};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// The action of the event a merge records.
pub const MERGE_ACTION: &str = "project.merge";
/// The target type of the event a merge records.
pub const MERGE_TARGET: &str = "project";

/// What a merge moves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeCounts {
    pub memories: u64,
    pub chunks: u64,
    pub sources: u64,
    /// Facts learned from the memories that move (a fact has no project of its own).
    pub facts: u64,
}

/// A merge that is allowed, with what it would move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub from: ResolvedProject,
    pub into: ResolvedProject,
    pub counts: MergeCounts,
}

/// What a merge did: the event states (see [`MergeState`]) and the counts.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeDone {
    pub from: ResolvedProject,
    pub into: ResolvedProject,
    pub counts: MergeCounts,
    pub before: serde_json::Value,
    pub after: serde_json::Value,
}

/// The project row as undo recreates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRow {
    pub id: String,
    pub key: String,
    pub slug: String,
    pub name: String,
    pub git_remote: Option<String>,
    pub root_path: Option<String>,
    pub created_at: String,
}

/// The before-state of a merge event: what undo needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeBefore {
    pub merge_id: String,
    pub from: ProjectRow,
    pub into_id: String,
    pub into_slug: String,
    /// Aliases that named `from` (they point at `into` after the merge), sorted.
    pub aliases: Vec<String>,
    /// Keys of earlier merges into `from`, sorted.
    pub key_aliases: Vec<String>,
    pub memories: u64,
    pub chunks: u64,
    pub sources: u64,
    pub facts: u64,
    /// The root path `into` had before the merge (a later write by a checkout of `from` may
    /// have given a root-less `into` the root of that checkout; undo takes it back).
    #[serde(default)]
    pub into_root: Option<String>,
    /// Moved memories that had `project_defaulted = 1` (a merge is a decision, so it clears the
    /// flag; undo sets it again on exactly these).
    #[serde(default)]
    pub defaulted: Vec<String>,
}

/// The after-state of a merge event, and what [`current_state`] reads back to compare it with:
/// whether the old project row is back, and how many of the moved rows are still in `into`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeState {
    pub merge_id: String,
    pub from_id: String,
    pub into_id: String,
    pub from_present: bool,
    pub memories: u64,
    pub chunks: u64,
    pub sources: u64,
}

fn count(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<u64> {
    Ok(conn.query_row(sql, p, |r| r.get::<_, i64>(0))? as u64)
}

fn counts_of(conn: &Connection, project_id: &str) -> Result<MergeCounts> {
    Ok(MergeCounts {
        memories: count(
            conn,
            "SELECT count(*) FROM memories WHERE project_id = ?1",
            [project_id],
        )?,
        chunks: count(
            conn,
            "SELECT count(*) FROM chunks WHERE project_id = ?1",
            [project_id],
        )?,
        sources: count(
            conn,
            "SELECT count(*) FROM sources WHERE project_id = ?1",
            [project_id],
        )?,
        facts: count(
            conn,
            "SELECT count(*) FROM relations r JOIN memories m ON m.id = r.source_memory_id WHERE m.project_id = ?1",
            [project_id],
        )?,
    })
}

/// Check that `from` can be merged into `into` and count what would move. Refuses, in one
/// sentence each: an unknown project, `global` as the project merged away, the same project
/// twice, and `global` as the target (its memories are visible from every project, so a
/// project merged into it would leak into all of them).
pub fn plan(conn: &Connection, from: &str, into: &str) -> Result<MergePlan> {
    let find = |name: &str| -> Result<ResolvedProject> {
        projects::resolve_slug(conn, name)?.ok_or_else(|| {
            AliceError::invalid(format!(
                "unknown project \"{name}\"; list the existing slugs with `alice projects` and use one of them"
            ))
        })
    };
    let from_p = find(from)?;
    let into_p = find(into)?;
    if from_p.id == GLOBAL_ID {
        return Err(AliceError::invalid(
            "project \"global\" cannot be merged into another project: it holds what belongs to no project; pick the project to fix as <from>",
        ));
    }
    if from_p.id == into_p.id {
        return Err(AliceError::invalid(format!(
            "\"{from}\" and \"{into}\" are the same project (slug \"{}\"); name two different projects",
            from_p.slug
        )));
    }
    if into_p.id == GLOBAL_ID {
        return Err(AliceError::invalid(
            "a project cannot be merged into \"global\": its memories would show up in every project and its checkout would write to global; move single memories with `alice review`, or pick another project",
        ));
    }
    let counts = counts_of(conn, &from_p.id)?;
    Ok(MergePlan {
        from: from_p,
        into: into_p,
        counts,
    })
}

/// The part of a memory row that its content hash is made of.
struct MemoryText {
    seq: i64,
    status: String,
    kind: String,
    title: String,
    body: String,
}

/// New hash of a memory that moves to project `to`, or `None` to keep the stored one: the
/// project already holds the same text, and the unique index would refuse a second copy.
fn moved_hash(conn: &Connection, to: &ResolvedProject, m: &MemoryText) -> Result<Option<String>> {
    let hash = content_hash(
        &to.slug,
        Kind::parse(&m.kind).unwrap_or(Kind::Note),
        &m.title,
        &m.body,
    );
    if m.status != "trashed" {
        let taken: Option<i64> = conn
            .query_row(
                "SELECT seq FROM memories WHERE project_id = ?1 AND content_hash = ?2 AND status <> 'trashed' AND seq <> ?3",
                params![to.id, hash, m.seq],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_some() {
            return Ok(None);
        }
    }
    Ok(Some(hash))
}

/// Move the memories `ids` (those that are in project `from_id`) to `to`, with the content hash
/// of their new project. `updated_at` stays as it was (it orders recall and context); `hlc`
/// moves on so sync sees the change. `clear_defaulted` clears `project_defaulted` (a merge is a
/// decision about the project; undo leaves the flag to be put back by the caller).
fn move_memories(
    conn: &Connection,
    ids: &[String],
    from_id: &str,
    to: &ResolvedProject,
    hlc: &str,
    clear_defaulted: bool,
) -> Result<()> {
    let mut read = conn.prepare_cached(
        "SELECT seq, status, kind, title, body FROM memories WHERE id = ?1 AND project_id = ?2",
    )?;
    let mut keep = conn.prepare_cached(
        "UPDATE memories SET project_id = ?1, hlc = ?2, project_defaulted = CASE WHEN ?3 THEN 0 ELSE project_defaulted END WHERE seq = ?4",
    )?;
    let mut rehash = conn.prepare_cached(
        "UPDATE memories SET project_id = ?1, content_hash = ?2, hlc = ?3, project_defaulted = CASE WHEN ?4 THEN 0 ELSE project_defaulted END WHERE seq = ?5",
    )?;
    for id in ids {
        let Some(m) = read
            .query_row(params![id, from_id], |r| {
                Ok(MemoryText {
                    seq: r.get(0)?,
                    status: r.get(1)?,
                    kind: r.get(2)?,
                    title: r.get(3)?,
                    body: r.get(4)?,
                })
            })
            .optional()?
        else {
            continue;
        };
        match moved_hash(conn, to, &m)? {
            Some(h) => rehash.execute(params![to.id, h, hlc, clear_defaulted, m.seq])?,
            None => keep.execute(params![to.id, hlc, clear_defaulted, m.seq])?,
        };
    }
    Ok(())
}

fn ids_of(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<String>> {
    Ok(conn
        .prepare(sql)?
        .query_map(p, |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

/// Merge `from` into `into` (both already checked by [`plan`], inside the transaction that
/// calls this). Returns the event states.
pub fn merge(conn: &Connection, from: &str, into: &str, hlc: &str) -> Result<MergeDone> {
    let MergePlan { from, into, counts } = plan(conn, from, into)?;
    let merge_id = new_id();
    let row: ProjectRow = conn.query_row(
        "SELECT id, key, slug, name, git_remote, root_path, created_at FROM projects WHERE id = ?1",
        [&from.id],
        |r| {
            Ok(ProjectRow {
                id: r.get(0)?,
                key: r.get(1)?,
                slug: r.get(2)?,
                name: r.get(3)?,
                git_remote: r.get(4)?,
                root_path: r.get(5)?,
                created_at: r.get(6)?,
            })
        },
    )?;
    let aliases = ids_of(
        conn,
        "SELECT alias FROM project_aliases WHERE project_id = ?1 ORDER BY alias",
        [&from.id],
    )?;
    let key_aliases = ids_of(
        conn,
        "SELECT key FROM project_key_aliases WHERE project_id = ?1 ORDER BY key",
        [&from.id],
    )?;

    // The rows that move, recorded before they do.
    for (kind, table) in [
        ("memory", "memories"),
        ("source", "sources"),
        ("chunk", "chunks"),
    ] {
        conn.execute(
            &format!(
                "INSERT INTO project_merge_rows (merge_id, kind, row_id) SELECT ?1, ?2, id FROM {table} WHERE project_id = ?3"
            ),
            params![merge_id, kind, from.id],
        )?;
    }
    let memory_ids = ids_of(
        conn,
        "SELECT row_id FROM project_merge_rows WHERE merge_id = ?1 AND kind = 'memory'",
        [&merge_id],
    )?;
    let defaulted = ids_of(
        conn,
        "SELECT id FROM memories WHERE project_id = ?1 AND project_defaulted = 1 ORDER BY id",
        [&from.id],
    )?;
    let into_root = projects::root_of(conn, &into.id)?;
    move_memories(conn, &memory_ids, &from.id, &into, hlc, true)?;
    conn.execute(
        "UPDATE sources SET project_id = ?1 WHERE project_id = ?2",
        params![into.id, from.id],
    )?;
    conn.execute(
        "UPDATE chunks SET project_id = ?1 WHERE project_id = ?2",
        params![into.id, from.id],
    )?;

    // The names: every alias of `from`, its slug and its key now mean `into`.
    conn.execute(
        "UPDATE project_aliases SET project_id = ?1 WHERE project_id = ?2",
        params![into.id, from.id],
    )?;
    conn.execute(
        "UPDATE project_key_aliases SET project_id = ?1 WHERE project_id = ?2",
        params![into.id, from.id],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO project_key_aliases (key, project_id) VALUES (?1, ?2)",
        params![row.key, into.id],
    )?;
    conn.execute("DELETE FROM projects WHERE id = ?1", [&from.id])?;
    conn.execute(
        "INSERT INTO project_aliases (alias, project_id) VALUES (?1, ?2) ON CONFLICT(alias) DO UPDATE SET project_id = excluded.project_id",
        params![row.slug, into.id],
    )?;

    let before = MergeBefore {
        merge_id: merge_id.clone(),
        from: row,
        into_id: into.id.clone(),
        into_slug: into.slug.clone(),
        aliases,
        key_aliases,
        memories: counts.memories,
        chunks: counts.chunks,
        sources: counts.sources,
        facts: counts.facts,
        into_root,
        defaulted,
    };
    let after = MergeState {
        merge_id,
        from_id: from.id.clone(),
        into_id: into.id.clone(),
        from_present: false,
        memories: counts.memories,
        chunks: counts.chunks,
        sources: counts.sources,
    };
    Ok(MergeDone {
        from,
        into,
        counts,
        before: to_json(&before)?,
        after: to_json(&after)?,
    })
}

fn to_json<T: Serialize>(v: &T) -> Result<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| AliceError::Other(format!("merge snapshot: {e}")))
}

/// The merge `spec` (a recorded state of the event) describes, as it is now: undo compares it
/// with the event's after-state to see whether anything moved on.
pub fn current_state(conn: &Connection, spec: &serde_json::Value) -> Result<serde_json::Value> {
    let spec: MergeState = serde_json::from_value(spec.clone()).map_err(|e| {
        AliceError::invalid(format!("the recorded project merge is unreadable ({e})"))
    })?;
    let still_in = |kind: &str, table: &str| -> Result<u64> {
        count(
            conn,
            &format!(
                "SELECT count(*) FROM project_merge_rows r JOIN {table} t ON t.id = r.row_id
                 WHERE r.merge_id = ?1 AND r.kind = ?2 AND t.project_id = ?3"
            ),
            params![spec.merge_id, kind, spec.into_id],
        )
    };
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM projects WHERE id = ?1",
        [&spec.from_id],
        |r| r.get(0),
    )?;
    to_json(&MergeState {
        from_present: present > 0,
        memories: still_in("memory", "memories")?,
        chunks: still_in("chunk", "chunks")?,
        sources: still_in("source", "sources")?,
        ..spec
    })
}

/// Refuse to undo merge event `ev` while a later merge still depends on it: when `into` was
/// merged away since, `into` no longer exists and the rows are not where undo would look. Names
/// that later merge's event, so the person undoes it first. Nothing is written.
pub fn check_undo_possible(conn: &Connection, ev: &crate::store::events::EventRow) -> Result<()> {
    use crate::store::events::{self, EventQuery};
    let Some(after) = ev.after.as_ref() else {
        return Ok(());
    };
    let Ok(state) = serde_json::from_value::<MergeState>(after.clone()) else {
        return Ok(());
    };
    let into_exists = count(
        conn,
        "SELECT count(*) FROM projects WHERE id = ?1",
        [&state.into_id],
    )? > 0;
    if into_exists {
        return Ok(());
    }
    let later = events::list(
        conn,
        &EventQuery {
            target_id: Some(state.into_id.clone()),
            limit: Some(500),
            ..Default::default()
        },
    )?;
    for l in later {
        if l.id == ev.id {
            break;
        }
        if l.action != MERGE_ACTION || l.target_type != MERGE_TARGET {
            continue;
        }
        if events::undone_by(conn, &l.id)?.is_some() {
            continue;
        }
        return Err(AliceError::invalid(format!(
            "cannot undo event {}: its target project was merged into another one by the later merge event {}; undo event {} first, then this one",
            ev.id, l.id, l.id
        )));
    }
    Ok(())
}

/// Reverse a merge from its before-state: the project row comes back with the names it had, and
/// the rows that moved and are still in `into` go back. Rows that left `into` since (force undo)
/// stay where they are. The merge's ids are dropped.
pub fn unmerge(conn: &Connection, before: &serde_json::Value, hlc: &str) -> Result<()> {
    let b: MergeBefore = serde_json::from_value(before.clone()).map_err(|e| {
        AliceError::invalid(format!("the recorded project merge is unreadable ({e})"))
    })?;
    let taken = |sql: &str, v: &str| -> Result<bool> { Ok(count(conn, sql, [v])? > 0) };
    if taken("SELECT count(*) FROM projects WHERE id = ?1", &b.from.id)? {
        return Err(AliceError::invalid(format!(
            "cannot undo: project \"{}\" exists again; nothing was changed",
            b.from.slug
        )));
    }
    if taken(
        "SELECT count(*) FROM projects WHERE slug = ?1",
        &b.from.slug,
    )? || taken("SELECT count(*) FROM projects WHERE key = ?1", &b.from.key)?
    {
        return Err(AliceError::invalid(format!(
            "cannot undo: another project now has the slug or the key of \"{}\"; nothing was changed",
            b.from.slug
        )));
    }
    let into = projects::get_by_id(conn, &b.into_id)?;
    conn.execute(
        "INSERT INTO projects (id, key, slug, name, git_remote, root_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            b.from.id,
            b.from.key,
            b.from.slug,
            b.from.name,
            b.from.git_remote,
            b.from.root_path,
            b.from.created_at
        ],
    )?;
    // The names that were `from`'s go back to it, unless someone pointed them elsewhere since.
    conn.execute(
        "DELETE FROM project_aliases WHERE alias = ?1 AND project_id = ?2",
        params![b.from.slug, b.into_id],
    )?;
    conn.execute(
        "DELETE FROM project_key_aliases WHERE key = ?1 AND project_id = ?2",
        params![b.from.key, b.into_id],
    )?;
    for alias in &b.aliases {
        conn.execute(
            "UPDATE project_aliases SET project_id = ?1 WHERE alias = ?2 AND project_id = ?3",
            params![b.from.id, alias, b.into_id],
        )?;
    }
    for key in &b.key_aliases {
        conn.execute(
            "UPDATE project_key_aliases SET project_id = ?1 WHERE key = ?2 AND project_id = ?3",
            params![b.from.id, key, b.into_id],
        )?;
    }
    let back = ResolvedProject {
        id: b.from.id.clone(),
        slug: b.from.slug.clone(),
        name: b.from.name.clone(),
    };
    let memory_ids = ids_of(
        conn,
        "SELECT row_id FROM project_merge_rows WHERE merge_id = ?1 AND kind = 'memory'",
        [&b.merge_id],
    )?;
    move_memories(conn, &memory_ids, &into.id, &back, hlc, false)?;
    let mut flag = conn.prepare_cached(
        "UPDATE memories SET project_defaulted = 1 WHERE id = ?1 AND project_id = ?2",
    )?;
    for id in &b.defaulted {
        flag.execute(params![id, b.from.id])?;
    }
    // A root that `into` took after the merge, from a checkout of `from`, goes back to none (a
    // checkout of `into` itself sets it again on its next write).
    if b.into_root.is_none() {
        conn.execute(
            "UPDATE projects SET root_path = NULL WHERE id = ?1",
            [&b.into_id],
        )?;
    }
    for (kind, table) in [("source", "sources"), ("chunk", "chunks")] {
        conn.execute(
            &format!(
                "UPDATE {table} SET project_id = ?1 WHERE project_id = ?2
                 AND id IN (SELECT row_id FROM project_merge_rows WHERE merge_id = ?3 AND kind = ?4)"
            ),
            params![b.from.id, b.into_id, b.merge_id, kind],
        )?;
    }
    conn.execute(
        "DELETE FROM project_merge_rows WHERE merge_id = ?1",
        [&b.merge_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use crate::project::ProjectRef;
    use crate::store::events::{self, Actor, EventInput};
    use crate::store::kg::{self, AssertFact};
    use crate::store::memories::{self, InsertOutcome, NewMemoryInput};
    use crate::store::sources::{self, ChunkInput, SourceInput};
    use crate::store::{history, open_writer};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    struct T {
        _d: tempfile::TempDir,
        c: Connection,
        n: u64,
    }

    fn pref(key: &str, slug: &str) -> ProjectRef {
        ProjectRef {
            key: key.into(),
            slug: slug.into(),
            name: slug.into(),
            root: None,
            remote: None,
        }
    }

    /// Everything that says which project a row is in, plus the names, as a comparable value.
    fn world(c: &Connection) -> serde_json::Value {
        let rows = |sql: &str| -> Vec<Vec<String>> {
            c.prepare(sql)
                .unwrap()
                .query_map([], |r| {
                    let mut v = Vec::new();
                    let mut i = 0;
                    while let Ok(x) = r.get::<_, Option<String>>(i) {
                        v.push(x.unwrap_or_default());
                        i += 1;
                    }
                    Ok(v)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        serde_json::json!({
            "projects": rows("SELECT id, key, slug, name, ifnull(git_remote,''), ifnull(root_path,''), created_at FROM projects ORDER BY id"),
            "aliases": rows("SELECT alias, project_id FROM project_aliases ORDER BY alias"),
            "keys": rows("SELECT key, project_id FROM project_key_aliases ORDER BY key"),
            "memories": rows("SELECT id, project_id, content_hash FROM memories ORDER BY id"),
            "sources": rows("SELECT id, project_id FROM sources ORDER BY id"),
            "chunks": rows("SELECT id, project_id FROM chunks ORDER BY id"),
            "relations": rows("SELECT id, subject_id, predicate, ifnull(source_memory_id,'') FROM relations ORDER BY id"),
        })
    }

    impl T {
        fn new() -> T {
            let d = tempfile::tempdir().unwrap();
            let c = open_writer(&d.path().join("a.db")).unwrap();
            T { _d: d, c, n: 0 }
        }
        fn hlc(&mut self) -> String {
            self.n += 1;
            format!("{:013}-0000-dev", 1_000 + self.n)
        }
        fn project(&self, key: &str, slug: &str) -> ResolvedProject {
            projects::upsert_project(&self.c, &pref(key, slug)).unwrap()
        }
        fn remember(&mut self, p: &ResolvedProject, text: &str) -> String {
            let h = self.hlc();
            let m = memories::prepare(
                NewMemoryInput {
                    content: text.into(),
                    kind: Some(Kind::Fact),
                    ..Default::default()
                },
                p,
                h,
            )
            .unwrap();
            let InsertOutcome::New { id, .. } = memories::insert(&self.c, &m).unwrap() else {
                panic!("duplicate")
            };
            id
        }
        fn ingest(&self, p: &ResolvedProject, uri: &str, chunks: usize) {
            let cs: Vec<ChunkInput> = (0..chunks)
                .map(|i| ChunkInput {
                    ord: i as i64,
                    text: format!("{uri} chunk {i}"),
                    ..Default::default()
                })
                .collect();
            sources::ingest(
                &self.c,
                p,
                &SourceInput {
                    uri: uri.into(),
                    ..Default::default()
                },
                &cs,
            )
            .unwrap();
        }
        fn fact(&mut self, subject: &str, source: &str) {
            let h = self.hlc();
            kg::assert_fact(
                &self.c,
                &AssertFact {
                    subject: subject.into(),
                    predicate: "uses".into(),
                    object: "sqlite".into(),
                    source_memory_id: Some(source.into()),
                    ..Default::default()
                },
                h,
            )
            .unwrap();
        }
        /// Merge and record the event as the writer does; returns its id.
        fn merge(&mut self, from: &str, into: &str) -> (MergeDone, String) {
            let h = self.hlc();
            let done = merge(&self.c, from, into, &h).unwrap();
            let id = events::record(
                &self.c,
                &EventInput {
                    actor: &Actor::User,
                    action: MERGE_ACTION,
                    target_type: MERGE_TARGET,
                    target_id: &done.from.id,
                    before: Some(done.before.clone()),
                    after: Some(done.after.clone()),
                    group: None,
                },
                &h,
            )
            .unwrap();
            (done, id)
        }
        fn undo(&mut self, event: &str, force: bool) -> Result<history::UndoOutcome> {
            let mut n = self.n;
            let mut tick = || {
                n += 1;
                format!("{:013}-0000-dev", 5_000 + n)
            };
            let redact =
                |_: &Connection, _: Option<&str>| Arc::new(crate::redact::Redactor::builtin());
            let f = if force {
                history::force_undo
            } else {
                history::undo
            };
            f(&self.c, event, &Actor::User, &mut tick, &redact)
        }
        fn in_project(&self, table: &str, p: &str) -> BTreeSet<String> {
            self.c
                .prepare(&format!("SELECT id FROM {table} WHERE project_id = ?1"))
                .unwrap()
                .query_map([p], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        }
    }

    /// `alice` (a real project) and `project-alice` (from a wing), each with memories, a
    /// source with chunks and a fact.
    fn two_projects() -> (T, ResolvedProject, ResolvedProject) {
        let mut t = T::new();
        let a = t.project("github.com/icueth/alice", "alice");
        let b = t.project("import:project-alice", "project-alice");
        projects::add_alias(&t.c, "wing_alice", &a.id).unwrap();
        let m1 = t.remember(&a, "alice uses sqlite for storage");
        t.remember(&a, "alice decision about wal");
        t.remember(&b, "project alice keeps notes");
        t.fact("alice", &m1);
        t.ingest(&a, "s-a", 3);
        t.ingest(&b, "s-b", 2);
        (t, a, b)
    }

    #[test]
    fn merge_moves_memories_chunks_sources_and_counts_facts() {
        let (mut t, a, b) = two_projects();
        let p = plan(&t.c, "alice", "project-alice").unwrap();
        assert_eq!(
            p.counts,
            MergeCounts {
                memories: 2,
                chunks: 3,
                sources: 1,
                facts: 1
            }
        );
        let a_memories = t.in_project("memories", &a.id);
        let a_chunks = t.in_project("chunks", &a.id);
        let (done, _) = t.merge("alice", "project-alice");
        assert_eq!(done.counts, p.counts);
        assert!(t.in_project("memories", &a.id).is_empty());
        assert!(t.in_project("chunks", &a.id).is_empty());
        assert!(t.in_project("sources", &a.id).is_empty());
        assert!(a_memories.is_subset(&t.in_project("memories", &b.id)));
        assert!(a_chunks.is_subset(&t.in_project("chunks", &b.id)));
        assert_eq!(t.in_project("memories", &b.id).len(), 3);
        assert_eq!(t.in_project("sources", &b.id).len(), 2);
        // The old row is gone, the names are not.
        assert!(projects::get_by_id(&t.c, &a.id).is_err());
        assert_eq!(
            projects::resolve_slug(&t.c, "alice").unwrap().unwrap().id,
            b.id
        );
        assert_eq!(
            projects::resolve_slug(&t.c, "Wing_Alice")
                .unwrap()
                .unwrap()
                .id,
            b.id
        );
        assert_eq!(
            projects::find_by_key(&t.c, "github.com/icueth/alice")
                .unwrap()
                .unwrap()
                .id,
            b.id
        );
        // The facts stay with their memories: counted for the project now.
        assert_eq!(counts_of(&t.c, &b.id).unwrap().facts, 1);
    }

    #[test]
    fn full_text_search_by_the_merged_project_still_finds_the_moved_memories() {
        let (mut t, a, b) = two_projects();
        let hits = |c: &Connection, project: &str| -> usize {
            c.query_row(
                "SELECT count(*) FROM memories_fts f JOIN memories m ON m.seq = f.rowid
                 WHERE memories_fts MATCH 'sqlite' AND m.project_id = ?1",
                [project],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize
        };
        assert_eq!(hits(&t.c, &a.id), 1);
        t.merge("alice", "project-alice");
        // The old slug resolves to the project that holds the memory now.
        let id = projects::resolve_slug(&t.c, "alice").unwrap().unwrap().id;
        assert_eq!(hits(&t.c, &id), 1);
        assert_eq!(hits(&t.c, &a.id), 0);
        let _ = b;
    }

    #[test]
    fn a_later_write_naming_the_old_project_lands_in_the_new_one() {
        let (mut t, a, b) = two_projects();
        t.merge("alice", "project-alice");
        // Detected from the checkout of the old project (same key, same slug).
        let again =
            projects::upsert_project(&t.c, &pref("github.com/icueth/alice", "alice")).unwrap();
        assert_eq!(again.id, b.id);
        // A name that was an alias of the old project.
        assert_eq!(
            projects::resolve_slug(&t.c, "wing_alice")
                .unwrap()
                .unwrap()
                .id,
            b.id
        );
        assert_eq!(
            projects::list_projects(&t.c).unwrap().len(),
            2,
            "global + project-alice"
        );
        assert!(projects::get_by_id(&t.c, &a.id).is_err());
        let m = t.remember(&again, "written after the merge");
        assert!(t.in_project("memories", &b.id).contains(&m));
    }

    #[test]
    fn a_merged_import_placeholder_is_adopted_by_the_real_project_of_the_same_slug() {
        let mut t = T::new();
        let target = t.project("github.com/icueth/project-alice", "project-alice");
        let ph = t.project("import:alice", "alice");
        t.remember(&ph, "from the wing");
        t.merge("alice", "project-alice");
        // The import names the wing again.
        let again = projects::upsert_project(&t.c, &pref("import:alice", "alice")).unwrap();
        assert_eq!(again.id, target.id);
        // The real project of that slug shows up later: it is the project it was merged into.
        let real =
            projects::upsert_project(&t.c, &pref("github.com/icueth/alice", "alice")).unwrap();
        assert_eq!(real.id, target.id, "no alice-<hash> twin");
        assert_eq!(projects::list_projects(&t.c).unwrap().len(), 2);
    }

    #[test]
    fn a_detected_checkout_of_the_old_project_does_not_replace_the_root_of_the_new_one() {
        let mut t = T::new();
        let d = tempfile::tempdir().unwrap();
        let (root_a, root_b) = (d.path().join("a"), d.path().join("b"));
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let mut ra = pref("k/a", "a");
        ra.root = Some(root_a.clone());
        let mut rb = pref("k/b", "b");
        rb.root = Some(root_b.clone());
        projects::upsert_project(&t.c, &ra).unwrap();
        let b = projects::upsert_project(&t.c, &rb).unwrap();
        t.merge("a", "b");
        projects::upsert_project(&t.c, &ra).unwrap();
        assert_eq!(
            projects::root_of(&t.c, &b.id).unwrap().as_deref(),
            root_b.to_str()
        );
    }

    #[test]
    fn undo_restores_the_project_its_names_and_exactly_the_rows_that_moved() {
        let (mut t, a, _b) = two_projects();
        // A third project already merged into `alice`: its key and slug follow along, and
        // come back to `alice` with the undo.
        let old = t.project("github.com/icueth/old-alice", "old-alice");
        t.remember(&old, "very old alice notes");
        t.merge("old-alice", "alice");
        let before = world(&t.c);
        let (_, event) = t.merge("alice", "project-alice");
        let merged = world(&t.c);
        assert_ne!(before, merged);
        let out = t.undo(&event, false).unwrap();
        assert!(
            matches!(out, history::UndoOutcome::Undone { .. }),
            "{out:?}"
        );
        let after = world(&t.c);
        // Only the memories' bookkeeping (time, hlc) differs; ids, projects and hashes match.
        assert_eq!(before, after);
        assert_eq!(projects::get_by_id(&t.c, &a.id).unwrap().slug, "alice");
        assert_eq!(
            projects::find_by_key(&t.c, "github.com/icueth/old-alice")
                .unwrap()
                .unwrap()
                .id,
            a.id
        );
        let rows: i64 =
            t.c.query_row(
                "SELECT count(*) FROM project_merge_rows WHERE merge_id = ?1",
                [event.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    fn cell(t: &T, id: &str, col: &str) -> String {
        t.c.query_row(
            &format!("SELECT CAST({col} AS TEXT) FROM memories WHERE id = ?1"),
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_merge_keeps_updated_at_and_undo_puts_project_defaulted_back() {
        let (mut t, a, b) = two_projects();
        let ids: Vec<String> = t.in_project("memories", &a.id).into_iter().collect();
        let (defaulted, plain) = (ids[0].clone(), ids[1].clone());
        t.c.execute(
            "UPDATE memories SET project_defaulted = 1, updated_at = '2020-01-01T00:00:00Z' WHERE id = ?1",
            [&defaulted],
        )
        .unwrap();
        t.c.execute(
            "UPDATE memories SET updated_at = '2021-02-02T00:00:00Z' WHERE id = ?1",
            [&plain],
        )
        .unwrap();
        let (_, event) = t.merge("alice", "project-alice");
        assert!(t.in_project("memories", &b.id).contains(&defaulted));
        assert_eq!(cell(&t, &defaulted, "project_defaulted"), "0");
        assert_eq!(cell(&t, &defaulted, "updated_at"), "2020-01-01T00:00:00Z");
        assert_eq!(cell(&t, &plain, "updated_at"), "2021-02-02T00:00:00Z");
        t.undo(&event, false).unwrap();
        assert_eq!(cell(&t, &defaulted, "project_defaulted"), "1");
        assert_eq!(cell(&t, &plain, "project_defaulted"), "0");
        assert_eq!(cell(&t, &defaulted, "updated_at"), "2020-01-01T00:00:00Z");
        assert_eq!(cell(&t, &plain, "updated_at"), "2021-02-02T00:00:00Z");
    }

    #[test]
    fn undo_takes_back_a_root_that_the_target_adopted_after_the_merge() {
        let mut t = T::new();
        let d = tempfile::tempdir().unwrap();
        let root_a = d.path().join("a");
        std::fs::create_dir_all(&root_a).unwrap();
        let mut ra = pref("k/a", "a");
        ra.root = Some(root_a.clone());
        let a = projects::upsert_project(&t.c, &ra).unwrap();
        let b = t.project("k/b", "b");
        let (_, event) = t.merge("a", "b");
        // A write from the old checkout reaches `b`, which had no root and takes this one.
        projects::upsert_project(&t.c, &ra).unwrap();
        assert_eq!(
            projects::root_of(&t.c, &b.id).unwrap().as_deref(),
            root_a.to_str()
        );
        t.undo(&event, false).unwrap();
        assert_eq!(projects::root_of(&t.c, &b.id).unwrap(), None);
        assert_eq!(
            projects::root_of(&t.c, &a.id).unwrap().as_deref(),
            root_a.to_str()
        );
    }

    #[test]
    fn an_older_merge_cannot_be_undone_while_a_later_one_depends_on_it() {
        let mut t = T::new();
        let a = t.project("k/a", "a");
        let b = t.project("k/b", "b");
        let c = t.project("k/c", "c");
        t.remember(&a, "in a");
        let (_, first) = t.merge("a", "b");
        let (_, second) = t.merge("b", "c");
        let before = world(&t.c);
        let err = t.undo(&first, false).unwrap_err().to_string();
        assert!(
            err.contains(&second) && err.contains("first"),
            "names the later merge and says to undo it first: {err}"
        );
        let err = t.undo(&first, true).unwrap_err().to_string();
        assert!(err.contains(&second), "{err}");
        assert_eq!(world(&t.c), before, "nothing changed");
        let open_reviews: i64 =
            t.c.query_row("SELECT count(*) FROM review_items", [], |r| r.get(0))
                .unwrap();
        assert_eq!(open_reviews, 0, "no review item is opened");
        // Newest first works, and then the older one.
        t.undo(&second, false).unwrap();
        t.undo(&first, false).unwrap();
        assert!(projects::get_by_id(&t.c, &a.id).is_ok());
        assert!(projects::get_by_id(&t.c, &b.id).is_ok());
    }

    #[test]
    fn undo_is_refused_for_the_undo_itself() {
        let (mut t, _a, _b) = two_projects();
        let (_, event) = t.merge("alice", "project-alice");
        t.undo(&event, false).unwrap();
        let undo_event: String =
            t.c.query_row(
                "SELECT id FROM events WHERE action = 'undo:project.merge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let err = t.undo(&undo_event, false).unwrap_err();
        assert!(err.to_string().contains("cannot be undone"), "{err}");
    }

    #[test]
    fn undo_asks_instead_when_a_moved_memory_went_elsewhere_since() {
        let (mut t, a, b) = two_projects();
        let other = t.project("k/other", "other");
        let (_, event) = t.merge("alice", "project-alice");
        let moved = t.in_project("memories", &a.id);
        assert!(moved.is_empty());
        let one: String = t
            .c
            .query_row(
                "SELECT row_id FROM project_merge_rows WHERE merge_id = (SELECT merge_id FROM (SELECT json_extract(after_json,'$.merge_id') AS merge_id FROM events WHERE id = ?1)) AND kind = 'memory' LIMIT 1",
                [event.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        t.c.execute(
            "UPDATE memories SET project_id = ?1 WHERE id = ?2",
            params![other.id, one],
        )
        .unwrap();
        let before = world(&t.c);
        let out = t.undo(&event, false).unwrap();
        assert!(
            matches!(out, history::UndoOutcome::Conflict { .. }),
            "{out:?}"
        );
        assert_eq!(world(&t.c), before, "nothing changed");
        // "Undo anyway" moves back what is still there and leaves the one that was moved on.
        t.undo(&event, true).unwrap();
        assert_eq!(t.in_project("memories", &a.id).len(), 1);
        assert_eq!(t.in_project("memories", &other.id), BTreeSet::from([one]));
        assert_eq!(t.in_project("memories", &b.id).len(), 1);
    }

    #[test]
    fn a_memory_event_from_before_the_merge_can_still_be_undone_anyway() {
        let (mut t, a, b) = two_projects();
        let id = t.remember(&a, "text before the edit");
        let before = history::snapshot_memory(&t.c, &id).unwrap().unwrap();
        let h = t.hlc();
        memories::apply_update(
            &t.c,
            &id,
            &memories::MemoryUpdate {
                action: memories::UpdateAction::Edit,
                body: Some("text after the edit".into()),
                ..Default::default()
            },
            h.clone(),
        )
        .unwrap();
        let ev = events::record(
            &t.c,
            &EventInput {
                actor: &Actor::User,
                action: "memory.edit",
                target_type: "memory",
                target_id: &id,
                before: Some(serde_json::to_value(&before).unwrap()),
                after: Some(
                    serde_json::to_value(history::snapshot_memory(&t.c, &id).unwrap().unwrap())
                        .unwrap(),
                ),
                group: None,
            },
            &h,
        )
        .unwrap();
        t.merge("alice", "project-alice");
        // The memory is in another project than the event saw: asked, then done on request.
        assert!(matches!(
            t.undo(&ev, false).unwrap(),
            history::UndoOutcome::Conflict { .. }
        ));
        t.undo(&ev, true).unwrap();
        let s = history::snapshot_memory(&t.c, &id).unwrap().unwrap();
        assert_eq!(s.body, "text before the edit");
        assert_eq!(
            s.project_id, b.id,
            "it stays in the project that holds it now"
        );
    }

    #[test]
    fn refusals_name_the_fix() {
        let (t, _a, _b) = two_projects();
        let msg = |from: &str, into: &str| plan(&t.c, from, into).unwrap_err().to_string();
        assert!(msg("global", "alice").contains("global"));
        assert!(msg("alice", "alice").contains("same project"));
        assert!(
            msg("wing_alice", "alice").contains("same project"),
            "an alias is the project"
        );
        let unknown = msg("nope", "alice");
        assert!(unknown.contains("unknown project \"nope\"") && unknown.contains("alice projects"));
        assert!(msg("alice", "nope").contains("unknown project \"nope\""));
        assert!(msg("alice", "global").contains("global"));
    }

    #[test]
    fn a_copy_of_the_same_text_in_the_target_keeps_its_own_hash() {
        let mut t = T::new();
        let a = t.project("k/a", "a");
        let b = t.project("k/b", "b");
        let ma = t.remember(&a, "same words");
        let mb = t.remember(&b, "same words");
        let hash_of = |t: &T, id: &str| -> String {
            t.c.query_row(
                "SELECT content_hash FROM memories WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let (ha, hb) = (hash_of(&t, &ma), hash_of(&t, &mb));
        let (_, event) = t.merge("a", "b");
        // The unique index did not refuse the second copy: the moved one keeps its hash.
        assert_eq!(
            t.in_project("memories", &b.id),
            BTreeSet::from([ma.clone(), mb.clone()])
        );
        assert_eq!(
            (hash_of(&t, &ma), hash_of(&t, &mb)),
            (ha.clone(), hb.clone())
        );
        t.undo(&event, false).unwrap();
        assert_eq!(
            t.in_project("memories", &a.id),
            BTreeSet::from([ma.clone()])
        );
        assert_eq!((hash_of(&t, &ma), hash_of(&t, &mb)), (ha, hb));
    }

    #[test]
    fn moved_memories_take_the_hash_of_their_new_project() {
        let mut t = T::new();
        let a = t.project("k/a", "a");
        let b = t.project("k/b", "b");
        let ma = t.remember(&a, "only in a");
        t.merge("a", "b");
        // The same text, written to the project now, is the memory that moved: a duplicate.
        let h = t.hlc();
        let m = memories::prepare(
            NewMemoryInput {
                content: "only in a".into(),
                kind: Some(Kind::Fact),
                ..Default::default()
            },
            &b,
            h,
        )
        .unwrap();
        match memories::insert(&t.c, &m).unwrap() {
            InsertOutcome::Duplicate { id } => assert_eq!(id, ma),
            other => panic!("{other:?}"),
        }
    }
}
