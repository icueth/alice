//! The writer's own WAL hygiene (spec §4.1), moved out of `writer.rs`.

use rusqlite::Connection;
use std::time::{Duration, Instant};

/// The writer's own WAL hygiene (spec §4.1). SQLite's automatic checkpoint is PASSIVE: it
/// copies frames home but never restarts the log, and the log restarts only when no reader
/// is still using it. Under constant readers (a large import while clients recall) that
/// moment never comes and the `-wal` file grows without bound (364 MiB at a 30k-chunk
/// import, measured in `docs/benchmarks/m2.md`), which makes every read walk a longer
/// wal-index. So, between its own transactions and on its own connection, the writer
/// restarts the log once the file is over [`WalPolicy::limit`].
#[derive(Debug, Clone)]
pub struct WalPolicy {
    /// A `-wal` file larger than this is restarted; `journal_size_limit` truncates a
    /// restarted log back to it.
    pub limit: u64,
    /// At most one restart attempt per this long.
    pub min_gap: Duration,
    /// How long one attempt waits for readers that began before it (total, not per reader).
    pub busy_wait: Duration,
}

impl Default for WalPolicy {
    fn default() -> Self {
        Self {
            limit: 64 * 1024 * 1024,
            min_gap: Duration::from_secs(2),
            busy_wait: Duration::from_millis(100),
        }
    }
}

/// What one look at the log came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalCheck {
    /// Under the limit, or tried less than `min_gap` ago.
    NotDue,
    /// The log was restarted: the next write begins at its start.
    Restarted,
    /// A reader still used the log, or the checkpoint failed; the next opportunity retries.
    Skipped,
}

/// The writer's checkpoint bookkeeping.
pub(crate) struct WalKeeper {
    policy: WalPolicy,
    wal: std::path::PathBuf,
    last_try: Option<Instant>,
}

impl WalKeeper {
    /// Applies `journal_size_limit` to `conn` and remembers where its log lives.
    pub(crate) fn new(conn: &Connection, policy: WalPolicy) -> Self {
        let limit = i64::try_from(policy.limit).unwrap_or(i64::MAX);
        if let Err(e) = conn.execute_batch(&format!("PRAGMA journal_size_limit = {limit};")) {
            tracing::warn!(error = %e, "journal_size_limit was not set");
        }
        let mut wal = conn
            .path()
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .into_os_string();
        wal.push("-wal");
        Self {
            policy,
            wal: wal.into(),
            last_try: None,
        }
    }

    /// Restart the log if it is over the limit and the last attempt is old enough. Call it
    /// only between transactions. It waits for readers at most `busy_wait` and never fails.
    pub(crate) fn check(&mut self, conn: &Connection, now: Instant) -> WalCheck {
        if self
            .last_try
            .is_some_and(|t| now.duration_since(t) < self.policy.min_gap)
        {
            return WalCheck::NotDue;
        }
        let len = std::fs::metadata(&self.wal).map(|m| m.len()).unwrap_or(0);
        if len <= self.policy.limit {
            return WalCheck::NotDue;
        }
        self.last_try = Some(now);
        let _ = conn.busy_timeout(self.policy.busy_wait);
        // (busy, frames in the log, frames checkpointed); busy = 1: a reader kept it from finishing.
        let r = conn.query_row("PRAGMA wal_checkpoint(RESTART)", [], |r| r.get::<_, i64>(0));
        let _ = conn.busy_timeout(alice_core::store::BUSY_TIMEOUT);
        match r {
            Ok(0) => WalCheck::Restarted,
            Ok(_) => WalCheck::Skipped,
            Err(e) => {
                tracing::debug!(error = %e, "wal restart skipped");
                WalCheck::Skipped
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn wal_bytes(db: &Path) -> u64 {
        let mut p = db.as_os_str().to_owned();
        p.push("-wal");
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    }

    #[test]
    fn the_keeper_reports_what_it_did() {
        let d = tempfile::tempdir().unwrap();
        let conn = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
        let policy = WalPolicy {
            limit: 1,
            min_gap: Duration::from_secs(2),
            busy_wait: Duration::from_millis(50),
        };
        let mut keeper = WalKeeper::new(&conn, policy);
        let t0 = Instant::now();
        conn.execute_batch("INSERT INTO meta (key, value) VALUES ('k', 'v')")
            .unwrap();
        assert!(wal_bytes(&d.path().join("a.db")) > 1);
        // A reader pinned to the log: the attempt is skipped.
        let held = alice_core::store::open_reader(&d.path().join("a.db")).unwrap();
        held.execute_batch("BEGIN").unwrap();
        let _: i64 = held
            .query_row("SELECT count(*) FROM meta", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch("INSERT INTO meta (key, value) VALUES ('k2', 'v')")
            .unwrap();
        assert_eq!(keeper.check(&conn, t0), WalCheck::Skipped);
        // Tried a moment ago: not due, however large the log is.
        assert_eq!(
            keeper.check(&conn, t0 + Duration::from_secs(1)),
            WalCheck::NotDue
        );
        held.execute_batch("COMMIT").unwrap();
        assert_eq!(
            keeper.check(&conn, t0 + Duration::from_secs(3)),
            WalCheck::Restarted
        );
        // The writer's own transactions are still fine afterwards.
        conn.execute_batch("INSERT INTO meta (key, value) VALUES ('k3', 'v')")
            .unwrap();
    }
}
