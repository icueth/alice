//! Review inbox (spec §5.4): questions Alice could not settle on its own. Items are added by
//! the judge and by undo, listed here, and resolved with one of the choices their type offers
//! (the writer applies the choice; `mark_resolved` only records it).

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::store::events::Actor;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewType {
    Supersede,
    Contradiction,
    Merge,
    Predicate,
    Project,
    SyncConflict,
    /// An undo whose target changed since the event.
    Undo,
}

impl ReviewType {
    pub const ALL: [ReviewType; 7] = [
        ReviewType::Supersede,
        ReviewType::Contradiction,
        ReviewType::Merge,
        ReviewType::Predicate,
        ReviewType::Project,
        ReviewType::SyncConflict,
        ReviewType::Undo,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReviewType::Supersede => "supersede",
            ReviewType::Contradiction => "contradiction",
            ReviewType::Merge => "merge",
            ReviewType::Predicate => "predicate",
            ReviewType::Project => "project",
            ReviewType::SyncConflict => "sync_conflict",
            ReviewType::Undo => "undo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Open,
    Resolved,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewReview {
    pub kind: ReviewType,
    pub payload: serde_json::Value,
    pub confidence: Option<f64>,
    /// Same key as an open item: no second item (the existing one already asks this).
    pub dedupe_key: Option<String>,
}

/// Insert an open item; `None` when an open item with the same `dedupe_key` exists. Only that
/// conflict is skipped: any other failure is an error.
pub fn create(conn: &Connection, r: &NewReview) -> Result<Option<String>> {
    let id = new_id();
    let n = conn.execute(
        "INSERT INTO review_items (id, type, payload_json, confidence, status, created_at, dedupe_key)
         VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6)
         ON CONFLICT (dedupe_key) WHERE dedupe_key IS NOT NULL AND status = 'open' DO NOTHING",
        params![
            id,
            r.kind.as_str(),
            r.payload.to_string(),
            r.confidence,
            now_rfc3339(),
            r.dedupe_key
        ],
    )?;
    Ok((n == 1).then_some(id))
}

/// The open item asking `r`'s question: the existing one (`false`) or a new one (`true`).
pub fn open_or_create(conn: &Connection, r: &NewReview) -> Result<(String, bool)> {
    if let Some(id) = create(conn, r)? {
        return Ok((id, true));
    }
    let key = r.dedupe_key.as_deref().unwrap_or_default();
    match find_open(conn, key)? {
        Some(id) => Ok((id, false)),
        None => Err(AliceError::Other(format!(
            "review item for \"{key}\" was neither added nor found open; retry"
        ))),
    }
}

/// The same question was asked before and the answer was "reject" (do not suggest it again).
pub fn was_rejected(conn: &Connection, dedupe_key: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM review_items WHERE dedupe_key = ?1 AND resolution IN ('reject', 'keep', 'keep_both'))",
        [dedupe_key],
        |r| r.get(0),
    )?)
}

/// The open item asking the question `dedupe_key` names, if there is one.
pub fn find_open(conn: &Connection, dedupe_key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM review_items WHERE dedupe_key = ?1 AND status = 'open'",
            [dedupe_key],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn count_open(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM review_items WHERE status = 'open'",
        [],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

/// Open items of `t`.
pub fn count_open_of(conn: &Connection, t: ReviewType) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM review_items WHERE status = 'open' AND type = ?1",
        [t.as_str()],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

/// How many items wait, in all and by type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewCounts {
    pub total: u64,
    pub by_type: std::collections::BTreeMap<String, u64>,
}

/// Open items, narrowed to a type and to the `provider` their payload names (the judge, or
/// `cluster` for the weekly merge job).
pub fn counts_open(
    conn: &Connection,
    t: Option<ReviewType>,
    provider: Option<&str>,
) -> Result<ReviewCounts> {
    let mut stmt = conn.prepare(
        "SELECT type, count(*) FROM review_items
         WHERE status = 'open' AND (?1 IS NULL OR type = ?1)
           AND (?2 IS NULL OR json_extract(payload_json, '$.provider') = ?2)
         GROUP BY type",
    )?;
    let rows = stmt.query_map(params![t.map(|t| t.as_str()), provider], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
    })?;
    let mut counts = ReviewCounts::default();
    for row in rows {
        let (kind, n) = row?;
        counts.total += n;
        counts.by_type.insert(kind, n);
    }
    Ok(counts)
}

