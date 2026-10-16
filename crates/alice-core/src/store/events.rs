//! History: one row per change, with who made it and the target before and after (spec §5.5).

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::Author;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Who made a change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(into = "String", from = "String")]
#[schemars(with = "String")]
pub enum Actor {
    Agent(String),
    User,
    Judge(String),
    /// A memory distilled from a captured session, by the provider that chose it (SP2 spec §6.5).
    Distill(String),
    Sync,
    Import,
    /// Maintenance jobs (purges, retention).
    System,
}

impl Actor {
    pub fn as_string(&self) -> String {
        match self {
            Actor::Agent(a) => format!("agent:{a}"),
            Actor::User => "user".into(),
            Actor::Judge(p) => format!("judge:{p}"),
            Actor::Distill(p) => format!("distill:{p}"),
            Actor::Sync => "sync".into(),
            Actor::Import => "import".into(),
            Actor::System => "system".into(),
        }
    }

    pub fn parse(s: &str) -> Actor {
        match s {
            "user" => Actor::User,
            "sync" => Actor::Sync,
            "import" => Actor::Import,
            "system" => Actor::System,
            _ => match s.split_once(':') {
                Some(("agent", a)) => Actor::Agent(a.into()),
                Some(("judge", p)) => Actor::Judge(p.into()),
                Some(("distill", p)) => Actor::Distill(p.into()),
                _ => Actor::Agent(s.into()),
            },
        }
    }

    /// A write by an MCP client is the agent's; one from the CLI (`cli`) or the API is the user's.
    pub fn from_author(a: &Author) -> Actor {
        match a.agent.as_deref().map(str::trim) {
            None | Some("") | Some("cli") => Actor::User,
            Some(name) => Actor::Agent(name.into()),
        }
    }
}

impl From<Actor> for String {
    fn from(a: Actor) -> String {
        a.as_string()
    }
}

impl From<String> for Actor {
    fn from(s: String) -> Actor {
        Actor::parse(&s)
    }
}

/// One change to record. `before`/`after` are the target's snapshots (`None`: it did not
/// exist); `group` ties together the rows of one change that touched several targets.
pub struct EventInput<'a> {
    pub actor: &'a Actor,
    pub action: &'a str,
    pub target_type: &'a str,
    pub target_id: &'a str,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub group: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventRow {
    pub id: String,
    pub ts: String,
    pub hlc: String,
    /// `agent:<name>`, `user`, `judge:<provider>`, `distill:<provider>`, `sync`, `import` or
    /// `system`.
    pub actor: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub group_id: Option<String>,
}

/// Events, newest first. `before` is an event id: only older events are returned (paging).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EventQuery {
    /// Only changes to this memory, relation or event.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Only changes by this actor, written as in the history (`user`, `agent:claude-code`); the
    /// daemon also takes a bare agent name (`claude-code`).
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub before: Option<String>,
    /// Default 50, at most 500.
    #[serde(default)]
    pub limit: Option<usize>,
}

const COLUMNS: &str =
    "id, ts, hlc, actor, action, target_type, target_id, before_json, after_json, group_id";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    let json = |i: usize| -> rusqlite::Result<Option<serde_json::Value>> {
        Ok(r.get::<_, Option<String>>(i)?
            .and_then(|s| serde_json::from_str(&s).ok()))
    };
    Ok(EventRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        hlc: r.get(2)?,
        actor: r.get(3)?,
        action: r.get(4)?,
        target_type: r.get(5)?,
        target_id: r.get(6)?,
        before: json(7)?,
        after: json(8)?,
        group_id: r.get(9)?,
    })
}

