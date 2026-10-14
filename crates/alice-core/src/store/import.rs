//! Legacy ids of imported records: where each one went (a collapsed duplicate points at its survivor).

use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyTarget {
    pub target_type: String,
    pub target_id: String,
}

/// Record where `legacy_id` went; the first writer wins.
pub fn legacy_put(
    conn: &Connection,
    legacy_id: &str,
    target_type: &str,
    target_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO legacy_ids (legacy_id, target_type, target_id) VALUES (?1, ?2, ?3)",
        params![legacy_id, target_type, target_id],
    )?;
    Ok(())
}

pub fn legacy_get(conn: &Connection, legacy_id: &str) -> Result<Option<LegacyTarget>> {
    Ok(conn
        .query_row(
            "SELECT target_type, target_id FROM legacy_ids WHERE legacy_id = ?1",
            [legacy_id],
            |r| {
                Ok(LegacyTarget {
                    target_type: r.get(0)?,
                    target_id: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// The subset of `ids` already stored, queried in chunks of at most 500.
pub fn legacy_existing(conn: &Connection, ids: &[String]) -> Result<HashSet<String>> {
    let mut found = HashSet::new();
    for part in ids.chunks(500) {
        let marks = vec!["?"; part.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT legacy_id FROM legacy_ids WHERE legacy_id IN ({marks})"
        ))?;
        for row in stmt.query_map(params_from_iter(part.iter()), |r| r.get::<_, String>(0))? {
            found.insert(row?);
        }
    }
    Ok(found)
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

    #[test]
    fn legacy_ids_round_trip_and_ignore_repeats() {
        let (_d, conn) = conn();
        legacy_put(&conn, "drawer_a", "memory", "M1").unwrap();
        legacy_put(&conn, "drawer_a", "memory", "OTHER").unwrap(); // first writer wins
        let hit = legacy_get(&conn, "drawer_a").unwrap().unwrap();
        assert_eq!(
            (hit.target_type.as_str(), hit.target_id.as_str()),
            ("memory", "M1")
        );
        assert!(legacy_get(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn existing_finds_only_stored_ids_even_for_thousands() {
        let (_d, conn) = conn();
        for i in 0..1200 {
            legacy_put(&conn, &format!("d{i}"), "chunk", &format!("C{i}")).unwrap();
        }
        let ask: Vec<String> = (1100..1300).map(|i| format!("d{i}")).collect();
        let got = legacy_existing(&conn, &ask).unwrap();
        assert_eq!(got.len(), 100);
        assert!(got.contains("d1199") && !got.contains("d1200"));
    }
}
