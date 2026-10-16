//! Maintenance (spec §5.6): jobs run on a low-priority thread when the daemon has had no
//! requests for `maintenance.idle_secs`, in short slices, and stop between slices as soon as a
//! request arrives (the next idle period continues them). Last runs live in `meta`.
//!
//! # What the jobs do and do not remove
//!
//! - **Trash purge** deletes a trashed memory (30 days by default) with its topics, links,
//!   mentions, provenance, FTS rows, vectors and the snapshots of its history. It does *not*
//!   remove: the words of its FTS rows until the weekly merge has rewritten the segments;
//!   its text in backups (up to the oldest kept: 7 daily + 4 weekly, so about 3 weeks) and in
//!   pre-migration / before-restore copies (30 days); relation literals of facts that
//!   mention it (a fact is not a copy of the memory); and a database file that has not been
//!   vacuumed (freed pages are reused, the file does not shrink).
//! - **Raw retention** by age counts the time a chunk was *stored in Alice*, not the time it
//!   claims to have been written: raw data younger than the retention is never deleted. By size
//!   it counts text bytes of the chunks; neither mode shrinks the file without a `VACUUM`.
//! - **Capture retention** (`capture.raw_retention`) applies the same rules to the chunks of
//!   capture sources only. A capture source left without chunks (by either retention) stays,
//!   with its cursor and turn keys, so its transcript is not captured or imported again.
//!
//! Each job keeps the writer's transactions short: deletes go 500 rows per `WriteOp`, FTS merges
//! 500 pages. Reads (what to delete, the checks, the backup) use the reader pool or their own
//! read-only connection, never the writer's.

use crate::backup::{self, BackupKind};
use crate::config::SettingsHandle;
use crate::paths::AliceHome;
use crate::service::{ActiveSlot, IndexSlot, snapshot};
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};
use alice_core::error::{AliceError, Result};
use alice_core::settings::{MaintenanceSettings, Retention};
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::{ReaderPool, maintenance as m, reviews};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// When requests last came, and how many are running.
pub struct Activity {
    epoch: Instant,
    last_ms: AtomicI64,
    inflight: AtomicUsize,
}

/// Held for as long as something counts as activity (a request, a long import): maintenance
/// waits until every guard is dropped and the idle period has run its course.
pub struct ActivityGuard<'a>(&'a Activity);

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

/// An [`ActivityGuard`] that owns its share of the activity record, for a job that runs on its
/// own thread (an import) and so cannot borrow it.
pub struct OwnedActivityGuard(Arc<Activity>);

impl Drop for OwnedActivityGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: Instant::now(),
            last_ms: AtomicI64::new(0),
            inflight: AtomicUsize::new(0),
        })
    }

    fn touch(&self) {
        self.last_ms
            .store(self.epoch.elapsed().as_millis() as i64, Ordering::SeqCst);
    }

    /// Held for the length of a request.
    pub fn enter(&self) -> ActivityGuard<'_> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        ActivityGuard(self)
    }

    /// Like [`enter`](Self::enter), for a guard that moves to another thread.
    pub fn enter_owned(self: &Arc<Self>) -> OwnedActivityGuard {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        OwnedActivityGuard(self.clone())
    }

    /// A guard is held right now (a request or a long job is running).
    pub fn is_busy(&self) -> bool {
        self.inflight.load(Ordering::SeqCst) > 0
    }

    /// How long nothing has happened (zero while a guard is held).
    pub fn idle_for(&self) -> Duration {
        if self.inflight.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        let now = self.epoch.elapsed().as_millis() as i64;
        Duration::from_millis((now - self.last_ms.load(Ordering::SeqCst)).max(0) as u64)
    }

    /// A request started or ran since `since` (an `epoch`-relative mark from `mark`).
    pub fn active_since(&self, since: i64) -> bool {
        self.inflight.load(Ordering::SeqCst) > 0 || self.last_ms.load(Ordering::SeqCst) > since
    }

    pub fn mark(&self) -> i64 {
        self.epoch.elapsed().as_millis() as i64
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Job {
    Backup,
    QuickCheck,
    IndexCheck,
    FtsOptimize,
    RawRetention,
    /// `capture.raw_retention`: ages out the chunks of capture sources only.
    CaptureRetention,
    TrashPurge,
    JudgeRetry,
    MergeSuggestions,
    EventsPrune,
    /// Delete the vectors and index files of embedding models that were switched away from.
    ModelPurge,
}

impl Job {
    pub const ALL: [Job; 11] = [
        Job::ModelPurge,
        Job::JudgeRetry,
        Job::TrashPurge,
        Job::RawRetention,
        Job::CaptureRetention,
        Job::EventsPrune,
        Job::QuickCheck,
        Job::IndexCheck,
        Job::Backup,
        Job::FtsOptimize,
        Job::MergeSuggestions,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Job::Backup => "backup",
            Job::QuickCheck => "quick_check",
            Job::IndexCheck => "index_check",
            Job::FtsOptimize => "fts_optimize",
            Job::RawRetention => "raw_retention",
            Job::CaptureRetention => "capture_retention",
            Job::TrashPurge => "trash_purge",
            Job::JudgeRetry => "judge_retry",
            Job::MergeSuggestions => "merge_suggestions",
            Job::EventsPrune => "events_prune",
            Job::ModelPurge => "model_purge",
        }
    }

    pub fn parse(s: &str) -> Option<Job> {
        Job::ALL.into_iter().find(|j| j.name() == s)
    }

    /// `parse`, or an error that names the jobs.
    pub fn parse_or_err(s: &str) -> Result<Job> {
        Job::parse(s.trim()).ok_or_else(|| {
            AliceError::invalid(format!(
                "unknown maintenance job \"{s}\"; use one of: {}",
                Job::names().join(", ")
            ))
        })
    }

    pub fn names() -> Vec<&'static str> {
        Job::ALL.iter().map(|j| j.name()).collect()
    }

    /// How often the job is due.
    pub fn every(self) -> Duration {
        const H: u64 = 3_600;
        Duration::from_secs(match self {
            Job::JudgeRetry => H,
            Job::FtsOptimize | Job::MergeSuggestions => 7 * 24 * H,
            _ => 24 * H,
        })
    }
}

/// A job that failed is due again after this long (or its period, if shorter), so a backup
/// that failed does not wait a day.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(3_600);
/// A job that failed for want of disk space waits longer: an hour later there is no more room.
const RETRY_AFTER_NO_SPACE: Duration = Duration::from_secs(12 * 3_600);
pub use crate::backup::DISK_SPACE;
/// Once the newest backup is older than this many backup periods, the daemon warns (health,
/// `alice status`) and starts jobs after `OVERDUE_IDLE_SECS` of quiet instead of `idle_secs`.
const OVERDUE_PERIODS: u32 = 2;
const OVERDUE_IDLE_SECS: u64 = 10;
/// A writer slice stops after about this long (the rows it was given may not all be done).
pub const SLICE_BUDGET: Duration = Duration::from_millis(25);

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Done,
    /// A request arrived (or the daemon is stopping); the job continues in the next idle period.
    Paused,
    /// There was nothing it could check now (the index is being rebuilt, say): not a run, not
    /// an "ok"; it is tried again at the next quiet moment.
    Skipped,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct JobReport {
    pub job: Job,
    pub outcome: JobOutcome,
    pub detail: String,
    pub took_ms: u64,
}

/// Free bytes on the volume of a path (`None`: unknown).
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

pub struct MaintenanceCtx {
    pub home: AliceHome,
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    /// The embedding model in force (the index holds its vectors) and its embedder; follows a
    /// model switch.
    pub active: Arc<ActiveSlot>,
    pub settings: Arc<SettingsHandle>,
    pub activity: Arc<Activity>,
    pub judge_wake: crossbeam_channel::Sender<()>,
    /// Set when the daemon is stopping: a job ends at its next slice.
    pub stop: Arc<AtomicBool>,
    /// Free bytes where a backup would go (`backup::free_space`; tests replace it).
    pub free_space: FreeSpace,
}

pub struct Scheduler {
    ctx: MaintenanceCtx,
    /// One job runs at a time, whoever started it (the timer, `alice maintenance run`).
    running: parking_lot::Mutex<()>,
    /// The connection of the integrity check in progress, so a stopping daemon can interrupt it.
    interrupt: parking_lot::Mutex<Option<rusqlite::InterruptHandle>>,
    /// An interrupt was sent to the check in progress. SQLite reports it as `SQLITE_INTERRUPT`
    /// inside the check, but as a plain "vtable constructor failed" when it lands while the
    /// check is still setting up the full-text tables: either way the check was stopped, not
    /// failed.
    interrupted: std::sync::atomic::AtomicBool,
    /// Tests only: runs on the integrity check's connection once its interrupt handle is
    /// registered and before the check starts.
    #[cfg(test)]
    quick_check_hook: parking_lot::Mutex<Option<QuickCheckHook>>,
}

/// A test's hook into the integrity check (see `Scheduler::quick_check_hook`).
#[cfg(test)]
type QuickCheckHook = Arc<dyn Fn(&rusqlite::Connection) + Send + Sync>;

/// Rows per write in purges and retention.
const SLICE: usize = 500;
/// Memories looked at per slice of the merge-suggestion job.
const MERGE_PAGE: i64 = 200;
/// The job adds no `merge` item while this many are open, and never more than fit under it, so
/// one run cannot flood the inbox.
const MERGE_OPEN_MAX: u64 = 50;
/// The most memories one `merge` item groups.
const MERGE_GROUP_MAX: usize = 5;

/// Two memories are only suggested as duplicates when their words agree this much.
const MERGE_TEXT_JACCARD: f64 = 0.6;

type TextSet = std::collections::HashSet<String>;

/// A letter for text comparison: alphabetic, or a Thai letter, vowel or tone mark (tone marks
/// are not "alphabetic" to Unicode, and would otherwise cut a Thai word apart).
fn is_text_letter(c: char) -> bool {
    c.is_alphabetic() || ((0x0E01..=0x0E4E).contains(&(c as u32)) && c as u32 != 0x0E3F)
}

fn is_thai(c: char) -> bool {
    (0x0E00..=0x0E7F).contains(&(c as u32))
}

/// The comparison set of a memory's title and body: lower-case runs of letters, so numbers,
/// dates, times, hashes and ids (any run with a digit) are dropped. A run with Thai letters,
/// which has no word spaces, contributes its character trigrams instead of the run.
fn text_tokens(title: &str, body: &str) -> TextSet {
    let mut out = std::collections::HashSet::new();
    let text = format!("{title} {body}").to_lowercase();
    // Split at anything that is neither a letter nor a digit; a run holding a digit is dropped
    // whole (`m3`, `abc1234`, `21`), which is what removes ids and hashes.
    for run in text.split(|c: char| !(is_text_letter(c) || c.is_numeric())) {
        if run.is_empty() || run.chars().any(|c| c.is_numeric()) {
            continue;
        }
        if run.chars().any(is_thai) {
            let chars: Vec<char> = run.chars().collect();
            if chars.len() < 3 {
                out.insert(run.to_string());
            } else {
                out.extend(chars.windows(3).map(|w| w.iter().collect::<String>()));
            }
        } else {
            out.insert(run.to_string());
        }
    }
    out
}

/// The numbers of a memory's title and body as written, sorted (a multiset): runs of digits,
/// joined across `.`, `,`, `:`, `/` and `-` between digits, so a port (`5432`), a version
/// (`1.4.2`), an amount (`1,000`), a date (`2026-10-16`) and a time (`21:30`) are one token
/// each. Two memories whose numbers differ are a value update, never a duplicate.
fn number_tokens(title: &str, body: &str) -> Vec<String> {
    let text: Vec<char> = format!("{title} {body}").chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if !text[i].is_numeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < text.len()
            && (text[i].is_numeric()
                || (matches!(text[i], '.' | ',' | ':' | '/' | '-')
                    && text.get(i + 1).is_some_and(|c| c.is_numeric())
                    && i > start))
        {
            i += 1;
        }
        out.push(text[start..i].iter().collect());
    }
    out.sort();
    out
}

/// What a merge suggestion compares of a memory's text: its words, and its numbers.
struct MergeText {
    words: TextSet,
    numbers: Vec<String>,
}

fn merge_text(title: &str, body: &str) -> MergeText {
    MergeText {
        words: text_tokens(title, body),
        numbers: number_tokens(title, body),
    }
}

/// Jaccard similarity of two token sets (0 when either is empty: no text, no agreement).
fn jaccard(a: &TextSet, b: &TextSet) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    inter / (a.len() as f64 + b.len() as f64 - inter)
}

/// Cosine similarity of two vectors (0 for a zero vector or sizes that differ).
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let d = norm(a) * norm(b);
    if d == 0.0 { 0.0 } else { dot / d }
}

enum Step {
    More,
    Finished(String),
    /// Nothing to do now; not a run (see [`JobOutcome::Skipped`]).
    Skipped(String),
    /// Stopped for the daemon's sake (see [`JobOutcome::Paused`]).
    Stopped(String),
}

/// What a job carries from one slice to the next within a run.
#[derive(Default)]
struct Scratch {
    /// Raw retention by size: bytes still to delete.
    over_bytes: Option<u64>,
    deleted: usize,
    deleted_other: usize,
    /// Raw retention: the setting that applied, for the run's event.
    reason: String,
}

/// The chunks a raw retention job ages out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawScope {
    /// Every raw chunk (`storage.raw_retention`).
    All,
    /// The chunks of capture sources only (`capture.raw_retention`, spec §3.9): imported
    /// transcripts and other raw sources are never aged out by it.
    Capture,
}

impl RawScope {
    /// The chunks of the scope stored before `cutoff`, oldest first, a slice at most.
    fn older_than(self, c: &rusqlite::Connection, cutoff: &str) -> Result<Vec<i64>> {
        match self {
            RawScope::All => m::chunks_older_than(c, cutoff, SLICE),
            RawScope::Capture => m::capture_chunks_older_than(c, cutoff, SLICE),
        }
    }

    /// The text bytes of the scope's chunks.
    fn bytes(self, c: &rusqlite::Connection) -> Result<u64> {
        match self {
            RawScope::All => m::raw_bytes(c),
            RawScope::Capture => m::capture_raw_bytes(c),
        }
    }

    /// (seq, text bytes) of the scope's oldest chunks, a slice at most.
    fn oldest(self, c: &rusqlite::Connection) -> Result<Vec<(i64, u64)>> {
        match self {
            RawScope::All => m::oldest_chunks(c, SLICE),
            RawScope::Capture => m::oldest_capture_chunks(c, SLICE),
        }
    }
}

impl Scheduler {
    pub fn new(ctx: MaintenanceCtx) -> Self {
        Self {
            ctx,
            running: parking_lot::Mutex::new(()),
            interrupt: parking_lot::Mutex::new(None),
            interrupted: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            quick_check_hook: parking_lot::Mutex::new(None),
        }
    }

    /// Interrupt the integrity check, if one runs (the daemon is stopping). SQLite honours the
    /// interrupt inside `PRAGMA quick_check`: the check returns `SQLITE_INTERRUPT` at once.
    pub fn interrupt_running(&self) {
        if let Some(h) = self.interrupt.lock().as_ref() {
            self.interrupted.store(true, Ordering::SeqCst);
            h.interrupt();
        }
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.ctx
            .readers
            .with(|c| m::get_meta(c, key))
            .ok()
            .flatten()
    }

