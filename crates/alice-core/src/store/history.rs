//! Snapshots of rows that events record, and undo (spec §5.5): an event is reversed only if
//! its target still looks exactly as the event left it; otherwise a review item asks.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::{Kind, Status};
use crate::normalize::content_hash;
use crate::redact::Redactor;
use crate::store::events::{self, Actor, EventInput, EventRow};
use crate::store::reviews::{self, NewReview, ReviewType};
use crate::store::{fts, kg, memories};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

/// The redactor in force for a project (by id; `None`: no project), looked up with the
/// connection of the transaction that undoes. Whatever undo restores or records goes through it.
pub type RedactorFor<'a> = &'a dyn Fn(&Connection, Option<&str>) -> Arc<Redactor>;

/// What undo restores for a memory: content, status, topics and outgoing links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub status: Status,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub importance: u8,
    pub project_id: String,
    pub topics: Vec<String>,
    /// (to_id, link type), sorted.
    pub links_out: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationSnapshot {
    pub subject_id: String,
    pub predicate: String,
    pub object_entity_id: Option<String>,
    pub object_literal: Option<String>,
    pub valid_from: String,
    pub valid_to: Option<String>,
    /// Kept so a relation that undo deleted comes back with its source.
    #[serde(default)]
    pub source_memory_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    /// `group` holds the undo's own events; `reverted` the events it reversed, newest first.
    Undone {
        group: String,
        reverted: Vec<String>,
    },
    /// Nothing was changed: the target changed since, and review item `review_id` asks
    /// (`created`: the item is new; otherwise it was already open, asking the same).
    Conflict {
        review_id: String,
        reason: String,
        created: bool,
    },
}

pub fn snapshot_memory(conn: &Connection, id: &str) -> Result<Option<MemorySnapshot>> {
    let Some((status, kind, title, body, importance, project_id)) = conn
        .prepare_cached(
            "SELECT status, kind, title, body, importance, project_id FROM memories WHERE id = ?1",
        )?
        .query_row([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u8>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .optional()?
    else {
        return Ok(None);
    };
    let topics: Vec<String> = conn
        .prepare_cached("SELECT topic FROM memory_topics WHERE memory_id = ?1 ORDER BY topic")?
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let links_out: Vec<(String, String)> = conn
        .prepare_cached("SELECT to_id, type FROM links WHERE from_id = ?1 ORDER BY to_id, type")?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(Some(MemorySnapshot {
        status: Status::parse(&status).unwrap_or(Status::Active),
        kind: Kind::parse(&kind).unwrap_or(Kind::Note),
        title,
        body,
        importance,
        project_id,
        topics,
        links_out,
    }))
}

pub fn snapshot_relation(conn: &Connection, id: &str) -> Result<Option<RelationSnapshot>> {
    Ok(conn
        .prepare_cached(
            "SELECT subject_id, predicate, object_entity_id, object_literal, valid_from, valid_to, source_memory_id
             FROM relations WHERE id = ?1",
        )?
        .query_row([id], |r| {
            Ok(RelationSnapshot {
                subject_id: r.get(0)?,
                predicate: r.get(1)?,
                object_entity_id: r.get(2)?,
                object_literal: r.get(3)?,
                valid_from: r.get(4)?,
                valid_to: r.get(5)?,
                source_memory_id: r.get(6)?,
            })
        })
        .optional()?)
}

fn to_json<T: Serialize>(v: T) -> Result<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| AliceError::Other(format!("snapshot: {e}")))
}

/// The two kinds of target undo can restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Memory,
    Relation,
    /// A predicate mapping: the vocabulary rows and relations it touched.
    Predicate,
}

/// Undo knows memories and relations; any other target type is refused as such.
fn target_kind(target_type: &str) -> Result<Target> {
    match target_type {
        "memory" => Ok(Target::Memory),
        "relation" => Ok(Target::Relation),
        "predicate" => Ok(Target::Predicate),
        other => Err(AliceError::invalid(format!(
            "changes to a {other} cannot be undone; only memory, fact and predicate changes can"
        ))),
    }
}

/// The target as it is now, in the form events record (`None`: the row does not exist).
/// `spec` is a recorded state of the same target: a predicate mapping names the vocabulary
/// entries and relations it touched there.
fn current(
    conn: &Connection,
    target_type: &str,
    id: &str,
    spec: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    match target_kind(target_type)? {
        Target::Memory => snapshot_memory(conn, id)?.map(to_json).transpose(),
        Target::Relation => snapshot_relation(conn, id)?.map(to_json).transpose(),
        Target::Predicate => {
            let Some(spec) = spec else { return Ok(None) };
            let spec: kg::PredicateSnapshot =
                serde_json::from_value(spec.clone()).map_err(|e| {
                    AliceError::invalid(format!("the recorded mapping is unreadable ({e})"))
                })?;
            let ids: Vec<String> = spec.relations.iter().map(|(i, _)| i.clone()).collect();
            to_json(kg::snapshot_predicates(conn, &spec.names, &ids)?).map(Some)
        }
    }
}

