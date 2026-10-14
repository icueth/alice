//! The import job: it runs on a thread of its own (never on the writer or on an async runtime
//! thread) and reports through a shared [`ImportStatus`].
//!
//! Phases: `snapshot` (copy the palace read-only into `ALICE_HOME/import-tmp/<job>/`), `survey`
//! (one pass over every drawer: duplicates, chunk positions), then the planning passes
//! (`knowledge`, `raw`) that turn drawers into what Alice would store. Without `dry_run` the
//! planning passes will hand their batches to the writer; here they only count.
//!
//! Redaction is this module's contract: the writer stores what it is given. Everything that is
//! planned goes through [`RedactFilter`], which always redacts (whatever `privacy.redact`
//! says) and, in `block` mode, holds back the drawers that contain a secret.
//!
//! Never log a drawer, a planned record or a knowledge-graph value with `{:?}`: they hold raw text.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use alice_core::error::{AliceError, Result};
use alice_core::ids::now_rfc3339;
use alice_core::import::palace::{
    Drawer, PalaceInfo, PalaceReader, Snapshot, count_closets, detect_backend, open_snapshot,
    snapshot,
};
use alice_core::import::plan::{
    Mapper, Planned, Planner, Survey, TextFilter, Tier, classify, parse_time, survey,
};
use alice_core::import::report::{
    ImportReport, PalaceSummary, PhaseTiming, ProjectCounts, WingMapEntry,
};
use alice_core::import::wings::{Via, WingMap, WingTarget};
use alice_core::import::{ImportRequest, ImportState, ImportStatus, local_offset};
use alice_core::model::{Kind, SourceType};
use alice_core::normalize::{content_hash, derive_title, nfc};
use alice_core::redact::Redactor;
use alice_core::settings::{EmbeddingProvider, Settings};
use alice_core::store::ReaderPool;
use alice_core::store::import::legacy_existing;
use parking_lot::Mutex;

use crate::config::SettingsHandle;
use crate::maintenance::OwnedActivityGuard;
use crate::paths::AliceHome;
use crate::redaction::Redactors;
use crate::writer::WriterHandle;

/// What a stopped job says (the daemon is shutting down); also the text of a failed status.
pub const STOPPED: &str = "daemon stopped; run the import again to continue";

/// This build can read and plan an import but does not write it yet.
const NOT_WRITING: &str = "the phases that store the records are not part of this build yet; \
                           run with --dry-run to see what an import would add";

/// Drawers per read of the knowledge pass.
const KNOWLEDGE_GROUP: usize = 100;
/// Drawers per page of the raw pass.
const RAW_PAGE: usize = 2_000;

/// The single import slot of a service.
pub struct ImportSlot {
    pub status: Arc<Mutex<ImportStatus>>,
    pub handle: Option<JoinHandle<()>>,
}

impl ImportSlot {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(ImportStatus::idle())),
            handle: None,
        }
    }
}

impl Default for ImportSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the thread needs, and nothing that points back at the service.
pub struct ImportEnv {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub redactors: Arc<Redactors>,
    pub settings: Arc<SettingsHandle>,
    pub stop: Arc<AtomicBool>,
    /// ALICE_HOME.
    pub home: AliceHome,
    /// MemPalace's own folder, which a snapshot must never be written into.
    pub mempalace_home: Option<PathBuf>,
    /// Held for the whole job, so maintenance stays off the writer.
    pub guard: OwnedActivityGuard,
    pub status: Arc<Mutex<ImportStatus>>,
}

// ----------------------------------------------------------------------------------- preflight

/// `since` must be a date or a time; checked before any file is created.
pub fn check_since(since: &str) -> Result<()> {
    if parse_time(since, local_offset()).is_none() {
        return Err(AliceError::invalid(format!(
            "--since \"{}\" is not a date; use YYYY-MM-DD (2026-04-16) or an RFC 3339 time (2026-04-16T09:00:00+07:00)",
            since.trim()
        )));
    }
    Ok(())
}

