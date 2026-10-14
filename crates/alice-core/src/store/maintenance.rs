//! Row-level maintenance used by the scheduler (spec §5.6). Each function does a bounded amount
//! of work so the writer's transactions stay short.

use crate::error::{AliceError, Result};
use crate::store::fts;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

const FTS_TABLES: [&str; 4] = ["memories_fts", "memories_tri", "chunks_fts", "chunks_tri"];

/// The most judge-cache entries kept, newest first: the cache is an optimisation (a repeated
/// question is answered without a paid call), so a bound keeps the table from growing for ever.
pub const JUDGE_CACHE_KEEP: usize = 50_000;

/// What the history keeps of a purged memory's snapshots (see [`purge_memories`]).
pub const PURGED_TITLE: &str = "[purged]";

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

/// Trashed memories last changed before `cutoff`, oldest first.
pub fn trash_due(conn: &Connection, cutoff: &str, limit: usize) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT seq FROM memories WHERE status = 'trashed' AND updated_at < ?1 ORDER BY seq LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![cutoff, limit as i64], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

/// Delete memories for good: rows, topics, FTS rows, vectors, links both ways, mentions and
/// provenance. Returns (id, title) of each one deleted. The caller removes the index vectors.
///
/// History is scrubbed too, so the purged text does not live on in `events`: every snapshot
/// (`before_json`, `after_json`) of an event that targets a purged memory becomes the stub
/// `{"id": …, "title": "[purged]"}`. A stub (not NULL) keeps the event honest: undo still sees
/// that the target had a state and answers "was purged from the trash; it cannot be restored".
/// Snapshots that were already NULL stay NULL.
pub fn purge_memories(conn: &Connection, seqs: &[i64]) -> Result<Vec<(String, String)>> {
    let mut gone = Vec::new();
    for &seq in seqs {
        let Some((id, title)) = conn
            .query_row(
                "SELECT id, title FROM memories WHERE seq = ?1",
                [seq],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            continue;
        };
        fts::unindex_memory(conn, seq)?;
        conn.execute("DELETE FROM memory_topics WHERE memory_id = ?1", [&id])?;
        conn.execute("DELETE FROM links WHERE from_id = ?1 OR to_id = ?1", [&id])?;
        conn.execute("DELETE FROM mentions WHERE memory_id = ?1", [&id])?;
        conn.execute("DELETE FROM provenance WHERE memory_id = ?1", [&id])?;
        conn.execute(
            "DELETE FROM embeddings WHERE owner_type = 'memory' AND owner_seq = ?1",
            [seq],
        )?;
        conn.execute("DELETE FROM memories WHERE seq = ?1", [seq])?;
        conn.execute(
            "UPDATE events SET
               before_json = CASE WHEN before_json IS NULL THEN NULL ELSE json_object('id', ?1, 'title', ?2) END,
               after_json  = CASE WHEN after_json  IS NULL THEN NULL ELSE json_object('id', ?1, 'title', ?2) END
             WHERE target_type = 'memory' AND target_id = ?1 AND (before_json IS NOT NULL OR after_json IS NOT NULL)",
            params![id, PURGED_TITLE],
        )?;
        gone.push((id, title));
    }
    Ok(gone)
}

/// Chunks stored in Alice before `cutoff`, oldest first. Age is the time a chunk was *stored*
/// (`created_at`): what a chunk says about when it was written (`authored_at`, which an import
/// may set to years ago) never makes it old, so Alice never deletes raw data that has been in
/// Alice for less than the retention. Uses the `chunks_created` index.
pub fn chunks_older_than(conn: &Connection, cutoff: &str, limit: usize) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT seq FROM chunks WHERE created_at < ?1 ORDER BY created_at, seq LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![cutoff, limit as i64], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

/// Raw tier size as text bytes (what `storage.raw_retention = "<size>"` limits).
pub fn raw_bytes(conn: &Connection) -> Result<u64> {
    let n: Option<i64> = conn.query_row("SELECT sum(octet_length(text)) FROM chunks", [], |r| {
        r.get(0)
    })?;
    Ok(n.unwrap_or(0) as u64)
}