    /// Committed before it returns, so `due` sees it at once.
    fn set_meta(&self, key: &str, value: &str) {
        if let Err(e) = self.ctx.writer.submit_blocking(WriteOp::SetMeta {
            key: key.into(),
            value: value.into(),
        }) {
            tracing::warn!(error = %e, key, "recording a maintenance run failed");
        }
    }

    fn parse_time(&self, key: &str) -> Option<DateTime<Utc>> {
        self.meta(key)
            .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.with_timezone(&Utc))
    }

    /// Jobs whose period has passed since they last ran (never run: due). A job whose last run
    /// failed is due again after an hour at the latest.
    pub fn due(&self, now: DateTime<Utc>) -> Vec<Job> {
        Job::ALL
            .into_iter()
            .filter(|j| {
                let Some(last) = self.parse_time(&format!("maint.{}.last_run", j.name())) else {
                    return true;
                };
                let failed = self
                    .parse_time(&format!("maint.{}.last_ok", j.name()))
                    .is_none_or(|ok| ok < last);
                let no_space = failed
                    && self
                        .meta(&format!("maint.{}.last_error", j.name()))
                        .and_then(|e| e.split_once(' ').map(|(_, what)| what.to_string()))
                        .is_some_and(|what| what.starts_with(DISK_SPACE));
                let every = if no_space {
                    j.every().min(RETRY_AFTER_NO_SPACE)
                } else if failed {
                    j.every().min(RETRY_AFTER_FAILURE)
                } else {
                    j.every()
                };
                now - last >= chrono::Duration::from_std(every).unwrap_or_default()
            })
            .collect()
    }

    /// Quiet for `idle_secs` (10 s once a backup is overdue, so a busy machine still gets one).
    pub fn idle_enough(&self, s: &MaintenanceSettings) -> bool {
        s.enabled && self.ctx.activity.idle_for() >= Duration::from_secs(self.quiet_secs(s))
    }

    /// The quiet period jobs wait for: `idle_secs`, or 10 s while a backup is overdue.
    pub fn quiet_secs(&self, s: &MaintenanceSettings) -> u64 {
        if self.backup_warning(Utc::now()).is_some() {
            s.idle_secs.min(OVERDUE_IDLE_SECS)
        } else {
            s.idle_secs
        }
    }

    /// Why the backups are behind, when the newest is older than two backup periods (or none was
    /// ever made in a data directory that old). `None`: they are up to date.
    pub fn backup_warning(&self, now: DateTime<Utc>) -> Option<String> {
        backup_warning(
            &self.ctx.readers,
            &backup::backup_dir(&self.ctx.home, &self.ctx.settings.get().backup),
            now,
        )
    }

    /// Run `job` in slices. Without `force` it stops (Paused) when a request arrives; `force`
    /// (the CLI's `maintenance run`) runs it to the end. A stopping daemon ends either.
    pub fn run_job(&self, job: Job, now: DateTime<Utc>, force: bool) -> JobReport {
        let _one_at_a_time = self.running.lock();
        let started = Instant::now();
        let mark = self.ctx.activity.mark();
        let mut outcome = JobOutcome::Done;
        let detail;
        let mut scratch = Scratch::default();
        loop {
            // A panic in a job is that run's failure, not the end of the scheduler thread.
            let stepped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.step(job, now, &mut scratch)
            }))
            .unwrap_or_else(|p| {
                let what = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                Err(AliceError::Other(format!("the job panicked: {what}")))
            });
            match stepped {
                Ok(Step::Finished(d)) => {
                    detail = d;
                    break;
                }
                Ok(Step::Skipped(d)) => {
                    outcome = JobOutcome::Skipped;
                    detail = d;
                    break;
                }
                Ok(Step::Stopped(d)) => {
                    outcome = JobOutcome::Paused;
                    detail = d;
                    break;
                }
                Ok(Step::More) => {
                    let stopping = self.ctx.stop.load(Ordering::Relaxed);
                    if stopping || (!force && self.ctx.activity.active_since(mark)) {
                        outcome = JobOutcome::Paused;
                        detail = if stopping {
                            "paused: the daemon is stopping".into()
                        } else {
                            "paused: a request arrived".into()
                        };
                        break;
                    }
                }
                Err(e) => {
                    outcome = JobOutcome::Failed;
                    detail = e.to_string();
                    break;
                }
            }
        }
        // One history row per run with the totals (not one per slice: a run is hundreds).
        if matches!(job, Job::RawRetention | Job::CaptureRetention) && scratch.deleted > 0 {
            let recorded = self.ctx.writer.submit_blocking(WriteOp::RecordEvent {
                actor: alice_core::store::events::Actor::System,
                action: "raw.retention".into(),
                target_type: "raw".into(),
                target_id: "chunks".into(),
                before: None,
                after: Some(
                    serde_json::json!({ "deleted": scratch.deleted, "reason": scratch.reason }),
                ),
            });
            if let Err(e) = recorded {
                tracing::warn!(error = %e, "recording the raw retention run failed");
            }
        }
        let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if !matches!(outcome, JobOutcome::Paused | JobOutcome::Skipped) {
            self.set_meta(&format!("maint.{}.last_run", job.name()), &stamp);
        }
        match outcome {
            JobOutcome::Done => self.set_meta(&format!("maint.{}.last_ok", job.name()), &stamp),
            JobOutcome::Failed => {
                tracing::warn!(job = job.name(), error = %detail, "maintenance job failed");
                self.set_meta(
                    &format!("maint.{}.last_error", job.name()),
                    &format!("{stamp} {detail}"),
                );
            }
            JobOutcome::Paused | JobOutcome::Skipped => {}
        }
        JobReport {
            job,
            outcome,
            detail,
            took_ms: started.elapsed().as_millis() as u64,
        }
    }

    fn step(&self, job: Job, now: DateTime<Utc>, sc: &mut Scratch) -> Result<Step> {
        let s = self.ctx.settings.get();
        let w = &self.ctx.writer;
        let ts = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        match job {
            Job::TrashPurge => {
                let cutoff = ts(now - chrono::Duration::days(i64::from(s.maintenance.trash_days)));
                let seqs = self.ctx.readers.with(|c| m::trash_due(c, &cutoff, SLICE))?;
                if seqs.is_empty() {
                    return Ok(Step::Finished(format!("{} purged", sc.deleted)));
                }
                // The writer re-checks the cutoff and does as many as fit in its time box; the
                // rest are found again by the next look.
                if let WriteOutcome::Purged(done) =
                    w.submit_blocking(WriteOp::PurgeMemories { seqs, cutoff })?
                {
                    sc.deleted += done.len();
                    if let Some(idx) = self.ctx.index.read().clone() {
                        for seq in &done {
                            let _ = idx.remove(VecKey::Memory(*seq));
                        }
                    }
                }
                Ok(Step::More)
            }
            Job::RawRetention => {
                self.raw_retention(now, sc, s.storage.raw_retention, RawScope::All)
            }
            Job::EventsPrune => {
                let before = ts(now - chrono::Duration::days(i64::from(s.maintenance.events_days)));
                if let WriteOutcome::Count(n) = w.submit_blocking(WriteOp::PruneEvents {
                    before: before.clone(),
                    limit: SLICE,
                })? && n > 0
                {
                    sc.deleted += n;
                    return Ok(Step::More);
                }
                // The judge cache goes by the same age, and by a cap on its size.
                if let WriteOutcome::Count(n) = w.submit_blocking(WriteOp::PruneJudgeCache {
                    before,
                    keep: m::JUDGE_CACHE_KEEP,
                    limit: SLICE,
                })? && n > 0
                {
                    sc.deleted_other += n;
                    return Ok(Step::More);
                }
                Ok(Step::Finished(format!(
                    "{} events and {} judge-cache entries pruned",
                    sc.deleted, sc.deleted_other
                )))
            }
            Job::JudgeRetry => {
                // Only memories in `retry` come back; imports (`skipped`) are never judged.
                w.submit_blocking(WriteOp::RequeueJudgeRetries {
                    max_attempts: crate::judge_worker::MAX_ATTEMPTS,
                    now: ts(now),
                })?;
                let _ = self.ctx.judge_wake.try_send(());
                Ok(Step::Finished("judge retries queued".into()))
            }
            Job::FtsOptimize => {
                for table in ["memories_fts", "memories_tri", "chunks_fts", "chunks_tri"] {
                    if let WriteOutcome::Flag(true) = w.submit_blocking(WriteOp::FtsMerge {
                        table: table.into(),
                        pages: 500,
                    })? {
                        return Ok(Step::More);
                    }
                }
                Ok(Step::Finished("full-text indexes merged".into()))
            }
            Job::QuickCheck => {
                if self.ctx.stop.load(Ordering::Relaxed) {
                    return Ok(Step::Stopped("paused: the daemon is stopping".into()));
                }
                let c = rusqlite::Connection::open_with_flags(
                    self.ctx.home.db(),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                self.interrupted.store(false, Ordering::SeqCst);
                *self.interrupt.lock() = Some(c.get_interrupt_handle());
                #[cfg(test)]
                if let Some(hook) = self.quick_check_hook.lock().clone() {
                    hook(&c);
                }
                let checked = m::quick_check(&c);
                *self.interrupt.lock() = None;
                self.quick_check_step(checked, self.interrupted.load(Ordering::SeqCst))
            }
            Job::IndexCheck => self.index_check(),
            Job::ModelPurge => self.model_purge(sc),
            Job::Backup => self.backup(now),
            Job::MergeSuggestions => self.merge_suggestions(s.judge.cluster_similarity),
            Job::CaptureRetention => {
                self.raw_retention(now, sc, s.capture.raw_retention, RawScope::Capture)
            }
        }
    }

    /// One slice of raw retention: delete up to a slice of the `scope`'s chunks that `retention`
    /// no longer keeps. A capture source emptied here keeps its cursor and turn keys
    /// (`m::delete_chunks`).
    fn raw_retention(
        &self,
        now: DateTime<Utc>,
        sc: &mut Scratch,
        retention: Retention,
        scope: RawScope,
    ) -> Result<Step> {
        // (seq, text bytes) of the chunks to delete in this slice.
        let take: Vec<(i64, u64)> = match retention {
            Retention::Forever => {
                return Ok(Step::Finished(
                    match scope {
                        RawScope::All => "retention is forever",
                        RawScope::Capture => "capture retention is forever",
                    }
                    .into(),
                ));
            }
            // Age is the time a chunk was stored in Alice (`created_at`), never the
            // time it says it was written.
            Retention::Age(age) => {
                // An age that does not convert (the settings refuse anything over 100
                // years, but a stale value must never turn into zero): delete nothing.
                let Some(since) = age_cutoff(now, age) else {
                    return Ok(Step::Finished(
                        match scope {
                            RawScope::All => "raw_retention is too large to apply; nothing deleted",
                            RawScope::Capture => {
                                "capture.raw_retention is too large to apply; nothing deleted"
                            }
                        }
                        .into(),
                    ));
                };
                let cutoff = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                self.ctx
                    .readers
                    .with(|c| scope.older_than(c, &cutoff))?
                    .into_iter()
                    .map(|seq| (seq, 0))
                    .collect()
            }
            Retention::Size(limit) => {
                // The total is read once per run; slices count down from it.
                let over = match sc.over_bytes {
                    Some(o) => o,
                    None => {
                        let total = self.ctx.readers.with(|c| scope.bytes(c))?;
                        total.saturating_sub(limit)
                    }
                };
                sc.over_bytes = Some(over);
                let mut left = over;
                let mut take = Vec::new();
                if over > 0 {
                    for (seq, bytes) in self.ctx.readers.with(|c| scope.oldest(c))? {
                        if left == 0 {
                            break;
                        }
                        take.push((seq, bytes));
                        left = left.saturating_sub(bytes);
                    }
                }
                take
            }
        };
        if take.is_empty() {
            return Ok(Step::Finished(match scope {
                RawScope::All => {
                    format!("raw tier within retention; {} chunks deleted", sc.deleted)
                }
                RawScope::Capture => format!(
                    "captured transcripts within retention; {} chunks deleted",
                    sc.deleted
                ),
            }));
        }
        sc.reason = match scope {
            RawScope::All => retention.to_string(),
            RawScope::Capture => format!("capture: {retention}"),
        };
        let seqs: Vec<i64> = take.iter().map(|(seq, _)| *seq).collect();
        let WriteOutcome::Purged(done) = self
            .ctx
            .writer
            .submit_blocking(WriteOp::DeleteChunks { seqs })?
        else {
            return Ok(Step::More);
        };
        sc.deleted += done.len();
        if let Some(o) = sc.over_bytes {
            let freed: u64 = take
                .iter()
                .filter(|(seq, _)| done.contains(seq))
                .map(|(_, b)| *b)
                .sum();
            sc.over_bytes = Some(o.saturating_sub(freed));
        }
        if let Some(idx) = self.ctx.index.read().clone() {
            idx.remove_chunks(&done)?;
        }
        Ok(Step::More)
    }

    /// What the result of `PRAGMA quick_check` means. An interrupt (`interrupted`: the daemon is
    /// stopping) ends the check as paused, whether SQLite reports `SQLITE_INTERRUPT` or, when it
    /// lands while the full-text tables are still being set up, "vtable constructor failed". A
    /// corruption result is never turned into "paused", interrupted or not.
    fn quick_check_step(&self, checked: Result<Vec<String>>, interrupted: bool) -> Result<Step> {
        use rusqlite::ErrorCode;
        let paused = || Ok(Step::Stopped("paused: the daemon is stopping".into()));
        match checked {
            Ok(problems) if problems.is_empty() => Ok(Step::Finished("quick_check ok".into())),
            Ok(problems) => Err(self.damaged(&format!(
                "PRAGMA quick_check found damage ({})",
                problems.first().cloned().unwrap_or_default()
            ))),
            Err(AliceError::Db(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == ErrorCode::OperationInterrupted =>
            {
                paused()
            }
            Err(AliceError::Db(rusqlite::Error::SqliteFailure(e, msg)))
                if matches!(e.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) =>
            {
                Err(self.damaged(&format!(
                    "PRAGMA quick_check could not read the database ({})",
                    msg.unwrap_or_else(|| e.to_string())
                )))
            }
            Err(e) if interrupted && e.to_string().contains("vtable constructor failed") => {
                paused()
            }
            Err(e) => Err(e),
        }
    }

    /// The database is damaged: writes stop (read-only, as for a failing disk), so no backup is
    /// made of it either (the scheduler does not run while the writer is read-only) and the
    /// backups already made stay as they are. Returns the failure to report.
    fn damaged(&self, reason: &str) -> AliceError {
        // Recorded first: once the writer is read-only nothing more can be written, and
        // `alice maintenance status` should still say what happened.
        let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.set_meta("maint.quick_check.last_run", &stamp);
        self.set_meta("maint.quick_check.last_error", &format!("{stamp} {reason}"));
        self.ctx.writer.read_only.set(reason.to_string());
        AliceError::Other(format!(
            "{reason}; writes are stopped and no more backups are made until it is repaired or restored from a backup"
        ))
    }

    /// Daily backup. Order: make room if the volume is short (policy rotation first), copy,
    /// then the housekeeping that may fail without costing the backup: adopting M1's
    /// pre-migration files and rotating are warnings, not failures.
    fn backup(&self, now: DateTime<Utc>) -> Result<Step> {
        let s = self.ctx.settings.get();
        let dir = backup::backup_dir(&self.ctx.home, &s.backup);
        let db = self.ctx.home.db();
        let mut warnings = Vec::new();
        backup::ensure_room(
            &db,
            &dir,
            now,
            (s.backup.daily, s.backup.weekly),
            &*self.ctx.free_space,
            &mut warnings,
        )?;
        let stop = &self.ctx.stop;
        let Some(b) = backup::create_stoppable(&db, &dir, BackupKind::Scheduled, now, &|| {
            stop.load(Ordering::Relaxed)
        })?
        else {
            return Ok(Step::Stopped("paused: the daemon is stopping".into()));
        };
        let adopted = backup::adopt_legacy(&self.ctx.home, &dir).unwrap_or_else(|e| {
            warnings.push(format!("moving the old pre-migration files failed: {e}"));
            0
        });
        let removed = backup::rotate(&dir, now, s.backup.daily, s.backup.weekly)
            .map(|d| d.len())
            .unwrap_or_else(|e| {
                warnings.push(format!("rotation failed: {e}"));
                0
            });
        for w in &warnings {
            tracing::warn!(warning = %w, "backup housekeeping");
        }
        let mut detail = format!(
            "{} ({} bytes); {removed} old backups removed; {adopted} legacy files moved",
            b.name, b.bytes
        );
        if !warnings.is_empty() {
            detail.push_str(&format!("; warnings: {}", warnings.join("; ")));
        }
        Ok(Step::Finished(detail))
    }

    /// Daily: the vector index must hold exactly the stored vectors, compared by key. A
    /// difference is repaired in place (add what is missing, drop what nothing backs); the whole
    /// index is rebuilt only when that cannot be done (a stored vector that cannot be read or
    /// added).
    fn index_check(&self) -> Result<Step> {
        /// Repairs tried before the difference counts as lasting: the embedding worker and a
        /// purge move the stored vectors while the check reads them.
        const ATTEMPTS: usize = 3;
        let (active, idx) = snapshot(&self.ctx.active, &self.ctx.index);
        let (model, dim) = (active.model.clone(), active.embedder.dim());
        let Some(idx) = idx.filter(|i| i.model() == model) else {
            return Ok(Step::Skipped("index is being rebuilt".into()));
        };
        let (mut added, mut removed) = (0, 0);
        let mut why = String::new();
        for _ in 0..ATTEMPTS {
            let repaired = self.ctx.readers.with(|c| {
                let r = idx.catch_up(c)?;
                Ok((r, embeddings::count(c, &model)?))
            });
            match repaired {
                Ok((r, stored)) => {
                    added += r.added;
                    removed += r.removed;
                    if idx.len() == stored {
                        return Ok(Step::Finished(if added + removed == 0 {
                            format!("index holds all {stored} vectors")
                        } else {
                            format!(
                                "index differed from the stored vectors; added {added}, removed {removed} ({stored} vectors)"
                            )
                        }));
                    }
                    why = format!(
                        "index holds {} of {stored} vectors after catching up",
                        idx.len()
                    );
                }
                Err(e) => why = format!("catching up failed ({e})"),
            }
        }
        // Only while the model checked is still the one in force: a switch that happened
        // meanwhile put the new model's index in the slot, and that one is not to be dropped.
        {
            let a = self.ctx.active.read();
            if a.model != model {
                return Ok(Step::Skipped(
                    "the embedding model changed meanwhile".into(),
                ));
            }
            *self.ctx.index.write() = None;
        }
        crate::service::spawn_rebuild(
            self.ctx.index.clone(),
            self.ctx.active.clone(),
            self.ctx.home.db(),
            model,
            dim,
            None,
        )?;
        Ok(Step::Finished(format!(
            "{why}, {ATTEMPTS} times; rebuild started"
        )))
    }

    /// Daily: the vectors and index files of models the daemon switched away from (listed in
    /// `meta.purge_model`) go, 500 rows per step. The model in force and the target of a
    /// re-embed in progress are never purged, whatever the list says.
    fn model_purge(&self, sc: &mut Scratch) -> Result<Step> {
        let list = crate::reembed::purge_list(&self.ctx.readers);
        if list.is_empty() {
            return Ok(Step::Finished("no model to purge".into()));
        }
        let keep_active = self.ctx.active.read().model.clone();
        let keep_target = self
            .meta(crate::reembed::TARGET_KEY)
            .filter(|t| !t.is_empty())
            .and_then(|t| serde_json::from_str::<alice_core::settings::EmbeddingSettings>(&t).ok())
            .map(|s| alice_embed::expected_model_id(&s));
        let (due, wanted): (Vec<String>, Vec<String>) = list
            .into_iter()
            .partition(|m| *m != keep_active && Some(m) != keep_target.as_ref());
        let Some(model) = due.first().cloned() else {
            return Ok(Step::Finished("the models listed are in use again".into()));
        };
        let WriteOutcome::Count(n) =
            self.ctx
                .writer
                .submit_blocking(WriteOp::DeleteModelVectors {
                    model: model.clone(),
                    limit: SLICE,
                })?
        else {
            return Err(AliceError::Other("unexpected writer outcome".into()));
        };
        if n > 0 {
            sc.deleted += n;
            return Ok(Step::More);
        }
        let base = self.ctx.home.vector_file(&model);
        for ext in [
            "memories.usearch",
            "chunks.bq",
            "chunks.usearch",
            "clean",
            "usearch.tmp",
            "bq.tmp",
        ] {
            let _ = std::fs::remove_file(base.with_extension(ext));
        }
        let rest: Vec<String> = due.into_iter().skip(1).chain(wanted).collect();
        self.ctx.writer.submit_blocking(WriteOp::SetMeta {
            key: crate::reembed::PURGE_KEY.into(),
            value: rest.join("\n"),
        })?;
        sc.deleted_other += 1;
        if rest
            .iter()
            .any(|m| *m != keep_active && Some(m) != keep_target.as_ref())
        {
            return Ok(Step::More);
        }
        Ok(Step::Finished(format!(
            "purged {} stored vectors and the index files of {} model(s)",
            sc.deleted, sc.deleted_other
        )))
    }

    /// Weekly: groups of live memories of one kind in one project whose vectors are at least
    /// `judge.cluster_similarity` alike and whose texts agree (token Jaccard 0.6) become one
    /// `merge` review item (asked once), with `p` the group's lowest pairwise cosine.
    fn merge_suggestions(&self, threshold: f64) -> Result<Step> {
        let (active, idx) = snapshot(&self.ctx.active, &self.ctx.index);
        let Some(idx) = idx.filter(|i| i.model() == active.model) else {
            return Ok(Step::Skipped("index not ready".into()));
        };
        let model = active.model.clone();
        let open = self
            .ctx
            .readers
            .with(|c| reviews::count_open_of(c, reviews::ReviewType::Merge))?;
        if open >= MERGE_OPEN_MAX {
            return Ok(Step::Skipped(format!(
                "skipped: {MERGE_OPEN_MAX} merge reviews already open"
            )));
        }
        let room = (MERGE_OPEN_MAX - open) as usize;
        let cursor: i64 = self
            .meta("maint.merge.cursor")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // Diaries are session logs and status notes time-stamped snapshots: they share a format
        // by design, so their vectors sit close without being duplicates. Never clustered.
        // Distilled memories are the judge's alone (SP2 spec §6.6): never put to a person.
        let rows: Vec<(i64, String, String)> = self.ctx.readers.with(|c| {
            let mut stmt = c.prepare(
                "SELECT seq, id, project_id FROM memories
                 WHERE seq > ?1 AND status IN ('active','pending') AND kind NOT IN ('diary','status')
                   AND origin <> 'distill'
                 ORDER BY seq LIMIT ?2",
            )?;
            Ok(stmt
                .query_map(rusqlite::params![cursor, MERGE_PAGE], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<std::result::Result<_, _>>()?)
        })?;
        if rows.is_empty() {
            self.set_meta("maint.merge.cursor", "0");
            return Ok(Step::Finished("merge suggestions done".into()));
        }
        // A memory already in an open merge item is not put in another.
        let mut claimed: std::collections::HashSet<String> = self.ctx.readers.with(|c| {
            let mut stmt = c.prepare(
                "SELECT payload_json FROM review_items WHERE type = 'merge' AND status = 'open'",
            )?;
            let mut ids = std::collections::HashSet::new();
            for p in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let p = p?;
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&p) else {
                    tracing::warn!(
                        "merge suggestions: an open merge item has a malformed payload; skipped"
                    );
                    continue;
                };
                ids.extend(v["survivor"].as_str().map(str::to_string));
                for o in v["others"].as_array().into_iter().flatten() {
                    ids.extend(o.as_str().map(str::to_string));
                }
            }
            Ok(ids)
        })?;
        let mut made = 0;
        let mut last = cursor;
        for (seq, id, project) in &rows {
            if made >= room {
                break;
            }
            last = *seq;
            if claimed.contains(id) {
                continue;
            }
            let Some((_, v)) = self
                .ctx
                .readers
                .with(|c| embeddings::get_many(c, &model, &[VecKey::Memory(*seq)]))?
                .pop()
            else {
                continue;
            };
            let Some((seed_kind, seed_text)) = self.ctx.readers.with(|c| {
                Ok(c.query_row(
                    "SELECT kind, title, body FROM memories WHERE seq = ?1",
                    [seq],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            merge_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?),
                        ))
                    },
                )
                .optional()?)
            })?
            else {
                continue;
            };
            let near: Vec<i64> = idx
                .search(&v, 10, alice_core::vector::KeyFilter::Memories)?
                .into_iter()
                .filter_map(|(k, sim)| match k {
                    VecKey::Memory(o) if o != *seq && f64::from(sim) >= threshold => Some(o),
                    _ => None,
                })
                .collect();
            if near.is_empty() {
                continue;
            }
            // Live, same project, not a diary, not distilled, not in an open item: (seq, id,
            // vector).
            let candidates: Vec<(String, Vec<f32>, MergeText)> = self.ctx.readers.with(|c| {
                let mut out = Vec::new();
                for o in &near {
                    let Some((oid, oproject, okind, otitle, obody)) = c
                        .query_row(
                            "SELECT id, project_id, kind, title, body FROM memories
                             WHERE seq = ?1 AND status IN ('active','pending') AND kind NOT IN ('diary','status')
                               AND origin <> 'distill'",
                            [o],
                            |r| {
                                Ok((
                                    r.get::<_, String>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, String>(2)?,
                                    r.get::<_, String>(3)?,
                                    r.get::<_, String>(4)?,
                                ))
                            },
                        )
                        .optional()?
                    else {
                        continue;
                    };
                    if &oproject != project || okind != seed_kind || claimed.contains(&oid) {
                        continue;
                    }
                    if let Some((_, ov)) =
                        embeddings::get_many(c, &model, &[VecKey::Memory(*o)])?.pop()
                    {
                        out.push((oid, ov, merge_text(&otitle, &obody)));
                    }
                }
                Ok(out)
            })?;
            // Complete linkage: a candidate joins only when it is alike every member so far, not
            // just the seed (a chain A~B, B~C is not a group), in vectors and in words, and has
            // the same numbers (a changed port, version, amount or date is an update, not a
            // duplicate), up to `MERGE_GROUP_MAX`. `lowest` is the least pairwise cosine.
            let mut members: Vec<(&str, &[f32], &MergeText)> =
                vec![(id.as_str(), v.as_slice(), &seed_text)];
            let mut lowest = f64::INFINITY;
            for (cid, cv, ctext) in &candidates {
                if members.len() >= MERGE_GROUP_MAX {
                    break;
                }
                let sims: Vec<f64> = members.iter().map(|(_, mv, _)| cosine(mv, cv)).collect();
                if sims.iter().all(|s| *s >= threshold)
                    && members.iter().all(|(_, _, mt)| {
                        mt.numbers == ctext.numbers
                            && jaccard(&mt.words, &ctext.words) >= MERGE_TEXT_JACCARD
                    })
                {
                    lowest = sims.into_iter().fold(lowest, f64::min);
                    members.push((cid.as_str(), cv.as_slice(), ctext));
                }
            }
            let mut group: Vec<String> = members.iter().map(|(i, _, _)| (*i).to_string()).collect();
            if group.len() < 2 {
                continue;
            }
            group.sort();
            group.dedup();
            let key = format!("merge:{}", group.join(":"));
            if self.ctx.readers.with(|c| reviews::was_rejected(c, &key))? {
                continue;
            }
            // The survivor: the oldest (smallest id, ULIDs sort by time).
            let survivor = group[0].clone();
            let others: Vec<String> = group[1..].to_vec();
            if let WriteOutcome::Count(n) =
                self.ctx
                    .writer
                    .submit_blocking(WriteOp::CreateReview(reviews::NewReview {
                        kind: reviews::ReviewType::Merge,
                        payload: serde_json::json!({
                            "survivor": survivor,
                            "others": others,
                            "p": lowest,
                            "provider": "cluster",
                        }),
                        confidence: Some(lowest),
                        dedupe_key: Some(key),
                    }))?
            {
                made += n;
                if n > 0 {
                    claimed.extend(group.iter().cloned());
                }
            }
        }
        self.set_meta("maint.merge.cursor", &last.to_string());
        tracing::debug!(suggested = made, up_to_seq = last, "merge suggestions");
        Ok(Step::More)
    }

    /// Every 5 s: when idle long enough, run the due jobs one after another. Takes the shared
    /// `Arc` so `Service::run_job` can run jobs on demand on the same scheduler.
    pub fn spawn(self: Arc<Self>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("alice-maintenance".into())
            .spawn(move || {
                lower_priority();
                while !stop.load(Ordering::Relaxed) {
                    let settings = self.ctx.settings.get();
                    if self.idle_enough(&settings.maintenance)
                        && self.ctx.writer.read_only.reason().is_none()
                    {
                        for job in self.due(Utc::now()) {
                            if stop.load(Ordering::Relaxed)
                                || !self.idle_enough(&settings.maintenance)
                            {
                                break;
                            }
                            let r = self.run_job(job, Utc::now(), false);
                            tracing::info!(
                                job = job.name(),
                                outcome = ?r.outcome,
                                took_ms = r.took_ms,
                                detail = %r.detail,
                                "maintenance"
                            );
                        }
                    }
                    for _ in 0..50 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("spawn maintenance thread")
    }
}

/// Why the backups are behind, when the newest one (a scheduled or manual copy) is older than
/// two backup periods; with none at all, when the data directory itself is that old. `None`:
/// they are up to date.
pub fn backup_warning(readers: &ReaderPool, dir: &Path, now: DateTime<Utc>) -> Option<String> {
    let limit = chrono::Duration::from_std(Job::Backup.every() * OVERDUE_PERIODS).ok()?;
    let newest = backup::list(dir)
        .ok()?
        .into_iter()
        .find(|b| matches!(b.kind, BackupKind::Scheduled | BackupKind::Manual))
        .map(|b| b.created_at);
    let fix = "run \"alice backup now\", and see \"alice maintenance status\" for why the daily backup did not run";
    match newest {
        Some(t) if now - t > limit => Some(format!(
            "the newest backup is {} days old; {fix}",
            (now - t).num_days()
        )),
        Some(_) => None,
        None => {
            let created: Option<String> = readers
                .with(
                    |c| Ok(c.query_row("SELECT min(created_at) FROM projects", [], |r| r.get(0))?),
                )
                .ok()
                .flatten();
            let since = created
                .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
                .map(|t| t.with_timezone(&Utc))?;
            (now - since > limit).then(|| {
                format!(
                    "no backup has been made in {} days; {fix}",
                    (now - since).num_days()
                )
            })
        }
    }
}

/// `now` minus `age`; `None` when that is not a time (an age too large for the clock). The
/// caller then deletes nothing: an overflow must never read as "zero", which is every chunk.
fn age_cutoff(now: DateTime<Utc>, age: Duration) -> Option<DateTime<Utc>> {
    chrono::Duration::from_std(age)
        .ok()
        .and_then(|d| now.checked_sub_signed(d))
}

/// Best effort: below normal priority so maintenance (and local capture) never competes with
/// requests.
pub(crate) fn lower_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: changes only this thread's nice value.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: sets this thread's QoS class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}

#[cfg(test)]
mod tests {
    /// Trashed memories the purge-latency test purges.
    const PURGE_TEST_MEMORIES: usize = 6_000;

    use super::*;
    use crate::writer::{ProjectChoice, spawn_writer};
    use alice_core::store::memories::{InsertOutcome, NewMemoryInput};
    use alice_core::vector::VectorIndex;
    use parking_lot::RwLock;
    use rusqlite::params;

    const DIM: usize = 3;

    struct Rig {
        dir: tempfile::TempDir,
        home: AliceHome,
        writer: WriterHandle,
        index: Arc<IndexSlot>,
        settings: Arc<SettingsHandle>,
        activity: Arc<Activity>,
        sched: Arc<Scheduler>,
        stop: Arc<AtomicBool>,
        judge_woken: crossbeam_channel::Receiver<()>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.writer.send_nowait(WriteOp::Shutdown);
        }
    }

    fn rig() -> Rig {
        rig_with("")
    }

    /// A rig whose `config.toml` holds `config`.
    fn rig_with(config: &str) -> Rig {
        rig_custom(config, |_| {})
    }

    /// [`rig_with`], letting the test change the scheduler's context (the model, the free-space
    /// probe) before it is built.
    fn rig_custom(config: &str, tweak: impl FnOnce(&mut MaintenanceCtx)) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let home = AliceHome::new(dir.path().to_path_buf());
        home.ensure_dirs().unwrap();
        if !config.is_empty() {
            let root = dir.path().to_string_lossy().replace('\\', "/");
            std::fs::write(home.config_file(), config.replace("{ROOT}", &root)).unwrap();
        }
        let settings = SettingsHandle::load_with_env(&home, Vec::new());
        let conn = alice_core::store::open_writer(&home.db()).unwrap();
        let (wake_tx, _wake_rx) = crossbeam_channel::bounded(1);
        let (writer, _t) = spawn_writer(conn, "dev".into(), wake_tx);
        let readers = Arc::new(ReaderPool::new(home.db(), 2));
        let index: Arc<IndexSlot> = Arc::new(RwLock::new(Some(Arc::new(
            VectorIndex::for_model("fake", DIM).unwrap(),
        ))));
        let activity = Activity::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (judge_tx, judge_rx) = crossbeam_channel::bounded(1);
        let mut ctx = MaintenanceCtx {
            home: home.clone(),
            readers,
            writer: writer.clone(),
            index: index.clone(),
            active: Arc::new(RwLock::new(Arc::new(crate::service::Active {
                embedder: Arc::new(alice_core::embed::FakeEmbedder::named("fake", DIM)),
                model: "fake".into(),
                vector_path: home.vector_file("fake"),
            }))),
            settings: settings.clone(),
            activity: activity.clone(),
            judge_wake: judge_tx,
            stop: stop.clone(),
            free_space: Arc::new(|_| None),
        };
        tweak(&mut ctx);
        let sched = Arc::new(Scheduler::new(ctx));
        Rig {
            dir,
            home,
            writer,
            index,
            settings,
            activity,
            sched,
            stop,
            judge_woken: judge_rx,
        }
    }

    /// [`rig_with`] whose settings also read the variables `env` (`ALICE_*`), as the daemon's
    /// do the process environment (a tenant's env file pins keys this way).
    fn rig_with_env(config: &str, env: &[(&str, &str)]) -> Rig {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut pinned = None;
        let mut r = rig_custom(config, |ctx| {
            let settings = SettingsHandle::load_with_env(&ctx.home, env);
            ctx.settings = settings.clone();
            pinned = Some(settings);
        });
        r.settings = pinned.unwrap();
        r
    }

    impl Rig {
        /// A second connection for setup that needs past timestamps or states.
        fn sql(&self) -> rusqlite::Connection {
            alice_core::store::open_writer(&self.home.db()).unwrap()
        }

        fn count(&self, sql: &str) -> i64 {
            alice_core::store::open_reader(&self.home.db())
                .unwrap()
                .query_row(sql, [], |r| r.get(0))
                .unwrap()
        }

        fn global(&self) -> String {
            "00000000000000000000GLOBAL".to_string()
        }

        /// Insert one memory row directly; returns its seq.
        fn raw_memory(
            &self,
            c: &rusqlite::Connection,
            n: usize,
            status: &str,
            updated: &str,
            judge: &str,
        ) -> i64 {
            c.execute(
                "INSERT INTO memories (id, project_id, kind, title, body, status, content_hash, created_at, updated_at, hlc, embed_state, judge_state)
                 VALUES (?1, ?2, 'note', ?3, ?3, ?4, ?5, ?6, ?6, 'h', 'done', ?7)",
                params![
                    format!("M{n:025}"),
                    self.global(),
                    format!("memory number {n}"),
                    status,
                    format!("hash{n}"),
                    updated,
                    judge
                ],
            )
            .unwrap();
            c.last_insert_rowid()
        }

        fn insert_trashed_memories(&self, n: usize, updated: &str) {
            let mut c = self.sql();
            let tx = c.transaction().unwrap();
            for i in 0..n {
                self.raw_memory(&tx, i, "trashed", updated, "done");
            }
            tx.commit().unwrap();
        }

        /// Live memories with a stored vector each and the same vectors in the index.
        fn insert_embedded_memories(&self, n: usize) -> Vec<i64> {
            let mut c = self.sql();
            let tx = c.transaction().unwrap();
            let idx = self.index.read().clone().unwrap();
            let mut seqs = Vec::new();
            for i in 0..n {
                let seq = self.raw_memory(&tx, i, "active", "2026-10-01T00:00:00.000Z", "done");
                let a = i as f32 * 0.37;
                let v = [a.cos(), a.sin(), 0.1];
                embeddings::put(&tx, VecKey::Memory(seq), "fake", &v).unwrap();
                idx.upsert(VecKey::Memory(seq), &v).unwrap();
                seqs.push(seq);
            }
            tx.commit().unwrap();
            seqs
        }

        fn insert_memories_with_vectors(&self, items: &[(&str, [f32; 3])]) {
            let mut c = self.sql();
            let tx = c.transaction().unwrap();
            let idx = self.index.read().clone().unwrap();
            for (i, (name, v)) in items.iter().enumerate() {
                let seq = self.raw_memory(&tx, i, "active", "2026-10-01T00:00:00.000Z", "done");
                tx.execute(
                    "UPDATE memories SET title = ?1, body = ?1 WHERE seq = ?2",
                    params![name, seq],
                )
                .unwrap();
                embeddings::put(&tx, VecKey::Memory(seq), "fake", v).unwrap();
                idx.upsert(VecKey::Memory(seq), v).unwrap();
            }
            tx.commit().unwrap();
        }

        /// Live memories of the given kind and text (title and body), each with its vector.
        fn insert_kinded(&self, items: &[(&str, &str, [f32; 3])]) {
            let mut c = self.sql();
            let tx = c.transaction().unwrap();
            let idx = self.index.read().clone().unwrap();
            for (i, (kind, text, v)) in items.iter().enumerate() {
                let seq = self.raw_memory(&tx, i, "active", "2026-10-01T00:00:00.000Z", "done");
                tx.execute(
                    "UPDATE memories SET kind = ?1, title = ?2, body = ?2 WHERE seq = ?3",
                    params![kind, text, seq],
                )
                .unwrap();
                embeddings::put(&tx, VecKey::Memory(seq), "fake", v).unwrap();
                idx.upsert(VecKey::Memory(seq), v).unwrap();
            }
            tx.commit().unwrap();
        }

        fn index_len(&self) -> Option<usize> {
            self.index.read().as_ref().map(|i| i.len())
        }

        fn wait_index_len(&self, n: usize) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while self.index_len() != Some(n) {
                assert!(
                    Instant::now() < deadline,
                    "index len {:?}, wanted {n}",
                    self.index_len()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn jobs_are_due_by_their_period_and_run_only_when_idle() {
        let r = rig();
        let now = chrono::Utc::now();
        assert_eq!(r.sched.due(now).len(), Job::ALL.len(), "never run: all due");
        for job in [Job::TrashPurge, Job::JudgeRetry] {
            let report = r.sched.run_job(job, now, false);
            assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        }
        let due = r.sched.due(now);
        assert!(!due.contains(&Job::TrashPurge) && !due.contains(&Job::JudgeRetry));
        // Each job comes back after its own period, not before.
        assert!(
            !r.sched
                .due(now + chrono::Duration::hours(23))
                .contains(&Job::TrashPurge)
        );
        assert!(
            r.sched
                .due(now + chrono::Duration::days(1) + chrono::Duration::seconds(1))
                .contains(&Job::TrashPurge)
        );
        assert!(
            !r.sched
                .due(now + chrono::Duration::minutes(30))
                .contains(&Job::JudgeRetry),
            "hourly, not sooner"
        );
        assert!(
            r.sched
                .due(now + chrono::Duration::hours(1) + chrono::Duration::seconds(1))
                .contains(&Job::JudgeRetry)
        );
        // Busy: a request in flight means nothing runs on the timer.
        let _g = r.activity.enter();
        assert!(!r.sched.idle_enough(&r.settings.get().maintenance));
    }

    #[test]
    fn idleness_needs_the_configured_quiet_and_the_switch_on() {
        let r = rig_with("[maintenance]\nidle_secs = 5\n");
        let s = r.settings.get().maintenance.clone();
        assert_eq!(s.idle_secs, 5);
        assert!(
            !r.sched.idle_enough(&s),
            "just started: not quiet for 5 s yet"
        );
        let quiet = MaintenanceSettings {
            idle_secs: 0,
            ..s.clone()
        };
        assert!(r.sched.idle_enough(&quiet));
        let off = MaintenanceSettings {
            enabled: false,
            idle_secs: 0,
            ..s
        };
        assert!(!r.sched.idle_enough(&off), "maintenance.enabled = false");
        // A request just ended: the quiet starts over (for a 1 hour requirement).
        drop(r.activity.enter());
        let hour = MaintenanceSettings {
            idle_secs: 3_600,
            ..quiet
        };
        assert!(!r.sched.idle_enough(&hour));
    }

    /// A job that failed comes back within the hour, a successful one after its period.
    #[test]
    fn a_failed_job_is_due_again_after_an_hour() {
        let r = rig();
        let now = chrono::Utc::now();
        let stamp = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // Ran and succeeded: due after its day.
        r.sched.set_meta("maint.backup.last_run", &stamp(now));
        r.sched.set_meta("maint.backup.last_ok", &stamp(now));
        assert!(
            !r.sched
                .due(now + chrono::Duration::hours(2))
                .contains(&Job::Backup)
        );
        // A later run that failed (last_run after last_ok): due again within the hour.
        let later = now + chrono::Duration::hours(3);
        r.sched.set_meta("maint.backup.last_run", &stamp(later));
        assert!(
            !r.sched
                .due(later + chrono::Duration::minutes(30))
                .contains(&Job::Backup)
        );
        assert!(
            r.sched
                .due(later + chrono::Duration::minutes(61))
                .contains(&Job::Backup)
        );
    }

    #[test]
    fn a_long_job_yields_when_a_request_arrives() {
        let r = rig();
        r.insert_trashed_memories(1_200, "2020-01-01T00:00:00.000Z");
        {
            // A request is in flight for the whole run: the job does one 500-row slice, sees
            // it, and stops there (deterministic: no timing involved).
            let _g = r.activity.enter();
            let report = r.sched.run_job(Job::TrashPurge, chrono::Utc::now(), false);
            assert_eq!(report.outcome, JobOutcome::Paused, "{report:?}");
            // One time-boxed slice ran: some of the 500 it was given (all of them on a fast
            // machine), not the whole 1,200.
            let left = r.count("SELECT count(*) FROM memories");
            assert!((700..1_200).contains(&left), "{left} left after one slice");
            assert_eq!(
                r.sched.meta("maint.trash_purge.last_run"),
                None,
                "a paused job is not a run"
            );
        }
        let forced = r.sched.run_job(Job::TrashPurge, chrono::Utc::now(), true);
        assert_eq!(forced.outcome, JobOutcome::Done);
        assert_eq!(r.count("SELECT count(*) FROM memories"), 0);
        assert!(r.sched.meta("maint.trash_purge.last_ok").is_some());
        assert_eq!(
            r.count("SELECT count(*) FROM events WHERE action = 'memory.purge'"),
            1_200
        );
    }

    /// A daemon that is stopping ends a job at its next slice even when it is forced.
    #[test]
    fn a_stopping_daemon_ends_a_job_between_slices() {
        let r = rig();
        r.insert_trashed_memories(1_200, "2020-01-01T00:00:00.000Z");
        r.stop.store(true, Ordering::SeqCst);
        let report = r.sched.run_job(Job::TrashPurge, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Paused, "{report:?}");
        let left = r.count("SELECT count(*) FROM memories");
        assert!((700..1_200).contains(&left), "{left} left after one slice");
    }

    /// Purged memories lose their vectors in the index too; a memory restored from the trash
    /// since the job looked is kept; memories trashed recently stay.
    #[test]
    fn trash_purge_waits_thirty_days_and_drops_the_vectors() {
        let r = rig();
        let seqs = r.insert_embedded_memories(3);
        let now = chrono::Utc::now();
        let old =
            (now - chrono::Duration::days(31)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let recent =
            (now - chrono::Duration::days(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let c = r.sql();
        c.execute(
            "UPDATE memories SET status = 'trashed', updated_at = ?1 WHERE seq = ?2",
            params![old, seqs[0]],
        )
        .unwrap();
        c.execute(
            "UPDATE memories SET status = 'trashed', updated_at = ?1 WHERE seq = ?2",
            params![recent, seqs[1]],
        )
        .unwrap();
        let report = r.sched.run_job(Job::TrashPurge, now, true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(r.count("SELECT count(*) FROM memories"), 2);
        assert_eq!(r.index_len(), Some(2), "the purged vector left the index");
        assert!(
            !r.index
                .read()
                .clone()
                .unwrap()
                .contains(VecKey::Memory(seqs[0]))
        );
        assert_eq!(r.count("SELECT count(*) FROM embeddings"), 2);
    }

    /// A purge run touches trashed memories only: pending, active and imported (`skipped`)
    /// memories stay, however old, and so does a trashed one younger than the cutoff.
    #[test]
    fn a_purge_leaves_pending_active_and_skipped_memories_in_place() {
        let r = rig();
        let c = r.sql();
        let old = "2020-01-01T00:00:00.000Z";
        let pending = r.raw_memory(&c, 1, "pending", old, "pending");
        let active = r.raw_memory(&c, 2, "active", old, "done");
        let skipped = r.raw_memory(&c, 3, "active", old, "skipped");
        let skipped_pending = r.raw_memory(&c, 4, "pending", old, "skipped");
        let archived = r.raw_memory(&c, 5, "archived", old, "done");
        let superseded = r.raw_memory(&c, 6, "superseded", old, "done");
        let recent = r.raw_memory(
            &c,
            7,
            "trashed",
            &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "done",
        );
        let trashed = r.raw_memory(&c, 8, "trashed", old, "done");
        let report = r.sched.run_job(Job::TrashPurge, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(report.detail, "1 purged");
        let alive = |seq: i64| -> bool {
            c.query_row("SELECT count(*) FROM memories WHERE seq = ?1", [seq], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
                == 1
        };
        for (what, seq) in [
            ("pending", pending),
            ("active", active),
            ("skipped", skipped),
            ("skipped pending", skipped_pending),
            ("archived", archived),
            ("superseded", superseded),
            ("recently trashed", recent),
        ] {
            assert!(alive(seq), "{what} stays");
        }
        assert!(!alive(trashed), "the old trashed one goes");
        let state: String = c
            .query_row(
                "SELECT judge_state FROM memories WHERE seq = ?1",
                [skipped],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "skipped");
    }

    /// B1 through the scheduler: no event row holds the purged text afterwards.
    #[test]
    fn a_purge_leaves_no_trace_of_the_text_in_the_history() {
        let r = rig();
        let out = r
            .writer
            .submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "the vault combination is 4-8-15-16-23-42".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .unwrap();
        let WriteOutcome::Remembered(InsertOutcome::New { id, .. }, _) = out else {
            panic!("{out:?}")
        };
        r.writer
            .submit_blocking(WriteOp::Update {
                id: id.clone(),
                update: alice_core::store::memories::MemoryUpdate {
                    action: alice_core::store::memories::UpdateAction::Forget,
                    ..Default::default()
                },
                actor: alice_core::store::events::Actor::User,
            })
            .unwrap();
        assert!(
            r.count("SELECT count(*) FROM events WHERE before_json LIKE '%vault combination%' OR after_json LIKE '%vault combination%'") > 0,
            "the history holds the text before the purge"
        );
        let later = chrono::Utc::now() + chrono::Duration::days(31);
        let report = r.sched.run_job(Job::TrashPurge, later, true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(r.count("SELECT count(*) FROM memories"), 0);
        let all: String = r
            .sql()
            .query_row(
                "SELECT group_concat(coalesce(before_json, '') || coalesce(after_json, ''), ' ') FROM events",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!all.contains("vault"), "{all}");
        assert!(!all.contains("4-8-15"), "{all}");
        assert_eq!(
            r.count("SELECT count(*) FROM events WHERE action = 'memory.purge'"),
            1
        );
    }

    #[test]
    fn raw_retention_by_age_deletes_old_chunks_and_their_vectors() {
        let r = rig_with("[storage]\nraw_retention = \"30d\"\n");
        let now = chrono::Utc::now();
        let fmt = |d: i64| {
            (now - chrono::Duration::days(d)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let c = r.sql();
        c.execute("INSERT INTO sources (id, project_id, type, uri, created_at) VALUES ('S1', ?1, 'file', 'u', ?2)", params![r.global(), fmt(1)]).unwrap();
        let idx = r.index.read().clone().unwrap();
        for i in 0..6 {
            c.execute(
                "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, authored_at, created_at, embed_state)
                 VALUES (?1, 'S1', ?2, ?3, ?4, ?5, ?6, ?6, 'done')",
                params![format!("C{i}"), r.global(), i, format!("chunk text {i}"), format!("ch{i}"), fmt(if i < 4 { 60 } else { 2 })],
            )
            .unwrap();
            let seq = c.last_insert_rowid();
            embeddings::put(&c, VecKey::Chunk(seq), "fake", &[1.0, 0.0, 0.0]).unwrap();
            idx.upsert(VecKey::Chunk(seq), &[1.0, 0.0, 0.0]).unwrap();
        }
        let report = r.sched.run_job(Job::RawRetention, now, true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(r.count("SELECT count(*) FROM chunks"), 2);
        assert_eq!(
            r.count("SELECT count(*) FROM sources"),
            1,
            "a source with chunks left stays"
        );
        assert_eq!(r.index_len(), Some(2));
        assert_eq!(
            r.count("SELECT count(*) FROM events WHERE action = 'raw.retention'"),
            1
        );
        let after: String = c
            .query_row(
                "SELECT after_json FROM events WHERE action = 'raw.retention'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            after.contains("\"deleted\":4") && after.contains("30d"),
            "{after}"
        );
    }

    #[test]
    fn raw_retention_by_size_deletes_the_oldest_until_it_fits() {
        // 10 chunks of 300 kB, kept at 1 MB in all: the 7 oldest go (the limit is at least 1 MB).
        let r = rig_with("[storage]\nraw_retention = \"1MB\"\n");
        let c = r.sql();
        c.execute("INSERT INTO sources (id, project_id, type, uri, created_at) VALUES ('S1', ?1, 'file', 'u', '2026-01-01T00:00:00.000Z')", [r.global()]).unwrap();
        for i in 0..10 {
            c.execute(
                "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at, embed_state)
                 VALUES (?1, 'S1', ?2, ?3, ?4, ?5, '2026-01-01T00:00:00.000Z', 'done')",
                params![format!("C{i}"), r.global(), i, "x".repeat(300_000), format!("ch{i}")],
            )
            .unwrap();
        }
        assert!(matches!(
            r.settings.get().storage.raw_retention,
            Retention::Size(_)
        ));
        let report = r.sched.run_job(Job::RawRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(r.count("SELECT count(*) FROM chunks"), 3);
        let oldest: String = c
            .query_row("SELECT min(id) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(oldest, "C7", "the newest stay");
        // Forever: nothing happens.
        let r2 = rig();
        let report = r2
            .sched
            .run_job(Job::RawRetention, chrono::Utc::now(), true);
        assert_eq!(report.detail, "retention is forever");
    }

    impl Rig {
        /// A transcript source of `session` (type `session`). `capture`: a capture source (uri
        /// `claude-code:session:<session>`, with its `capture_state` row); otherwise an imported
        /// transcript (a `file://` uri, no capture state).
        fn transcript_source(
            &self,
            c: &rusqlite::Connection,
            id: &str,
            session: &str,
            capture: bool,
        ) {
            let uri = if capture {
                format!("claude-code:session:{session}")
            } else {
                format!("file:///h/.claude/projects/-Users-me-Code-x/{session}.jsonl")
            };
            c.execute(
                "INSERT INTO sources (id, project_id, type, agent, uri, session_id, created_at)
                 VALUES (?1, ?2, 'session', 'claude-code', ?3, ?4, '2026-01-01T00:00:00.000Z')",
                params![id, self.global(), uri, session],
            )
            .unwrap();
            if capture {
                c.execute(
                    "INSERT INTO capture_state (source_id, agent, session_id, created_at)
                     VALUES (?1, 'claude-code', ?2, '2026-01-01T00:00:00.000Z')",
                    params![id, session],
                )
                .unwrap();
            }
        }

        /// Chunks `ords` of `source`, stored `days` ago, of `bytes` bytes of text each, each with
        /// a vector (stored and in the index); for a capture source also each turn's key and the
        /// cursor past it, as capture records them. Returns their seqs.
        fn transcript_chunks(
            &self,
            c: &rusqlite::Connection,
            source: &str,
            ords: std::ops::Range<i64>,
            days: i64,
            bytes: usize,
        ) -> Vec<i64> {
            let created = (chrono::Utc::now() - chrono::Duration::days(days))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let idx = self.index.read().clone().unwrap();
            let mut seqs = Vec::new();
            for ord in ords {
                c.execute(
                    "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at, embed_state)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'done')",
                    params![
                        format!("{source}-C{ord}"),
                        source,
                        self.global(),
                        ord,
                        "x".repeat(bytes),
                        format!("{source}-h{ord}"),
                        created
                    ],
                )
                .unwrap();
                let seq = c.last_insert_rowid();
                embeddings::put(c, VecKey::Chunk(seq), "fake", &[1.0, 0.0, 0.0]).unwrap();
                idx.upsert(VecKey::Chunk(seq), &[1.0, 0.0, 0.0]).unwrap();
                c.execute(
                    "INSERT INTO capture_turns (agent, turn_key, source_id, first_ord)
                     SELECT agent, ?2, source_id, ?3 FROM capture_state WHERE source_id = ?1",
                    params![source, format!("{source}-turn{ord}"), ord],
                )
                .unwrap();
                c.execute(
                    "UPDATE capture_state SET next_ord = ?2, byte_offset = byte_offset + ?3
                     WHERE source_id = ?1",
                    params![source, ord + 1, bytes as i64],
                )
                .unwrap();
                seqs.push(seq);
            }
            seqs
        }

        /// The seqs of the chunks of `source` left, in order.
        fn chunk_seqs(&self, source: &str) -> Vec<i64> {
            alice_core::store::open_reader(&self.home.db())
                .unwrap()
                .prepare("SELECT seq FROM chunks WHERE source_id = ?1 ORDER BY seq")
                .unwrap()
                .query_map([source], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        }

        /// The one `raw.retention` history row's totals.
        fn retention_event(&self) -> serde_json::Value {
            let after: String = self
                .sql()
                .query_row(
                    "SELECT after_json FROM events WHERE action = 'raw.retention'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&after).unwrap()
        }
    }

    /// How the MemPalace importer plans a transcript drawer of `session` (a Claude Code file
    /// named after it) while capture holds the sessions `captured`.
    fn plan_transcript_of(
        session: &str,
        captured: &std::collections::HashSet<String>,
    ) -> alice_core::import::plan::Planned {
        use alice_core::import::palace::{Drawer, DrawerMeta};
        use alice_core::import::plan::{Mapper, NoFilter, Planner, Survey};
        use alice_core::import::wings::WingMap;
        let wings = WingMap::build(&[], &std::collections::BTreeMap::new());
        let survey = Survey::default();
        let planner = Planner {
            mapper: Mapper {
                wings: &wings,
                assume: chrono::FixedOffset::east_opt(0).unwrap(),
                filter: &NoFilter,
            },
            survey: &survey,
            captured_sessions: captured,
        };
        planner.plan(&Drawer {
            id: "drawer-0".into(),
            text: "a turn of the session".into(),
            meta: DrawerMeta {
                wing: "sessions".into(),
                room: "general".into(),
                source_file: Some(format!(
                    "/h/.claude/projects/-Users-me-Code-x/{session}.jsonl"
                )),
                ingest_mode: Some("convos".into()),
                ..Default::default()
            },
        })
    }

    /// SP2 §3.9: `capture.raw_retention` ages out the chunks of capture sources only; imported
    /// transcripts and other raw sources follow `storage.raw_retention` alone.
    #[test]
    fn capture_retention_deletes_only_captured_chunks() {
        let r = rig_with("[capture]\nraw_retention = \"180d\"\n");
        let c = r.sql();
        let now = chrono::Utc::now();
        r.transcript_source(&c, "IMP", "imported-session", false);
        r.transcript_source(&c, "CAP", "captured-session", true);
        let imported = r.transcript_chunks(&c, "IMP", 0..3, 200, 40);
        r.transcript_chunks(&c, "CAP", 0..3, 200, 40);
        assert_eq!(r.index_len(), Some(6));
        let report = r.sched.run_job(Job::CaptureRetention, now, true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert!(r.chunk_seqs("CAP").is_empty(), "the captured chunks go");
        assert_eq!(r.chunk_seqs("IMP"), imported, "the imported ones stay");
        assert_eq!(r.index_len(), Some(3), "the vectors go with their chunks");
        assert_eq!(
            r.count("SELECT count(*) FROM embeddings WHERE owner_type = 'chunk'"),
            3
        );
        // One history row for the run, naming the capture setting.
        assert_eq!(
            r.count("SELECT count(*) FROM events WHERE action = 'raw.retention'"),
            1
        );
        let after = r.retention_event();
        assert_eq!(after["deleted"], 3, "{after}");
        assert_eq!(after["reason"], "capture: 180d", "{after}");
        // `storage.raw_retention = forever`: the raw tier's own job leaves the rest alone.
        let report = r.sched.run_job(Job::RawRetention, now, true);
        assert_eq!(report.detail, "retention is forever");
        assert_eq!(r.chunk_seqs("IMP"), imported);
        // Run again: nothing left to delete, and no second history row.
        let again = r.sched.run_job(Job::CaptureRetention, now, true);
        assert_eq!(again.outcome, JobOutcome::Done, "{again:?}");
        assert_eq!(r.chunk_seqs("IMP"), imported);
        assert_eq!(
            r.count("SELECT count(*) FROM events WHERE action = 'raw.retention'"),
            1
        );
    }

    /// §3.5: the old chunks of a live session go, while its cursor (`capture_state`, newer chunks
    /// still there) and the keys of every turn stay, so the file is not captured again.
    #[test]
    fn capture_retention_keeps_cursor_and_keys_of_a_live_source() {
        use alice_core::store::capture;
        let r = rig_with("[capture]\nraw_retention = \"180d\"\n");
        let c = r.sql();
        r.transcript_source(&c, "CAP", "live-session", true);
        r.transcript_chunks(&c, "CAP", 0..4, 200, 40);
        let fresh = r.transcript_chunks(&c, "CAP", 4..6, 1, 40);
        let before = capture::by_source(&c, "CAP").unwrap().unwrap();
        assert_eq!((before.next_ord, before.byte_offset), (6, 6 * 40));
        let report = r
            .sched
            .run_job(Job::CaptureRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(
            r.chunk_seqs("CAP"),
            fresh,
            "the old chunks go, the new stay"
        );
        assert_eq!(
            capture::by_source(&c, "CAP").unwrap(),
            Some(before),
            "the cursor stays where it was"
        );
        let keys: Vec<String> = (0..6).map(|ord| format!("CAP-turn{ord}")).collect();
        assert_eq!(
            capture::known_turns(&c, "claude-code", &keys)
                .unwrap()
                .len(),
            6,
            "the keys of the deleted turns stay too"
        );
    }

    /// By size, `capture.raw_retention` counts the text of captured chunks only and deletes the
    /// oldest of them until the rest fit; imported chunks neither count nor go.
    #[test]
    fn capture_retention_by_size() {
        // 10 captured chunks of 300 kB kept at 1 MB: the 7 oldest go. 5 imported chunks of
        // 300 kB, stored before them (and 1.5 MB on their own), stay.
        let r = rig_with("[capture]\nraw_retention = \"1MB\"\n");
        let c = r.sql();
        r.transcript_source(&c, "IMP", "imported-session", false);
        r.transcript_source(&c, "CAP", "captured-session", true);
        let imported = r.transcript_chunks(&c, "IMP", 0..5, 300, 300_000);
        let captured = r.transcript_chunks(&c, "CAP", 0..10, 2, 300_000);
        let report = r
            .sched
            .run_job(Job::CaptureRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(
            r.chunk_seqs("CAP"),
            captured[7..].to_vec(),
            "the newest stay"
        );
        assert_eq!(r.chunk_seqs("IMP"), imported);
        let after = r.retention_event();
        assert_eq!(after["deleted"], 7, "{after}");
        assert_eq!(after["reason"], "capture: 1MB", "{after}");
        // Forever (the default): nothing happens.
        let r2 = rig();
        let c2 = r2.sql();
        r2.transcript_source(&c2, "CAP", "captured-session", true);
        let kept = r2.transcript_chunks(&c2, "CAP", 0..3, 400, 40);
        let report = r2
            .sched
            .run_job(Job::CaptureRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(report.detail, "capture retention is forever");
        assert_eq!(r2.chunk_seqs("CAP"), kept);
    }

    /// R25: on a tenant `ALICE_CAPTURE_RAW_RETENTION` (in the env file `alice-server` writes)
    /// pins the retention. A PATCH to `forever` (`SettingsHandle::patch`, what the HTTP PATCH
    /// runs) is saved, but the answer says the variable overrides it, and the job still deletes.
    #[test]
    fn the_env_pin_wins_over_a_patch() {
        let r = rig_with_env("", &[("ALICE_CAPTURE_RAW_RETENTION", "180d")]);
        let mut change = serde_json::Map::new();
        change.insert("capture.raw_retention".into(), serde_json::json!("forever"));
        let out = r.settings.patch(&change).unwrap();
        assert_eq!(
            out.overridden_by_env,
            vec!["capture.raw_retention".to_string()],
            "{out:?}"
        );
        // The answer's view gives the key's source: the variable, by name.
        let source =
            serde_json::to_string(&r.settings.resolved().sources["capture.raw_retention"]).unwrap();
        assert!(source.contains("ALICE_CAPTURE_RAW_RETENTION"), "{source}");
        let file = std::fs::read_to_string(r.home.config_file()).unwrap();
        assert!(file.contains("forever"), "the patch is saved: {file}");
        assert_eq!(
            r.settings.get().capture.raw_retention,
            Retention::Age(Duration::from_secs(180 * 86_400))
        );
        let c = r.sql();
        r.transcript_source(&c, "CAP", "captured-session", true);
        r.transcript_chunks(&c, "CAP", 0..3, 200, 40);
        let report = r
            .sched
            .run_job(Job::CaptureRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert!(r.chunk_seqs("CAP").is_empty(), "the pinned 180d applies");
        assert_eq!(r.retention_event()["reason"], "capture: 180d");
    }

    /// R11: a capture source whose chunks all aged out stays, with its `capture_state` row (the
    /// cursor) and its turn keys, so its session still counts as captured and the MemPalace
    /// importer still skips that session's transcript.
    #[test]
    fn capture_retention_keeps_an_emptied_capture_source_and_its_import_skip() {
        use alice_core::import::plan::{CAPTURED_BY_ALICE_CAPTURE, Planned};
        use alice_core::store::capture;
        let r = rig_with("[capture]\nraw_retention = \"180d\"\n");
        let c = r.sql();
        let session = "0b9e6a52-5c1d-4f7e-9a01-6d2b8c3e4f50";
        r.transcript_source(&c, "CAP", session, true);
        r.transcript_chunks(&c, "CAP", 0..3, 200, 40);
        let before = capture::by_source(&c, "CAP").unwrap().unwrap();
        let report = r
            .sched
            .run_job(Job::CaptureRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert!(r.chunk_seqs("CAP").is_empty(), "every chunk aged out");
        assert_eq!(
            r.count("SELECT count(*) FROM sources WHERE id = 'CAP'"),
            1,
            "the emptied capture source stays"
        );
        assert_eq!(
            capture::by_source(&c, "CAP").unwrap(),
            Some(before),
            "with its cursor"
        );
        assert_eq!(
            r.count("SELECT count(*) FROM capture_turns WHERE source_id = 'CAP'"),
            3,
            "and its turn keys"
        );
        let captured = capture::captured_session_ids(&c).unwrap();
        assert!(captured.contains(session), "{captured:?}");
        let planned = plan_transcript_of(session, &captured);
        assert!(
            matches!(
                planned,
                Planned::Skipped {
                    reason: CAPTURED_BY_ALICE_CAPTURE
                }
            ),
            "an import of the session's transcript is still skipped: {planned:?}"
        );
        // The skip is capture's: with no session held, the same drawer is not skipped for it.
        let free = plan_transcript_of(session, &Default::default());
        assert!(
            !matches!(
                free,
                Planned::Skipped {
                    reason: CAPTURED_BY_ALICE_CAPTURE
                }
            ),
            "{free:?}"
        );
    }

    /// R11 through the raw tier's own job: a `storage.raw_retention` that empties a capture
    /// source keeps it (with its cursor and turn keys); an emptied source of another kind goes,
    /// as before.
    #[test]
    fn raw_retention_keeps_an_emptied_capture_source_and_drops_an_emptied_other_one() {
        use alice_core::store::capture;
        let r = rig_with("[storage]\nraw_retention = \"30d\"\n");
        let c = r.sql();
        r.transcript_source(&c, "CAP", "captured-session", true);
        r.transcript_source(&c, "IMP", "imported-session", false);
        r.transcript_chunks(&c, "CAP", 0..3, 60, 40);
        r.transcript_chunks(&c, "IMP", 0..3, 60, 40);
        let before = capture::by_source(&c, "CAP").unwrap().unwrap();
        let report = r.sched.run_job(Job::RawRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(r.count("SELECT count(*) FROM chunks"), 0);
        assert_eq!(
            r.count("SELECT count(*) FROM sources WHERE id = 'CAP'"),
            1,
            "the emptied capture source stays"
        );
        assert_eq!(capture::by_source(&c, "CAP").unwrap(), Some(before));
        assert_eq!(
            r.count("SELECT count(*) FROM capture_turns WHERE source_id = 'CAP'"),
            3
        );
        assert!(
            capture::captured_session_ids(&c)
                .unwrap()
                .contains("captured-session")
        );
        assert_eq!(
            r.count("SELECT count(*) FROM sources WHERE id = 'IMP'"),
            0,
            "an emptied source that is not captured goes"
        );
    }

    /// B3: the events retention also prunes the judge cache, by age and by a cap.
    #[test]
    fn events_prune_also_prunes_the_judge_cache() {
        let r = rig();
        let c = r.sql();
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::days(400))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let fresh =
            (now - chrono::Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        c.execute("INSERT INTO events (id, ts, hlc, actor, action, target_type, target_id) VALUES ('E1', ?1, 'h', 'user', 'memory.edit', 'memory', 'X')", [&old]).unwrap();
        c.execute("INSERT INTO events (id, ts, hlc, actor, action, target_type, target_id) VALUES ('E2', ?1, 'h', 'user', 'memory.edit', 'memory', 'X')", [&fresh]).unwrap();
        for (h, t) in [("old1", &old), ("old2", &old), ("new1", &fresh)] {
            c.execute("INSERT INTO judge_cache (request_hash, provider, model, response_json, created_at) VALUES (?1, 'jev', 'm', '{}', ?2)", params![h, t]).unwrap();
        }
        let report = r.sched.run_job(Job::EventsPrune, now, true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert!(
            report.detail.contains("1 events") && report.detail.contains("2 judge-cache"),
            "{}",
            report.detail
        );
        assert_eq!(
            r.count("SELECT count(*) FROM events WHERE id IN ('E1','E2')"),
            1
        );
        assert_eq!(r.count("SELECT count(*) FROM judge_cache"), 1);
    }

    /// B5: the retry job requeues memories that failed; imports stay skipped.
    #[test]
    fn judge_retry_requeues_failures_and_never_touches_imports() {
        let r = rig();
        let c = r.sql();
        let retry = r.raw_memory(&c, 1, "pending", "2026-10-01T00:00:00.000Z", "retry");
        let skipped = r.raw_memory(&c, 2, "pending", "2026-10-01T00:00:00.000Z", "skipped");
        let report = r.sched.run_job(Job::JudgeRetry, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        let state = |seq: i64| -> String {
            c.query_row(
                "SELECT judge_state FROM memories WHERE seq = ?1",
                [seq],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(state(retry), "pending");
        assert_eq!(state(skipped), "skipped");
        assert!(
            r.judge_woken.try_recv().is_ok(),
            "the judge worker is woken"
        );
    }

    /// B4: a drifted index is repaired in place by key; no rebuild.
    #[test]
    fn index_check_repairs_a_drifted_index_in_place() {
        let r = rig();
        let seqs = r.insert_embedded_memories(31);
        let idx = r.index.read().clone().unwrap();
        // The last memory was purged (row and vector gone) but the saved index still holds it,
        // and another vector was lost: the counts balance at 30.
        let purged = seqs[30];
        let c = r.sql();
        c.execute("DELETE FROM embeddings WHERE owner_seq = ?1", [purged])
            .unwrap();
        c.execute("DELETE FROM memories WHERE seq = ?1", [purged])
            .unwrap();
        idx.remove(VecKey::Memory(seqs[7])).unwrap();
        assert_eq!(idx.len(), 30);
        let report = r.sched.run_job(Job::IndexCheck, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert!(
            report.detail.contains("added 1, removed 1") && !report.detail.contains("rebuild"),
            "{report:?}"
        );
        let same = r.index.read().clone().unwrap();
        assert!(Arc::ptr_eq(&idx, &same), "the same index, repaired");
        assert!(idx.contains(VecKey::Memory(seqs[7])) && !idx.contains(VecKey::Memory(purged)));
        let again = r.sched.run_job(Job::IndexCheck, chrono::Utc::now(), true);
        assert!(again.detail.contains("all 30 vectors"), "{again:?}");
    }

    /// B4: only when catching up cannot work (a stored vector the index cannot take) is the
    /// index rebuilt.
    #[test]
    fn index_check_rebuilds_only_when_catching_up_cannot() {
        let r = rig();
        let seqs = r.insert_embedded_memories(30);
        r.index
            .read()
            .clone()
            .unwrap()
            .remove(VecKey::Memory(seqs[0]))
            .unwrap();
        // A stored vector of the wrong size: cannot be added to the index.
        let c = r.sql();
        let bad = r.raw_memory(&c, 500, "active", "2026-10-01T00:00:00.000Z", "done");
        embeddings::put(&c, VecKey::Memory(bad), "fake", &[1.0, 2.0]).unwrap();
        let report = r.sched.run_job(Job::IndexCheck, chrono::Utc::now(), true);
        assert!(report.detail.contains("rebuild started"), "{report:?}");
        // The rebuild skips the vector it cannot take and holds the other 30.
        r.wait_index_len(30);
    }

    #[test]
    fn merge_suggestions_cluster_near_duplicates_once() {
        let r = rig();
        r.insert_memories_with_vectors(&[
            ("deploy the staging server", [1.0, 0.0, 0.0]),
            ("deploy the staging server", [0.99, 0.05, 0.0]),
            ("C", [0.0, 1.0, 0.0]),
        ]);
        let report = r
            .sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(
            r.count("SELECT count(*) FROM review_items WHERE type = 'merge' AND status = 'open'"),
            1
        );
        r.sched.run_job(
            Job::MergeSuggestions,
            chrono::Utc::now() + chrono::Duration::days(8),
            true,
        );
        assert_eq!(
            r.count("SELECT count(*) FROM review_items WHERE type = 'merge'"),
            1,
            "asked once"
        );
        // A rejected suggestion is not made again once its item is closed.
        r.sql()
            .execute("UPDATE review_items SET status = 'resolved', resolution = 'reject', resolved_at = 'x', resolved_by = 'user'", [])
            .unwrap();
        r.sched.run_job(
            Job::MergeSuggestions,
            chrono::Utc::now() + chrono::Duration::days(16),
            true,
        );
        assert_eq!(
            r.count("SELECT count(*) FROM review_items WHERE type = 'merge'"),
            1,
            "rejected: never suggested again"
        );
    }

    /// `n` clusters of two near-identical vectors, 2 degrees apart from each other (so a high
    /// `judge.cluster_similarity` pairs only the twins).
    fn twin_clusters(n: usize) -> Vec<(&'static str, [f32; 3])> {
        (0..n)
            .flat_map(|k| {
                let a = (k as f32 * 2.0).to_radians();
                let b = a + 0.2_f32.to_radians();
                [
                    ("twin", [a.cos(), a.sin(), 0.0]),
                    ("twin", [b.cos(), b.sin(), 0.0]),
                ]
            })
            .collect()
    }

    const TWINS: &str = "[judge]\ncluster_similarity = 0.9999\n";
    const OPEN_MERGES: &str =
        "SELECT count(*) FROM review_items WHERE type = 'merge' AND status = 'open'";

    #[test]
    fn merge_suggestions_wait_while_fifty_merge_items_are_open() {
        let r = rig_with(TWINS);
        r.insert_memories_with_vectors(&twin_clusters(3));
        let c = r.sql();
        for k in 0..50 {
            alice_core::store::reviews::create(
                &c,
                &reviews::NewReview {
                    kind: reviews::ReviewType::Merge,
                    payload: serde_json::json!({}),
                    confidence: Some(0.92),
                    dedupe_key: Some(format!("open{k}")),
                },
            )
            .unwrap();
        }
        let report = r
            .sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Skipped, "{report:?}");
        assert!(
            report
                .detail
                .contains("skipped: 50 merge reviews already open"),
            "{report:?}"
        );
        assert_eq!(r.count(OPEN_MERGES), 50, "none created");
        // Room again: it resumes where it stopped and makes the suggestions.
        c.execute(
            "UPDATE review_items SET status = 'resolved', resolution = 'reject', resolved_at = 'x', resolved_by = 'user' WHERE dedupe_key = 'open0'",
            [],
        )
        .unwrap();
        let again = r
            .sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert_eq!(again.outcome, JobOutcome::Skipped, "{again:?}");
        assert_eq!(
            r.count(OPEN_MERGES),
            50,
            "one slot: one suggestion, then full"
        );
    }

    #[test]
    fn merge_suggestions_create_at_most_fifty_in_one_run() {
        let r = rig_with(TWINS);
        r.insert_memories_with_vectors(&twin_clusters(60));
        let report = r
            .sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert_eq!(r.count(OPEN_MERGES), 50, "{report:?}");
        assert!(
            report.detail.contains("50 merge reviews already open"),
            "{report:?}"
        );
    }

    /// A run of vectors `step_deg` apart in a plane (every pair has the cosine of its angle).
    fn fan(n: usize, step_deg: f32) -> Vec<(&'static str, [f32; 3])> {
        (0..n)
            .map(|k| {
                let a = (k as f32 * step_deg).to_radians();
                ("fan", [a.cos(), a.sin(), 0.0])
            })
            .collect()
    }

    /// The memory ids of every open merge item, one list per item.
    fn open_groups(r: &Rig) -> Vec<Vec<String>> {
        let c = r.sql();
        let mut stmt = c
            .prepare(
                "SELECT payload_json FROM review_items WHERE type = 'merge' AND status = 'open'",
            )
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|p| {
                let v: serde_json::Value = serde_json::from_str(&p.unwrap()).unwrap();
                let mut ids = vec![v["survivor"].as_str().unwrap().to_string()];
                ids.extend(
                    v["others"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|o| o.as_str().unwrap().to_string()),
                );
                ids
            })
            .collect()
    }

    const NEAR: &str = "[judge]\ncluster_similarity = 0.99\n";

    #[test]
    fn merge_suggestions_never_cluster_diaries() {
        let r = rig_with(NEAR);
        r.insert_memories_with_vectors(&fan(4, 0.1));
        r.sql()
            .execute("UPDATE memories SET kind = 'diary' WHERE seq IN (SELECT seq FROM memories LIMIT 2)", [])
            .unwrap();
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        // The two diaries are alike but are session logs, not duplicates; the other two pair.
        let groups = open_groups(&r);
        assert_eq!(groups.len(), 1, "{groups:?}");
        let diaries: Vec<String> = {
            let c = r.sql();
            let mut stmt = c
                .prepare("SELECT id FROM memories WHERE kind = 'diary'")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(|x| x.unwrap())
                .collect()
        };
        assert!(
            groups[0].iter().all(|id| !diaries.contains(id)),
            "{groups:?} {diaries:?}"
        );
    }

    /// SP2 spec §6.6: distilled memories are left to the judge, as diaries are left out: never
    /// put in a merge suggestion.
    #[test]
    fn merge_suggestions_skip_distilled_memories() {
        let r = rig_with(NEAR);
        r.insert_memories_with_vectors(&fan(4, 0.1));
        r.sql()
            .execute(
                "UPDATE memories SET origin = 'distill', judge_state = 'done'
                 WHERE seq IN (SELECT seq FROM memories ORDER BY seq LIMIT 2)",
                [],
            )
            .unwrap();
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        // The two distilled memories are alike, and alike the other two; only those two pair.
        let groups = open_groups(&r);
        assert_eq!(groups.len(), 1, "{groups:?}");
        let distilled: Vec<String> = {
            let c = r.sql();
            let mut stmt = c
                .prepare("SELECT id FROM memories WHERE origin = 'distill'")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(|x| x.unwrap())
                .collect()
        };
        assert_eq!(distilled.len(), 2);
        assert_eq!(groups[0].len(), 2, "{groups:?}");
        assert!(
            groups[0].iter().all(|id| !distilled.contains(id)),
            "{groups:?} {distilled:?}"
        );
    }

    #[test]
    fn merge_suggestions_need_every_pair_alike_not_a_chain() {
        let r = rig_with(NEAR);
        // 0 and 5 degrees, 5 and 10 degrees are alike (cos 0.996); 0 and 10 are not (0.985).
        r.insert_memories_with_vectors(&fan(3, 5.0));
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        let groups = open_groups(&r);
        assert!(!groups.is_empty(), "the near pair is still suggested");
        assert!(groups.iter().all(|g| g.len() == 2), "{groups:?}");
    }

    #[test]
    fn merge_suggestions_never_put_a_memory_in_two_open_items() {
        let r = rig_with(NEAR);
        r.insert_memories_with_vectors(&fan(6, 5.0));
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        // A second run, as the weekly job would: nothing already open is taken again.
        r.sched.run_job(
            Job::MergeSuggestions,
            chrono::Utc::now() + chrono::Duration::days(8),
            true,
        );
        let groups = open_groups(&r);
        assert!(!groups.is_empty());
        let mut seen = std::collections::HashSet::new();
        for id in groups.iter().flatten() {
            assert!(seen.insert(id.clone()), "{id} is in two items: {groups:?}");
        }
    }

    #[test]
    fn merge_suggestions_group_at_most_five_memories() {
        let r = rig_with(NEAR);
        r.insert_memories_with_vectors(&fan(8, 0.01));
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        let groups = open_groups(&r);
        assert!(!groups.is_empty());
        assert!(groups.iter().all(|g| g.len() <= 5), "{groups:?}");
        assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), 8, "{groups:?}");
    }

    /// Three near vectors (a few degrees apart: cosine above 0.99 for every pair).
    fn near3() -> [[f32; 3]; 3] {
        let v = |deg: f32| {
            let a = deg.to_radians();
            [a.cos(), a.sin(), 0.0]
        };
        [v(0.0), v(3.0), v(6.0)]
    }

    #[test]
    fn merge_suggestions_need_the_texts_to_agree_not_only_the_vectors() {
        let r = rig_with(NEAR);
        let [a, b, c] = near3();
        // Status-shaped notes of one project for different milestones: same template, close
        // vectors, different content.
        r.insert_kinded(&[
            (
                "note",
                "STATUS 2026-09-07 21:30: Tailscale CLI TERM fix merged to main 5685ea1",
                a,
            ),
            (
                "note",
                "STATUS 2026-09-08 10:05: M3 reconnect lifecycle watcher MERGED to main 9f3c2d1",
                b,
            ),
            (
                "note",
                "STATUS 2026-09-09 18:40: M4 Files SFTP browser upload MERGED to main 77aa001",
                c,
            ),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert!(open_groups(&r).is_empty(), "{:?}", open_groups(&r));
    }

    /// Final review B-I2 (ruling): numbers are compared as written, as a multiset. The same
    /// numbers in other words still group; a different date, time or id does not.
    #[test]
    fn merge_suggestions_group_texts_only_when_their_numbers_are_the_same() {
        let r = rig_with(NEAR);
        let [a, b, _] = near3();
        r.insert_kinded(&[
            (
                "note",
                "Restart nginx on the staging server after a deploy 2026-09-07 21:30 abc1234",
                a,
            ),
            (
                "note",
                "Restart nginx on the staging server after the deploy, 21:30 2026-09-07 abc1234",
                b,
            ),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert_eq!(open_groups(&r).len(), 1, "{:?}", open_groups(&r));
        let r = rig_with(NEAR);
        r.insert_kinded(&[
            (
                "note",
                "Restart nginx on the staging server after a deploy 2026-09-07 21:30 abc1234",
                a,
            ),
            (
                "note",
                "Restart nginx on the staging server after a deploy 2026-10-01 08:15 f00d99",
                b,
            ),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert!(open_groups(&r).is_empty(), "{:?}", open_groups(&r));
    }

    /// Final review B-I2 (ruling): a value update is never offered as a duplicate: a port
    /// change and a version change, whose words and vectors agree, are not grouped.
    #[test]
    fn merge_suggestions_never_group_a_port_or_a_version_change() {
        let [a, b, c] = near3();
        for (old, new) in [
            ("Staging DB port is 5432", "Staging DB port is 6543"),
            (
                "pin the client library to version 1.4 in the lock file",
                "pin the client library to version 2.0 in the lock file",
            ),
            (
                "the monthly budget is 50 USD",
                "the monthly budget is 500 USD",
            ),
        ] {
            let r = rig_with(NEAR);
            r.insert_kinded(&[("fact", old, a), ("fact", new, b), ("fact", old, c)]);
            r.sched
                .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
            let groups = open_groups(&r);
            assert_eq!(groups.len(), 1, "only the two equal ones: {groups:?}");
            assert_eq!(groups[0].len(), 2, "{old} / {new}: {groups:?}");
        }
        assert_eq!(
            number_tokens(
                "v1.4.2 on port 5432,",
                "since 2026-10-16 21:30, 1,000 users"
            ),
            ["1,000", "1.4.2", "2026-10-16", "21:30", "5432"]
        );
    }

    #[test]
    fn merge_suggestions_never_group_status_notes() {
        let r = rig_with(NEAR);
        let [a, b, _] = near3();
        r.insert_kinded(&[
            ("status", "project is on track and building", a),
            ("status", "project is on track and building", b),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert!(open_groups(&r).is_empty(), "{:?}", open_groups(&r));
    }

    #[test]
    fn merge_suggestions_never_group_different_kinds() {
        let r = rig_with(NEAR);
        let [a, b, _] = near3();
        r.insert_kinded(&[
            ("note", "use sqlite wal mode for the store", a),
            ("decision", "use sqlite wal mode for the store", b),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert!(open_groups(&r).is_empty(), "{:?}", open_groups(&r));
    }

    #[test]
    fn merge_suggestion_p_is_the_groups_lowest_pairwise_cosine() {
        let r = rig_with(NEAR);
        let [a, b, c] = near3();
        r.insert_kinded(&[
            ("note", "use sqlite wal mode for the store", a),
            ("note", "use sqlite wal mode for the store", b),
            ("note", "use sqlite wal mode for the store", c),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        let sizes: Vec<usize> = open_groups(&r).iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![3]);
        let p: f64 = {
            let json: String = r
                .sql()
                .query_row(
                    "SELECT payload_json FROM review_items WHERE type = 'merge'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["p"]
                .as_f64()
                .unwrap()
        };
        let lowest = cosine(&a, &c);
        // Stored vectors are not bit-exact (quantised), hence the tolerance.
        assert!((p - lowest).abs() < 1e-4, "p = {p}, lowest = {lowest}");
        assert!(lowest < 0.9999 && p < cosine(&a, &b), "{p}");
    }

    #[test]
    fn merge_suggestions_compare_thai_on_trigrams() {
        let r = rig_with(NEAR);
        let [a, b, c] = near3();
        r.insert_kinded(&[
            ("note", "ขั้นตอนการติดตั้งเซิร์ฟเวอร์ใหม่บนอูบุนตู", a),
            ("note", "ขั้นตอนการติดตั้งเซิร์ฟเวอร์ใหม่บนอูบุนตูด้วย", b),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert_eq!(open_groups(&r).len(), 1, "a Thai near-duplicate is grouped");
        let r = rig_with(NEAR);
        r.insert_kinded(&[
            ("note", "สถานะ ติดตั้งเครื่องแม่ข่ายใหม่เรียบร้อยแล้ว", a),
            ("note", "สถานะ แก้ไขปัญหาการเชื่อมต่อฐานข้อมูลสำเร็จ", c),
        ]);
        r.sched
            .run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert!(open_groups(&r).is_empty(), "{:?}", open_groups(&r));
    }

    #[test]
    fn backups_run_rotate_and_adopt_old_files() {
        let r = rig();
        std::fs::write(
            r.home
                .root()
                .join("alice.pre-migration-2026-10-13T09-44-23-391Z.db"),
            b"x",
        )
        .unwrap();
        // B2: a secrets file next to the database is never copied.
        std::fs::write(r.home.secrets_file(), "typesafe_api_key = \"x\"\n").unwrap();
        let report = r.sched.run_job(Job::Backup, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        let backups = crate::backup::list(&r.home.backups_dir()).unwrap();
        assert!(backups.iter().any(|b| b.kind == BackupKind::Scheduled));
        assert!(backups.iter().any(|b| b.kind == BackupKind::PreMigration));
        let names: Vec<String> = std::fs::read_dir(r.home.backups_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| n.starts_with("alice-") && n.ends_with(".db")),
            "{names:?}"
        );
    }

    /// A failed backup records the failure, leaves no file and is tried again within the hour.
    #[test]
    fn a_failed_backup_is_recorded_and_leaves_no_file() {
        let r = rig_with("[backup]\ndir = \"{ROOT}/blocker/backups\"\n");
        // The directory cannot be made: its parent is a file.
        std::fs::write(r.dir.path().join("blocker"), b"x").unwrap();
        let now = chrono::Utc::now();
        // I-2: the copy comes first; the old pre-migration file is not touched when it fails.
        let legacy = r
            .home
            .root()
            .join("alice.pre-migration-2026-10-13T09-44-23-391Z.db");
        std::fs::write(&legacy, b"x").unwrap();
        let bad = r.sched.run_job(Job::Backup, now, true);
        assert!(legacy.exists(), "adoption waits for a successful backup");
        assert_eq!(bad.outcome, JobOutcome::Failed, "{bad:?}");
        assert!(r.sched.meta("maint.backup.last_error").is_some());
        assert!(r.sched.meta("maint.backup.last_ok").is_none());
        assert!(!r.home.backups_dir().join("alice-x.db").exists());
        assert!(
            r.sched
                .due(now + chrono::Duration::minutes(61))
                .contains(&Job::Backup)
        );
        assert!(
            !r.sched
                .due(now + chrono::Duration::minutes(5))
                .contains(&Job::Backup)
        );
    }

    #[test]
    fn quick_check_passes_on_a_sound_database_and_fts_optimize_merges() {
        let r = rig();
        for job in [Job::QuickCheck, Job::FtsOptimize] {
            let report = r.sched.run_job(job, chrono::Utc::now(), true);
            assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        }
        assert!(r.writer.read_only.reason().is_none());
    }

    /// `alice bench` starts its daemons with this variable, so no job runs under a measurement.
    #[test]
    fn the_environment_can_switch_maintenance_off() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let h = SettingsHandle::load_with_env(
            &home,
            vec![("ALICE_MAINTENANCE_ENABLED".into(), "false".into())],
        );
        assert!(!h.get().maintenance.enabled);
        assert!(h.errors().is_empty());
        assert!(
            SettingsHandle::load_with_env(&home, Vec::new())
                .get()
                .maintenance
                .enabled
        );
    }

    /// I-1 through the scheduler: an import authored in 2020 and stored today survives `90d`.
    #[test]
    fn an_imported_chunk_authored_long_ago_survives_age_retention() {
        let r = rig_with("[storage]\nraw_retention = \"90d\"\n");
        let c = r.sql();
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        c.execute("INSERT INTO sources (id, project_id, type, uri, created_at) VALUES ('S1', ?1, 'import', 'u', ?2)", params![r.global(), now]).unwrap();
        for (i, authored) in ["2020-01-01T00:00:00.000Z", "0", "", "10/13/2020"]
            .iter()
            .enumerate()
        {
            c.execute(
                "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, authored_at, created_at, embed_state)
                 VALUES (?1, 'S1', ?2, ?3, 'history', ?4, ?5, ?6, 'done')",
                params![format!("C{i}"), r.global(), i as i64, format!("h{i}"), authored, now],
            )
            .unwrap();
        }
        let report = r.sched.run_job(Job::RawRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert_eq!(r.count("SELECT count(*) FROM chunks"), 4);
        assert!(
            report.detail.contains("0 chunks deleted"),
            "{}",
            report.detail
        );
    }

    /// I-3: no room for a second copy is a failure with a plain reason, no file and no
    /// adoption, retried after 12 hours (not 1); policy rotation is tried first.
    #[test]
    fn a_backup_without_room_fails_plainly_and_is_retried_in_twelve_hours() {
        let free = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
        let probe = free.clone();
        let r = rig_custom("", move |ctx| {
            ctx.free_space = Arc::new(move |_| Some(probe.load(Ordering::SeqCst)));
        });
        let legacy = r
            .home
            .root()
            .join("alice.pre-migration-2026-10-13T09-44-23-391Z.db");
        std::fs::write(&legacy, b"x").unwrap();
        let now = chrono::Utc::now();
        let report = r.sched.run_job(Job::Backup, now, true);
        assert_eq!(report.outcome, JobOutcome::Failed, "{report:?}");
        assert!(report.detail.starts_with(DISK_SPACE), "{}", report.detail);
        assert!(
            report.detail.contains("free some space"),
            "{}",
            report.detail
        );
        assert!(legacy.exists(), "nothing was moved");
        assert!(
            crate::backup::list(&r.home.backups_dir())
                .unwrap()
                .is_empty()
        );
        assert!(
            !r.sched
                .due(now + chrono::Duration::hours(11))
                .contains(&Job::Backup)
        );
        assert!(
            r.sched
                .due(now + chrono::Duration::hours(13))
                .contains(&Job::Backup)
        );
        // Room again: it goes through.
        free.store(u64::MAX / 2, Ordering::SeqCst);
        let report = r.sched.run_job(Job::Backup, now, true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
    }

    #[test]
    fn rotation_makes_room_before_a_backup_gives_up() {
        // Room exists only once the directory holds at most 11 files (the policy).
        let dir_probe: Arc<std::sync::OnceLock<std::path::PathBuf>> = Arc::default();
        let seen = dir_probe.clone();
        let r = rig_custom("", move |ctx| {
            ctx.free_space = Arc::new(move |p| {
                let n = std::fs::read_dir(p).map(|d| d.count()).unwrap_or(0);
                let _ = seen.set(p.to_path_buf());
                Some(if n > 11 { 0 } else { u64::MAX / 2 })
            });
        });
        let now = chrono::Utc::now();
        std::fs::create_dir_all(r.home.backups_dir()).unwrap();
        for day in 1..=40 {
            let t = now - chrono::Duration::days(day);
            std::fs::write(
                r.home
                    .backups_dir()
                    .join(format!("alice-{}.db", t.format("%Y%m%dT%H%M%SZ"))),
                b"x",
            )
            .unwrap();
        }
        let report = r.sched.run_job(Job::Backup, now, true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert!(crate::backup::list(&r.home.backups_dir()).unwrap().len() <= 12);
    }

    /// M-3: a check that had nothing to check is skipped: not a run, not an "ok".
    #[test]
    fn a_skipped_check_is_not_stamped_as_a_run() {
        let r = rig();
        *r.index.write() = None; // being rebuilt
        for job in [Job::IndexCheck, Job::MergeSuggestions] {
            let report = r.sched.run_job(job, chrono::Utc::now(), true);
            assert_eq!(report.outcome, JobOutcome::Skipped, "{report:?}");
            assert!(
                r.sched
                    .meta(&format!("maint.{}.last_run", job.name()))
                    .is_none()
            );
            assert!(
                r.sched
                    .meta(&format!("maint.{}.last_ok", job.name()))
                    .is_none()
            );
        }
        assert!(r.sched.due(chrono::Utc::now()).contains(&Job::IndexCheck));
    }

    /// M-4: a panic in a job is that run's failure.
    #[test]
    fn a_panicking_job_is_a_failed_run_not_a_dead_scheduler() {
        /// An embedder whose dimension cannot be read.
        struct Broken;
        impl alice_core::embed::Embedder for Broken {
            fn model_id(&self) -> &str {
                "fake"
            }
            fn dim(&self) -> usize {
                panic!("boom")
            }
            fn embed_passages(&self, _: &[String]) -> Result<Vec<Vec<f32>>> {
                unreachable!()
            }
            fn embed_query(&self, _: &str) -> Result<Vec<f32>> {
                unreachable!()
            }
        }
        let r = rig_custom("", |ctx| {
            ctx.active = Arc::new(RwLock::new(Arc::new(crate::service::Active {
                embedder: Arc::new(Broken),
                model: "fake".into(),
                vector_path: ctx.home.vector_file("fake"),
            })))
        });
        let report = r.sched.run_job(Job::IndexCheck, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Failed, "{report:?}");
        assert!(
            report.detail.contains("panicked") && report.detail.contains("boom"),
            "{}",
            report.detail
        );
        assert!(r.sched.meta("maint.index_check.last_error").is_some());
        // The scheduler still works.
        let ok = r.sched.run_job(Job::JudgeRetry, chrono::Utc::now(), true);
        assert_eq!(ok.outcome, JobOutcome::Done);
    }

    /// M-5: a damaged database stops writes and says so; the job fails and no backup follows.
    #[test]
    fn quick_check_finding_damage_stops_writes_and_names_the_next_step() {
        let r = rig();
        r.insert_embedded_memories(200);
        // Everything into the main file, then damage its middle.
        r.sql()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let db = r.home.db();
        let mut bytes = std::fs::read(&db).unwrap();
        assert!(bytes.len() > 40_000, "{}", bytes.len());
        for b in &mut bytes[16_384..32_768] {
            *b = 0xA5;
        }
        std::fs::write(&db, bytes).unwrap();
        let report = r.sched.run_job(Job::QuickCheck, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Failed, "{report:?}");
        assert!(
            report.detail.contains("writes are stopped") && report.detail.contains("backup"),
            "{}",
            report.detail
        );
        let reason = r.writer.read_only.reason().expect("read-only");
        assert!(reason.contains("quick_check"), "{reason}");
        assert!(r.sched.meta("maint.quick_check.last_error").is_some());
    }

    /// Holds a running statement at its first progress callback until the test lets it go, so an
    /// interrupt sent meanwhile lands inside the statement however the threads are scheduled.
    #[derive(Default)]
    struct Gate {
        running: AtomicBool,
        go: AtomicBool,
    }

    /// SQLite's progress callback: announces that the statement is running, then waits for `go`.
    unsafe extern "C" fn hold_at_gate(arg: *mut std::ffi::c_void) -> std::ffi::c_int {
        // SAFETY: `arg` is the `Gate` the test registered; the test keeps it alive until the
        // job has ended, and the callback runs only inside a statement of that job.
        let gate = unsafe { &*(arg as *const Gate) };
        gate.running.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(60);
        while !gate.go.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        0
    }

    /// I-4: an integrity check is interrupted when the daemon stops. (SQLite honours it inside
    /// `PRAGMA quick_check`.) SQLite drops an interrupt that arrives while no statement runs, so
    /// the test holds the check inside its statement (a progress callback every 1,000
    /// operations parks it at the first one) and interrupts only then: no timing decides it.
    /// The first callback may fire while the full-text tables are still being set up rather
    /// than inside the check proper; the job maps both interrupt outcomes (`SQLITE_INTERRUPT`
    /// and "vtable constructor failed") to Paused, and the assertion accepts either.
    #[test]
    fn a_running_integrity_check_is_interrupted_at_shutdown() {
        let r = rig();
        {
            let c = r.sql();
            c.execute_batch("CREATE TABLE junk (x BLOB)").unwrap();
            for _ in 0..100 {
                c.execute("INSERT INTO junk VALUES (randomblob(500000))", [])
                    .unwrap();
            }
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        }
        let gate = Arc::new(Gate::default());
        let held = gate.clone();
        *r.sched.quick_check_hook.lock() = Some(Arc::new(move |c: &rusqlite::Connection| {
            // SAFETY: `held` outlives the connection's use of the pointer (see `hold_at_gate`),
            // and the handle is valid while `c` is.
            unsafe {
                rusqlite::ffi::sqlite3_progress_handler(
                    c.handle(),
                    1_000,
                    Some(hold_at_gate),
                    Arc::as_ptr(&held) as *mut std::ffi::c_void,
                );
            }
        }));
        let sched = r.sched.clone();
        let t =
            std::thread::spawn(move || sched.run_job(Job::QuickCheck, chrono::Utc::now(), true));
        let started = Instant::now();
        while !gate.running.load(Ordering::SeqCst) {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the check never reached a progress callback"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // The statement is parked inside the check: this interrupt cannot be lost.
        r.sched.interrupt_running();
        gate.go.store(true, Ordering::SeqCst);
        let report = t.join().unwrap();
        assert_eq!(report.outcome, JobOutcome::Paused, "{report:?}");
        assert!(
            r.sched.meta("maint.quick_check.last_run").is_none(),
            "an interrupted check is not a run"
        );
    }

    /// An interrupted check is paused only for an interrupt or the setup failure; a corruption
    /// result stays damage even while the interrupt flag is set.
    #[test]
    fn a_corruption_result_is_damage_even_when_the_check_was_interrupted() {
        let corrupt = || {
            AliceError::Db(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some("database disk image is malformed".into()),
            ))
        };
        let setup = || {
            AliceError::Db(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some("vtable constructor failed: chunks_fts".into()),
            ))
        };
        let r = rig();
        let e = r
            .sched
            .quick_check_step(Err(corrupt()), true)
            .err()
            .unwrap();
        assert!(e.to_string().contains("writes are stopped"), "{e}");
        assert!(
            r.writer.read_only.reason().is_some(),
            "the writer went read-only"
        );
        assert!(r.sched.meta("maint.quick_check.last_error").is_some());
        // The setup failure is paused while interrupted, and a failure otherwise.
        let r = rig();
        assert!(matches!(
            r.sched.quick_check_step(Err(setup()), true),
            Ok(Step::Stopped(_))
        ));
        assert!(r.sched.quick_check_step(Err(setup()), false).is_err());
        assert!(r.writer.read_only.reason().is_none());
    }

    /// M-6: the backups being behind is a health warning, and shortens the quiet jobs wait for.
    #[test]
    fn an_overdue_backup_warns_and_shortens_the_quiet_period() {
        let r = rig_with("[maintenance]\nidle_secs = 600\n");
        let now = chrono::Utc::now();
        let s = r.settings.get().maintenance.clone();
        assert!(
            r.sched.backup_warning(now).is_none(),
            "a new data directory has no backup due"
        );
        assert_eq!(r.sched.quiet_secs(&s), 600);
        // The data directory is old and no backup was ever made.
        assert!(
            r.sched
                .backup_warning(now + chrono::Duration::days(3))
                .is_some()
        );
        // A backup of last week.
        std::fs::create_dir_all(r.home.backups_dir()).unwrap();
        let old = now - chrono::Duration::days(3);
        std::fs::write(
            r.home
                .backups_dir()
                .join(format!("alice-{}.db", old.format("%Y%m%dT%H%M%SZ"))),
            b"x",
        )
        .unwrap();
        let warning = r
            .sched
            .backup_warning(now)
            .expect("3 days old is more than 2 periods");
        assert!(
            warning.contains("3 days old") && warning.contains("alice backup now"),
            "{warning}"
        );
        assert_eq!(
            r.sched.quiet_secs(&s),
            10,
            "an overdue backup waits 10 s, not 10 min"
        );
        // A fresh one clears it.
        std::fs::write(
            r.home
                .backups_dir()
                .join(format!("alice-{}.db", now.format("%Y%m%dT%H%M%SZ"))),
            b"x",
        )
        .unwrap();
        assert!(r.sched.backup_warning(now).is_none());
        assert_eq!(r.sched.quiet_secs(&s), 600);
    }

    /// I-8: a purge of thousands of memories does not make a `remember` wait for the lot. The
    /// bound does not depend on how fast the machine is: the worst wait is compared with the
    /// purge's own duration (one slice is a small part of it; without slicing the one big write
    /// takes about all of it), plus a hang guard.
    #[test]
    fn a_remember_during_a_big_purge_waits_for_a_slice_not_for_the_purge() {
        let r = rig();
        r.insert_trashed_memories(PURGE_TEST_MEMORIES, "2020-01-01T00:00:00.000Z");
        let sched = r.sched.clone();
        let t = std::thread::spawn(move || {
            let started = Instant::now();
            let report = sched.run_job(Job::TrashPurge, chrono::Utc::now(), true);
            (report, started.elapsed())
        });
        // The two longest waits: one stalled write (a descheduled thread, a slow fsync) is not
        // the regression, a purge that holds the writer for its whole length stalls every write.
        let (mut worst, mut second) = (Duration::ZERO, Duration::ZERO);
        let mut n = 0;
        let hang_guard = Instant::now() + Duration::from_secs(300);
        while !t.is_finished() {
            assert!(Instant::now() < hang_guard, "the purge never finished");
            let started = Instant::now();
            r.writer
                .submit_blocking(WriteOp::Remember {
                    input: NewMemoryInput {
                        content: format!("a note written during the purge {n}"),
                        ..Default::default()
                    },
                    project: ProjectChoice::Slug("global".into()),
                })
                .unwrap();
            let waited = started.elapsed();
            if waited > worst {
                (worst, second) = (waited, worst);
            } else {
                second = second.max(waited);
            }
            n += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        let (report, purge) = t.join().unwrap();
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        assert!(n > 3, "only {n} writes got in during a purge of {purge:?}");
        assert!(
            second < purge / 3,
            "second-longest remember {second:?} (longest {worst:?}) over {n} writes, during a purge of {purge:?}"
        );
        assert_eq!(
            r.count("SELECT count(*) FROM memories WHERE status = 'trashed'"),
            0
        );
    }

    /// M-2: a memory restored or edited after the job listed it is not purged.
    #[test]
    fn the_writer_rechecks_the_trash_and_its_age_before_purging() {
        let r = rig();
        r.insert_trashed_memories(3, "2020-01-01T00:00:00.000Z");
        let seqs: Vec<i64> = (1..=3).collect();
        let c = r.sql();
        c.execute("UPDATE memories SET status = 'active' WHERE seq = 1", [])
            .unwrap(); // restored
        c.execute(
            "UPDATE memories SET updated_at = '2999-01-01T00:00:00.000Z' WHERE seq = 2",
            [],
        )
        .unwrap(); // touched
        let out = r
            .writer
            .submit_blocking(WriteOp::PurgeMemories {
                seqs,
                cutoff: "2026-01-01T00:00:00.000Z".into(),
            })
            .unwrap();
        let WriteOutcome::Purged(done) = out else {
            panic!("{out:?}")
        };
        assert_eq!(done, vec![3]);
        assert_eq!(r.count("SELECT count(*) FROM memories"), 2);
    }

    /// N-1 at run time: an age that does not convert deletes nothing (it once became zero).
    #[test]
    fn an_age_too_large_for_the_clock_gives_no_cutoff_and_deletes_nothing() {
        let now = chrono::Utc::now();
        assert!(age_cutoff(now, Duration::from_secs(u64::MAX)).is_none());
        assert!(age_cutoff(now, Duration::from_secs(1 << 60)).is_none());
        let ok = age_cutoff(now, Duration::from_secs(90 * 86_400)).unwrap();
        assert_eq!((now - ok).num_days(), 90);
    }

    /// N-5: a run of raw retention leaves one history row with the totals, not one per slice.
    #[test]
    fn a_retention_run_records_one_event_with_its_totals() {
        let r = rig_with("[storage]\nraw_retention = \"1MB\"\n");
        let c = r.sql();
        c.execute("INSERT INTO sources (id, project_id, type, uri, created_at) VALUES ('S1', ?1, 'file', 'u', '2026-01-01T00:00:00.000Z')", [r.global()]).unwrap();
        // 60 chunks of 100 kB (6 MB): more than one 25 ms slice, or one fast one.
        for i in 0..60 {
            c.execute(
                "INSERT INTO chunks (id, source_id, project_id, ord, text, content_hash, created_at, embed_state) VALUES (?1, 'S1', ?2, ?3, ?4, ?5, '2026-01-01T00:00:00.000Z', 'done')",
                params![format!("C{i}"), r.global(), i as i64, "x".repeat(100_000), format!("ch{i}")],
            )
            .unwrap();
        }
        let report = r.sched.run_job(Job::RawRetention, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        let left = r.count("SELECT count(*) FROM chunks");
        assert!(left <= 10, "{left}");
        assert_eq!(
            r.count("SELECT count(*) FROM events WHERE action = 'raw.retention'"),
            1
        );
        let after: String = c
            .query_row(
                "SELECT after_json FROM events WHERE action = 'raw.retention'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let after: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(after["deleted"], 60 - left);
        assert_eq!(after["reason"], "1MB");
    }

    #[test]
    fn the_job_names_round_trip_and_unknown_ones_are_listed() {
        for j in Job::ALL {
            assert_eq!(Job::parse(j.name()), Some(j));
        }
        let e = Job::parse_or_err("nope").unwrap_err().to_string();
        assert!(
            e.contains("\"nope\"") && e.contains("trash_purge") && e.contains("merge_suggestions"),
            "{e}"
        );
    }
}