/// The refusals that need no job: returns the palace folder to read. No file is created.
pub fn preflight(
    req: &ImportRequest,
    mempalace_home: Option<&Path>,
    settings: &Settings,
) -> Result<PathBuf> {
    if let Some(s) = req.since.as_deref() {
        check_since(s)?;
    }
    let palace = match (req.palace.as_deref(), mempalace_home) {
        (Some(p), _) if !p.trim().is_empty() => {
            let p = PathBuf::from(p);
            if !p.is_absolute() {
                return Err(AliceError::invalid(format!(
                    "--palace {} is not an absolute path; the daemon does not know your working folder, so give the full path",
                    p.display()
                )));
            }
            p
        }
        (_, Some(home)) => home.join("palace"),
        _ => {
            return Err(AliceError::invalid(
                "no palace given; pass --palace <folder>",
            ));
        }
    };
    detect_backend(&palace)?;
    if settings.embedding.provider != EmbeddingProvider::Local && !req.allow_remote_embedding {
        let provider = serde_json::to_value(settings.embedding.provider)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "a remote provider".to_string());
        return Err(AliceError::invalid(format!(
            "importing queues every chunk for {provider}, which would send the owner's history to it; pass --allow-remote-embedding if that is what you want"
        )));
    }
    Ok(palace)
}

// ------------------------------------------------------------------------------------ redaction

/// The detectors as a [`TextFilter`]: always on, `block` mode honoured, changes counted.
pub struct RedactFilter {
    redactor: Arc<Redactor>,
    redacted: AtomicU64,
}

impl RedactFilter {
    pub fn new(redactor: Arc<Redactor>) -> Self {
        Self {
            redactor,
            redacted: AtomicU64::new(0),
        }
    }

    /// Fields that had a secret replaced so far.
    pub fn redacted_fields(&self) -> u64 {
        self.redacted.load(Ordering::Relaxed)
    }

    /// `text` with secrets replaced, for names shown in the report and lookups; not counted and
    /// never refused (a held-back drawer is counted where it is planned).
    pub fn quiet(&self, text: &str) -> String {
        self.redactor.apply(text).text
    }
}

impl TextFilter for RedactFilter {
    fn filter(&self, field: &str, text: &str) -> std::result::Result<String, Vec<String>> {
        match self.redactor.check(field, text) {
            Ok(out) => {
                if out != text {
                    self.redacted.fetch_add(1, Ordering::Relaxed);
                }
                Ok(out)
            }
            Err(AliceError::Refused { detectors, .. }) => Err(detectors),
            Err(_) => Err(vec!["redaction".to_string()]),
        }
    }
}

// --------------------------------------------------------------------------------------- helpers

fn check_stop(stop: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::SeqCst) {
        Err(AliceError::Other(STOPPED.to_string()))
    } else {
        Ok(())
    }
}

/// A palace reader that stops asking for pages once the daemon is stopping, so the survey (one
/// call into the planner) can end early.
struct StopReader {
    inner: Box<dyn PalaceReader>,
    stop: Arc<AtomicBool>,
}

impl PalaceReader for StopReader {
    fn info(&self) -> &PalaceInfo {
        self.inner.info()
    }
    fn page(&self, after: Option<&str>, limit: usize, since: Option<&str>) -> Result<Vec<Drawer>> {
        check_stop(&self.stop)?;
        self.inner.page(after, limit, since)
    }
    fn get(&self, ids: &[String]) -> Result<Vec<Drawer>> {
        check_stop(&self.stop)?;
        self.inner.get(ids)
    }
    fn wing_counts(&self) -> Result<Vec<(String, u64)>> {
        self.inner.wing_counts()
    }
}

/// Removes the snapshot folder when the job ends, however it ends.
struct SnapshotDir {
    dir: PathBuf,
}