/// The answer that closes an item of type `t` without changing any memory or fact, if the type
/// has one. It is the answer a person gives to say "leave it" (a rejected merge is not suggested
/// again).
fn dismissal(t: ReviewType) -> Option<&'static str> {
    match t {
        ReviewType::Merge | ReviewType::Supersede => Some("reject"),
        ReviewType::Predicate | ReviewType::Project => Some("keep"),
        ReviewType::Undo => Some("dismiss"),
        ReviewType::Contradiction | ReviewType::SyncConflict => None,
    }
}

/// Close every open item of type `t` (narrowed to `provider`) with the answer that leaves the
/// memories as they are; returns how many. Types without such an answer are refused.
pub fn dismiss_open(
    conn: &Connection,
    t: ReviewType,
    provider: Option<&str>,
    by: &Actor,
) -> Result<u64> {
    let choice = dismissal(t).ok_or_else(|| {
        AliceError::invalid(format!(
            "{} items cannot be dismissed together (every answer changes something); resolve them one by one with `alice review resolve`",
            t.as_str()
        ))
    })?;
    let n = conn.execute(
        "UPDATE review_items SET status = 'resolved', resolved_at = ?1, resolved_by = ?2, resolution = ?3
         WHERE status = 'open' AND type = ?4
           AND (?5 IS NULL OR json_extract(payload_json, '$.provider') = ?5)",
        params![now_rfc3339(), by.as_string(), choice, t.as_str(), provider],
    )?;
    Ok(n as u64)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewChoice {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubjectRef {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewItem {
    pub id: String,
    pub review_type: ReviewType,
    pub payload: serde_json::Value,
    pub confidence: Option<f64>,
    pub status: ReviewStatus,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
    /// Memories the item is about (titles, so a person can decide without opening them).
    pub subjects: Vec<SubjectRef>,
    pub choices: Vec<ReviewChoice>,
}

/// Open items by default, newest first. `before` is a review id and `offset` a number of items
/// to skip (paging); `before` wins when both are given.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReviewQuery {
    #[serde(default)]
    pub status: Option<ReviewStatus>,
    #[serde(default)]
    pub review_type: Option<ReviewType>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// What a person may answer to an item of type `t`.
pub fn choices(t: ReviewType) -> Vec<ReviewChoice> {
    let c = |id: &str, d: &str| ReviewChoice {
        id: id.into(),
        description: d.into(),
    };
    match t {
        ReviewType::Supersede => vec![
            c("accept", "the new memory replaces the old one"),
            c("reject", "keep both as they are"),
        ],
        ReviewType::Contradiction => vec![
            c(
                "keep_new",
                "the new memory is right; it supersedes the old one",
            ),
            c(
                "keep_old",
                "the old memory is right; the new one becomes superseded",
            ),
            c("keep_both", "both stay, linked as contradicting"),
        ],
        ReviewType::Merge => vec![
            c(
                "merge",
                "keep the survivor; the others become its duplicates",
            ),
            c("reject", "keep them apart"),
        ],
        ReviewType::Predicate => vec![
            c("map", "treat the new predicate as the existing one"),
            c("keep", "keep both predicates"),
        ],
        ReviewType::Project => vec![
            c("move", "move the memory to the suggested project"),
            c("keep", "leave it where it is"),
        ],
        ReviewType::Undo => vec![
            c(
                "apply",
                "undo anyway: restore the earlier state over the later change",
            ),
            c("dismiss", "keep the current state"),
        ],
        ReviewType::SyncConflict => vec![
            c("keep_local", "keep this device's version"),
            c("keep_remote", "take the other device's version"),
        ],
    }
}

fn subject_ids(t: ReviewType, p: &serde_json::Value) -> Vec<String> {
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(String::from);
    let mut ids: Vec<String> = match t {
        ReviewType::Supersede | ReviewType::Contradiction => {
            [s("new_id"), s("old_id")].into_iter().flatten().collect()
        }
        ReviewType::Merge => s("survivor")
            .into_iter()
            .chain(
                p["others"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(String::from)),
            )
            .collect(),
        ReviewType::Project => s("memory_id").into_iter().collect(),
        ReviewType::Undo => s("target_id")
            .filter(|_| s("target_type").as_deref() == Some("memory"))
            .into_iter()
            .collect(),
        ReviewType::Predicate | ReviewType::SyncConflict => vec![],
    };
    let mut seen = std::collections::HashSet::new();
    ids.retain(|i| seen.insert(i.clone()));
    ids
}

const COLS: &str =
    "id, type, payload_json, confidence, status, created_at, resolved_at, resolved_by, resolution";

/// One row as an item; `None` for a type this build does not know (a row written by a newer
/// Alice is skipped with a warning, never a failed list).
fn read(conn: &Connection, r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<ReviewItem>> {
    let t: String = r.get(1)?;
    let Some(review_type) = ReviewType::parse(&t) else {
        tracing::warn!(id = %r.get::<_, String>(0)?, review_type = %t, "skipping a review item of an unknown type");
        return Ok(None);
    };
    let payload: serde_json::Value =
        serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
    let status: String = r.get(4)?;
    let subjects = subject_ids(review_type, &payload)
        .into_iter()
        .filter_map(|id| {
            conn.query_row(
                "SELECT id, title, status FROM memories WHERE id = ?1",
                [&id],
                |m| {
                    Ok(SubjectRef {
                        id: m.get(0)?,
                        title: m.get(1)?,
                        status: m.get(2)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
        })
        .collect();
    Ok(Some(ReviewItem {
        id: r.get(0)?,
        review_type,
        payload,
        confidence: r.get(3)?,
        status: if status == "open" {
            ReviewStatus::Open
        } else {
            ReviewStatus::Resolved
        },
        created_at: r.get(5)?,
        resolved_at: r.get(6)?,
        resolved_by: r.get(7)?,
        resolution: r.get(8)?,
        subjects,
        choices: choices(review_type),
    }))
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<ReviewItem>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM review_items WHERE id = ?1"),
            [id],
            |r| read(conn, r),
        )
        .optional()?
        .flatten())
}

pub fn list(conn: &Connection, q: &ReviewQuery) -> Result<Vec<ReviewItem>> {
    let status = match q.status.unwrap_or(ReviewStatus::Open) {
        ReviewStatus::Open => "open",
        ReviewStatus::Resolved => "resolved",
    };
    let before: Option<i64> = match &q.before {
        Some(id) => Some(
            conn.query_row("SELECT rowid FROM review_items WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| {
                AliceError::invalid(format!(
                    "before \"{id}\" is not a review item id; pass the id of the last item you saw, or omit before"
                ))
            })?,
        ),
        None => None,
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM review_items
         WHERE status = ?1 AND (?2 IS NULL OR type = ?2) AND (?3 IS NULL OR rowid < ?3)
         ORDER BY rowid DESC LIMIT ?4 OFFSET ?5"
    ))?;
    let rows = stmt.query_map(
        params![
            status,
            q.review_type.map(|t| t.as_str()),
            before,
            q.limit.unwrap_or(50).clamp(1, 500) as i64,
            q.offset.unwrap_or(0) as i64
        ],
        |r| read(conn, r),
    )?;
    Ok(rows
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect())
}

/// Record a resolution. Fails for an unknown or closed item, or a choice the type does not offer.
pub fn mark_resolved(conn: &Connection, id: &str, choice: &str, by: &Actor) -> Result<()> {
    let item = get(conn, id)?.ok_or_else(|| {
        AliceError::NotFound(format!("review item {id}; list them with `alice review`"))
    })?;
    if item.status != ReviewStatus::Open {
        return Err(AliceError::invalid(format!(
            "review item {id} is already resolved ({}); nothing to do",
            item.resolution.unwrap_or_default()
        )));
    }
    if !item.choices.iter().any(|c| c.id == choice) {
        return Err(AliceError::invalid(format!(
            "\"{choice}\" is not a choice for this {} item; use one of: {}",
            item.review_type.as_str(),
            item.choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    conn.execute(
        "UPDATE review_items SET status = 'resolved', resolved_at = ?1, resolved_by = ?2, resolution = ?3 WHERE id = ?4",
        params![now_rfc3339(), by.as_string(), choice, id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn undo_item(key: &str) -> NewReview {
        NewReview {
            kind: ReviewType::Undo,
            payload: serde_json::json!({"event_id": "E"}),
            confidence: None,
            dedupe_key: Some(key.into()),
        }
    }

    #[test]
    fn one_open_item_per_dedupe_key() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let first = create(&c, &undo_item("undo:E")).unwrap().unwrap();
        assert_eq!(create(&c, &undo_item("undo:E")).unwrap(), None);
        assert_eq!(find_open(&c, "undo:E").unwrap(), Some(first.clone()));
        assert_eq!(count_open(&c).unwrap(), 1);
        let other = create(&c, &undo_item("undo:F")).unwrap();
        assert!(other.is_some());
        // Items without a key never collide.
        let mut free = undo_item("x");
        free.dedupe_key = None;
        assert!(create(&c, &free).unwrap().is_some());
        assert!(create(&c, &free).unwrap().is_some());
        assert_eq!(count_open(&c).unwrap(), 4);
        // Once resolved, the same question can be asked again.
        c.execute(
            "UPDATE review_items SET status = 'resolved' WHERE id = ?1",
            [&first],
        )
        .unwrap();
        assert_eq!(find_open(&c, "undo:E").unwrap(), None);
        let again = create(&c, &undo_item("undo:E")).unwrap().unwrap();
        assert_ne!(again, first);
        let (kind, status): (String, String) = c
            .query_row(
                "SELECT type, status FROM review_items WHERE id = ?1",
                [&again],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), status.as_str()), ("undo", "open"));
    }

    #[test]
    fn open_or_create_reuses_the_open_item_and_says_whether_it_is_new() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let (id, new) = open_or_create(&c, &undo_item("undo:G")).unwrap();
        assert!(new && !id.is_empty());
        assert_eq!(
            open_or_create(&c, &undo_item("undo:G")).unwrap(),
            (id.clone(), false)
        );
        assert_eq!(count_open(&c).unwrap(), 1);
    }

    #[test]
    fn review_types_round_trip_and_match_serde() {
        for t in ReviewType::ALL {
            assert_eq!(ReviewType::parse(t.as_str()), Some(t));
            assert_eq!(serde_json::to_value(t).unwrap(), t.as_str());
        }
        assert_eq!(ReviewType::parse("nope"), None);
    }

    #[test]
    fn items_list_newest_first_dedupe_while_open_and_resolve_once() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let item = |k: &str| NewReview {
            kind: ReviewType::Supersede,
            payload: serde_json::json!({"new_id": k, "old_id": "O", "p": 0.7, "provider": "heuristic"}),
            confidence: Some(0.7),
            dedupe_key: Some(format!("supersede:{k}:O")),
        };
        let a = create(&c, &item("A")).unwrap().unwrap();
        assert_eq!(
            create(&c, &item("A")).unwrap(),
            None,
            "same question while open"
        );
        let b = create(&c, &item("B")).unwrap().unwrap();
        let open = list(&c, &ReviewQuery::default()).unwrap();
        assert_eq!(
            open.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            vec![b.clone(), a.clone()]
        );
        assert_eq!(
            open[0]
                .choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["accept", "reject"]
        );
        mark_resolved(&c, &a, "reject", &Actor::User).unwrap();
        let err = mark_resolved(&c, &a, "accept", &Actor::User)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already resolved"), "{err}");
        assert!(
            mark_resolved(&c, &b, "maybe", &Actor::User)
                .unwrap_err()
                .to_string()
                .contains("accept, reject")
        );
        let got = get(&c, &a).unwrap().unwrap();
        assert_eq!(
            (
                got.status,
                got.resolution.as_deref(),
                got.resolved_by.as_deref()
            ),
            (ReviewStatus::Resolved, Some("reject"), Some("user"))
        );
        assert!(
            create(&c, &item("A")).unwrap().is_some(),
            "the question may be asked again once resolved"
        );
        assert_eq!(count_open(&c).unwrap(), 2);
    }

    #[test]
    fn listing_filters_by_status_and_type_and_pages_with_before() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let mk = |kind: ReviewType, key: &str| {
            create(
                &c,
                &NewReview {
                    kind,
                    payload: serde_json::json!({}),
                    confidence: None,
                    dedupe_key: Some(key.into()),
                },
            )
            .unwrap()
            .unwrap()
        };
        let a = mk(ReviewType::Undo, "u1");
        let b = mk(ReviewType::Merge, "m1");
        let cc = mk(ReviewType::Undo, "u2");
        mark_resolved(&c, &a, "dismiss", &Actor::User).unwrap();
        let ids = |q: &ReviewQuery| {
            list(&c, q)
                .unwrap()
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&ReviewQuery::default()), vec![cc.clone(), b.clone()]);
        assert_eq!(
            ids(&ReviewQuery {
                status: Some(ReviewStatus::Resolved),
                ..Default::default()
            }),
            vec![a.clone()]
        );
        assert_eq!(
            ids(&ReviewQuery {
                review_type: Some(ReviewType::Merge),
                ..Default::default()
            }),
            vec![b.clone()]
        );
        assert_eq!(
            ids(&ReviewQuery {
                before: Some(cc.clone()),
                ..Default::default()
            }),
            vec![b.clone()]
        );
        assert_eq!(
            ids(&ReviewQuery {
                limit: Some(1),
                ..Default::default()
            }),
            vec![cc]
        );
    }

    #[test]
    fn an_unknown_type_is_skipped_and_an_unknown_cursor_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let a = create(&c, &undo_item("k1")).unwrap().unwrap();
        // A row written by a newer Alice.
        c.execute(
            "INSERT INTO review_items (id, type, payload_json, status, created_at) VALUES ('ZNEWER', 'from_the_future', '{}', 'open', '2099-01-01T00:00:00.000Z')",
            [],
        )
        .unwrap();
        let listed = list(&c, &ReviewQuery::default()).unwrap();
        assert_eq!(
            listed.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            [a.as_str()]
        );
        assert!(get(&c, "ZNEWER").unwrap().is_none());
        let err = list(
            &c,
            &ReviewQuery {
                before: Some("NOSUCH".into()),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a review item id"), "{err}");
    }

    fn seed(c: &Connection, kind: ReviewType, provider: Option<&str>, key: &str) -> String {
        create(
            c,
            &NewReview {
                kind,
                payload: provider.map_or_else(
                    || serde_json::json!({}),
                    |p| serde_json::json!({ "provider": p }),
                ),
                confidence: Some(0.92),
                dedupe_key: Some(key.into()),
            },
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn listing_pages_with_an_offset() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let ids: Vec<String> = (0..5)
            .map(|k| seed(&c, ReviewType::Merge, Some("cluster"), &format!("m{k}")))
            .collect();
        let page = |offset: usize| {
            list(
                &c,
                &ReviewQuery {
                    limit: Some(2),
                    offset: Some(offset),
                    ..Default::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect::<Vec<_>>()
        };
        // Newest first: page 1 is the last two created.
        assert_eq!(page(0), vec![ids[4].clone(), ids[3].clone()]);
        assert_eq!(page(2), vec![ids[2].clone(), ids[1].clone()]);
        assert_eq!(page(4), vec![ids[0].clone()]);
        assert!(page(6).is_empty());
    }

    #[test]
    fn open_counts_are_totals_by_type_and_can_be_narrowed() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        for k in 0..3 {
            seed(&c, ReviewType::Merge, Some("cluster"), &format!("c{k}"));
        }
        seed(&c, ReviewType::Merge, Some("jev"), "j");
        seed(&c, ReviewType::Predicate, None, "p");
        let all = counts_open(&c, None, None).unwrap();
        assert_eq!(all.total, 5);
        assert_eq!(all.by_type["merge"], 4);
        assert_eq!(all.by_type["predicate"], 1);
        let narrowed = counts_open(&c, Some(ReviewType::Merge), Some("cluster")).unwrap();
        assert_eq!(narrowed.total, 3);
        assert_eq!(narrowed.by_type.len(), 1);
    }

    #[test]
    fn dismissing_closes_only_the_matching_open_items() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let ours: Vec<String> = (0..3)
            .map(|k| seed(&c, ReviewType::Merge, Some("cluster"), &format!("c{k}")))
            .collect();
        let other_provider = seed(&c, ReviewType::Merge, Some("jev"), "j");
        let predicate = seed(&c, ReviewType::Predicate, None, "p");
        let closed = seed(&c, ReviewType::Merge, Some("cluster"), "done");
        mark_resolved(&c, &closed, "merge", &Actor::User).unwrap();

        let n = dismiss_open(&c, ReviewType::Merge, Some("cluster"), &Actor::User).unwrap();
        assert_eq!(n, 3);
        for id in &ours {
            let item = get(&c, id).unwrap().unwrap();
            assert_eq!(item.status, ReviewStatus::Resolved);
            // Merge's "reject" means keep them apart: the job never suggests it again.
            assert_eq!(item.resolution.as_deref(), Some("reject"));
            assert_eq!(item.resolved_by.as_deref(), Some("user"));
        }
        for id in [&other_provider, &predicate] {
            assert_eq!(get(&c, id).unwrap().unwrap().status, ReviewStatus::Open);
        }
        assert_eq!(
            get(&c, &closed).unwrap().unwrap().resolution.as_deref(),
            Some("merge"),
            "an item resolved earlier keeps its answer"
        );
        assert!(was_rejected(&c, "c0").unwrap());
        // Without a provider every open item of the type goes; nothing is left to close twice.
        assert_eq!(
            dismiss_open(&c, ReviewType::Merge, None, &Actor::User).unwrap(),
            1
        );
        assert_eq!(
            dismiss_open(&c, ReviewType::Merge, None, &Actor::User).unwrap(),
            0
        );
        // Types whose every answer changes something are not closed in bulk.
        seed(&c, ReviewType::Contradiction, None, "x");
        let err = dismiss_open(&c, ReviewType::Contradiction, None, &Actor::User)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("contradiction") && err.contains("one by one"),
            "{err}"
        );
        assert_eq!(count_open_of(&c, ReviewType::Contradiction).unwrap(), 1);
    }
}