/// Insert an event; returns its id. Call inside the transaction that made the change.
pub fn record(conn: &Connection, e: &EventInput<'_>, hlc: &str) -> Result<String> {
    let id = new_id();
    conn.prepare_cached(&format!(
        "INSERT INTO events ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
    ))?
    .execute(params![
        id,
        now_rfc3339(),
        hlc,
        e.actor.as_string(),
        e.action,
        e.target_type,
        e.target_id,
        e.before.as_ref().map(|v| v.to_string()),
        e.after.as_ref().map(|v| v.to_string()),
        e.group,
    ])?;
    Ok(id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<EventRow>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM events WHERE id = ?1"))?
        .query_row([id], row)
        .optional()?)
}

/// Every event of a group, in the order they were recorded.
pub fn group(conn: &Connection, group_id: &str) -> Result<Vec<EventRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM events WHERE group_id = ?1 ORDER BY rowid"
    ))?;
    Ok(stmt
        .query_map([group_id], row)?
        .collect::<std::result::Result<_, _>>()?)
}

pub fn list(conn: &Connection, q: &EventQuery) -> Result<Vec<EventRow>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500) as i64;
    let before_rowid: Option<i64> = match &q.before {
        Some(id) => Some(
            conn.query_row("SELECT rowid FROM events WHERE id = ?1", [id], |r| r.get(0))
                .optional()?
                .ok_or_else(|| {
                    AliceError::invalid(format!(
                        "before: no event {id}; pass the id of the oldest event of the previous page"
                    ))
                })?,
        ),
        None => None,
    };
    let mut stmt = conn.prepare_cached(&list_sql(q.target_id.is_some(), q.actor.is_some()))?;
    Ok(stmt
        .query_map(params![q.target_id, q.actor, before_rowid, limit], row)?
        .collect::<std::result::Result<_, _>>()?)
}

/// The distinct target types, read from the `events_target` index one seek per type (a
/// skip-scan): a target filter then seeks the index without knowing the target's type.
const TARGET_TYPES: &str = "WITH RECURSIVE tt(x) AS (
        SELECT min(target_type) FROM events
        UNION ALL
        SELECT (SELECT min(target_type) FROM events WHERE target_type > tt.x) FROM tt WHERE tt.x IS NOT NULL
    ) SELECT x FROM tt WHERE x IS NOT NULL";

/// `list`'s query; parameters: target id, actor, rowid bound, limit. A target is read through
/// `events_target`, an actor alone through `events_actor`, and a page bound seeks by rowid, so
/// no filter or page reads the whole table.
fn list_sql(by_target: bool, by_actor: bool) -> String {
    let target = if by_target {
        format!("target_type IN ({TARGET_TYPES}) AND target_id = ?1")
    } else {
        "?1 IS NULL".to_string()
    };
    let actor = match (by_target, by_actor) {
        // The target's few rows are filtered; `+` keeps the planner on the target index.
        (true, true) => "+actor = ?2",
        (false, true) => "actor = ?2",
        (_, false) => "?2 IS NULL",
    };
    format!(
        "SELECT {COLUMNS} FROM events
         WHERE {target} AND {actor} AND rowid < coalesce(?3, 9223372036854775807)
         ORDER BY rowid DESC LIMIT ?4"
    )
}

/// The `undo` marker event that reverted `event_id`, if any (the newest one).
pub fn undone_by(conn: &Connection, event_id: &str) -> Result<Option<EventRow>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM events WHERE target_type = 'event' AND target_id = ?1 AND action = 'undo'
             ORDER BY rowid DESC LIMIT 1"
        ))?
        .query_row([event_id], row)
        .optional()?)
}