/// An event's recorded before and after states, read strictly: an unreadable one is an error,
/// never mistaken for "did not exist" (which undo would answer by trashing or deleting).
fn states(
    conn: &Connection,
    e: &EventRow,
) -> Result<(Option<serde_json::Value>, Option<serde_json::Value>)> {
    let (before, after): (Option<String>, Option<String>) = conn.query_row(
        "SELECT before_json, after_json FROM events WHERE id = ?1",
        [&e.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let parse = |which: &str, raw: Option<String>| -> Result<Option<serde_json::Value>> {
        raw.map(|s| {
            serde_json::from_str(&s).map_err(|err| {
                AliceError::invalid(format!(
                    "event {}: its recorded {which}-state is unreadable ({err}); it cannot be undone",
                    e.id
                ))
            })
        })
        .transpose()
    };
    Ok((parse("before", before)?, parse("after", after)?))
}

/// Activation (`pending` → `active` once background work is done) is not a change anyone
/// made: compare the two as one state, so a memory can be undone after it was activated.
fn comparable(v: Option<serde_json::Value>) -> Option<serde_json::Value> {
    v.map(|mut v| {
        if v.get("status").and_then(|s| s.as_str()) == Some("pending") {
            v["status"] = serde_json::Value::String("active".into());
        }
        v
    })
}

/// An undo that the rules refuse says so as an undo: block mode refused the restore, which
/// field and which detector (never the text), and that the change stays as it is.
fn refused_undo(e: AliceError) -> AliceError {
    match e {
        AliceError::Refused { field, detectors } => AliceError::invalid(format!(
            "cannot undo: the privacy rules (block mode) refused to restore the earlier {field}, which contains what looks like a secret ({}); nothing was changed",
            detectors.join(", ")
        )),
        e => e,
    }
}

/// `s` as it may be restored today: its text through the project's current redaction rules. A
/// secret that the rules would redact never comes back with an undo, and in `block` mode the
/// undo is refused (the message names the detector, not the text). With `refuse` false (an undo
/// that only trashes the memory) the text is redacted but never refused.
fn restorable(
    conn: &Connection,
    s: &MemorySnapshot,
    redact: RedactorFor<'_>,
    refuse: bool,
) -> Result<MemorySnapshot> {
    let r = redact(conn, Some(&s.project_id));
    let text = |field: &str, t: &str| -> Result<String> {
        if refuse {
            r.check(field, t).map_err(refused_undo)
        } else {
            Ok(r.apply(t).text)
        }
    };
    let mut out = s.clone();
    out.title = text("title", &s.title)?;
    out.body = text("body", &s.body)?;
    out.topics = s
        .topics
        .iter()
        .map(|t| text("topic", t))
        .collect::<Result<_>>()?;
    Ok(out)
}

/// The snapshots of one event, redacted by the rules of the project they name (`project_id`),
/// or the global rules: one lookup per event. History is never refused, only cleaned. The
/// writer records its events through this too.
pub fn scrub_snapshots(
    conn: &Connection,
    redact: RedactorFor<'_>,
    before: &mut Option<serde_json::Value>,
    after: &mut Option<serde_json::Value>,
) {
    let project = [&*before, &*after]
        .into_iter()
        .flatten()
        .find_map(|v| v.get("project_id").and_then(|p| p.as_str()))
        .map(String::from);
    let r = redact(conn, project.as_deref());
    for v in [before, after].into_iter().flatten() {
        r.apply_json(v);
    }
}

fn restore_memory(conn: &Connection, id: &str, s: &MemorySnapshot, hlc: &str) -> Result<()> {
    let seq =
        memories::seq_of(conn, id)?.ok_or_else(|| AliceError::NotFound(format!("memory {id}")))?;
    let slug: String = conn.query_row(
        "SELECT slug FROM projects WHERE id = ?1",
        [&s.project_id],
        |r| r.get(0),
    )?;
    let hash = content_hash(&slug, s.kind, &s.title, &s.body);
    if s.status != Status::Trashed {
        memories::ensure_unique_content(conn, seq, &s.project_id, &hash).map_err(|e| match e {
            AliceError::Invalid(m) => AliceError::invalid(format!("cannot undo: {m}")),
            e => e,
        })?;
    }
    let (old_title, old_body): (String, String) = conn.query_row(
        "SELECT title, body FROM memories WHERE seq = ?1",
        [seq],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let text_changed = old_title != s.title || old_body != s.body;
    // A restored `pending` means "live": the row's own embed/judge states say what is left to do.
    let status = if s.status == Status::Pending {
        Status::Active
    } else {
        s.status
    };
    conn.execute(
        "UPDATE memories SET status = ?1, kind = ?2, title = ?3, body = ?4, importance = ?5, project_id = ?6,
            content_hash = ?7, updated_at = ?8, hlc = ?9,
            embed_state = CASE WHEN ?10 THEN 'pending' ELSE embed_state END
         WHERE seq = ?11",
        params![
            status.as_str(),
            s.kind.as_str(),
            s.title,
            s.body,
            s.importance,
            s.project_id,
            hash,
            now_rfc3339(),
            hlc,
            text_changed,
            seq
        ],
    )?;
    conn.execute("DELETE FROM memory_topics WHERE memory_id = ?1", [id])?;
    for t in &s.topics {
        conn.execute(
            "INSERT INTO memory_topics (memory_id, topic) VALUES (?1, ?2)",
            params![id, t],
        )?;
    }
    fts::unindex_memory(conn, seq)?;
    fts::index_memory(conn, seq, &s.title, &s.body, &s.topics)?;
    conn.execute("DELETE FROM links WHERE from_id = ?1", [id])?;
    for (to, kind) in &s.links_out {
        conn.execute(
            "INSERT OR IGNORE INTO links (from_id, to_id, type, created_by, created_at) VALUES (?1, ?2, ?3, 'undo', ?4)",
            params![id, to, kind, now_rfc3339()],
        )?;
    }
    Ok(())
}

fn restore_relation(
    conn: &Connection,
    id: &str,
    s: Option<&RelationSnapshot>,
    hlc: &str,
    redact: RedactorFor<'_>,
) -> Result<()> {
    match s {
        // The relation did not exist before the event (an add): remove it.
        None => {
            conn.execute("DELETE FROM relations WHERE id = ?1", [id])?;
        }
        Some(s) => {
            let n = conn.execute(
                "UPDATE relations SET valid_from = ?1, valid_to = ?2, hlc = ?3 WHERE id = ?4",
                params![s.valid_from, s.valid_to, hlc, id],
            )?;
            if n == 0 {
                // Re-created from the recorded state: its text through today's rules.
                let r = redact(conn, None);
                let predicate = r.check("predicate", &s.predicate).map_err(refused_undo)?;
                let literal = s
                    .object_literal
                    .as_deref()
                    .map(|l| r.check("object", l).map_err(refused_undo))
                    .transpose()?;
                conn.execute(
                    "INSERT INTO relations (id, subject_id, predicate, object_entity_id, object_literal, valid_from, valid_to,
                        source_memory_id, created_at, hlc)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        id,
                        s.subject_id,
                        predicate,
                        s.object_entity_id,
                        literal,
                        s.valid_from,
                        s.valid_to,
                        s.source_memory_id,
                        now_rfc3339(),
                        hlc
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// Never undone: markers and changes whose previous state no longer exists anywhere.
fn undoable(action: &str) -> bool {
    !matches!(
        action,
        "undo"
            | "source.ingest"
            | "memory.purge"
            | "raw.retention"
            | "settings.change"
            | "secret.set"
            | "secret.delete"
    )
}

/// Why `e` cannot be undone now because of earlier undos, following undo → redo → undo… to
/// the newest step so the message names the event to undo instead. `None`: `e` was never
/// undone.
fn already_undone(conn: &Connection, e: &EventRow) -> Result<Option<String>> {
    let mut last = e.id.clone();
    let mut steps = 0usize;
    while let Some(marker) = events::undone_by(conn, &last)? {
        steps += 1;
        match events::reverting(conn, &marker)? {
            Some(u) => last = u.id,
            None => {
                last = marker.id;
                break;
            }
        }
    }
    Ok(match steps {
        0 => None,
        n if n % 2 == 1 => Some(format!(
            "event {} was already undone by event {last}; undo that one to redo the change",
            e.id
        )),
        _ => Some(format!(
            "event {} was undone and then redone by event {last}; undo that one to undo the change again",
            e.id
        )),
    })
}

/// Reverse `event_id` (and every event of its group, newest first). Run in a transaction (the
/// writer does): either all of them are reverted or none. A target that changed since its
/// event is never overwritten: a review item (one per group while it is open) asks instead.
pub fn undo(
    conn: &Connection,
    event_id: &str,
    actor: &Actor,
    hlc: &mut dyn FnMut() -> String,
    redact: RedactorFor<'_>,
) -> Result<UndoOutcome> {
    undo_with(conn, event_id, actor, hlc, redact, true)
}

/// Like [`undo`], without the "changed since" comparison: what a person decided when they
/// answered a review item with "undo anyway". Only that check is skipped. A purged target, an
/// event that cannot be undone, one already undone, and the privacy rules (block mode refuses a
/// restore that holds a secret, redact mode cleans it) apply as for `undo`.
pub fn force_undo(
    conn: &Connection,
    event_id: &str,
    actor: &Actor,
    hlc: &mut dyn FnMut() -> String,
    redact: RedactorFor<'_>,
) -> Result<UndoOutcome> {
    undo_with(conn, event_id, actor, hlc, redact, false)
}

fn undo_with(
    conn: &Connection,
    event_id: &str,
    actor: &Actor,
    hlc: &mut dyn FnMut() -> String,
    redact: RedactorFor<'_>,
    check_changed: bool,
) -> Result<UndoOutcome> {
    if event_id.trim().is_empty() {
        return Err(AliceError::invalid(
            "event_id is empty; pass the id of an event listed by \"alice history\"",
        ));
    }
    let ev = events::get(conn, event_id)?.ok_or_else(|| {
        AliceError::NotFound(format!(
            "event {event_id}; list events with \"alice history\""
        ))
    })?;
    if ev.action == "undo" {
        let hint = match events::reverting(conn, &ev)? {
            Some(u) => format!("; to reverse that undo, undo event {}", u.id),
            None => String::new(),
        };
        return Err(AliceError::invalid(format!(
            "event {event_id} only marks that event {} was undone{hint}",
            ev.target_id
        )));
    }
    if !undoable(&ev.action) {
        return Err(AliceError::invalid(format!(
            "event {event_id} ({}) cannot be undone",
            ev.action
        )));
    }
    let mut todo: Vec<EventRow> = match &ev.group_id {
        Some(g) => events::group(conn, g)?
            .into_iter()
            .filter(|e| undoable(&e.action))
            .collect(),
        None => vec![ev.clone()],
    };
    todo.reverse();
    for e in &todo {
        if let Some(why) = already_undone(conn, e)? {
            return Err(AliceError::invalid(why));
        }
    }
    // An event that changed nothing its snapshot holds (the survivor of a merge) has nothing to
    // revert and nothing to compare: a later change of that target neither blocks the undo nor
    // is overwritten by it.
    let mut unchanged = HashSet::new();
    for e in &todo {
        let (before, after) = states(conn, e)?;
        if before.is_some() && before == after {
            unchanged.insert(e.id.clone());
        }
    }
    // Each target must look as its newest event in the group left it.
    let mut checked = HashSet::new();
    for e in todo.iter().filter(|e| !unchanged.contains(&e.id)) {
        if !checked.insert((e.target_type.as_str(), e.target_id.as_str())) {
            continue;
        }
        let (_, after) = states(conn, e)?;
        let now = current(conn, &e.target_type, &e.target_id, after.as_ref())?;
        if now.is_none() && after.is_some() {
            return Err(AliceError::invalid(format!(
                "{} {} was purged from the trash; it cannot be restored",
                e.target_type, e.target_id
            )));
        }
        if check_changed && comparable(now) != comparable(after) {
            let reason = format!(
                "{} {} changed after event {}",
                e.target_type, e.target_id, e.id
            );
            let key = format!("undo:{}", ev.group_id.as_deref().unwrap_or(&ev.id));
            let (review_id, created) = reviews::open_or_create(
                conn,
                &NewReview {
                    kind: ReviewType::Undo,
                    payload: serde_json::json!({
                        "event_id": event_id,
                        "target_type": e.target_type,
                        "target_id": e.target_id,
                        "reason": reason,
                    }),
                    confidence: None,
                    dedupe_key: Some(key),
                },
            )?;
            return Ok(UndoOutcome::Conflict {
                review_id,
                reason,
                created,
            });
        }
    }
    let group = new_id();
    let mut reverted = Vec::new();
    for e in &todo {
        let h = hlc();
        let (before, after) = states(conn, e)?;
        let before_now = current(conn, &e.target_type, &e.target_id, after.as_ref())?;
        let bad = |err: serde_json::Error| {
            AliceError::invalid(format!(
                "event {}: its recorded state is unreadable ({err}); it cannot be undone",
                e.id
            ))
        };
        match (target_kind(&e.target_type)?, before) {
            // Nothing to put back (see `unchanged`); the undo is still recorded for it.
            _ if unchanged.contains(&e.id) => {}
            (Target::Memory, Some(b)) => {
                let s: MemorySnapshot = serde_json::from_value(b).map_err(bad)?;
                restore_memory(conn, &e.target_id, &restorable(conn, &s, redact, true)?, &h)?;
            }
            // Only a create has no memory before it: undoing it forgets the memory (it stays
            // in the trash for 30 days).
            (Target::Memory, None) if e.action == "memory.create" => {
                let mut s: MemorySnapshot =
                    serde_json::from_value(after.clone().unwrap_or_default()).map_err(bad)?;
                s.status = Status::Trashed;
                // It only goes to the trash: redact what it holds, never refuse.
                restore_memory(
                    conn,
                    &e.target_id,
                    &restorable(conn, &s, redact, false)?,
                    &h,
                )?;
            }
            (Target::Memory, None) => {
                return Err(AliceError::invalid(format!(
                    "event {} ({}) records no before-state for memory {}; it cannot be undone",
                    e.id, e.action, e.target_id
                )));
            }
            (Target::Relation, Some(b)) => {
                let s: RelationSnapshot = serde_json::from_value(b).map_err(bad)?;
                restore_relation(conn, &e.target_id, Some(&s), &h, redact)?;
            }
            (Target::Predicate, Some(b)) => {
                let s: kg::PredicateSnapshot = serde_json::from_value(b).map_err(bad)?;
                kg::restore_predicates(conn, &s, &h)?;
            }
            (Target::Predicate, None) => {
                return Err(AliceError::invalid(format!(
                    "event {} ({}) records no before-state for predicate {}; it cannot be undone",
                    e.id, e.action, e.target_id
                )));
            }
            // Only an add has no relation before it: undoing it deletes the relation.
            (Target::Relation, None) if e.action == "relation.add" => {
                restore_relation(conn, &e.target_id, None, &h, redact)?;
            }
            (Target::Relation, None) => {
                return Err(AliceError::invalid(format!(
                    "event {} ({}) records no before-state for fact {}; it cannot be undone",
                    e.id, e.action, e.target_id
                )));
            }
        }
        let mut after_now = current(conn, &e.target_type, &e.target_id, after.as_ref())?;
        let mut before_now = before_now;
        scrub_snapshots(conn, redact, &mut before_now, &mut after_now);
        events::record(
            conn,
            &EventInput {
                actor,
                action: &format!("undo:{}", e.action),
                target_type: &e.target_type,
                target_id: &e.target_id,
                before: before_now,
                after: after_now,
                group: Some(&group),
            },
            &h,
        )?;
        events::record(
            conn,
            &EventInput {
                actor,
                action: "undo",
                target_type: "event",
                target_id: &e.id,
                before: None,
                after: None,
                group: Some(&group),
            },
            &h,
        )?;
        reverted.push(e.id.clone());
    }
    Ok(UndoOutcome::Undone { group, reverted })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Kind, Status};
    use crate::project::ProjectRef;
    use crate::store::events::{self, Actor, EventInput};
    use crate::store::kg::{self, AssertFact, FactAction};
    use crate::store::memories::{self, MemoryUpdate, NewMemoryInput, UpdateAction};
    use crate::store::{open_writer, projects};

    struct T {
        _d: tempfile::TempDir,
        c: rusqlite::Connection,
        p: projects::ResolvedProject,
        n: u64,
    }

    fn snap(c: &Connection, id: &str) -> Option<serde_json::Value> {
        snapshot_memory(c, id)
            .unwrap()
            .map(|s| serde_json::to_value(s).unwrap())
    }

    fn rel(c: &Connection, id: &str) -> Option<serde_json::Value> {
        snapshot_relation(c, id)
            .unwrap()
            .map(|s| serde_json::to_value(s).unwrap())
    }

    impl T {
        fn new() -> T {
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
            T { _d: d, c, p, n: 0 }
        }
        fn hlc(&mut self) -> String {
            self.n += 1;
            format!("{:013}-0000-dev", 1_000 + self.n)
        }
        fn record(&self, e: EventInput<'_>, h: &str) -> String {
            events::record(&self.c, &e, h).unwrap()
        }
        /// remember + its event, as the writer does.
        fn remember(&mut self, text: &str) -> String {
            self.remember_superseding(text, &[]).0
        }
        /// remember + its events: the create and, in one group with it, every memory it
        /// superseded (snapshots taken before the insert), as the writer does.
        fn remember_superseding(&mut self, text: &str, old: &[&str]) -> (String, Vec<String>) {
            let h = self.hlc();
            let befores: Vec<_> = old.iter().map(|o| (*o, snap(&self.c, o))).collect();
            let m = memories::prepare(
                NewMemoryInput {
                    content: text.into(),
                    kind: Some(Kind::Fact),
                    supersedes: old.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
                &self.p,
                h.clone(),
            )
            .unwrap();
            let memories::InsertOutcome::New { id, .. } = memories::insert(&self.c, &m).unwrap()
            else {
                panic!()
            };
            let group = (!old.is_empty()).then(crate::ids::new_id);
            let mut evs = vec![self.record(
                EventInput {
                    actor: &Actor::Agent("claude-code".into()),
                    action: "memory.create",
                    target_type: "memory",
                    target_id: &id,
                    before: None,
                    after: snap(&self.c, &id),
                    group: group.as_deref(),
                },
                &h,
            )];
            for (o, before) in befores {
                evs.push(self.record(
                    EventInput {
                        actor: &Actor::Agent("claude-code".into()),
                        action: "memory.supersede",
                        target_type: "memory",
                        target_id: o,
                        before,
                        after: snap(&self.c, o),
                        group: group.as_deref(),
                    },
                    &h,
                ));
            }
            (id, evs)
        }
        fn update(&mut self, id: &str, u: MemoryUpdate, action: &str) -> String {
            let before = snap(&self.c, id);
            let h = self.hlc();
            memories::apply_update(&self.c, id, &u, h.clone()).unwrap();
            self.record(
                EventInput {
                    actor: &Actor::User,
                    action,
                    target_type: "memory",
                    target_id: id,
                    before,
                    after: snap(&self.c, id),
                    group: None,
                },
                &h,
            )
        }
        fn edit(&mut self, id: &str, body: &str) -> String {
            self.update(
                id,
                MemoryUpdate {
                    action: UpdateAction::Edit,
                    body: Some(body.into()),
                    ..Default::default()
                },
                "memory.edit",
            )
        }
        /// assert_fact + its events (open relations of the subject and predicate that changed,
        /// and the new relation), in one group, as the writer does.
        fn fact(&mut self, object: &str, action: FactAction) -> Vec<String> {
            let open: Vec<String> = self
                .c
                .prepare("SELECT id FROM relations WHERE valid_to IS NULL ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let befores: Vec<_> = open
                .iter()
                .map(|id| (id.clone(), rel(&self.c, id)))
                .collect();
            let h = self.hlc();
            let out = kg::assert_fact(
                &self.c,
                &AssertFact {
                    subject: "alice".into(),
                    predicate: "uses".into(),
                    object: object.into(),
                    action,
                    ..Default::default()
                },
                h.clone(),
            )
            .unwrap();
            let group = crate::ids::new_id();
            let mut evs = Vec::new();
            for (id, before) in befores {
                let after = rel(&self.c, &id);
                if after != before {
                    evs.push(self.record(
                        EventInput {
                            actor: &Actor::User,
                            action: "relation.close",
                            target_type: "relation",
                            target_id: &id,
                            before,
                            after,
                            group: Some(&group),
                        },
                        &h,
                    ));
                }
            }
            if let Some(rid) = out.relation_id.filter(|r| !open.contains(r)) {
                evs.push(self.record(
                    EventInput {
                        actor: &Actor::User,
                        action: "relation.add",
                        target_type: "relation",
                        target_id: &rid,
                        before: None,
                        after: rel(&self.c, &rid),
                        group: Some(&group),
                    },
                    &h,
                ));
            }
            evs
        }
        fn body(&self, id: &str) -> String {
            self.c
                .query_row("SELECT body FROM memories WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        }
        fn status(&self, id: &str) -> String {
            self.c
                .query_row("SELECT status FROM memories WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        }
        /// (object, valid_to) of every relation, by object.
        fn relations(&self) -> Vec<(String, Option<String>)> {
            self.c
                .prepare(
                    "SELECT COALESCE(e.name, r.object_literal), r.valid_to FROM relations r
                     LEFT JOIN entities e ON e.id = r.object_entity_id ORDER BY 1",
                )
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        }
        fn undo(&mut self, event: &str) -> Result<UndoOutcome> {
            self.undo_with(event, Redactor::builtin())
        }
        fn undo_with(&mut self, event: &str, redactor: Redactor) -> Result<UndoOutcome> {
            let mut n = self.n;
            let mut tick = || {
                n += 1;
                format!("{:013}-0000-dev", 1_000 + n)
            };
            let redactor = Arc::new(redactor);
            let rules = |_: &Connection, _: Option<&str>| redactor.clone();
            let out = undo(&self.c, event, &Actor::User, &mut tick, &rules);
            self.n = n;
            out
        }
        fn force_undo_with(&mut self, event: &str, redactor: Redactor) -> Result<UndoOutcome> {
            let mut n = self.n;
            let mut tick = || {
                n += 1;
                format!("{:013}-0000-dev", 1_000 + n)
            };
            let redactor = Arc::new(redactor);
            let rules = |_: &Connection, _: Option<&str>| redactor.clone();
            let out = force_undo(&self.c, event, &Actor::User, &mut tick, &rules);
            self.n = n;
            out
        }
        fn reverting(&self, ev: &str, action: &str) -> events::EventRow {
            let marker = events::undone_by(&self.c, ev).unwrap().unwrap();
            events::group(&self.c, marker.group_id.as_deref().unwrap())
                .unwrap()
                .into_iter()
                .find(|e| e.action == action)
                .unwrap()
        }
    }

    #[test]
    fn undo_edit_restores_text_reindexes_and_records_the_undo() {
        let mut t = T::new();
        let id = t.remember("Use WAL mode for readers");
        let ev = t.edit(&id, "Use rollback journal");
        let out = t.undo(&ev).unwrap();
        assert!(
            matches!(out, UndoOutcome::Undone { ref reverted, .. } if reverted == &vec![ev.clone()])
        );
        assert_eq!(t.body(&id), "Use WAL mode for readers");
        let hit: i64 =
            t.c.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH '\"WAL\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1, "FTS follows the restored text");
        let state: String =
            t.c.query_row(
                "SELECT embed_state FROM memories WHERE id = ?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "pending", "restored text is embedded again");
        assert!(events::undone_by(&t.c, &ev).unwrap().is_some());
        let undo_ev = t.reverting(&ev, "undo:memory.edit");
        assert_eq!(undo_ev.actor, "user");
        assert_eq!(undo_ev.target_id, id);
        assert_eq!(undo_ev.after.unwrap()["body"], "Use WAL mode for readers");
    }

    #[test]
    fn undo_after_a_later_change_creates_a_review_item_and_changes_nothing() {
        let mut t = T::new();
        let id = t.remember("first text");
        let ev = t.edit(&id, "second text");
        t.edit(&id, "third text");
        match t.undo(&ev).unwrap() {
            UndoOutcome::Conflict {
                review_id, reason, ..
            } => {
                assert!(reason.contains(&id), "{reason}");
                let kind: String =
                    t.c.query_row(
                        "SELECT type FROM review_items WHERE id = ?1",
                        [&review_id],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(kind, "undo");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.body(&id), "third text");
        assert!(
            events::undone_by(&t.c, &ev).unwrap().is_none(),
            "a refused undo records no marker"
        );
    }

    /// Asking again while the first question is open names the same review item.
    #[test]
    fn a_conflicting_undo_asked_again_names_the_same_review_item() {
        let mut t = T::new();
        let id = t.remember("first text");
        let ev = t.edit(&id, "second text");
        t.edit(&id, "third text");
        let first = match t.undo(&ev).unwrap() {
            UndoOutcome::Conflict {
                review_id, created, ..
            } => {
                assert!(created, "the first ask adds the item");
                review_id
            }
            other => panic!("{other:?}"),
        };
        let second = match t.undo(&ev).unwrap() {
            UndoOutcome::Conflict {
                review_id, created, ..
            } => {
                assert!(!created, "asking again reuses the open item");
                review_id
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(first, second);
        assert_eq!(reviews::count_open(&t.c).unwrap(), 1);
        let payload: String =
            t.c.query_row(
                "SELECT payload_json FROM review_items WHERE id = ?1",
                [&first],
                |r| r.get(0),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["event_id"], ev.as_str());
        assert_eq!(payload["target_id"], id.as_str());
    }

    #[test]
    fn undo_twice_is_refused_and_undo_of_the_undo_redoes() {
        let mut t = T::new();
        let id = t.remember("a");
        let ev = t.edit(&id, "b");
        t.undo(&ev).unwrap();
        let err = t.undo(&ev).unwrap_err().to_string();
        assert!(err.contains("already undone"), "{err}");
        let marker = events::undone_by(&t.c, &ev).unwrap().unwrap();
        let redo_target = events::group(&t.c, marker.group_id.as_deref().unwrap())
            .unwrap()
            .into_iter()
            .find(|e| e.action == "undo:memory.edit")
            .unwrap();
        assert!(
            err.contains(&redo_target.id),
            "the error names the event to undo to redo: {err}"
        );
        t.undo(&redo_target.id).unwrap();
        assert_eq!(t.body(&id), "b");
    }

    /// `alice history` lists the marker next to the change; the marker itself is not a change,
    /// so undoing it is refused with the event that does what was meant.
    #[test]
    fn undoing_a_marker_names_the_event_to_undo_instead() {
        let mut t = T::new();
        let id = t.remember("a");
        let ev = t.edit(&id, "b");
        t.undo(&ev).unwrap();
        let marker = events::undone_by(&t.c, &ev).unwrap().unwrap();
        let redo = t.reverting(&ev, "undo:memory.edit");
        let err = t.undo(&marker.id).unwrap_err();
        assert!(
            matches!(&err, AliceError::Invalid(m) if m.contains(&redo.id)),
            "{err}"
        );
        assert_eq!(t.body(&id), "a", "nothing changed");
        // After a redo, undoing the original again names the redo to undo.
        t.undo(&redo.id).unwrap();
        let again = t.reverting(&redo.id, "undo:undo:memory.edit");
        let err = t.undo(&ev).unwrap_err().to_string();
        assert!(err.contains("redone") && err.contains(&again.id), "{err}");
        t.undo(&again.id).unwrap();
        assert_eq!(t.body(&id), "a");
    }

    #[test]
    fn undo_of_a_create_trashes_and_a_purged_memory_cannot_come_back() {
        let mut t = T::new();
        let id = t.remember("temporary note");
        // Background activation (pending → active) does not count as a change since the event.
        let seq = memories::seq_of(&t.c, &id).unwrap().unwrap();
        memories::set_status_by_seq(&t.c, seq, Status::Active).unwrap();
        let create = events::list(
            &t.c,
            &events::EventQuery {
                target_id: Some(id.clone()),
                ..Default::default()
            },
        )
        .unwrap()
        .pop()
        .unwrap();
        t.undo(&create.id).unwrap();
        assert_eq!(t.status(&id), Status::Trashed.as_str());
        // Redo brings it back as it was.
        t.undo(&t.reverting(&create.id, "undo:memory.create").id)
            .unwrap();
        assert_eq!(t.status(&id), Status::Active.as_str());

        let id2 = t.remember("will be purged");
        let ev2 = t.edit(&id2, "edited");
        t.c.execute("DELETE FROM memories WHERE id = ?1", [&id2])
            .unwrap();
        let err = t.undo(&ev2).unwrap_err().to_string();
        assert!(err.contains("purged"), "{err}");
        let _ = Author::default();
    }

    /// A corrupt record must never read as "the memory did not exist", which undo answers by
    /// trashing it: an unreadable snapshot, or an edit without one, is refused and nothing
    /// changes.
    #[test]
    fn an_unreadable_or_missing_snapshot_is_refused_not_read_as_a_create() {
        let mut t = T::new();
        let id = t.remember("keep me");
        let ev = t.edit(&id, "keep me, edited");
        t.c.execute(
            "UPDATE events SET before_json = '{not json' WHERE id = ?1",
            [&ev],
        )
        .unwrap();
        let err = t.undo(&ev).unwrap_err();
        assert!(
            matches!(&err, AliceError::Invalid(m) if m.contains(&ev) && m.contains("unreadable")),
            "{err}"
        );
        assert_eq!(
            (t.status(&id).as_str(), t.body(&id).as_str()),
            ("pending", "keep me, edited")
        );
        t.c.execute("UPDATE events SET before_json = NULL WHERE id = ?1", [&ev])
            .unwrap();
        let err = t.undo(&ev).unwrap_err();
        assert!(
            matches!(&err, AliceError::Invalid(m) if m.contains(&ev) && m.contains("before")),
            "{err}"
        );
        assert_eq!(t.status(&id), "pending", "not trashed");
        // An unreadable after-state is refused too, not taken for a change.
        let ev2 = t.edit(&id, "edited again");
        t.c.execute("UPDATE events SET after_json = '[' WHERE id = ?1", [&ev2])
            .unwrap();
        let err = t.undo(&ev2).unwrap_err().to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert_eq!(reviews::count_open(&t.c).unwrap(), 0);
    }

    /// Undo knows memories and relations. A change to anything else is refused as such, never
    /// reported as purged.
    #[test]
    fn events_on_other_targets_are_refused_as_such() {
        let mut t = T::new();
        for (action, target_type) in [("widget.change", "widget"), ("source.ingest", "source")] {
            let ev = t.record(
                EventInput {
                    actor: &Actor::User,
                    action,
                    target_type,
                    target_id: "X1",
                    before: Some(serde_json::json!({"a": 1})),
                    after: Some(serde_json::json!({"a": 2})),
                    group: None,
                },
                "h",
            );
            let err = t.undo(&ev).unwrap_err().to_string();
            assert!(
                err.contains("cannot be undone") && !err.contains("purged"),
                "{action}: {err}"
            );
        }
    }

    #[test]
    fn unknown_events_are_not_found() {
        let mut t = T::new();
        let err = t.undo("01K7NOSUCHEVENT00000000000").unwrap_err();
        assert!(
            matches!(&err, AliceError::NotFound(m) if m.contains("alice history")),
            "{err}"
        );
        let err = t.undo("  ").unwrap_err();
        assert!(
            matches!(&err, AliceError::Invalid(m) if m.contains("empty")),
            "{err}"
        );
    }

    /// A supersede at write time touches two memories; undoing either event reverts both, and
    /// a later edit of either one stops the whole undo.
    #[test]
    fn a_group_is_undone_whole_or_not_at_all() {
        let mut t = T::new();
        let old = t.remember("port is 8080");
        let (new, evs) = t.remember_superseding("port is 9090", &[&old]);
        assert_eq!(t.status(&old), "superseded");
        let edit = t.edit(&new, "port is 9091");
        match t.undo(&evs[1]).unwrap() {
            UndoOutcome::Conflict { reason, .. } => assert!(reason.contains(&new), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(t.status(&old), "superseded", "nothing was reverted");

        // Once the later edit is undone, the group can be.
        t.undo(&edit).unwrap();
        match t.undo(&evs[1]).unwrap() {
            UndoOutcome::Undone { reverted, .. } => {
                let mut want = evs.clone();
                want.reverse();
                assert_eq!(reverted, want, "newest first");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            t.status(&old),
            "active",
            "live again (a restored pending is active)"
        );
        assert_eq!(t.status(&new), "trashed");
        let err = t.undo(&evs[0]).unwrap_err().to_string();
        assert!(err.contains("already undone"), "{err}");
    }

    /// Undo never overwrites a live memory: restoring text another memory now holds is refused
    /// with that memory's id, and nothing changes.
    #[test]
    fn restoring_text_another_memory_now_holds_is_refused_naming_it() {
        let mut t = T::new();
        let id = t.remember("gamma");
        let forget = t.update(
            &id,
            MemoryUpdate {
                action: UpdateAction::Forget,
                ..Default::default()
            },
            "memory.forget",
        );
        let id2 = t.remember("gamma");
        let err = t.undo(&forget).unwrap_err();
        assert!(
            matches!(&err, AliceError::Invalid(m) if m.contains(&id2)),
            "{err}"
        );
        assert_eq!(t.status(&id), "trashed");
        assert!(events::undone_by(&t.c, &forget).unwrap().is_none());
    }

    #[test]
    fn undo_of_a_fact_supersede_reopens_the_old_value_and_removes_the_new_one() {
        let mut t = T::new();
        t.fact("SQLite", FactAction::Add);
        let evs = t.fact("Postgres", FactAction::Supersede);
        assert_eq!(evs.len(), 2, "close + add");
        let closed = t.relations();
        assert!(closed[0].1.is_none() && closed[1].1.is_some(), "{closed:?}");
        match t.undo(&evs[1]).unwrap() {
            UndoOutcome::Undone { reverted, .. } => assert_eq!(reverted.len(), 2),
            other => panic!("{other:?}"),
        }
        assert_eq!(t.relations(), vec![("SQLite".to_string(), None)]);
        // Redo: the new value is back and the old one closed again, as before.
        let redo = t.reverting(&evs[1], "undo:relation.add");
        t.undo(&redo.id).unwrap();
        assert_eq!(t.relations(), closed);
    }

    #[test]
    fn undo_of_a_fact_end_reopens_it_unless_it_changed_since() {
        let mut t = T::new();
        t.fact("SQLite", FactAction::Add);
        let end = t.fact("SQLite", FactAction::End);
        assert_eq!(end.len(), 1);
        t.undo(&end[0]).unwrap();
        assert_eq!(t.relations(), vec![("SQLite".to_string(), None)]);

        let end = t.fact("SQLite", FactAction::End);
        // Someone moves the end date meanwhile.
        t.c.execute(
            "UPDATE relations SET valid_to = '2030-01-01T00:00:00.000Z'",
            [],
        )
        .unwrap();
        assert!(matches!(
            t.undo(&end[0]).unwrap(),
            UndoOutcome::Conflict { .. }
        ));
    }

    /// Text recorded before the rules existed comes back redacted, or not at all in block mode.
    #[test]
    fn undo_never_restores_a_secret_the_rules_would_redact() {
        use crate::settings::{PrivacySettings, RedactMode};
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let mut t = T::new();
        let id = t.remember(&format!("deploy with {token}"));
        let ev = t.edit(&id, "deploy by hand");
        // Block mode: refused, naming the detector and not the token; nothing changed.
        let block = Redactor::new(&PrivacySettings {
            redact_mode: RedactMode::Block,
            ..Default::default()
        })
        .unwrap();
        let err = t.undo_with(&ev, block).unwrap_err().to_string();
        assert!(
            err.contains("github_token") && !err.contains(token),
            "{err}"
        );
        // Worded as a refused restore: field and detector named, no advice for a new write.
        assert!(
            err.contains("cannot undo")
                && err.contains("privacy rules")
                && err.contains("earlier title"),
            "{err}"
        );
        assert!(
            !err.contains("send again") && !err.contains("remove it"),
            "{err}"
        );
        assert_eq!(t.body(&id), "deploy by hand");
        // Redact mode: restored with the token replaced, and the undo's own events carry no token.
        t.undo(&ev).unwrap();
        assert_eq!(t.body(&id), "deploy with [REDACTED:github_token]");
        let rows: Vec<String> = t
            .c
            .prepare("SELECT COALESCE(before_json, '') || COALESCE(after_json, '') FROM events WHERE action LIKE 'undo:%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            !rows.is_empty() && rows.iter().all(|r| !r.contains(token)),
            "{rows:?}"
        );
    }

    /// A relation re-created by an undo gets its text through the rules too (block mode refuses).
    #[test]
    fn undo_never_restores_relation_text_the_rules_would_redact() {
        use crate::settings::{PrivacySettings, RedactMode};
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let mut t = T::new();
        t.fact("seed", FactAction::Add);
        let subject: String =
            t.c.query_row("SELECT id FROM entities WHERE name = 'alice'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let state = |predicate: &str| {
            Some(serde_json::json!({
                "subject_id": subject,
                "predicate": predicate,
                "object_entity_id": null,
                "object_literal": format!("value {token}"),
                "valid_from": "2026-01-01T00:00:00.000Z",
                "valid_to": null,
                "source_memory_id": null,
            }))
        };
        // Events recorded by an older Alice: the relation row is gone, its text held a secret.
        let mut ev = |id: &str, predicate: &str| {
            let h = t.hlc();
            t.record(
                EventInput {
                    actor: &Actor::User,
                    action: "relation.close",
                    target_type: "relation",
                    target_id: id,
                    before: state(predicate),
                    after: None,
                    group: None,
                },
                &h,
            )
        };
        let (a, b) = (
            ev("01RELATIONGONE00000000A000", "uses"),
            ev("01RELATIONGONE00000000B000", "uses"),
        );
        let block = Redactor::new(&PrivacySettings {
            redact_mode: RedactMode::Block,
            ..Default::default()
        })
        .unwrap();
        let err = t.undo_with(&a, block).unwrap_err().to_string();
        assert!(
            err.contains("github_token") && !err.contains(token),
            "{err}"
        );
        assert!(
            err.contains("cannot undo") && err.contains("object") && !err.contains("send again"),
            "{err}"
        );
        t.undo(&b).unwrap();
        let literal: String =
            t.c.query_row(
                "SELECT object_literal FROM relations WHERE id = '01RELATIONGONE00000000B000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(literal, "value [REDACTED:github_token]");
    }

    /// R1: a relation event without a before-state is an add (undo deletes the relation); any
    /// other action with a NULL before is refused, never read as an add.
    #[test]
    fn a_relation_event_without_a_before_state_is_undone_only_when_it_is_an_add() {
        let mut t = T::new();
        let evs = t.fact("SQLite", FactAction::Add);
        let add = evs.last().unwrap().clone();
        let rid: String =
            t.c.query_row("SELECT target_id FROM events WHERE id = ?1", [&add], |r| {
                r.get(0)
            })
            .unwrap();
        // A close whose before-state is missing (damaged history) must not delete the relation.
        let h = t.hlc();
        let bad = t.record(
            EventInput {
                actor: &Actor::User,
                action: "relation.close",
                target_type: "relation",
                target_id: &rid,
                before: None,
                after: rel(&t.c, &rid),
                group: None,
            },
            &h,
        );
        let err = t.undo(&bad).unwrap_err().to_string();
        assert!(
            err.contains("relation.close") && err.contains("no before-state"),
            "{err}"
        );
        assert_eq!(t.relations().len(), 1, "the relation was not deleted");
        // The add is undone by deleting the relation.
        let h = t.hlc();
        let add2 = t.record(
            EventInput {
                actor: &Actor::User,
                action: "relation.add",
                target_type: "relation",
                target_id: &rid,
                before: None,
                after: rel(&t.c, &rid),
                group: None,
            },
            &h,
        );
        t.undo(&add2).unwrap();
        assert!(t.relations().is_empty());
    }

    /// "Force" skips the changed-since comparison and nothing else.
    #[test]
    fn force_undo_restores_over_a_later_change_but_not_twice_and_not_purged() {
        let mut t = T::new();
        let id = t.remember("first text");
        let ev = t.edit(&id, "second text");
        t.edit(&id, "third text");
        assert!(matches!(t.undo(&ev).unwrap(), UndoOutcome::Conflict { .. }));
        let out = t.force_undo_with(&ev, Redactor::builtin()).unwrap();
        assert!(
            matches!(out, UndoOutcome::Undone { ref reverted, .. } if reverted == &vec![ev.clone()])
        );
        assert_eq!(t.body(&id), "first text");
        let err = t
            .force_undo_with(&ev, Redactor::builtin())
            .unwrap_err()
            .to_string();
        assert!(err.contains("already undone"), "{err}");
        // Purged targets stay refused.
        let id2 = t.remember("to be purged");
        let ev2 = t.edit(&id2, "to be purged, edited");
        t.c.execute("DELETE FROM memories WHERE id = ?1", [&id2])
            .unwrap();
        let err = t
            .force_undo_with(&ev2, Redactor::builtin())
            .unwrap_err()
            .to_string();
        assert!(err.contains("purged"), "{err}");
    }

    /// R2: force skips the conflict check only, never the privacy rules.
    #[test]
    fn force_undo_never_restores_a_secret_the_rules_would_redact() {
        use crate::settings::{PrivacySettings, RedactMode};
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let mut t = T::new();
        let id = t.remember(&format!("deploy with {token}"));
        let ev = t.edit(&id, "deploy by hand");
        t.edit(&id, "deploy by pipeline"); // the target changed since: only force restores
        let block = Redactor::new(&PrivacySettings {
            redact_mode: RedactMode::Block,
            ..Default::default()
        })
        .unwrap();
        let err = t.force_undo_with(&ev, block).unwrap_err().to_string();
        assert!(
            err.contains("github_token") && err.contains("cannot undo") && !err.contains(token),
            "{err}"
        );
        assert_eq!(t.body(&id), "deploy by pipeline", "nothing was restored");
        t.force_undo_with(&ev, Redactor::builtin()).unwrap();
        assert_eq!(t.body(&id), "deploy with [REDACTED:github_token]");
        let rows: Vec<String> = t
            .c
            .prepare("SELECT COALESCE(before_json, '') || COALESCE(after_json, '') FROM events WHERE action LIKE 'undo:%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            !rows.is_empty() && rows.iter().all(|r| !r.contains(token)),
            "{rows:?}"
        );
    }

    /// A predicate mapping is recorded with everything it touched, and undone to the letter.
    #[test]
    fn undo_of_a_predicate_mapping_restores_the_vocabulary_and_every_relation() {
        let mut t = T::new();
        for (p, o) in [("uses", "SQLite"), ("uses", "Rust"), ("use", "Tokio")] {
            let h = t.hlc();
            kg::assert_fact(
                &t.c,
                &AssertFact {
                    subject: "alice".into(),
                    predicate: p.into(),
                    object: o.into(),
                    action: FactAction::Add,
                    ..Default::default()
                },
                h,
            )
            .unwrap();
        }
        t.c.execute(
            "INSERT INTO predicates (name, canonical) VALUES ('employs', 'uses')",
            [],
        )
        .unwrap();
        type Vocabulary = (Vec<(String, Option<String>)>, Vec<(String, String)>);
        let state = |t: &T| -> Vocabulary {
            let rows =
                t.c.prepare("SELECT name, canonical FROM predicates ORDER BY name")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect();
            let rels =
                t.c.prepare("SELECT id, predicate FROM relations ORDER BY id")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect();
            (rows, rels)
        };
        let before = state(&t);
        let h = t.hlc();
        let (b, a) = kg::map_predicate_recorded(&t.c, "uses", "use", &h).unwrap();
        assert_ne!(state(&t), before);
        let ev = t.record(
            EventInput {
                actor: &Actor::User,
                action: "predicate.map",
                target_type: "predicate",
                target_id: "uses",
                before: Some(serde_json::to_value(&b).unwrap()),
                after: Some(serde_json::to_value(&a).unwrap()),
                group: None,
            },
            &h,
        );
        t.undo(&ev).unwrap();
        assert_eq!(
            state(&t),
            before,
            "relations and alias rows are as they were"
        );
        let err = t.undo(&ev).unwrap_err().to_string();
        assert!(err.contains("already undone"), "{err}");
        // Redo (undo of the undo) maps again; a later change to a touched relation stops an undo.
        let marker = t.reverting(&ev, "undo:predicate.map");
        t.undo(&marker.id).unwrap();
        assert_ne!(state(&t), before);
        t.c.execute("UPDATE relations SET predicate = 'other' WHERE predicate = 'use' AND id = (SELECT min(id) FROM relations)", [])
            .unwrap();
        let ev2 = t.reverting(&marker.id, "undo:undo:predicate.map").id;
        assert!(matches!(
            t.undo(&ev2).unwrap(),
            UndoOutcome::Conflict { .. }
        ));
    }

    #[test]
    fn restoring_a_merged_duplicate_by_hand_drops_its_duplicate_link() {
        let mut t = T::new();
        let a = t.remember("survivor");
        let b = t.remember("duplicate");
        let h = t.hlc();
        memories::merge_into(&t.c, &a, std::slice::from_ref(&b), &h).unwrap();
        let links = |t: &T| -> i64 {
            t.c.query_row(
                "SELECT count(*) FROM links WHERE from_id = ?1 AND type = 'duplicate_of'",
                [&b],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(links(&t), 1);
        let h = t.hlc();
        memories::apply_update(
            &t.c,
            &b,
            &MemoryUpdate {
                action: UpdateAction::Restore,
                ..Default::default()
            },
            h,
        )
        .unwrap();
        assert_eq!((links(&t), t.status(&b).as_str()), (0, "active"));
    }
}