pub fn oldest_chunks(conn: &Connection, limit: usize) -> Result<Vec<(i64, u64)>> {
    let mut stmt =
        conn.prepare_cached("SELECT seq, octet_length(text) FROM chunks ORDER BY seq LIMIT ?1")?;
    Ok(stmt
        .query_map([limit as i64], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))
        })?
        .collect::<std::result::Result<_, _>>()?)
}

/// Delete chunks with their FTS rows, vectors and provenance; sources left empty go too.
pub fn delete_chunks(conn: &Connection, seqs: &[i64]) -> Result<usize> {
    if seqs.is_empty() {
        return Ok(0);
    }
    let mut ids = Vec::new();
    let mut sources = std::collections::BTreeSet::new();
    let mut present = Vec::new();
    for &seq in seqs {
        if let Some((id, src)) = conn
            .query_row(
                "SELECT id, source_id FROM chunks WHERE seq = ?1",
                [seq],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            fts::unindex_chunk(conn, seq)?;
            ids.push(id);
            present.push(seq);
            sources.insert(src);
        }
    }
    for batch in present.chunks(500) {
        let ph = placeholders(batch.len());
        conn.execute(
            &format!("DELETE FROM embeddings WHERE owner_type = 'chunk' AND owner_seq IN ({ph})"),
            params_from_iter(batch),
        )?;
        conn.execute(
            &format!("DELETE FROM chunks WHERE seq IN ({ph})"),
            params_from_iter(batch),
        )?;
    }
    for batch in ids.chunks(500) {
        conn.execute(
            &format!(
                "DELETE FROM provenance WHERE chunk_id IN ({})",
                placeholders(batch.len())
            ),
            params_from_iter(batch),
        )?;
    }
    for s in sources {
        conn.execute(
            "DELETE FROM sources WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM chunks WHERE source_id = ?1)",
            [&s],
        )?;
    }
    Ok(ids.len())
}

/// One bounded FTS5 merge step (`pages` pages of work). True while there was work to do.
pub fn fts_merge(conn: &Connection, table: &str, pages: i64) -> Result<bool> {
    if !FTS_TABLES.contains(&table) {
        return Err(AliceError::invalid(format!(
            "{table} is not one of Alice's full-text tables"
        )));
    }
    let before = conn.total_changes();
    conn.execute(
        &format!("INSERT INTO {table}({table}, rank) VALUES ('merge', ?1)"),
        [pages],
    )?;
    // The statement itself is one change; more means segments were written.
    Ok(conn.total_changes() - before > 1)
}

/// Delete up to `limit` events older than `before_ts`, a whole group or none of it: an event
/// whose group has a member at or after `before_ts` stays, so the history never keeps half of a
/// change (undo reverts a whole group).
pub fn prune_events(conn: &Connection, before_ts: &str, limit: usize) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM events WHERE rowid IN (
           SELECT e.rowid FROM events e
           WHERE e.ts < ?1
             AND (e.group_id IS NULL
                  OR NOT EXISTS (SELECT 1 FROM events g WHERE g.group_id = e.group_id AND g.ts >= ?1))
           ORDER BY e.rowid LIMIT ?2)",
        params![before_ts, limit as i64],
    )?)
}

/// Delete up to `limit` judge-cache entries older than `before_ts`, or, when none is, entries
/// beyond the `keep` newest. A deleted entry costs at most one repeated (paid) call. Both
/// queries walk the `judge_cache_created` index.
pub fn prune_judge_cache(
    conn: &Connection,
    before_ts: &str,
    keep: usize,
    limit: usize,
) -> Result<usize> {
    let old = conn.execute(
        "DELETE FROM judge_cache WHERE request_hash IN (
           SELECT request_hash FROM judge_cache WHERE created_at < ?1 ORDER BY created_at LIMIT ?2)",
        params![before_ts, limit as i64],
    )?;
    if old > 0 {
        return Ok(old);
    }
    Ok(conn.execute(
        "DELETE FROM judge_cache WHERE request_hash IN (
           SELECT request_hash FROM judge_cache ORDER BY created_at DESC, request_hash
           LIMIT ?2 OFFSET ?1)",
        params![keep as i64, limit as i64],
    )?)
}

