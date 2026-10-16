//! Importing from other memory systems (MemPalace first).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod chroma;
pub mod kg;
pub mod palace;
pub mod plan;
pub mod report;
pub mod wings;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;

pub use plan::{NoFilter, Planner, Survey, TextFilter, TierCounts, survey};
pub use report::ImportReport;

/// The machine's current UTC offset (MemPalace writes naive local times).
pub fn local_offset() -> chrono::FixedOffset {
    use chrono::Offset;
    chrono::Local::now().offset().fix()
}

/// What `POST /api/v1/import/mempalace` takes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ImportRequest {
    /// The palace folder. The CLI always sends an absolute path; `None` asks the daemon to use
    /// the palace under its configured MemPalace home (and fails when it has none).
    #[serde(default)]
    pub palace: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    /// `YYYY-MM-DD` or RFC 3339: only drawers filed at or after it.
    #[serde(default)]
    pub since: Option<String>,
    /// Wing -> project overrides.
    #[serde(default)]
    pub map_wing: BTreeMap<String, String>,
    /// The collection that holds the drawers (default `mempalace_drawers`).
    #[serde(default)]
    pub collection: Option<String>,
    /// Allow queueing every chunk for a remote embedding provider.
    #[serde(default)]
    pub allow_remote_embedding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImportState {
    Idle,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImportStatus {
    pub state: ImportState,
    pub job_id: Option<String>,
    pub phase: String,
    /// Drawers handled so far, out of `total`.
    pub done: u64,
    pub total: u64,
    pub report: Option<ImportReport>,
    pub error: Option<String>,
    /// Writer batches the job has had committed so far.
    #[serde(default)]
    pub batches: u64,
    /// The largest batch so far, in records and in bytes of text (a batch closes at 50 records
    /// or 16 KB of text, the importer's `BATCH_RECORDS` and `BATCH_BYTES`, so a short writer
    /// slice is visible here).
    #[serde(default)]
    pub max_batch_records: u64,
    #[serde(default)]
    pub max_batch_bytes: u64,
    /// The largest size of the write-ahead log file seen at the job's checks (every 20 batches).
    #[serde(default)]
    pub max_wal_bytes: u64,
}

impl ImportStatus {
    pub fn idle() -> Self {
        Self {
            state: ImportState::Idle,
            job_id: None,
            phase: String::new(),
            done: 0,
            total: 0,
            report: None,
            error: None,
            batches: 0,
            max_batch_records: 0,
            max_batch_bytes: 0,
            max_wal_bytes: 0,
        }
    }
}
