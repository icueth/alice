//! Judge cache (by request hash) and the per-day cost ledger (spec §3.3, §5.2).
//!
//! The cache keeps only the request hash and the provider's response JSON, never the request
//! text; the ledger keeps only counts and spend.

use crate::error::Result;
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
         ON CONFLICT(request_hash) DO UPDATE SET response_json = excluded.response_json, created_at = excluded.created_at",
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

pub fn add_cost(conn: &Connection, d: &CostDelta) -> Result<()> {
    conn.execute(
        "INSERT INTO cost_ledger (day, provider, calls, input_tokens, output_tokens, usd) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(day, provider) DO UPDATE SET calls = calls + excluded.calls,
           input_tokens = input_tokens + excluded.input_tokens,
           output_tokens = output_tokens + excluded.output_tokens, usd = usd + excluded.usd",
        params![
            d.day,
            d.provider,
            d.calls as i64,
            d.input_tokens as i64,
            d.output_tokens as i64,
            d.usd
        ],
    )?;
    Ok(())
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
        assert_eq!(
            (rows.len(), rows[0].calls, rows[0].input_tokens),
            (1, 3, 1_200)
        );
    }
}