/// Problems `PRAGMA quick_check` reports (empty when the database is sound).
pub fn quick_check(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA quick_check")?;
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(if rows == ["ok"] { Vec::new() } else { rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::embeddings::{self, VecKey};
    use crate::store::memories::{self, InsertOutcome, NewMemoryInput};
    use crate::store::sources::{self, ChunkInput, SourceInput};
    use crate::store::{open_writer, projects};

    fn setup() -> (tempfile::TempDir, Connection, projects::ResolvedProject) {
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

    fn remember(c: &Connection, p: &projects::ResolvedProject, text: &str) -> (String, i64) {
        let m = memories::prepare(
            NewMemoryInput {
                content: text.into(),
                ..Default::default()
            },
            p,
            "h".into(),
        )
        .unwrap();
        match memories::insert(c, &m).unwrap() {
            InsertOutcome::New { id, seq } => (id, seq),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn purge_removes_a_memory_and_everything_that_points_at_it() {
        let (_d, c, p) = setup();
        let (id, seq) = remember(&c, &p, "purge me completely");
        let (other, _) = remember(&c, &p, "keep me");
        embeddings::put(&c, VecKey::Memory(seq), "fake", &[1.0, 0.0]).unwrap();
        crate::store::memories::link(&c, &other, &id, "relates_to", "t").unwrap();
        c.execute(
            "UPDATE memories SET status = 'trashed', updated_at = '2020-01-01T00:00:00.000Z' WHERE seq = ?1",
            [seq],
        )
        .unwrap();
        assert_eq!(
            trash_due(&c, "2026-01-01T00:00:00.000Z", 10).unwrap(),
            vec![seq]
        );
        let gone = purge_memories(&c, &[seq]).unwrap();
        assert_eq!(gone, vec![(id.clone(), "purge me completely".to_string())]);
        for (sql, what) in [
            ("SELECT count(*) FROM memories WHERE id = ?1", "row"),
            (
                "SELECT count(*) FROM links WHERE to_id = ?1 OR from_id = ?1",
                "links",
            ),
            (
                "SELECT count(*) FROM memory_topics WHERE memory_id = ?1",
                "topics",
            ),
        ] {
            let n: i64 = c.query_row(sql, [&id], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{what}");
        }
        let fts: i64 = c
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH '\"purge\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 0);
        let vectors: i64 = c
            .query_row(
                "SELECT count(*) FROM embeddings WHERE owner_type = 'memory' AND owner_seq = ?1",
                [seq],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vectors, 0);
    }

    /// B1: the history of a purged memory holds none of its text (and the memory next to it keeps
    /// its own snapshots).
    #[test]
    fn purge_scrubs_the_snapshots_of_the_history() {
        let (_d, c, p) = setup();
        let (id, seq) = remember(&c, &p, "the launch code is swordfish");
        let (other, _) = remember(&c, &p, "unrelated survivor note");
        let snap = |text: &str| {
            serde_json::json!({"status": "trashed", "kind": "note", "title": text, "body": text,
                "importance": 3, "project_id": "p", "topics": [], "links_out": []})
        };
        let insert = |eid: &str,
                      target: &str,
                      before: Option<serde_json::Value>,
                      after: Option<serde_json::Value>| {
            c.execute(
                "INSERT INTO events (id, ts, hlc, actor, action, target_type, target_id, before_json, after_json)
                 VALUES (?1, '2026-01-01T00:00:00.000Z', 'h', 'user', 'memory.forget', 'memory', ?2, ?3, ?4)",
                params![eid, target, before.map(|v| v.to_string()), after.map(|v| v.to_string())],
            )
            .unwrap();
        };
        insert("E1", &id, None, Some(snap("the launch code is swordfish")));
        insert(
            "E2",
            &id,
            Some(snap("the launch code is swordfish")),
            Some(snap("the launch code is swordfish!")),
        );
        insert("E3", &other, None, Some(snap("unrelated survivor note")));
        purge_memories(&c, &[seq]).unwrap();
        let all: String = c
            .query_row(
                "SELECT group_concat(coalesce(before_json, '') || coalesce(after_json, ''), ' ') FROM events",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!all.contains("swordfish"), "{all}");
        let (before, after): (Option<String>, Option<String>) = c
            .query_row(
                "SELECT before_json, after_json FROM events WHERE id = 'E1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(before, None, "a snapshot that was NULL stays NULL");
        let stub: serde_json::Value = serde_json::from_str(&after.unwrap()).unwrap();
        assert_eq!(stub, serde_json::json!({"id": id, "title": "[purged]"}));
        let kept: String = c
            .query_row("SELECT after_json FROM events WHERE id = 'E3'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(kept.contains("unrelated survivor note"));
    }

    #[test]
    fn raw_retention_by_age_and_by_size_deletes_the_oldest_and_empty_sources() {
        let (_d, c, p) = setup();
        let chunks: Vec<ChunkInput> = (0..10)
            .map(|i| ChunkInput {
                ord: i,
                text: format!("chunk {i} {}", "x".repeat(90)),
                authored_at: Some(format!("2026-01-{:02}T00:00:00.000Z", i + 1)),
                ..Default::default()
            })
            .collect();
        let out = sources::ingest(
            &c,
            &p,
            &SourceInput {
                uri: "s1".into(),
                ..Default::default()
            },
            &chunks,
        )
        .unwrap();
        assert_eq!(out.inserted, 10);
        // Age is the time stored: backdate the rows (the ingest stored them now).
        c.execute(
            "UPDATE chunks SET created_at = '2026-01-' || printf('%02d', ord + 1) || 'T00:00:00.000Z'",
            [],
        )
        .unwrap();
        let old = chunks_older_than(&c, "2026-01-04T00:00:00.000Z", 100).unwrap();
        assert_eq!(old.len(), 3);
        assert_eq!(delete_chunks(&c, &old).unwrap(), 3);
        let before = raw_bytes(&c).unwrap();
        assert!(before >= 7 * 90);
        let exact: i64 = c
            .query_row(
                "SELECT sum(length(CAST(text AS BLOB))) FROM chunks",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, exact as u64, "octet_length counts bytes");
        let oldest = oldest_chunks(&c, 2).unwrap();
        assert_eq!(oldest.len(), 2);
        delete_chunks(&c, &oldest.iter().map(|(s, _)| *s).collect::<Vec<_>>()).unwrap();
        assert!(raw_bytes(&c).unwrap() < before);
        let all: Vec<i64> = c
            .prepare("SELECT seq FROM chunks")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        delete_chunks(&c, &all).unwrap();
        let sources_left: i64 = c
            .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sources_left, 0, "a source with no chunks left is removed");
    }

    /// I-1: what a chunk says about when it was written never makes it old. An import authored
    /// in 2020 and stored today survives a 90-day retention, and `authored_at` values that sort
    /// as "old" text (an epoch, an empty string, a US date) change nothing.
    #[test]
    fn age_retention_counts_the_time_stored_not_the_time_authored() {
        let (_d, c, p) = setup();
        let weird = [
            "0",
            "",
            "10/13/2026",
            "1",
            "2020-01-01T00:00:00.000Z",
            "1577836800",
        ];
        let chunks: Vec<ChunkInput> = weird
            .iter()
            .enumerate()
            .map(|(i, a)| ChunkInput {
                ord: i as i64,
                text: format!("imported chunk {i}"),
                authored_at: Some(a.to_string()),
                ..Default::default()
            })
            .collect();
        sources::ingest(
            &c,
            &p,
            &SourceInput {
                uri: "import".into(),
                ..Default::default()
            },
            &chunks,
        )
        .unwrap();
        // Stored today: nothing is older than 90 days.
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(90))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        assert!(chunks_older_than(&c, &cutoff, 100).unwrap().is_empty());
        // The same malformed values written straight into the table (an older build) too.
        c.execute("UPDATE chunks SET authored_at = CASE ord WHEN 0 THEN '0' WHEN 1 THEN '' ELSE '10/13/2026' END", []).unwrap();
        assert!(chunks_older_than(&c, &cutoff, 100).unwrap().is_empty());
        // Stored long ago: old, whatever the text says.
        c.execute("UPDATE chunks SET created_at = '2025-01-01T00:00:00.000Z', authored_at = '2030-01-01T00:00:00.000Z' WHERE ord < 2", []).unwrap();
        assert_eq!(chunks_older_than(&c, &cutoff, 100).unwrap().len(), 2);
    }

    /// M-1: events of one group go together or not at all.
    #[test]
    fn pruning_events_never_takes_part_of_a_group() {
        let (_d, c, _p) = setup();
        let ev = |id: &str, ts: &str, group: Option<&str>| {
            c.execute(
                "INSERT INTO events (id, ts, hlc, actor, action, target_type, target_id, group_id) VALUES (?1, ?2, 'h', 'user', 'memory.edit', 'memory', 'X', ?3)",
                params![id, ts, group],
            )
            .unwrap();
        };
        ev("A1", "2020-01-01T00:00:00.000Z", Some("G1")); // whole group old
        ev("A2", "2020-01-01T00:00:01.000Z", Some("G1"));
        ev("B1", "2020-01-01T00:00:02.000Z", Some("G2")); // straddles the cutoff
        ev("B2", "2026-06-01T00:00:00.000Z", Some("G2"));
        ev("C1", "2020-01-01T00:00:03.000Z", None);
        let n = prune_events(&c, "2025-01-01T00:00:00.000Z", 500).unwrap();
        assert_eq!(n, 3, "G1 and the ungrouped event");
        let left: Vec<String> = c
            .prepare("SELECT id FROM events ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(left, vec!["B1", "B2"]);
    }

    #[test]
    fn events_prune_fts_merge_and_quick_check() {
        let (_d, c, p) = setup();
        for i in 0..5 {
            remember(&c, &p, &format!("note {i}"));
        }
        c.execute("INSERT INTO events (id, ts, hlc, actor, action, target_type, target_id) VALUES ('E1', '2020-01-01T00:00:00.000Z', 'h', 'user', 'memory.edit', 'memory', 'X')", []).unwrap();
        assert_eq!(
            prune_events(&c, "2025-01-01T00:00:00.000Z", 500).unwrap(),
            1
        );
        let mut rounds = 0;
        while fts_merge(&c, "memories_fts", 500).unwrap() {
            rounds += 1;
            assert!(rounds < 100, "merge terminates");
        }
        assert!(quick_check(&c).unwrap().is_empty());
        assert!(
            fts_merge(&c, "not_a_table", 1).is_err(),
            "only Alice's FTS tables"
        );
    }

    /// An FTS table with many small segments has work for the merge step, and the step ends.
    #[test]
    fn fts_merge_reports_work_while_segments_remain() {
        let (_d, c, p) = setup();
        for i in 0..40 {
            remember(&c, &p, &format!("segment note number {i}"));
        }
        let segments = |c: &Connection| -> i64 {
            c.query_row("SELECT count(*) FROM memories_fts_data", [], |r| r.get(0))
                .unwrap()
        };
        let before = segments(&c);
        let mut rounds = 0;
        let mut worked = false;
        while fts_merge(&c, "memories_fts", 1).unwrap() {
            worked = true;
            rounds += 1;
            assert!(rounds < 500, "merge terminates");
        }
        assert!(worked, "work was reported ({before} rows before)");
        assert!(segments(&c) < before, "the segments were merged");
        assert!(quick_check(&c).unwrap().is_empty());
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH '\"segment\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 40, "merging loses nothing");
    }

    #[test]
    fn the_judge_cache_is_pruned_by_age_and_by_size() {
        let (_d, c, _p) = setup();
        for i in 0..10 {
            let created = if i < 3 {
                format!("2020-01-0{}T00:00:00.000Z", i + 1)
            } else {
                format!("2026-10-{:02}T00:00:00.000Z", i + 1)
            };
            c.execute(
                "INSERT INTO judge_cache (request_hash, provider, model, response_json, created_at) VALUES (?1, 'jev', 'm', '{}', ?2)",
                params![format!("h{i}"), created],
            )
            .unwrap();
        }
        // By age: the three of 2020 go, in slices of at most `limit`.
        assert_eq!(
            prune_judge_cache(&c, "2025-01-01T00:00:00.000Z", 100, 2).unwrap(),
            2
        );
        assert_eq!(
            prune_judge_cache(&c, "2025-01-01T00:00:00.000Z", 100, 2).unwrap(),
            1
        );
        assert_eq!(
            prune_judge_cache(&c, "2025-01-01T00:00:00.000Z", 100, 2).unwrap(),
            0
        );
        // By size: only the newest four stay.
        assert_eq!(
            prune_judge_cache(&c, "2025-01-01T00:00:00.000Z", 4, 100).unwrap(),
            3
        );
        let left: Vec<String> = c
            .prepare("SELECT request_hash FROM judge_cache ORDER BY created_at")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(left, vec!["h6", "h7", "h8", "h9"]);
    }
}