impl Drop for SnapshotDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        if let Some(parent) = self.dir.parent() {
            // Only when empty.
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// The chunk positions already stored for `uri` (the raw `file://` form the survey sees; the
/// stored form is redacted, so the lookup is too).
pub fn reserved_ords(
    readers: &ReaderPool,
    filter: &RedactFilter,
    uri: &str,
) -> Result<BTreeSet<i64>> {
    let stored = filter.quiet(uri);
    readers.with(|c| {
        let mut stmt = c.prepare_cached(
            "SELECT c.ord FROM chunks c JOIN sources s ON s.id = c.source_id WHERE s.uri = ?1",
        )?;
        let rows = stmt.query_map([&stored], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<BTreeSet<i64>>>()?)
    })
}

/// Peak resident memory of the process so far, in MB.
#[cfg(unix)]
fn peak_rss_mb() -> Option<u64> {
    // SAFETY: getrusage fills a plain struct it is given the address of.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let raw = usage.ru_maxrss.max(0) as u64;
    // macOS counts bytes, Linux kilobytes.
    Some(if cfg!(target_os = "macos") {
        raw / (1024 * 1024)
    } else {
        raw / 1024
    })
}

#[cfg(not(unix))]
fn peak_rss_mb() -> Option<u64> {
    None
}

fn set_phase(status: &Mutex<ImportStatus>, phase: &str) {
    status.lock().phase = phase.to_string();
}

fn set_progress(status: &Mutex<ImportStatus>, done: u64, total: Option<u64>) {
    let mut s = status.lock();
    s.done = done;
    if let Some(t) = total {
        s.total = t;
    }
}

fn wing_entry(wings: &WingMap, wing: &str, drawers: u64) -> WingMapEntry {
    let (to, via, agent) = match wings.target(wing) {
        WingTarget::Project { slug } => {
            let via = match wings.via(wing) {
                Via::Alias => "alias",
                Via::Override => "override",
                _ => "wing",
            };
            (slug, via, None)
        }
        WingTarget::Agent { agent } => ("inferred per drawer".to_string(), "inferred", Some(agent)),
        WingTarget::Sessions => ("the transcript folder".to_string(), "transcript", None),
        WingTarget::Unnamed => ("global".to_string(), "global", None),
    };
    WingMapEntry {
        to,
        via: via.to_string(),
        agent,
        drawers,
    }
}

/// The dedupe identity the store gives a memory (`content_hash` of project, kind, title, body).
fn memory_identity(slug: &str, input: &alice_core::store::memories::NewMemoryInput) -> String {
    let content = nfc(input.content.trim());
    let title = match input.title.as_deref() {
        Some(t) => nfc(t.trim()),
        None => derive_title(&content),
    };
    content_hash(slug, input.kind.unwrap_or(Kind::Note), &title, &content)
}

// ---------------------------------------------------------------------------------------- the job

/// What the planning passes count.
#[derive(Default)]
struct Tally {
    /// Stored by an earlier run, by tier.
    already: u64,
    already_knowledge: u64,
    already_session: u64,
    already_file: u64,
    knowledge_duplicates: u64,
    raw_collapsed: u64,
    memory_drawers: u64,
    chunk_drawers: u64,
    blocked_drawers: u64,
    skipped: BTreeMap<String, u64>,
    projects: BTreeMap<String, ProjectCounts>,
    split_memories: u64,
    split_chunks: u64,
    blocked: BTreeMap<String, u64>,
    transcripts: HashSet<String>,
    seen_identities: HashSet<String>,
}

struct Ctx<'a> {
    env: &'a ImportEnv,
    filter: &'a RedactFilter,
    survey: &'a Survey,
    reader: &'a StopReader,
    planner: Planner<'a>,
    tally: Tally,
    done: u64,
}

