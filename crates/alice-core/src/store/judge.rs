//! Judge cache (by request hash) and the per-day cost ledger (spec §3.3, §5.2).
//!
//! The cache keeps only the request hash and the provider's response JSON, never the request
//! text; the ledger keeps only counts and spend.

use crate::error::{AliceError, Result};
use crate::ids::now_rfc3339;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CostDelta {
    /// UTC day, `YYYY-MM-DD`.
    pub day: String,
    pub provider: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
}

pub type CostRow = CostDelta;

pub fn cache_get(conn: &Connection, hash: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT response_json FROM judge_cache WHERE request_hash = ?1",
            [hash],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn cache_put(
    conn: &Connection,
    hash: &str,
    provider: &str,
    model: &str,
    response_json: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO judge_cache (request_hash, provider, model, response_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(request_hash) DO UPDATE SET provider = excluded.provider, model = excluded.model,
           response_json = excluded.response_json, created_at = excluded.created_at",
        params![hash, provider, model, response_json, now_rfc3339()],
    )?;
    Ok(())
}

pub fn spent(conn: &Connection, day: &str, provider: &str) -> Result<f64> {
    Ok(conn
        .query_row(
            "SELECT usd FROM cost_ledger WHERE day = ?1 AND provider = ?2",
            params![day, provider],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0.0))
}

/// Adds a delta to the day's totals. The sums are checked in Rust inside a savepoint: a total
/// that would overflow is an error and nothing is written.
pub fn add_cost(conn: &Connection, d: &CostDelta) -> Result<()> {
    if !d.usd.is_finite() || d.usd < 0.0 {
        return Err(AliceError::invalid(
            "a cost must be a finite amount of at least zero",
        ));
    }
    let to_i64 = |n: u64| {
        i64::try_from(n)
            .map_err(|_| AliceError::invalid("a call or token count is too large to record"))
    };
    let (calls, input, output) = (
        to_i64(d.calls)?,
        to_i64(d.input_tokens)?,
        to_i64(d.output_tokens)?,
    );
    conn.execute_batch("SAVEPOINT add_cost")?;
    let result = (|| -> Result<()> {
        let (c0, i0, o0, u0): (i64, i64, i64, f64) = conn
            .query_row(
                "SELECT calls, input_tokens, output_tokens, usd FROM cost_ledger WHERE day = ?1 AND provider = ?2",
                params![d.day, d.provider],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .unwrap_or((0, 0, 0, 0.0));
        let too_big = || AliceError::invalid("the day's cost totals are too large to record");
        let usd = u0 + d.usd;
        if !usd.is_finite() {
            return Err(too_big());
        }
        conn.execute(
            "INSERT OR REPLACE INTO cost_ledger (day, provider, calls, input_tokens, output_tokens, usd) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                d.day,
                d.provider,
                c0.checked_add(calls).ok_or_else(too_big)?,
                i0.checked_add(input).ok_or_else(too_big)?,
                o0.checked_add(output).ok_or_else(too_big)?,
                usd
            ],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("RELEASE add_cost")?,
        Err(_) => conn.execute_batch("ROLLBACK TO add_cost; RELEASE add_cost")?,
    }
    result
}

/// A memory that waits for the judge (spec §5.1 step 5).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingJudgement {
    pub seq: i64,
    pub id: String,
    /// `content_hash` when read: the judgement applies only if the text is unchanged.
    pub stamp: String,
    /// `updated_at` when read (topics and importance are not in the hash): also compared.
    pub updated_at: String,
    pub project_id: String,
    pub project_slug: String,
    pub kind_explicit: bool,
    pub project_defaulted: bool,
    pub attempts: u32,
    /// Failed judgements that were only the judge being unreachable.
    pub unavailable: u32,
}

// Only `pending` rows are ever judged: `skipped` (imported memories) and `retry` (waiting for
// `requeue_retries`) are left alone. `judge_state <> 'done'` repeats the partial index's term so
// SQLite uses `memories_judge`.
// A memory waits for older ones that are still being embedded: it is compared only with older
// memories, so those must be in the index (and judged) first.
const PENDING_SQL: &str = "SELECT m.seq, m.id, m.content_hash, m.updated_at, m.project_id, p.slug, m.kind_explicit, m.project_defaulted, m.judge_attempts, m.judge_unavailable
     FROM memories m JOIN projects p ON p.id = m.project_id
     WHERE m.judge_state <> 'done' AND m.judge_state = 'pending' AND m.embed_state IN ('done', 'failed') AND m.status <> 'trashed'
       AND NOT EXISTS (SELECT 1 FROM memories o WHERE o.seq < m.seq AND o.judge_state <> 'done' AND o.judge_state = 'pending'
                       AND o.embed_state IN ('pending', 'retry') AND o.status <> 'trashed')
     ORDER BY m.seq LIMIT ?1";

/// Memories whose embedding step is over and that wait for the judge, oldest first.
pub fn pending(conn: &Connection, limit: usize) -> Result<Vec<PendingJudgement>> {
    let mut stmt = conn.prepare_cached(PENDING_SQL)?;
    Ok(stmt
        .query_map([limit as i64], |r| {
            Ok(PendingJudgement {
                seq: r.get(0)?,
                id: r.get(1)?,
                stamp: r.get(2)?,
                updated_at: r.get(3)?,
                project_id: r.get(4)?,
                project_slug: r.get(5)?,
                kind_explicit: r.get::<_, i64>(6)? != 0,
                project_defaulted: r.get::<_, i64>(7)? != 0,
                attempts: r.get::<_, i64>(8)?.max(0) as u32,
                unavailable: r.get::<_, i64>(9)?.max(0) as u32,
            })
        })?
        .collect::<std::result::Result<_, _>>()?)
}

/// Memories still waiting for the judge (embedding finished or not).
pub fn judge_backlog(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM memories WHERE judge_state <> 'done' AND judge_state = 'pending' AND status <> 'trashed'",
        [],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

/// The wait before a `retry` memory is judged again after its `failures`-th failed judgement
/// (counted or not): `base` (5 minutes in the daemon), doubling every time, at most 6 hours.
pub fn retry_backoff(base: std::time::Duration, failures: u32) -> std::time::Duration {
    let wait = base.saturating_mul(1u32 << failures.saturating_sub(1).min(10));
    wait.min(std::time::Duration::from_secs(6 * 3600))
}

/// The default `base` of [`retry_backoff`].
pub const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(300);

/// `retry` memories whose time has come (`judge_next_try_at` passed, `now` is an RFC 3339
/// string) become `pending` for another try; those that failed `max_attempts` times stop
/// (`done`). Only `retry` rows change. Returns (requeued, given up).
pub fn requeue_retries(conn: &Connection, max_attempts: u32, now: &str) -> Result<(usize, usize)> {
    let given_up = conn.execute(
        "UPDATE memories SET judge_state = 'done' WHERE judge_state <> 'done' AND judge_state = 'retry' AND judge_attempts >= ?1",
        [max_attempts],
    )?;
    let requeued = conn.execute(
        "UPDATE memories SET judge_state = 'pending' WHERE judge_state <> 'done' AND judge_state = 'retry'
           AND judge_attempts < ?1 AND (judge_next_try_at IS NULL OR judge_next_try_at <= ?2)",
        params![max_attempts, now],
    )?;
    Ok((requeued, given_up))
}

pub fn cost_since(conn: &Connection, day: &str) -> Result<Vec<CostRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT day, provider, calls, input_tokens, output_tokens, usd FROM cost_ledger WHERE day >= ?1 ORDER BY day DESC, provider",
    )?;
    Ok(stmt
        .query_map([day], |r| {
            Ok(CostDelta {
                day: r.get(0)?,
                provider: r.get(1)?,
                calls: r.get::<_, i64>(2)? as u64,
                input_tokens: r.get::<_, i64>(3)? as u64,
                output_tokens: r.get::<_, i64>(4)? as u64,
                usd: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(c: &Connection) -> (i64, i64, i64, f64) {
        c.query_row(
            "SELECT calls, input_tokens, output_tokens, usd FROM cost_ledger WHERE day = '2026-10-20' AND provider = 'jev'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    fn delta(calls: u64, input: u64, usd: f64) -> CostDelta {
        CostDelta {
            day: "2026-10-20".into(),
            provider: "jev".into(),
            calls,
            input_tokens: input,
            output_tokens: 0,
            usd,
        }
    }

    #[test]
    fn add_cost_overflow_on_calls_is_refused_and_leaves_the_row() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        add_cost(&c, &delta(5, 10, 0.5)).unwrap();
        assert!(matches!(
            add_cost(&c, &delta(i64::MAX as u64, 0, 0.0)),
            Err(AliceError::Invalid(_))
        ));
        assert_eq!(row(&c), (5, 10, 0, 0.5));
        add_cost(&c, &delta(1, 1, 0.25)).unwrap();
        assert_eq!(row(&c), (6, 11, 0, 0.75), "usable after a refusal");
    }

    #[test]
    fn add_cost_overflow_on_input_tokens_is_refused_and_leaves_the_row() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        add_cost(&c, &delta(1, 10, 0.5)).unwrap();
        assert!(matches!(
            add_cost(&c, &delta(1, i64::MAX as u64, 0.0)),
            Err(AliceError::Invalid(_))
        ));
        assert_eq!(row(&c), (1, 10, 0, 0.5));
        add_cost(&c, &delta(1, 1, 0.0)).unwrap();
        assert_eq!(row(&c), (2, 11, 0, 0.5));
    }

    #[test]
    fn add_cost_usd_sum_that_is_not_finite_is_refused_and_leaves_the_row() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        add_cost(&c, &delta(1, 1, 1e308)).unwrap();
        assert!(matches!(
            add_cost(&c, &delta(1, 1, 1e308)),
            Err(AliceError::Invalid(_))
        ));
        assert_eq!(row(&c), (1, 1, 0, 1e308));
        add_cost(&c, &delta(1, 1, 0.0)).unwrap();
        assert_eq!(row(&c), (2, 2, 0, 1e308));
    }

    fn memory_with(
        c: &Connection,
        id: &str,
        judge: &str,
        embed: &str,
        status: &str,
        attempts: i64,
    ) {
        c.execute(
            "INSERT INTO memories (id, project_id, kind, title, body, status, content_hash, created_at, updated_at, hlc, judge_state, embed_state, judge_attempts)
             VALUES (?1, '00000000000000000000GLOBAL', 'note', 't', 'b', ?2, ?1,
                     '2026-10-13T00:00:00.000Z', '2026-10-13T00:00:00.000Z', 'h', ?3, ?4, ?5)",
            params![id, status, judge, embed, attempts],
        )
        .unwrap();
    }

    fn state_of(c: &Connection, id: &str) -> String {
        c.query_row(
            "SELECT judge_state FROM memories WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn only_pending_memories_whose_embedding_is_over_are_judged_and_imports_never() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        memory_with(&c, "A-READY", "pending", "done", "pending", 0);
        memory_with(&c, "B-FAILED-EMBED", "pending", "failed", "pending", 0);
        memory_with(&c, "C-EMBEDDING", "pending", "pending", "pending", 0);
        memory_with(&c, "D-IMPORTED", "skipped", "done", "active", 0);
        memory_with(&c, "E-DONE", "done", "done", "active", 0);
        memory_with(&c, "F-RETRY", "retry", "done", "active", 1);
        memory_with(&c, "G-TRASHED", "pending", "done", "trashed", 0);
        let ids: Vec<String> = pending(&c, 10).unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["A-READY", "B-FAILED-EMBED"]);
        let first = &pending(&c, 1).unwrap()[0];
        assert_eq!(
            (
                first.stamp.as_str(),
                first.project_slug.as_str(),
                first.kind_explicit,
                first.project_defaulted,
                first.attempts
            ),
            ("A-READY", "global", true, false, 0)
        );
        // The backlog counts what the judge will do (the embedding step may still run).
        assert_eq!(
            judge_backlog(&c).unwrap(),
            3,
            "A, B and C wait; imports, retries and the trashed do not"
        );
    }

    #[test]
    fn retries_are_requeued_until_the_attempts_run_out_and_other_states_are_untouched() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        memory_with(&c, "R1", "retry", "done", "active", 1);
        memory_with(&c, "R3", "retry", "done", "active", 3);
        memory_with(&c, "SKIP", "skipped", "done", "active", 9);
        memory_with(&c, "DONE", "done", "done", "active", 0);
        memory_with(&c, "PEND", "pending", "done", "pending", 0);
        assert_eq!(
            requeue_retries(&c, 3, "2099-01-01T00:00:00.000Z").unwrap(),
            (1, 1)
        );
        assert_eq!(state_of(&c, "R1"), "pending");
        assert_eq!(state_of(&c, "R3"), "done", "given up after three attempts");
        assert_eq!(state_of(&c, "SKIP"), "skipped", "imports keep their state");
        assert_eq!(state_of(&c, "DONE"), "done");
        assert_eq!(state_of(&c, "PEND"), "pending");
        assert_eq!(
            requeue_retries(&c, 3, "2099-01-01T00:00:00.000Z").unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn a_retry_waits_for_its_time_and_the_wait_doubles_up_to_six_hours() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        memory_with(&c, "LATER", "retry", "done", "active", 1);
        memory_with(&c, "DUE", "retry", "done", "active", 1);
        memory_with(&c, "UNSET", "retry", "done", "active", 1);
        c.execute(
            "UPDATE memories SET judge_next_try_at = '2026-10-14T12:00:00.000Z' WHERE id = 'LATER'",
            [],
        )
        .unwrap();
        c.execute(
            "UPDATE memories SET judge_next_try_at = '2026-10-14T10:00:00.000Z' WHERE id = 'DUE'",
            [],
        )
        .unwrap();
        assert_eq!(
            requeue_retries(&c, 5, "2026-10-14T11:00:00.000Z").unwrap(),
            (2, 0)
        );
        assert_eq!(state_of(&c, "LATER"), "retry");
        assert_eq!(state_of(&c, "DUE"), "pending");
        assert_eq!(state_of(&c, "UNSET"), "pending");
        let secs: Vec<u64> = (1..=14)
            .map(|n| retry_backoff(RETRY_BASE, n).as_secs())
            .collect();
        assert_eq!(&secs[..3], &[300, 600, 1200], "5 minutes, doubling");
        assert!(
            secs.iter().all(|s| *s <= 6 * 3600) && secs[13] == 6 * 3600,
            "{secs:?}"
        );
    }

    #[test]
    fn the_queue_query_uses_the_partial_index_for_both_the_rows_and_the_older_check() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let plan: Vec<String> = c
            .prepare(&format!("EXPLAIN QUERY PLAN {PENDING_SQL}"))
            .unwrap()
            .query_map([10], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let plan = plan.join("\n");
        assert!(plan.matches("memories_judge").count() >= 2, "{plan}");
    }

    #[test]
    fn a_memory_waits_while_an_older_one_is_still_being_embedded() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        memory_with(&c, "A-OLD", "pending", "pending", "pending", 0);
        memory_with(&c, "B-NEW", "pending", "done", "pending", 0);
        assert!(
            pending(&c, 10).unwrap().is_empty(),
            "B would miss A as a neighbour"
        );
        c.execute(
            "UPDATE memories SET embed_state = 'done' WHERE id = 'A-OLD'",
            [],
        )
        .unwrap();
        let ids: Vec<String> = pending(&c, 10).unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["A-OLD", "B-NEW"], "oldest first");
    }

    #[test]
    fn cache_and_costs() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        assert_eq!(cache_get(&c, "h").unwrap(), None);
        cache_put(&c, "h", "jev", "jev-1.13.0", "{\"answers\":{}}").unwrap();
        cache_put(&c, "h", "jev", "jev-1.13.0", "{\"answers\":{\"q\":1}}").unwrap();
        assert_eq!(
            cache_get(&c, "h").unwrap().as_deref(),
            Some("{\"answers\":{\"q\":1}}")
        );
        for _ in 0..3 {
            add_cost(
                &c,
                &CostDelta {
                    day: "2026-10-13".into(),
                    provider: "jev".into(),
                    calls: 1,
                    input_tokens: 400,
                    output_tokens: 0,
                    usd: 0.0000168,
                },
            )
            .unwrap();
        }
        assert!((spent(&c, "2026-10-13", "jev").unwrap() - 0.0000504).abs() < 1e-12);
        assert_eq!(spent(&c, "2026-10-14", "jev").unwrap(), 0.0);
        let rows = cost_since(&c, "2026-10-01").unwrap();
        let bad = |usd: f64, calls: u64| CostDelta {
            day: "2026-10-13".into(),
            provider: "jev".into(),
            calls,
            input_tokens: 0,
            output_tokens: 0,
            usd,
        };
        for d in [
            bad(-1.0, 1),
            bad(f64::NAN, 1),
            bad(f64::INFINITY, 1),
            bad(0.0, u64::MAX),
        ] {
            assert!(
                matches!(add_cost(&c, &d), Err(AliceError::Invalid(_))),
                "{d:?}"
            );
        }
        assert_eq!(
            cost_since(&c, "2026-10-01").unwrap()[0].calls,
            3,
            "nothing was written"
        );
        cache_put(&c, "h", "heuristic", "m2", "{}").unwrap();
        let (p, m): (String, String) = c
            .query_row(
                "SELECT provider, model FROM judge_cache WHERE request_hash = 'h'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((p.as_str(), m.as_str()), ("heuristic", "m2"));
        assert_eq!(
            (rows.len(), rows[0].calls, rows[0].input_tokens),
            (1, 3, 1_200)
        );
    }
}
