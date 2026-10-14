//! Knowledge graph: entities, predicates, time-bounded relations and mentions.
//!
//! Relation times are stored as RFC 3339 UTC with milliseconds, so text order is time order.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::Author;
use crate::normalize::{nfc, truncate_chars};
use chrono::{DateTime, NaiveDate, NaiveTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_normalization::char::is_combining_mark;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FactAction {
    #[default]
    Add,
    Supersede,
    End,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    /// Entity for short names; literal for long text, numbers, dates, URLs and addresses.
    #[default]
    Auto,
    Entity,
    Literal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AssertFact {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    #[serde(default)]
    pub object_kind: ObjectKind,
    /// When the fact became true (or, for `end`, when it stopped): RFC 3339 such as
    /// 2026-10-13T09:44:23Z, or a date such as 2026-10-13 (midnight UTC). Defaults to now.
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub action: FactAction,
    #[serde(default)]
    pub source_memory_id: Option<String>,
    /// Who asserts it (filled by the daemon: the MCP client, or the CLI); history only.
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FactOutcome {
    pub relation_id: Option<String>,
    pub ended: usize,
    pub predicate: String,
    /// An existing predicate that looks similar (filled by the Judge in M2).
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Fact {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub object_is_entity: bool,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub source_memory_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FactsQuery {
    pub entity: String,
    #[serde(default)]
    pub predicate: Option<String>,
    /// Only facts valid at this time: RFC 3339 or a date such as 2026-10-13 (midnight UTC).
    #[serde(default)]
    pub as_of: Option<String>,
    /// All facts, past and present, ordered by start time.
    #[serde(default)]
    pub timeline: bool,
}

pub fn alias_key(name: &str) -> String {
    nfc(name)
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lowercase snake_case: each run of other characters becomes one `_`. Combining marks (Thai
/// vowels and tone marks) stay with their letter.
pub fn normalize_predicate(p: &str) -> String {
    let mut out = String::new();
    for ch in nfc(p).to_lowercase().chars() {
        if ch.is_alphanumeric() || is_combining_mark(ch) {
            out.push(ch);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

pub fn find_entity(conn: &Connection, name: &str) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT e.id, e.name FROM entity_aliases a JOIN entities e ON e.id = a.entity_id WHERE a.alias = ?1",
            [alias_key(name)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub fn ensure_entity(conn: &Connection, name: &str) -> Result<String> {
    let name = nfc(name.trim());
    if name.is_empty() {
        return Err(AliceError::invalid(
            "entity name is empty; pass a name such as alice or api-server",
        ));
    }
    if let Some((id, _)) = find_entity(conn, &name)? {
        return Ok(id);
    }
    let id = new_id();
    conn.execute(
        "INSERT INTO entities (id, name, created_at) VALUES (?1, ?2, ?3)",
        params![id, name, now_rfc3339()],
    )?;
    conn.execute(
        "INSERT INTO entity_aliases (alias, entity_id) VALUES (?1, ?2)",
        params![alias_key(&name), id],
    )?;
    Ok(id)
}

fn looks_literal(s: &str) -> bool {
    s.chars().count() > 60
        || s.contains('\n')
        || s.contains("://")
        || s.split_whitespace().count() > 6
        || s.chars().filter(|c| c.is_ascii_digit()).count() * 2 > s.chars().count()
        || (s.contains([':', '/']) && s.chars().any(|c| c.is_ascii_digit()))
}

/// An optional field; blank counts as absent (clients often send `""` for "not set").
fn given(field: &Option<String>) -> Option<&str> {
    field.as_deref().filter(|s| !s.trim().is_empty())
}

/// A caller's time in the stored form (RFC 3339, UTC, milliseconds). A bare date is midnight UTC.
fn stored_time(field: &str, raw: &str) -> Result<String> {
    let s = raw.trim();
    let utc = if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        t.with_timezone(&Utc)
    } else if let Ok(day) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        day.and_time(NaiveTime::MIN).and_utc()
    } else {
        return Err(AliceError::invalid(format!(
            "{field} \"{}\" is not a time; use RFC 3339 such as 2026-10-13T09:44:23.391Z or a date such as 2026-10-13",
            truncate_chars(s, 40)
        )));
    };
    Ok(utc.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn predicate_key(raw: &str) -> Result<String> {
    let p = normalize_predicate(raw);
    if p.is_empty() {
        return Err(AliceError::invalid(
            "predicate is empty; use a verb phrase such as uses or deployed_on",
        ));
    }
    Ok(p)
}

/// Canonical name of a normalised predicate; `None` when the vocabulary does not have it.
fn canonical_predicate(conn: &Connection, p: &str) -> Result<Option<String>> {
    let canonical: Option<Option<String>> = conn
        .query_row(
            "SELECT canonical FROM predicates WHERE name = ?1",
            [p],
            |r| r.get(0),
        )
        .optional()?;
    Ok(canonical.map(|c| c.unwrap_or_else(|| p.to_string())))
}

fn ensure_predicate(conn: &Connection, p: &str) -> Result<String> {
    if let Some(canonical) = canonical_predicate(conn, p)? {
        return Ok(canonical);
    }
    conn.execute("INSERT INTO predicates (name) VALUES (?1)", [p])?;
    Ok(p.to_string())
}

/// `valid_to` for a relation closed at `?3`. A close time before the relation's start would
/// make `valid_to < valid_from` (say, added at 09:44 today, then superseded with today's bare
/// date, midnight). So the close time is clamped to at least 1 ms after the start: intervals are
/// half-open (`valid_from <= t < valid_to`), and a zero-length one would vanish from every
/// `as_of` query. Stored times are canonical RFC 3339 text, so text `MAX` is time order.
const CLOSED_AT: &str =
    "MAX(?3, COALESCE(strftime('%Y-%m-%dT%H:%M:%fZ', valid_from, '+0.001 seconds'), valid_from))";

/// Add, supersede or end a fact. `hlc` stamps every relation this call creates or closes, so
/// sync sees the change.
pub fn assert_fact(conn: &Connection, f: &AssertFact, hlc: String) -> Result<FactOutcome> {
    // Check every part before writing anything.
    if f.subject.trim().is_empty() {
        return Err(AliceError::invalid(
            "subject is empty; name what the fact is about, such as alice or api-server",
        ));
    }
    let predicate = predicate_key(&f.predicate)?;
    let object = nfc(f.object.trim());
    if object.is_empty() {
        return Err(AliceError::invalid(
            "object is empty; pass the value of the fact, such as SQLite or port 8080",
        ));
    }
    let at = match given(&f.valid_from) {
        Some(t) => stored_time("valid_from", t)?,
        None => now_rfc3339(),
    };
    if f.action == FactAction::End {
        return end_fact(conn, f, predicate, &object, &at, &hlc);
    }

    let subject = ensure_entity(conn, &f.subject)?;
    let predicate = ensure_predicate(conn, &predicate)?;
    let as_entity = match f.object_kind {
        ObjectKind::Entity => true,
        ObjectKind::Literal => false,
        ObjectKind::Auto => find_entity(conn, &object)?.is_some() || !looks_literal(&object),
    };
    let (obj_entity, obj_literal) = if as_entity {
        (Some(ensure_entity(conn, &object)?), None)
    } else {
        (None, Some(object))
    };
    let current: Option<String> = conn
        .query_row(
            "SELECT id FROM relations WHERE subject_id = ?1 AND predicate = ?2 AND valid_to IS NULL
               AND object_entity_id IS ?3 AND object_literal IS ?4 LIMIT 1",
            params![subject, predicate, obj_entity, obj_literal],
            |r| r.get(0),
        )
        .optional()?;
    // Supersede leaves one current value. If it is already current it stays as it is, so a
    // retried call does not cut its history in two.
    let (ended, valid_from) = if f.action == FactAction::Supersede {
        let mut close = conn.prepare(&format!(
            "UPDATE relations SET valid_to = {CLOSED_AT}, hlc = ?4
             WHERE subject_id = ?1 AND predicate = ?2 AND valid_to IS NULL AND id IS NOT ?5
             RETURNING valid_to"
        ))?;
        let closed: Vec<String> = close
            .query_map(params![subject, predicate, at, hlc, current], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        // The new value never starts before the values it replaces end.
        (closed.len(), closed.into_iter().fold(at, std::cmp::max))
    } else {
        (0, at)
    };
    if let Some(id) = current {
        return Ok(FactOutcome {
            relation_id: Some(id),
            ended,
            predicate,
            suggestion: None,
        });
    }
    let id = new_id();
    conn.execute(
        "INSERT INTO relations (id, subject_id, predicate, object_entity_id, object_literal, valid_from, source_memory_id, created_at, hlc)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, subject, predicate, obj_entity, obj_literal, valid_from, f.source_memory_id, now_rfc3339(), hlc],
    )?;
    Ok(FactOutcome {
        relation_id: Some(id),
        ended,
        predicate,
        suggestion: None,
    })
}

/// The part of the vocabulary a predicate mapping touches, as events record it so a mapping
/// can be undone: the names involved, their `predicates` rows (a name without a row did not
/// exist), and the predicate each affected relation carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateSnapshot {
    pub names: Vec<String>,
    /// `(name, canonical)` of the names that have a row, sorted by name.
    pub rows: Vec<(String, Option<String>)>,
    /// `(relation id, predicate)`, sorted by id.
    pub relations: Vec<(String, String)>,
}

/// The snapshot of `names` and the relations `ids` as they are now.
pub fn snapshot_predicates(
    conn: &Connection,
    names: &[String],
    ids: &[String],
) -> Result<PredicateSnapshot> {
    let mut rows = Vec::new();
    for n in names {
        if let Some(c) = conn
            .query_row(
                "SELECT canonical FROM predicates WHERE name = ?1",
                [n],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
        {
            rows.push((n.clone(), c));
        }
    }
    rows.sort();
    let mut relations = Vec::new();
    for id in ids {
        if let Some(p) = conn
            .query_row("SELECT predicate FROM relations WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            relations.push((id.clone(), p));
        }
    }
    relations.sort();
    Ok(PredicateSnapshot {
        names: names.to_vec(),
        rows,
        relations,
    })
}

/// Put the vocabulary and the relations back as `s` says: the rows of its names are replaced
/// by its rows, and each relation it lists gets its predicate back.
pub fn restore_predicates(conn: &Connection, s: &PredicateSnapshot, hlc: &str) -> Result<()> {
    for n in &s.names {
        conn.execute("DELETE FROM predicates WHERE name = ?1", [n])?;
    }
    for (n, c) in &s.rows {
        conn.execute(
            "INSERT INTO predicates (name, canonical) VALUES (?1, ?2)",
            params![n, c],
        )?;
    }
    for (id, p) in &s.relations {
        conn.execute(
            "UPDATE relations SET predicate = ?1, hlc = ?2 WHERE id = ?3",
            params![p, hlc, id],
        )?;
    }
    Ok(())
}

/// From now on `name` means `canonical`: existing relations are renamed and later asserts with
/// `name` use `canonical`. Returns how many relations were renamed.
pub fn map_predicate(conn: &Connection, name: &str, canonical: &str, hlc: &str) -> Result<usize> {
    Ok(map_predicate_recorded(conn, name, canonical, hlc)?
        .0
        .relations
        .len())
}

/// [`map_predicate`], also returning the snapshots (before, after) of everything it touched,
/// for the event that makes the mapping undoable.
pub fn map_predicate_recorded(
    conn: &Connection,
    name: &str,
    canonical: &str,
    hlc: &str,
) -> Result<(PredicateSnapshot, PredicateSnapshot)> {
    let name = predicate_key(name)?;
    let wanted = predicate_key(canonical)?;
    let target = canonical_predicate(conn, &wanted)?
        .ok_or_else(|| AliceError::NotFound(format!("predicate {canonical}")))?;
    if name == target || name == wanted {
        return Err(AliceError::invalid(format!(
            "{name} cannot map to itself; pick a different predicate"
        )));
    }
    let mut names = vec![name.clone()];
    {
        let mut stmt =
            conn.prepare("SELECT name FROM predicates WHERE canonical = ?1 ORDER BY name")?;
        for n in stmt.query_map([&name], |r| r.get::<_, String>(0))? {
            names.push(n?);
        }
    }
    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM relations WHERE predicate = ?1 ORDER BY id")?;
        stmt.query_map([&name], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?
    };
    let before = snapshot_predicates(conn, &names, &ids)?;
    conn.execute(
        "INSERT INTO predicates (name, canonical) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET canonical = excluded.canonical",
        params![name, target],
    )?;
    // Names that pointed at `name` follow it.
    conn.execute(
        "UPDATE predicates SET canonical = ?1 WHERE canonical = ?2",
        params![target, name],
    )?;
    conn.execute(
        "UPDATE relations SET predicate = ?1, hlc = ?2 WHERE predicate = ?3",
        params![target, hlc, name],
    )?;
    let after = snapshot_predicates(conn, &names, &ids)?;
    Ok((before, after))
}

/// Ids of the open relations of `subject` and `predicate` (resolved as `assert_fact` resolves
/// them), sorted: every relation an `assert_fact` for them can close. Empty when the subject
/// or the predicate is unknown; names are looked up, never created.
pub fn open_relation_ids(conn: &Connection, subject: &str, predicate: &str) -> Result<Vec<String>> {
    let p = normalize_predicate(predicate);
    if p.is_empty() {
        return Ok(Vec::new());
    }
    let (Some((subject_id, _)), Some(p)) =
        (find_entity(conn, subject)?, canonical_predicate(conn, &p)?)
    else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM relations WHERE subject_id = ?1 AND predicate = ?2 AND valid_to IS NULL ORDER BY id",
    )?;
    Ok(stmt
        .query_map(params![subject_id, p], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

/// Close the current relations for subject, predicate and object. Names are looked up, never
/// created, and the object matches whether it was stored as an entity or as text.
fn end_fact(
    conn: &Connection,
    f: &AssertFact,
    predicate: String,
    object: &str,
    at: &str,
    hlc: &str,
) -> Result<FactOutcome> {
    let outcome = |predicate: String, ended: usize| FactOutcome {
        relation_id: None,
        ended,
        predicate,
        suggestion: None,
    };
    let Some(predicate) = canonical_predicate(conn, &predicate)? else {
        return Ok(outcome(predicate, 0));
    };
    let Some((subject, _)) = find_entity(conn, &f.subject)? else {
        return Ok(outcome(predicate, 0));
    };
    let obj_entity = match f.object_kind {
        ObjectKind::Literal => None,
        ObjectKind::Auto | ObjectKind::Entity => find_entity(conn, object)?.map(|(id, _)| id),
    };
    let obj_literal = (f.object_kind != ObjectKind::Entity).then_some(object);
    let ended = conn.execute(
        &format!(
            "UPDATE relations SET valid_to = {CLOSED_AT}, hlc = ?4
             WHERE subject_id = ?1 AND predicate = ?2 AND valid_to IS NULL
               AND (object_entity_id = ?5 OR object_literal = ?6)"
        ),
        params![subject, predicate, at, hlc, obj_entity, obj_literal],
    )?;
    Ok(outcome(predicate, ended))
}

pub fn query_facts(conn: &Connection, q: &FactsQuery) -> Result<Vec<Fact>> {
    let as_of = given(&q.as_of)
        .map(|t| stored_time("as_of", t))
        .transpose()?;
    let Some((entity_id, _)) = find_entity(conn, &q.entity)? else {
        return Ok(Vec::new());
    };
    let predicate = match given(&q.predicate).map(normalize_predicate) {
        Some(p) => Some(canonical_predicate(conn, &p)?.unwrap_or(p)),
        None => None,
    };
    let mut sql = String::from(
        "SELECT r.id, s.name, r.predicate, COALESCE(o.name, r.object_literal), r.object_entity_id IS NOT NULL,
                r.valid_from, r.valid_to, r.source_memory_id
         FROM relations r JOIN entities s ON s.id = r.subject_id LEFT JOIN entities o ON o.id = r.object_entity_id
         WHERE (r.subject_id = ?1 OR r.object_entity_id = ?1) AND (?2 IS NULL OR r.predicate = ?2)",
    );
    // Every branch binds ?3 so the statement always takes three parameters. Keep the
    // parentheses in the timeline branch: without them `OR` would escape the entity filter.
    if !q.timeline {
        match &as_of {
            Some(_) => {
                sql.push_str(" AND r.valid_from <= ?3 AND (r.valid_to IS NULL OR r.valid_to > ?3)")
            }
            None => sql.push_str(" AND r.valid_to IS NULL AND ?3 IS NULL"),
        }
    } else {
        sql.push_str(" AND (?3 IS NULL OR ?3 IS NOT NULL)");
    }
    sql.push_str(" ORDER BY r.valid_from, r.id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![entity_id, predicate, as_of], |r| {
            Ok(Fact {
                id: r.get(0)?,
                subject: r.get(1)?,
                predicate: r.get(2)?,
                object: r.get(3)?,
                object_is_entity: r.get(4)?,
                valid_from: r.get(5)?,
                valid_to: r.get(6)?,
                source_memory_id: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// In-memory list of entity aliases used to find mentions in memory text.
pub struct EntityMatcher {
    aliases: Vec<(String, String)>, // (alias_key, entity_id)
}

impl EntityMatcher {
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT alias, entity_id FROM entity_aliases")?;
        let aliases = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(Self { aliases })
    }

    /// Entity ids whose alias occurs in `text`, each once. ASCII aliases must match whole
    /// words: no ASCII letter or digit right before or after. Any other character is a
    /// boundary, so an English name written straight after Thai text (no spaces) is found.
    pub fn find_in(&self, text: &str) -> Vec<String> {
        let hay = alias_key(text);
        let mut out = Vec::new();
        for (alias, id) in &self.aliases {
            if alias.chars().count() < 2 {
                continue;
            }
            let found = if alias.is_ascii() {
                hay.match_indices(alias.as_str()).any(|(i, _)| {
                    let before = hay[..i].chars().next_back();
                    let after = hay[i + alias.len()..].chars().next();
                    !before.is_some_and(|c| c.is_ascii_alphanumeric())
                        && !after.is_some_and(|c| c.is_ascii_alphanumeric())
                })
            } else {
                hay.contains(alias.as_str())
            };
            if found && !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }
}

pub fn link_mentions(conn: &Connection, memory_id: &str, entity_ids: &[String]) -> Result<usize> {
    let mut n = 0;
    for e in entity_ids {
        n += conn.execute(
            "INSERT OR IGNORE INTO mentions (memory_id, entity_id) VALUES (?1, ?2)",
            params![memory_id, e],
        )?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::open_writer;

    fn conn() -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        (d, c)
    }

    fn fact(s: &str, p: &str, o: &str, action: FactAction, from: &str) -> AssertFact {
        AssertFact {
            subject: s.into(),
            predicate: p.into(),
            object: o.into(),
            object_kind: ObjectKind::Auto,
            valid_from: Some(from.into()),
            action,
            // Later fields keep their defaults, so adding one does not touch this helper.
            ..Default::default()
        }
    }

    #[test]
    fn predicates_are_normalised() {
        assert_eq!(normalize_predicate("Has Bug"), "has_bug");
        assert_eq!(normalize_predicate("deploys-to"), "deploys_to");
    }

    #[test]
    fn add_is_idempotent_and_query_returns_current() {
        let (_d, c) = conn();
        let a = assert_fact(
            &c,
            &fact(
                "Alice",
                "uses",
                "SQLite",
                FactAction::Add,
                "2026-10-01T00:00:00.000Z",
            ),
            "h".into(),
        )
        .unwrap();
        let b = assert_fact(
            &c,
            &fact(
                "alice",
                "Uses",
                "sqlite",
                FactAction::Add,
                "2026-10-02T00:00:00.000Z",
            ),
            "h".into(),
        )
        .unwrap();
        assert_eq!(a.relation_id, b.relation_id);
        let facts = query_facts(
            &c,
            &FactsQuery {
                entity: "ALICE".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].object, "SQLite");
        assert!(facts[0].object_is_entity);
    }

    #[test]
    fn supersede_ends_previous_value_and_as_of_sees_history() {
        let (_d, c) = conn();
        assert_fact(
            &c,
            &fact(
                "api",
                "runs_on",
                "port 8080",
                FactAction::Add,
                "2026-01-01T00:00:00.000Z",
            ),
            "h".into(),
        )
        .unwrap();
        let out = assert_fact(
            &c,
            &fact(
                "api",
                "runs_on",
                "port 9090",
                FactAction::Supersede,
                "2026-06-01T00:00:00.000Z",
            ),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 1);
        let now = query_facts(
            &c,
            &FactsQuery {
                entity: "api".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].object, "port 9090");
        let then = query_facts(
            &c,
            &FactsQuery {
                entity: "api".into(),
                as_of: Some("2026-03-01T00:00:00.000Z".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(then[0].object, "port 8080");
        let all = query_facts(
            &c,
            &FactsQuery {
                entity: "api".into(),
                timeline: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].valid_from < all[1].valid_from);
    }

    #[test]
    fn end_closes_a_fact() {
        let (_d, c) = conn();
        assert_fact(
            &c,
            &fact(
                "svc",
                "has_bug",
                "leak",
                FactAction::Add,
                "2026-01-01T00:00:00.000Z",
            ),
            "h".into(),
        )
        .unwrap();
        let out = assert_fact(
            &c,
            &fact(
                "svc",
                "has_bug",
                "leak",
                FactAction::End,
                "2026-02-01T00:00:00.000Z",
            ),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 1);
        assert!(
            query_facts(
                &c,
                &FactsQuery {
                    entity: "svc".into(),
                    ..Default::default()
                }
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn long_or_numeric_objects_are_literals() {
        let (_d, c) = conn();
        assert_fact(
            &c,
            &fact(
                "svc",
                "listens_on",
                "127.0.0.1:8765",
                FactAction::Add,
                "2026-01-01T00:00:00.000Z",
            ),
            "h".into(),
        )
        .unwrap();
        let f = &query_facts(
            &c,
            &FactsQuery {
                entity: "svc".into(),
                ..Default::default()
            },
        )
        .unwrap()[0];
        assert!(!f.object_is_entity);
        assert!(find_entity(&c, "127.0.0.1:8765").unwrap().is_none());
    }

    #[test]
    fn matcher_finds_entity_names_in_text() {
        let (_d, c) = conn();
        let id = ensure_entity(&c, "Reborn Kingdom").unwrap();
        ensure_entity(&c, "Go").unwrap();
        let m = EntityMatcher::load(&c).unwrap();
        let found = m.find_in("Working on reborn kingdom today; going home");
        assert_eq!(found, vec![id], "short names must match whole words only");
    }

    #[test]
    fn unknown_entity_returns_empty() {
        let (_d, c) = conn();
        assert!(
            query_facts(
                &c,
                &FactsQuery {
                    entity: "ghost".into(),
                    ..Default::default()
                }
            )
            .unwrap()
            .is_empty()
        );
    }

    // ---- Beyond the brief: sync stamps, time formats, Thai text, idempotent supersede/end. ----

    const T1: &str = "2026-01-01T00:00:00.000Z";
    const T2: &str = "2026-02-01T00:00:00.000Z";
    const T3: &str = "2026-03-01T00:00:00.000Z";

    fn add(c: &Connection, s: &str, p: &str, o: &str, from: &str) -> FactOutcome {
        assert_fact(c, &fact(s, p, o, FactAction::Add, from), "h0".into()).unwrap()
    }

    fn current(c: &Connection, entity: &str) -> Vec<Fact> {
        query_facts(
            c,
            &FactsQuery {
                entity: entity.into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn timeline(c: &Connection, entity: &str) -> Vec<Fact> {
        query_facts(
            c,
            &FactsQuery {
                entity: entity.into(),
                timeline: true,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn hlc_of(c: &Connection, relation_id: &str) -> String {
        c.query_row(
            "SELECT hlc FROM relations WHERE id = ?1",
            [relation_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn count(c: &Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn invalid_message(r: Result<impl std::fmt::Debug>) -> String {
        match r {
            Err(AliceError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn ending_or_superseding_a_relation_stamps_the_new_hlc() {
        // Sync exports what changed since the last synced HLC; a closed relation changed.
        let (_d, c) = conn();
        let first = add(&c, "api", "runs_on", "port 8080", T1)
            .relation_id
            .unwrap();
        assert_eq!(hlc_of(&c, &first), "h0");
        let out = assert_fact(
            &c,
            &fact("api", "runs_on", "port 9090", FactAction::Supersede, T2),
            "h1".into(),
        )
        .unwrap();
        assert_eq!(
            hlc_of(&c, &first),
            "h1",
            "superseded relation keeps a stale hlc"
        );
        let second = out.relation_id.unwrap();
        assert_eq!(hlc_of(&c, &second), "h1");
        let out = assert_fact(
            &c,
            &fact("api", "runs_on", "port 9090", FactAction::End, T3),
            "h2".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 1);
        assert_eq!(
            hlc_of(&c, &second),
            "h2",
            "ended relation keeps a stale hlc"
        );
        assert_eq!(
            hlc_of(&c, &first),
            "h1",
            "an already-closed relation is left alone"
        );
    }

    #[test]
    fn valid_from_is_stored_as_utc_rfc3339_with_millis() {
        let (_d, c) = conn();
        add(
            &c,
            "api",
            "runs_on",
            "port 8080",
            "2026-03-01T10:00:00+07:00",
        );
        add(&c, "web", "runs_on", "port 80", "2026-03-01");
        add(
            &c,
            "db",
            "runs_on",
            "port 5432",
            " 2026-03-01T03:04:05.123456Z ",
        );
        assert_eq!(current(&c, "api")[0].valid_from, "2026-03-01T03:00:00.000Z");
        assert_eq!(current(&c, "web")[0].valid_from, "2026-03-01T00:00:00.000Z");
        assert_eq!(current(&c, "db")[0].valid_from, "2026-03-01T03:04:05.123Z");

        // End times are normalised too, so `as_of` comparisons stay chronological.
        assert_fact(
            &c,
            &fact(
                "api",
                "runs_on",
                "port 8080",
                FactAction::End,
                "2026-03-02T06:00:00+07:00",
            ),
            "h".into(),
        )
        .unwrap();
        assert_eq!(
            timeline(&c, "api")[0].valid_to.as_deref(),
            Some("2026-03-01T23:00:00.000Z")
        );
    }

    #[test]
    fn as_of_is_compared_as_an_instant() {
        let (_d, c) = conn();
        add(
            &c,
            "api",
            "runs_on",
            "port 8080",
            "2026-03-01T03:00:00.000Z",
        );
        let at = |t: &str| {
            query_facts(
                &c,
                &FactsQuery {
                    entity: "api".into(),
                    as_of: Some(t.into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .len()
        };
        // 09:00 in Bangkok is 02:00 UTC, an hour before the fact started.
        assert_eq!(at("2026-03-01T09:00:00+07:00"), 0);
        assert_eq!(at("2026-03-01T11:00:00+07:00"), 1);
        assert_eq!(at("2026-03-02"), 1);
    }

    #[test]
    fn unparseable_times_are_refused_before_anything_is_written() {
        let (_d, c) = conn();
        let msg = invalid_message(assert_fact(
            &c,
            &fact("api", "runs_on", "port 8080", FactAction::Add, "last week"),
            "h".into(),
        ));
        assert!(
            msg.contains("valid_from") && msg.contains("2026-10-13"),
            "{msg}"
        );
        assert_eq!(count(&c, "entities"), 0);
        assert_eq!(count(&c, "predicates"), 0);

        add(&c, "api", "runs_on", "port 8080", T1);
        let msg = invalid_message(query_facts(
            &c,
            &FactsQuery {
                entity: "api".into(),
                as_of: Some("yesterday".into()),
                ..Default::default()
            },
        ));
        assert!(msg.contains("as_of"), "{msg}");
    }

    #[test]
    fn blank_optional_fields_count_as_absent() {
        let (_d, c) = conn();
        add(&c, "api", "runs_on", "port 8080", T1);
        let f = AssertFact {
            valid_from: Some("  ".into()),
            ..fact("api", "uses", "SQLite", FactAction::Add, T1)
        };
        assert_fact(&c, &f, "h".into()).unwrap();
        let q = FactsQuery {
            entity: "api".into(),
            predicate: Some(String::new()),
            as_of: Some(" ".into()),
            timeline: false,
        };
        let facts = query_facts(&c, &q).unwrap();
        assert_eq!(facts.len(), 2);
        assert!(
            facts[1].valid_from.as_str() > T1,
            "blank valid_from means now"
        );
    }

    #[test]
    fn empty_parts_are_refused_before_anything_is_written() {
        let (_d, c) = conn();
        for (s, p, o) in [
            ("  ", "uses", "x"),
            ("api", " - ", "x"),
            ("api", "uses", " \n "),
        ] {
            for action in [FactAction::Add, FactAction::Supersede, FactAction::End] {
                invalid_message(assert_fact(&c, &fact(s, p, o, action, T1), "h".into()));
            }
        }
        assert_eq!(count(&c, "entities"), 0);
        assert_eq!(count(&c, "predicates"), 0);
        assert_eq!(count(&c, "relations"), 0);
    }

    #[test]
    fn superseding_with_the_current_value_keeps_it() {
        // A retried supersede must not cut the fact's history in two.
        let (_d, c) = conn();
        let a = add(&c, "api", "runs_on", "port 9090", T1).relation_id;
        let out = assert_fact(
            &c,
            &fact("api", "runs_on", "port 9090", FactAction::Supersede, T2),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.relation_id, a);
        assert_eq!(out.ended, 0);
        let all = timeline(&c, "api");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].valid_from, T1);
        assert_eq!(all[0].valid_to, None);

        // With several current values, superseding with one of them ends only the others.
        add(&c, "api", "runs_on", "port 8080", T2);
        let out = assert_fact(
            &c,
            &fact("api", "runs_on", "port 9090", FactAction::Supersede, T3),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.relation_id, a);
        assert_eq!(out.ended, 1);
        let now = current(&c, "api");
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].object, "port 9090");
    }

    #[test]
    fn ending_an_unknown_fact_writes_nothing() {
        // A mistyped `end` must not create entities that the matcher would then link everywhere.
        let (_d, c) = conn();
        add(&c, "svc", "has_bug", "leak", T1);
        let (entities, predicates) = (count(&c, "entities"), count(&c, "predicates"));
        for (s, p, o) in [
            ("svc", "has_bug", "lek"),
            ("svc", "haunts", "leak"),
            ("ghost", "has_bug", "leak"),
        ] {
            let out = assert_fact(&c, &fact(s, p, o, FactAction::End, T2), "h".into()).unwrap();
            assert_eq!(out.ended, 0, "{s} {p} {o}");
            assert_eq!(out.relation_id, None);
        }
        assert_eq!(count(&c, "entities"), entities);
        assert_eq!(count(&c, "predicates"), predicates);
        assert_eq!(current(&c, "svc").len(), 1);
        assert!(find_entity(&c, "lek").unwrap().is_none());
        assert!(find_entity(&c, "ghost").unwrap().is_none());
    }

    #[test]
    fn end_matches_the_object_however_it_was_stored() {
        let (_d, c) = conn();
        let literal = AssertFact {
            object_kind: ObjectKind::Literal,
            ..fact("svc", "uses", "SQLite", FactAction::Add, T1)
        };
        assert_fact(&c, &literal, "h".into()).unwrap();
        ensure_entity(&c, "SQLite").unwrap();
        add(&c, "svc", "uses", "Redis", T1);
        let out = assert_fact(
            &c,
            &fact("svc", "uses", "SQLite", FactAction::End, T2),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 1);
        let out = assert_fact(
            &c,
            &fact("svc", "uses", "redis", FactAction::End, T2),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 1);
        assert!(current(&c, "svc").is_empty());
    }

    fn objects_as_of(c: &Connection, entity: &str, t: &str) -> Vec<String> {
        query_facts(
            c,
            &FactsQuery {
                entity: entity.into(),
                as_of: Some(t.into()),
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|f| f.object)
        .collect()
    }

    fn added_now(c: &Connection, s: &str, p: &str, o: &str) {
        let f = AssertFact {
            valid_from: None,
            ..fact(s, p, o, FactAction::Add, T1)
        };
        assert_fact(c, &f, "h".into()).unwrap();
    }

    #[test]
    fn same_day_supersede_with_a_bare_date_never_runs_backwards() {
        // Added at "now" (say 09:44Z), superseded with today's date (midnight UTC).
        let (_d, c) = conn();
        added_now(&c, "api", "runs_on", "port 8080");
        let today = &now_rfc3339()[..10];
        let out = assert_fact(
            &c,
            &fact("api", "runs_on", "port 9090", FactAction::Supersede, today),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 1);
        let all = timeline(&c, "api");
        assert_eq!(all.len(), 2);
        let (old, new) = (&all[0], &all[1]);
        assert_eq!(
            (old.object.as_str(), new.object.as_str()),
            ("port 8080", "port 9090"),
            "timeline must list the old value first"
        );
        let old_to = old.valid_to.as_deref().unwrap();
        assert!(old_to >= old.valid_from.as_str(), "{old:?}");
        assert!(
            new.valid_from >= old.valid_from,
            "{new:?} starts before {old:?}"
        );
        assert_eq!(
            new.valid_from, old_to,
            "successor starts when predecessor ends"
        );
        assert_eq!(objects_as_of(&c, "api", &old.valid_from), ["port 8080"]);
        assert_eq!(objects_as_of(&c, "api", &new.valid_from), ["port 9090"]);
    }

    #[test]
    fn same_day_end_with_a_bare_date_never_runs_backwards() {
        let (_d, c) = conn();
        added_now(&c, "svc", "has_bug", "leak");
        let today = &now_rfc3339()[..10];
        let out = assert_fact(
            &c,
            &fact("svc", "has_bug", "leak", FactAction::End, today),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 1);
        assert!(current(&c, "svc").is_empty());
        let f = &timeline(&c, "svc")[0];
        assert!(
            f.valid_to.as_deref().unwrap() >= f.valid_from.as_str(),
            "{f:?}"
        );
        assert_eq!(objects_as_of(&c, "svc", &f.valid_from), ["leak"]);
    }

    #[test]
    fn closing_before_a_start_keeps_each_row_visible_at_its_start() {
        // Two current values, then a supersede dated before both: each closed row keeps a
        // 1 ms interval and the successor starts where the latest of them ends.
        let (_d, c) = conn();
        add(&c, "api", "runs_on", "port 8080", T2);
        add(&c, "api", "runs_on", "port 8081", T3);
        let out = assert_fact(
            &c,
            &fact("api", "runs_on", "port 9090", FactAction::Supersede, T1),
            "h".into(),
        )
        .unwrap();
        assert_eq!(out.ended, 2);
        let spans: Vec<_> = timeline(&c, "api")
            .into_iter()
            .map(|f| (f.object, f.valid_from, f.valid_to))
            .collect();
        let span = |o: &str, from: &str, to: Option<&str>| {
            (o.to_string(), from.to_string(), to.map(str::to_string))
        };
        assert_eq!(
            spans,
            [
                span("port 8080", T2, Some("2026-02-01T00:00:00.001Z")),
                span("port 8081", T3, Some("2026-03-01T00:00:00.001Z")),
                span("port 9090", "2026-03-01T00:00:00.001Z", None),
            ]
        );
        assert_eq!(objects_as_of(&c, "api", T2), ["port 8080"]);
        assert_eq!(objects_as_of(&c, "api", T3), ["port 8081"]);

        // A value whose start is before `at` still closes exactly at `at`.
        add(&c, "web", "runs_on", "port 80", T1);
        assert_fact(
            &c,
            &fact("web", "runs_on", "port 443", FactAction::Supersede, T2),
            "h".into(),
        )
        .unwrap();
        let web = timeline(&c, "web");
        assert_eq!(web[0].valid_to.as_deref(), Some(T2));
        assert_eq!(web[1].valid_from, T2);

        // End before the start: same 1 ms clamp.
        add(&c, "svc", "has_bug", "leak", T3);
        assert_fact(
            &c,
            &fact("svc", "has_bug", "leak", FactAction::End, T1),
            "h".into(),
        )
        .unwrap();
        assert_eq!(
            timeline(&c, "svc")[0].valid_to.as_deref(),
            Some("2026-03-01T00:00:00.001Z")
        );
    }

    #[test]
    fn end_closes_only_the_named_subjects_current_relation() {
        let (_d, c) = conn();
        add(&c, "svc", "listens_on", "127.0.0.1:8765", T1);
        add(&c, "web", "listens_on", "127.0.0.1:8765", T1);
        let end = |at: &str| {
            assert_fact(
                &c,
                &fact("svc", "listens_on", "127.0.0.1:8765", FactAction::End, at),
                "h".into(),
            )
            .unwrap()
            .ended
        };
        assert_eq!(end(T2), 1);
        assert_eq!(
            current(&c, "web").len(),
            1,
            "another subject's fact was ended"
        );
        assert_eq!(end(T3), 0, "an ended fact was ended again");
        assert_eq!(timeline(&c, "svc")[0].valid_to.as_deref(), Some(T2));
    }

    #[test]
    fn predicate_aliases_resolve_on_write_and_on_query() {
        let (_d, c) = conn();
        add(&c, "svc", "has bug", "leak", T1);
        c.execute(
            "INSERT INTO predicates (name, canonical) VALUES ('bug', 'has_bug')",
            [],
        )
        .unwrap();
        let out = add(&c, "svc", "Bug", "oom", T1);
        assert_eq!(out.predicate, "has_bug");
        let q = FactsQuery {
            entity: "svc".into(),
            predicate: Some("BUG".into()),
            ..Default::default()
        };
        let facts = query_facts(&c, &q).unwrap();
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().all(|f| f.predicate == "has_bug"));
    }

    #[test]
    fn thai_predicates_keep_their_vowels_and_tone_marks() {
        assert_eq!(normalize_predicate("ใช้งาน"), "ใช้งาน");
        assert_eq!(normalize_predicate("ไม่ ชอบ"), "ไม่_ชอบ");
        assert_eq!(normalize_predicate("ทำงานที่"), "ทำงานที่");
        // The sara am fold from `nfc` applies.
        assert_eq!(normalize_predicate("จ\u{0E4D}\u{0E32}"), "จำ");
        assert_eq!(normalize_predicate("  _has__Bug_ "), "has_bug");
    }

    #[test]
    fn ascii_names_are_found_next_to_thai_text() {
        // Thai does not put spaces between words, so a Thai letter is a word boundary for an
        // English name.
        let (_d, c) = conn();
        let rust = ensure_entity(&c, "Rust").unwrap();
        let m = EntityMatcher::load(&c).unwrap();
        assert_eq!(m.find_in("ใช้Rustเขียน daemon"), vec![rust.clone()]);
        assert!(m.find_in("trusty rusted code").is_empty());
        assert_eq!(m.find_in("rust_core and RUST."), vec![rust]);
    }

    #[test]
    fn matcher_finds_thai_names_inside_text_and_ignores_one_letter_names() {
        let (_d, c) = conn();
        let th = ensure_entity(&c, "ระบบความจำ").unwrap();
        ensure_entity(&c, "x").unwrap();
        let m = EntityMatcher::load(&c).unwrap();
        assert_eq!(m.find_in("กำลังทำระบบความจำสำหรับเอเจนต์ x"), vec![th]);
    }

    #[test]
    fn facts_are_found_from_the_object_side_and_timeline_keeps_the_entity_filter() {
        let (_d, c) = conn();
        add(&c, "alice", "uses", "SQLite", T1);
        add(&c, "bob", "uses", "Postgres", T2);
        let from_object = current(&c, "sqlite");
        assert_eq!(from_object.len(), 1);
        assert_eq!(from_object[0].subject, "alice");
        let all = timeline(&c, "alice");
        assert_eq!(all.len(), 1, "timeline leaked other entities' facts");
        assert_eq!(all[0].object, "SQLite");
    }

    #[test]
    fn link_mentions_counts_only_new_links() {
        let (_d, c) = conn();
        let a = ensure_entity(&c, "alice").unwrap();
        let b = ensure_entity(&c, "bob").unwrap();
        assert_eq!(link_mentions(&c, "m1", &[a.clone(), b.clone()]).unwrap(), 2);
        assert_eq!(link_mentions(&c, "m1", &[a, b]).unwrap(), 0);
        assert_eq!(count(&c, "mentions"), 2);
    }

    /// What history snapshots before a fact is asserted: the subject's open relations for the
    /// predicate, found the way `assert_fact` finds them (aliases, predicate synonyms), and
    /// nothing created by looking.
    #[test]
    fn open_relation_ids_are_the_relations_an_assert_can_close() {
        let (_d, c) = conn();
        assert!(open_relation_ids(&c, "api", "uses").unwrap().is_empty());
        let n_entities = |c: &Connection| -> i64 {
            c.query_row("SELECT count(*) FROM entities", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(n_entities(&c), 0, "looking up creates nothing");
        let a = assert_fact(
            &c,
            &fact("api", "uses", "SQLite", FactAction::Add, T1),
            "h".into(),
        )
        .unwrap()
        .relation_id
        .unwrap();
        let b = assert_fact(
            &c,
            &fact("api", "uses", "Redis", FactAction::Add, T1),
            "h".into(),
        )
        .unwrap()
        .relation_id
        .unwrap();
        assert_fact(
            &c,
            &fact("api", "runs_on", "port 8080", FactAction::Add, T1),
            "h".into(),
        )
        .unwrap();
        let mut want = vec![a.clone(), b];
        want.sort();
        assert_eq!(open_relation_ids(&c, " API ", "Uses").unwrap(), want);
        assert_fact(
            &c,
            &fact("api", "uses", "SQLite", FactAction::End, T2),
            "h".into(),
        )
        .unwrap();
        assert!(!open_relation_ids(&c, "api", "uses").unwrap().contains(&a));
        assert!(open_relation_ids(&c, "api", "  ").unwrap().is_empty());
    }

    #[test]
    fn source_memory_is_recorded() {
        let (_d, c) = conn();
        let f = AssertFact {
            source_memory_id: Some("01MEMORY".into()),
            ..fact("api", "runs_on", "port 8080", FactAction::Add, T1)
        };
        assert_fact(&c, &f, "h".into()).unwrap();
        assert_eq!(
            current(&c, "api")[0].source_memory_id.as_deref(),
            Some("01MEMORY")
        );
    }

    #[test]
    fn serde_defaults_and_names() {
        let f: AssertFact =
            serde_json::from_str(r#"{"subject":"a","predicate":"p","object":"o"}"#).unwrap();
        assert_eq!(f.action, FactAction::Add);
        assert_eq!(f.object_kind, ObjectKind::Auto);
        let f: AssertFact = serde_json::from_str(
            r#"{"subject":"a","predicate":"p","object":"o","action":"supersede","object_kind":"literal"}"#,
        )
        .unwrap();
        assert_eq!(f.action, FactAction::Supersede);
        assert_eq!(f.object_kind, ObjectKind::Literal);
        let q: FactsQuery = serde_json::from_str(r#"{"entity":"a"}"#).unwrap();
        assert!(!q.timeline && q.as_of.is_none() && q.predicate.is_none());
    }

    #[test]
    fn mapping_a_predicate_renames_its_relations_and_future_asserts() {
        let (_d, c) = conn();
        for (p, o) in [("uses", "SQLite"), ("use", "Rust")] {
            assert_fact(
                &c,
                &AssertFact {
                    subject: "alice".into(),
                    predicate: p.into(),
                    object: o.into(),
                    ..Default::default()
                },
                "h".into(),
            )
            .unwrap();
        }
        assert_eq!(map_predicate(&c, "uses", "use", "h2").unwrap(), 1);
        let facts = query_facts(
            &c,
            &FactsQuery {
                entity: "alice".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(facts.iter().all(|f| f.predicate == "use"), "{facts:?}");
        let out = assert_fact(
            &c,
            &AssertFact {
                subject: "alice".into(),
                predicate: "uses".into(),
                object: "Tokio".into(),
                ..Default::default()
            },
            "h3".into(),
        )
        .unwrap();
        assert_eq!(out.predicate, "use");
        assert!(
            map_predicate(&c, "uses", "uses", "h4").is_err(),
            "a predicate cannot map to itself"
        );
        assert!(matches!(
            map_predicate(&c, "uses", "unknown_verb", "h5"),
            Err(AliceError::NotFound(_))
        ));
    }
}
