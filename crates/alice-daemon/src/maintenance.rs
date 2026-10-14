//! Maintenance (spec §5.6): jobs run on a low-priority thread when the daemon has had no
//! requests for `maintenance.idle_secs`, in short slices, and stop between slices as soon as a
//! request arrives (the next idle period continues them). Last runs live in `meta`.
//!
//! Each job keeps the writer's transactions short: deletes go 500 rows per `WriteOp`, FTS merges
//! 500 pages. Reads (what to delete, the checks, the backup) use the reader pool or their own
//! read-only connection, never the writer's.

use crate::backup::{self, BackupKind};
use crate::config::SettingsHandle;
use crate::paths::AliceHome;
use crate::service::IndexSlot;
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};
use alice_core::error::{AliceError, Result};
use alice_core::settings::{MaintenanceSettings, Retention};
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::{ReaderPool, maintenance as m, reviews};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
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
    TrashPurge,
    JudgeRetry,
    MergeSuggestions,
    EventsPrune,
}

impl Job {
    pub const ALL: [Job; 9] = [
        Job::JudgeRetry,
        Job::TrashPurge,
        Job::RawRetention,
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
            Job::TrashPurge => "trash_purge",
            Job::JudgeRetry => "judge_retry",
            Job::MergeSuggestions => "merge_suggestions",
            Job::EventsPrune => "events_prune",
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
/// that failed on a full disk does not wait a day.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(3_600);

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Done,
    /// A request arrived; the job continues in the next idle period.
    Paused,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct JobReport {
    pub job: Job,
    pub outcome: JobOutcome,
    pub detail: String,
    pub took_ms: u64,
}

pub struct MaintenanceCtx {
    pub home: AliceHome,
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    pub settings: Arc<SettingsHandle>,
    pub activity: Arc<Activity>,
    /// The embedding model whose vectors the index holds.
    pub model: Arc<dyn Fn() -> String + Send + Sync>,
    pub dim: usize,
    pub judge_wake: crossbeam_channel::Sender<()>,
    /// Set when the daemon is stopping: a job ends at its next slice.
    pub stop: Arc<AtomicBool>,
}

pub struct Scheduler {
    ctx: MaintenanceCtx,
    /// One job runs at a time, whoever started it (the timer, `alice maintenance run`).
    running: parking_lot::Mutex<()>,
}

/// Rows per write in purges and retention.
const SLICE: usize = 500;
/// Memories looked at per slice of the merge-suggestion job.
const MERGE_PAGE: i64 = 200;

enum Step {
    More,
    Finished(String),
}

/// What a job carries from one slice to the next within a run.
#[derive(Default)]
struct Scratch {
    /// Raw retention by size: bytes still to delete.
    over_bytes: Option<u64>,
    deleted: usize,
    deleted_other: usize,
}

impl Scheduler {
    pub fn new(ctx: MaintenanceCtx) -> Self {
        Self {
            ctx,
            running: parking_lot::Mutex::new(()),
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
                let every = if failed {
                    j.every().min(RETRY_AFTER_FAILURE)
                } else {
                    j.every()
                };
                now - last >= chrono::Duration::from_std(every).unwrap_or_default()
            })
            .collect()
    }

    pub fn idle_enough(&self, s: &MaintenanceSettings) -> bool {
        s.enabled && self.ctx.activity.idle_for() >= Duration::from_secs(s.idle_secs)
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
            match self.step(job, now, &mut scratch) {
                Ok(Step::Finished(d)) => {
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
        let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if outcome != JobOutcome::Paused {
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
            JobOutcome::Paused => {}
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
                if let WriteOutcome::Purged(done) =
                    w.submit_blocking(WriteOp::PurgeMemories { seqs })?
                {
                    sc.deleted += done.len();
                    if let Some(idx) = self.ctx.index.read().clone() {
                        for seq in &done {
                            let _ = idx.remove(VecKey::Memory(*seq));
                        }
                    }
                    if done.is_empty() {
                        // Everything named was restored meanwhile; the next look finds the rest.
                        return Ok(Step::More);
                    }
                }
                Ok(Step::More)
            }
            Job::RawRetention => {
                let seqs: Vec<i64> = match s.storage.raw_retention {
                    Retention::Forever => {
                        return Ok(Step::Finished("retention is forever".into()));
                    }
                    Retention::Age(age) => {
                        let cutoff = ts(now - chrono::Duration::from_std(age).unwrap_or_default());
                        self.ctx
                            .readers
                            .with(|c| m::chunks_older_than(c, &cutoff, SLICE))?
                    }
                    Retention::Size(limit) => {
                        // The total is read once per run; slices count down from it.
                        let over = match sc.over_bytes {
                            Some(o) => o,
                            None => {
                                let total = self.ctx.readers.with(m::raw_bytes)?;
                                total.saturating_sub(limit)
                            }
                        };
                        sc.over_bytes = Some(over);
                        if over == 0 {
                            return Ok(Step::Finished(format!(
                                "raw tier within retention; {} chunks deleted",
                                sc.deleted
                            )));
                        }
                        let oldest = self.ctx.readers.with(|c| m::oldest_chunks(c, SLICE))?;
                        let mut left = over;
                        let mut take = Vec::new();
                        for (seq, bytes) in oldest {
                            if left == 0 {
                                break;
                            }
                            take.push(seq);
                            left = left.saturating_sub(bytes);
                        }
                        sc.over_bytes = Some(left);
                        if take.is_empty() {
                            sc.over_bytes = Some(0);
                        }
                        take
                    }
                };
                if seqs.is_empty() {
                    return Ok(Step::Finished(format!(
                        "raw tier within retention; {} chunks deleted",
                        sc.deleted
                    )));
                }
                if let WriteOutcome::Count(n) = w.submit_blocking(WriteOp::DeleteChunks {
                    seqs: seqs.clone(),
                    reason: s.storage.raw_retention.to_string(),
                })? {
                    sc.deleted += n;
                }
                if let Some(idx) = self.ctx.index.read().clone() {
                    idx.remove_chunks(&seqs)?;
                }
                Ok(Step::More)
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
                let c = rusqlite::Connection::open_with_flags(
                    self.ctx.home.db(),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                let problems = m::quick_check(&c)?;
                if problems.is_empty() {
                    return Ok(Step::Finished("quick_check ok".into()));
                }
                let reason = format!(
                    "PRAGMA quick_check found damage ({})",
                    problems.first().cloned().unwrap_or_default()
                );
                self.ctx.writer.read_only.set(reason.clone());
                Err(AliceError::Other(reason))
            }
            Job::IndexCheck => self.index_check(),
            Job::Backup => {
                let dir = backup::backup_dir(&self.ctx.home, &s.backup);
                let adopted = backup::adopt_legacy(&self.ctx.home, &dir)?;
                let b = backup::create(&self.ctx.home.db(), &dir, BackupKind::Scheduled, now)?;
                let deleted = backup::rotate(&dir, now, s.backup.daily, s.backup.weekly)?;
                Ok(Step::Finished(format!(
                    "{} ({} bytes); {} old backups removed; {adopted} legacy files moved",
                    b.name,
                    b.bytes,
                    deleted.len()
                )))
            }
            Job::MergeSuggestions => self.merge_suggestions(s.judge.cluster_similarity),
        }
    }

    /// Daily: the vector index must hold exactly the stored vectors, compared by key. A
    /// difference is repaired in place (add what is missing, drop what nothing backs); the whole
    /// index is rebuilt only when that cannot be done (a stored vector that cannot be read or
    /// added).
    fn index_check(&self) -> Result<Step> {
        let model = (self.ctx.model)();
        let Some(idx) = self.ctx.index.read().clone() else {
            return Ok(Step::Finished("index is being rebuilt".into()));
        };
        let repaired = self.ctx.readers.with(|c| {
            let r = idx.catch_up(c)?;
            Ok((r, embeddings::count(c, &model)?))
        });
        match repaired {
            Ok((r, stored)) if idx.len() == stored => {
                Ok(Step::Finished(if r.added + r.removed == 0 {
                    format!("index holds all {stored} vectors")
                } else {
                    format!(
                        "index differed from the stored vectors; added {}, removed {} ({stored} vectors)",
                        r.added, r.removed
                    )
                }))
            }
            other => {
                let why = match other {
                    Ok((_, stored)) => {
                        format!(
                            "index holds {} of {stored} vectors after catching up",
                            idx.len()
                        )
                    }
                    Err(e) => format!("catching up failed ({e})"),
                };
                *self.ctx.index.write() = None;
                crate::service::spawn_rebuild(
                    self.ctx.index.clone(),
                    self.ctx.home.db(),
                    model,
                    self.ctx.dim,
                    None,
                )?;
                Ok(Step::Finished(format!("{why}; rebuild started")))
            }
        }
    }

    /// Weekly: groups of live memories in one project whose vectors are at least
    /// `judge.cluster_similarity` alike become one `merge` review item (asked once).
    fn merge_suggestions(&self, threshold: f64) -> Result<Step> {
        let Some(idx) = self.ctx.index.read().clone() else {
            return Ok(Step::Finished("index not ready".into()));
        };
        let model = (self.ctx.model)();
        let cursor: i64 = self
            .meta("maint.merge.cursor")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let rows: Vec<(i64, String, String)> = self.ctx.readers.with(|c| {
            let mut stmt = c.prepare(
                "SELECT seq, id, project_id FROM memories
                 WHERE seq > ?1 AND status IN ('active','pending') ORDER BY seq LIMIT ?2",
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
        let mut made = 0;
        for (seq, id, project) in &rows {
            let Some((_, v)) = self
                .ctx
                .readers
                .with(|c| embeddings::get_many(c, &model, &[VecKey::Memory(*seq)]))?
                .pop()
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
            let members: Vec<(String, String)> = self.ctx.readers.with(|c| {
                let mut out = Vec::new();
                for o in &near {
                    if let Some(r) = c
                        .query_row(
                            "SELECT id, project_id FROM memories WHERE seq = ?1 AND status IN ('active','pending')",
                            [o],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?
                    {
                        out.push(r);
                    }
                }
                Ok(out)
            })?;
            let mut group: Vec<String> = members
                .into_iter()
                .filter(|(_, p)| p == project)
                .map(|(i, _)| i)
                .collect();
            if group.is_empty() {
                continue;
            }
            group.push(id.clone());
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
                            "p": threshold,
                            "provider": "cluster",
                        }),
                        confidence: Some(threshold),
                        dedupe_key: Some(key),
                    }))?
            {
                made += n;
            }
        }
        let last = rows.last().map(|r| r.0).unwrap_or(cursor);
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

/// Best effort: below normal priority so maintenance never competes with requests.
fn lower_priority() {
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
        let sched = Arc::new(Scheduler::new(MaintenanceCtx {
            home: home.clone(),
            readers,
            writer: writer.clone(),
            index: index.clone(),
            settings: settings.clone(),
            activity: activity.clone(),
            model: Arc::new(|| "fake".to_string()),
            dim: DIM,
            judge_wake: judge_tx,
            stop: stop.clone(),
        }));
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
            assert_eq!(
                r.count("SELECT count(*) FROM memories"),
                700,
                "exactly one slice ran"
            );
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
        assert_eq!(r.count("SELECT count(*) FROM memories"), 700);
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
            ("A", [1.0, 0.0, 0.0]),
            ("B", [0.99, 0.05, 0.0]),
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
        let bad = r.sched.run_job(Job::Backup, now, true);
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
