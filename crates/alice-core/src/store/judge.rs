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
