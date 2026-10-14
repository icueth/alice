//! Review inbox (spec §5.4): questions Alice could not settle on its own. This is the part
//! undo needs (adding an item); listing and resolving come with the inbox.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
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

#[derive(Debug, Clone)]
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
}