impl Ctx<'_> {
    fn project(&self, slug: &str) -> String {
        self.filter.quiet(slug)
    }

    fn take(&mut self, d: &Drawer) {
        let planned = self.planner.plan(d);
        match planned {
            Planned::Memories(parts) => {
                let Some(first) = parts.first() else { return };
                let slug = first.project.slug.clone();
                let identity = memory_identity(&slug, &first.input);
                if !self.tally.seen_identities.insert(identity) {
                    self.tally.knowledge_duplicates += 1;
                    return;
                }
                self.tally.memory_drawers += 1;
                if parts.len() > 1 {
                    self.tally.split_memories += 1;
                }
                let key = self.project(&slug);
                self.tally.projects.entry(key).or_default().memories += parts.len() as u64;
            }
            Planned::Chunks(parts) => {
                let Some(first) = parts.first() else { return };
                self.tally.chunk_drawers += 1;
                if parts.len() > 1 {
                    self.tally.split_chunks += 1;
                }
                if first.source.source_type == SourceType::Session {
                    self.tally.transcripts.insert(first.source.uri.clone());
                }
                let key = self.project(&first.project.slug);
                self.tally.projects.entry(key).or_default().chunks += parts.len() as u64;
            }
            Planned::Skipped { reason } => {
                *self.tally.skipped.entry(reason.to_string()).or_default() += 1;
            }
            Planned::Blocked { detectors } => {
                self.tally.blocked_drawers += 1;
                for d in detectors {
                    *self.tally.blocked.entry(d).or_default() += 1;
                }
            }
            Planned::Collapsed { .. } => self.tally.raw_collapsed += 1,
        }
    }

    fn progress(&self) {
        set_progress(&self.env.status, self.done, None);
    }

    /// Drawers an earlier run stored, among `ids`.
    fn stored(&self, ids: &[String]) -> Result<HashSet<String>> {
        self.env.readers.with(|c| legacy_existing(c, ids))
    }

    fn knowledge_pass(&mut self) -> Result<()> {
        let order: &[String] = &self.survey.knowledge_order;
        for group in order.chunks(KNOWLEDGE_GROUP) {
            check_stop(&self.env.stop)?;
            // Drawers stored before cost nothing more: no reading, no redaction, no planning.
            let stored = self.stored(group)?;
            self.tally.already += stored.len() as u64;
            self.tally.already_knowledge += stored.len() as u64;
            let fresh: Vec<String> = group
                .iter()
                .filter(|id| !stored.contains(*id))
                .cloned()
                .collect();
            let mut by_id: HashMap<String, Drawer> = self
                .reader
                .get(&fresh)?
                .into_iter()
                .map(|d| (d.id.clone(), d))
                .collect();
            // In survey order: of identical memories the earliest filed is the one kept.
            for id in &fresh {
                if let Some(d) = by_id.remove(id) {
                    self.take(&d);
                }
            }
            self.done += group.len() as u64;
            self.progress();
        }
        Ok(())
    }

    fn raw_pass(&mut self, since: Option<&str>) -> Result<()> {
        let mut after: Option<String> = None;
        loop {
            check_stop(&self.env.stop)?;
            let page = self.reader.page(after.as_deref(), RAW_PAGE, since)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id.clone());
            let raw: Vec<(&Drawer, Tier)> = page
                .iter()
                .map(|d| (d, classify(&d.id, &d.meta)))
                .filter(|(_, t)| matches!(t, Tier::RawSession | Tier::RawFile))
                .collect();
            let ids: Vec<String> = raw.iter().map(|(d, _)| d.id.clone()).collect();
            let stored = self.stored(&ids)?;
            for (d, tier) in &raw {
                if stored.contains(&d.id) {
                    self.tally.already += 1;
                    if *tier == Tier::RawSession {
                        self.tally.already_session += 1;
                    } else {
                        self.tally.already_file += 1;
                    }
                    continue;
                }
                self.take(d);
            }
            self.done += raw.len() as u64;
            self.progress();
        }
        Ok(())
    }
}