/// For an `undo` marker: the `undo:<action>` event recorded with it, i.e. the change that
/// reverted the marker's target (undo records it just before the marker, in the same group).
pub fn reverting(conn: &Connection, marker: &EventRow) -> Result<Option<EventRow>> {
    let Some(group) = marker.group_id.as_deref() else {
        return Ok(None);
    };
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM events
             WHERE group_id = ?1 AND action LIKE 'undo:%'
               AND rowid < (SELECT rowid FROM events WHERE id = ?2)
             ORDER BY rowid DESC LIMIT 1"
        ))?
        .query_row(params![group, marker.id], row)
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actors_round_trip_as_strings() {
        for a in [
            Actor::Agent("claude-code".into()),
            Actor::User,
            Actor::Judge("jev".into()),
            Actor::Distill("jev".into()),
            Actor::Sync,
            Actor::Import,
            Actor::System,
        ] {
            let s = a.as_string();
            assert_eq!(Actor::parse(&s), a, "{s}");
            assert_eq!(
                serde_json::to_value(&a).unwrap(),
                serde_json::Value::String(s)
            );
        }
        assert_eq!(Actor::Distill("jev".into()).as_string(), "distill:jev");
        use crate::model::Author;
        assert_eq!(Actor::from_author(&Author::default()), Actor::User);
        assert_eq!(
            Actor::from_author(&Author {
                agent: Some("cli".into()),
                ..Default::default()
            }),
            Actor::User
        );
        assert_eq!(
            Actor::from_author(&Author {
                agent: Some("codex".into()),
                ..Default::default()
            }),
            Actor::Agent("codex".into())
        );
    }

    fn input<'a>(target: &'a str, after: serde_json::Value) -> EventInput<'a> {
        EventInput {
            actor: &Actor::User,
            action: "memory.edit",
            target_type: "memory",
            target_id: target,
            before: None,
            after: Some(after),
            group: None,
        }
    }

    #[test]
    fn list_is_newest_first_filters_and_pages() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(
                record(
                    &c,
                    &input(
                        if i % 2 == 0 { "A" } else { "B" },
                        serde_json::json!({"i": i}),
                    ),
                    &format!("{:013}-0000-d", i),
                )
                .unwrap(),
            );
        }
        let all = list(&c, &EventQuery::default()).unwrap();
        assert_eq!(
            all.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            ids.iter().rev().cloned().collect::<Vec<_>>()
        );
        let a = list(
            &c,
            &EventQuery {
                target_id: Some("A".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(a.len(), 3);
        let page = list(
            &c,
            &EventQuery {
                before: Some(ids[3].clone()),
                limit: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            page.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![ids[2].clone(), ids[1].clone()]
        );
    }

    #[test]
    fn rows_keep_their_actor_json_and_group() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let agent = Actor::Agent("codex".into());
        let first = record(
            &c,
            &EventInput {
                actor: &agent,
                action: "relation.close",
                target_type: "relation",
                target_id: "R1",
                before: Some(serde_json::json!({"valid_to": null})),
                after: Some(serde_json::json!({"valid_to": "2026-10-14T00:00:00.000Z"})),
                group: Some("G"),
            },
            "0000000000001-0000-d",
        )
        .unwrap();
        let second = record(
            &c,
            &EventInput {
                actor: &agent,
                action: "relation.add",
                target_type: "relation",
                target_id: "R2",
                before: None,
                after: Some(serde_json::json!({"valid_to": null})),
                group: Some("G"),
            },
            "0000000000002-0000-d",
        )
        .unwrap();
        let row = get(&c, &first).unwrap().unwrap();
        assert_eq!(row.actor, "agent:codex");
        assert_eq!(row.hlc, "0000000000001-0000-d");
        assert_eq!(row.before.unwrap()["valid_to"], serde_json::Value::Null);
        assert_eq!(row.group_id.as_deref(), Some("G"));
        assert!(get(&c, "NOPE").unwrap().is_none());
        let g: Vec<String> = group(&c, "G").unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(g, vec![first.clone(), second], "in the order recorded");
        let by_actor = list(
            &c,
            &EventQuery {
                actor: Some("agent:codex".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_actor.len(), 2);
        assert!(
            list(
                &c,
                &EventQuery {
                    actor: Some("user".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .is_empty()
        );
        assert!(undone_by(&c, &first).unwrap().is_none());
        let marker = record(
            &c,
            &EventInput {
                actor: &Actor::User,
                action: "undo",
                target_type: "event",
                target_id: &first,
                before: None,
                after: None,
                group: Some("U"),
            },
            "0000000000003-0000-d",
        )
        .unwrap();
        assert_eq!(undone_by(&c, &first).unwrap().unwrap().id, marker);
    }

    /// `alice history <id>` must not read the whole table: with a target the query seeks the
    /// `events_target` index (its distinct types by a skip-scan), whatever the type.
    #[test]
    fn listing_by_target_uses_the_target_index() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let plan: String = c
            .prepare(&format!("EXPLAIN QUERY PLAN {}", list_sql(true, false)))
            .unwrap()
            .query_map(params!["M", None::<String>, None::<i64>, 50], |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(plan.contains("USING INDEX events_target"), "{plan}");
        assert!(!plan.contains("SCAN events"), "{plan}");
        // Every target type is found, whichever comes first in the index.
        for (ty, id) in [
            ("relation", "R"),
            ("memory", "M"),
            ("event", "E"),
            ("zz", "Z"),
        ] {
            record(
                &c,
                &EventInput {
                    target_type: ty,
                    ..input(id, serde_json::json!(ty))
                },
                "h",
            )
            .unwrap();
        }
        for id in ["R", "M", "E", "Z"] {
            let got = list(
                &c,
                &EventQuery {
                    target_id: Some(id.into()),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(got.len(), 1, "{id}");
            assert_eq!(got[0].target_id, id);
        }
    }

    fn plan(c: &Connection, sql: &str, p: impl rusqlite::Params) -> String {
        c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(p, |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// Deep pages (`before=`) seek by rowid instead of walking down from the newest row, and
    /// the actor filter reads the `events_actor` index; a target still uses its own index.
    #[test]
    fn paging_and_the_actor_filter_use_indexes() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let none = None::<String>;
        let page = plan(
            &c,
            &list_sql(false, false),
            params![none, none, 10i64, 50i64],
        );
        assert!(
            page.contains("USING INTEGER PRIMARY KEY (rowid<?)"),
            "{page}"
        );
        let actor = plan(
            &c,
            &list_sql(false, true),
            params![none, "user", 10i64, 50i64],
        );
        assert!(
            actor.contains("USING INDEX events_actor (actor=? AND rowid<?)"),
            "{actor}"
        );
        let both = plan(
            &c,
            &list_sql(true, true),
            params!["M", "user", 10i64, 50i64],
        );
        assert!(both.contains("USING INDEX events_target"), "{both}");
        // The same answers as before.
        let mut ids = Vec::new();
        for (i, who) in [Actor::User, Actor::Agent("codex".into()), Actor::User]
            .iter()
            .enumerate()
        {
            ids.push(
                record(
                    &c,
                    &EventInput {
                        actor: who,
                        ..input(if i == 1 { "B" } else { "A" }, serde_json::json!(i))
                    },
                    "h",
                )
                .unwrap(),
            );
        }
        let users = list(
            &c,
            &EventQuery {
                actor: Some("user".into()),
                before: Some(ids[2].clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            users.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            [ids[0].clone()]
        );
        let both = list(
            &c,
            &EventQuery {
                target_id: Some("B".into()),
                actor: Some("agent:codex".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(both.len(), 1);
    }

    /// A page cursor that names no event is refused: silently starting again from the newest
    /// would show the first page twice.
    #[test]
    fn an_unknown_page_cursor_is_an_error_and_limits_are_clamped() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        for i in 0..3 {
            record(&c, &input("A", serde_json::json!(i)), "h").unwrap();
        }
        let err = list(
            &c,
            &EventQuery {
                before: Some("01K7NOSUCHEVENT00000000000".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, crate::error::AliceError::Invalid(m) if m.contains("01K7NOSUCHEVENT00000000000")),
            "{err}"
        );
        let one = list(
            &c,
            &EventQuery {
                limit: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(one.len(), 1, "a limit of 0 means the smallest page");
    }
}