fn run(env: &ImportEnv, req: &ImportRequest, palace: &Path, job_id: &str) -> Result<ImportReport> {
    let started_at = now_rfc3339();
    let settings = env.settings.get();
    let assume = local_offset();
    let filter = RedactFilter::new(env.redactors.for_root_always_on(None));
    let mut phases: Vec<PhaseTiming> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let lap = |phases: &mut Vec<PhaseTiming>, name: &str, t: Instant| {
        phases.push(PhaseTiming {
            phase: name.to_string(),
            seconds: (t.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        });
    };

    // 1. snapshot
    set_phase(&env.status, "snapshot");
    let t = Instant::now();
    let dir = env.home.import_tmp().join(job_id);
    std::fs::create_dir_all(&dir)?;
    let _cleanup = SnapshotDir { dir: dir.clone() };
    let snap: Snapshot = snapshot(palace, &dir, env.mempalace_home.as_deref())?;
    warnings.extend(snap.notes.iter().cloned());
    let closets = count_closets(&snap);
    lap(&mut phases, "snapshot", t);
    check_stop(&env.stop)?;

    // 2. survey
    set_phase(&env.status, "survey");
    let t = Instant::now();
    let reader = StopReader {
        inner: open_snapshot(&snap, req.collection.as_deref())?,
        stop: env.stop.clone(),
    };
    let info = reader.info().clone();
    set_progress(&env.status, 0, Some(info.drawers));
    let mut wings = WingMap::build(&reader.wing_counts()?, &req.map_wing);
    let lookup_error: Mutex<Option<AliceError>> = Mutex::new(None);
    let reserved = |uri: &str| -> BTreeSet<i64> {
        match reserved_ords(&env.readers, &filter, uri) {
            Ok(set) => set,
            Err(e) => {
                lookup_error.lock().get_or_insert(e);
                BTreeSet::new()
            }
        }
    };
    let since = req.since.as_deref().map(str::trim);
    let surveyed = survey(&reader, since, &mut wings, assume, &reserved)?;
    if let Some(e) = lookup_error.lock().take() {
        return Err(e);
    }
    lap(&mut phases, "survey", t);
    set_progress(&env.status, 0, Some(surveyed.total));
    check_stop(&env.stop)?;

    // 3. plan every drawer that is not stored yet and count what it would add
    let planner = Planner {
        mapper: Mapper {
            wings: &wings,
            assume,
            filter: &filter,
        },
        survey: &surveyed,
    };
    let skipped_by_survey: u64 = surveyed.skipped.values().sum();
    let mut ctx = Ctx {
        env,
        filter: &filter,
        survey: &surveyed,
        reader: &reader,
        planner,
        tally: Tally::default(),
        done: skipped_by_survey,
    };
    set_phase(&env.status, "knowledge");
    let t = Instant::now();
    ctx.knowledge_pass()?;
    lap(&mut phases, "knowledge", t);
    set_phase(&env.status, "raw");
    let t = Instant::now();
    ctx.raw_pass(since)?;
    lap(&mut phases, "raw", t);
    let tally = ctx.tally;

    let mut skipped: BTreeMap<String, u64> = BTreeMap::new();
    for (k, v) in &surveyed.skipped {
        *skipped.entry((*k).to_string()).or_default() += v;
    }
    for (k, v) in tally.skipped {
        *skipped.entry(k).or_default() += v;
    }
    let skipped_total: u64 = skipped.values().sum();
    let wing_map: BTreeMap<String, WingMapEntry> = surveyed
        .wing_counts
        .iter()
        .map(|(w, n)| (filter.quiet(w), wing_entry(&wings, w, *n)))
        .collect();
    let mut report = ImportReport {
        job_id: job_id.to_string(),
        dry_run: req.dry_run,
        started_at,
        finished_at: None,
        palace: PalaceSummary {
            path: filter.quiet(&palace.display().to_string()),
            backend: match info.backend {
                alice_core::import::palace::Backend::SqliteExact => "sqlite_exact",
                alice_core::import::palace::Backend::Chroma => "chroma",
            }
            .to_string(),
            collection: req
                .collection
                .clone()
                .unwrap_or_else(|| alice_core::import::palace::DEFAULT_COLLECTION.to_string()),
            drawers: info.drawers,
        },
        assumed_utc_offset: assume.to_string(),
        tiers: alice_core::import::report::TierReport {
            knowledge: surveyed.tiers.knowledge - tally.already_knowledge,
            raw_session_chunks: surveyed.tiers.raw_session - tally.already_session,
            raw_session_transcripts: tally.transcripts.len() as u64,
            raw_file_chunks: surveyed.tiers.raw_file - tally.already_file,
        },
        projects: tally.projects,
        wing_map,
        duplicates: alice_core::import::report::DuplicateCounts {
            knowledge: tally.knowledge_duplicates,
            raw: tally.raw_collapsed,
        },
        drawers_seen: surveyed.total,
        drawers_accounted: skipped_total
            + tally.already
            + tally.knowledge_duplicates
            + tally.raw_collapsed
            + tally.memory_drawers
            + tally.chunk_drawers
            + tally.blocked_drawers,
        closets_dropped: closets,
        already_imported: tally.already,
        skipped,
        split: alice_core::import::report::SplitCounts {
            memories: tally.split_memories,
            chunks: tally.split_chunks,
        },
        blocked_by_redaction: tally.blocked,
        redacted_fields: filter.redacted_fields(),
        redaction_note: (!settings.privacy.redact)
            .then(|| "privacy.redact is off: Alice redacted anyway".to_string()),
        warnings,
        ..Default::default()
    };
    if report.drawers_seen != report.drawers_accounted {
        report.errors.push(format!(
            "{} drawers were read but {} are accounted for; please report this",
            report.drawers_seen, report.drawers_accounted
        ));
    }
    if !req.dry_run {
        return Err(AliceError::Other(NOT_WRITING.to_string()));
    }
    report.phases = phases;
    report.peak_rss_mb = peak_rss_mb();
    report.finished_at = Some(now_rfc3339());
    Ok(report)
}

/// Starts the job thread. The status is already `Running`; the thread ends it as `Done` or
/// `Failed`, after the snapshot is removed and the activity guard released.
pub fn spawn(
    env: ImportEnv,
    req: ImportRequest,
    palace: PathBuf,
    job_id: String,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("alice-import".into())
        .spawn(move || thread_main(env, req, palace, job_id))
}

fn thread_main(env: ImportEnv, req: ImportRequest, palace: PathBuf, job_id: String) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&env, &req, &palace, &job_id)
    }));
    let ImportEnv {
        stop,
        status,
        guard,
        ..
    } = env;
    // The snapshot folder went with `run`'s frame; the guard goes now, before the status says
    // the job is over, so a caller that sees `done` also sees maintenance allowed again.
    drop(guard);
    let mut s = status.lock();
    match outcome {
        Ok(Ok(report)) => {
            s.state = ImportState::Done;
            s.phase = "done".to_string();
            s.done = s.total;
            s.report = Some(report);
            s.error = None;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "the import failed");
            s.state = ImportState::Failed;
            s.error = Some(if stop.load(Ordering::SeqCst) {
                STOPPED.to_string()
            } else {
                e.to_string()
            });
        }
        Err(_) => {
            tracing::error!("the import thread panicked");
            s.state = ImportState::Failed;
            s.error = Some(
                "the import stopped unexpectedly; see the daemon log, then run it again to continue"
                    .to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::settings::PrivacySettings;

    fn filter() -> RedactFilter {
        let r = Redactors::fixed(PrivacySettings::default());
        RedactFilter::new(r.for_root_always_on(None))
    }

    #[test]
    fn the_filter_redacts_counts_changes_and_leaves_clean_text_alone() {
        let f = filter();
        let secret = concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here");
        let out = f.filter("chunk", secret).unwrap();
        assert!(out.contains("[REDACTED:") && !out.contains("ghp_"), "{out}");
        assert_eq!(f.filter("chunk", "nothing here").unwrap(), "nothing here");
        assert_eq!(f.redacted_fields(), 1);
        assert!(!f.quiet(secret).contains("ghp_"));
        assert_eq!(f.redacted_fields(), 1, "quiet does not count");
    }

    #[test]
    fn block_mode_holds_a_drawer_back_and_names_the_detector() {
        let r = Redactors::fixed(PrivacySettings {
            redact_mode: alice_core::settings::RedactMode::Block,
            ..Default::default()
        });
        let f = RedactFilter::new(r.for_root_always_on(None));
        let secret = concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here");
        assert_eq!(
            f.filter("chunk", secret),
            Err(vec!["github_token".to_string()])
        );
        assert!(
            !f.quiet(secret).contains("ghp_"),
            "names stay clean even in block mode"
        );
    }

    #[test]
    fn since_must_be_a_date_or_a_time() {
        for ok in [
            "2026-04-16",
            "2026-04-16T09:00:00+07:00",
            " 2026-04-16T09:00:00 ",
        ] {
            assert!(check_since(ok).is_ok(), "{ok}");
        }
        let e = check_since("last tuesday").unwrap_err().to_string();
        assert!(e.contains("--since") && e.contains("YYYY-MM-DD"), "{e}");
    }

    #[test]
    fn preflight_refuses_with_a_sentence_that_names_the_fix() {
        let s = Settings::default();
        let none = ImportRequest::default();
        let e = preflight(&none, None, &s).unwrap_err().to_string();
        assert!(e.contains("--palace"), "{e}");
        let rel = ImportRequest {
            palace: Some("palace".into()),
            ..Default::default()
        };
        assert!(
            preflight(&rel, None, &s)
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
    }
}
