//! The import job: it runs on a thread of its own (never on the writer or on an async runtime
//! thread) and reports through a shared [`ImportStatus`].
//!
//! Phases: `snapshot` (copy the palace read-only into `ALICE_HOME/import-tmp/<job>/`), `survey`
//! (one pass over every drawer: duplicates, chunk positions), then the planning passes
//! (`knowledge`, `raw`) that turn drawers into what Alice would store. Without `dry_run` the
//! planning passes hand their batches to the writer (one transaction each, closing at 50
//! records or 16 KB of text and only between two drawers); a dry run does the same work and
//! only counts.
//!
//! Redaction is this module's contract: the writer stores what it is given. Everything that is
//! planned goes through [`RedactFilter`], which always redacts (whatever `privacy.redact`
//! says) and, in `block` mode, holds back the drawers that contain a secret.
//!
//! Never log a drawer, a planned record or a knowledge-graph value with `{:?}`: they hold raw text.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use alice_core::error::{AliceError, Result};
use alice_core::ids::now_rfc3339;
use alice_core::import::kg::{PlannedFact, plan_kg, read_kg, read_tunnels};
use alice_core::import::palace::{
    Drawer, PalaceInfo, PalaceReader, Snapshot, count_closets, detect_backend, open_snapshot,
    snapshot,
};
use alice_core::import::plan::{
    Mapper, Planned, Planner, Survey, TextFilter, Tier, classify, parse_time, survey,
};
use alice_core::import::report::{
    FactCounts, ImportReport, PalaceSummary, PhaseTiming, ProjectCounts, TunnelCounts, WingMapEntry,
};
use alice_core::import::wings::{Via, WingMap, WingTarget};
use alice_core::import::{ImportRequest, ImportState, ImportStatus, local_offset};
use alice_core::model::{Kind, SourceType};
use alice_core::normalize::{content_hash, derive_title, nfc, sha256_hex, slugify};
use alice_core::redact::Redactor;
use alice_core::settings::{EmbeddingProvider, Settings, Source};
use alice_core::store::ReaderPool;
use alice_core::store::events::Actor;
use alice_core::store::import::{
    BatchOutcome, ImportBatch, ImportFact, ImportMemory, ImportProject, ImportSource,
    legacy_existing,
};
use alice_core::store::kg::normalize_predicate;
use alice_core::store::memories::NewMemoryInput;
use alice_core::store::reviews::NewReview;
use alice_core::store::sources::{ChunkInput, SourceInput};
use parking_lot::Mutex;

use crate::config::SettingsHandle;
use crate::maintenance::OwnedActivityGuard;
use crate::paths::AliceHome;
use crate::redaction::Redactors;
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};

/// What a stopped job says (the daemon is shutting down); also the text of a failed status.
pub const STOPPED: &str = "daemon stopped; run the import again to continue";

/// Drawers per read of the knowledge pass.
const KNOWLEDGE_GROUP: usize = 100;
/// Drawers per page of the raw pass.
const RAW_PAGE: usize = 2_000;
/// Pause after each committed batch (spec §10: 20 clients stay under 50 ms while an import runs).
const BATCH_PAUSE: std::time::Duration = std::time::Duration::from_millis(2);
/// A batch closes at this many records ...
const BATCH_RECORDS: usize = 50;
/// ... or this many bytes of text, whichever comes first, and only between two drawers.
const BATCH_BYTES: usize = 16 * 1024;
/// Collapsed pairs per batch (they carry no text): one writer slice.
const COLLAPSED_PER_BATCH: usize = 200;
/// Why a repeated chunk is not recorded: its keeper is not stored.
const REPEAT_OF_UNSTORED: &str = "repeat of a chunk that was not stored";
/// Facts per graph batch.
const FACTS_PER_BATCH: usize = 200;
/// Tunnel links and review items per batch of the closing phase.
const TAIL_PER_BATCH: usize = 200;
/// Knowledge drawers per mention-linking batch.
const MENTIONS_PER_BATCH: usize = 200;
/// The report keeps this many store errors and warnings; the rest are counted.
const REPORT_LIST_CAP: usize = 100;

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
    /// Tests only: a pause and a panic inside the job.
    pub hook: Option<Arc<ImportHook>>,
}

/// Test hook: makes the job wait (`hold`) or panic (`panic`) right after its snapshot is taken.
#[doc(hidden)]
#[derive(Default)]
pub struct ImportHook {
    pub hold: AtomicBool,
    pub panic: AtomicBool,
    /// Parks the job after the survey, right before the first record is planned.
    pub hold_planning: AtomicBool,
    /// True while the job is parked by `hold_planning`.
    pub parked: AtomicBool,
    /// Parks the job after the graph and the duplicates, right before the mentions.
    pub hold_mentions: AtomicBool,
    /// True while the job is parked by `hold_mentions`.
    pub parked_mentions: AtomicBool,
    /// Drawers the planner was run on (a rerun that stores nothing new plans none).
    pub planned: std::sync::atomic::AtomicU64,
}

impl ImportHook {
    fn park_before_planning(&self, stop: &AtomicBool) {
        Self::park(&self.hold_planning, &self.parked, stop);
    }

    fn park_before_mentions(&self, stop: &AtomicBool) {
        Self::park(&self.hold_mentions, &self.parked_mentions, stop);
    }

    fn park(hold: &AtomicBool, parked: &AtomicBool, stop: &AtomicBool) {
        parked.store(true, Ordering::SeqCst);
        while hold.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        parked.store(false, Ordering::SeqCst);
    }

    fn run(&self, stop: &AtomicBool) {
        while self.hold.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if self.panic.load(Ordering::SeqCst) {
            panic!("import test hook");
        }
    }
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
    if req.palace.as_deref().is_some_and(|p| p.trim().is_empty()) {
        return Err(AliceError::invalid(
            "--palace is empty; pass the palace folder, or leave --palace out to use the default palace under ~/.mempalace",
        ));
    }
    let palace = match (req.palace.as_deref(), mempalace_home) {
        (Some(p), _) => {
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
    ///
    /// A second pass starts again after every `_`: the detectors need a word boundary before a
    /// key prefix, and `wing_sk-...` or `diary_ghp_...` has none (the tail alone does).
    pub fn quiet(&self, text: &str) -> String {
        let mut out = self.redactor.apply(text).text;
        let mut from = 0;
        while let Some(off) = out[from..].find('_') {
            let cut = from + off + 1;
            let tail = self.redactor.apply(&out[cut..]).text;
            if tail != out[cut..] {
                out.truncate(cut);
                out.push_str(&tail);
            }
            from = cut;
        }
        out
    }
}

impl RedactFilter {
    /// What storage would hold for `text` (the same detectors as [`TextFilter::filter`]), without
    /// counting it; `None` when `block` mode would hold the text back.
    pub fn stored_form(&self, field: &str, text: &str) -> Option<String> {
        self.redactor.check(field, text).ok()
    }

    /// The id a drawer is filed under in `legacy_ids`. Drawer ids embed wing and room names, so
    /// an id that holds a secret is stored as its redacted spelling plus a hash of the original
    /// (two different secrets stay two different ids). The prefilter and the stored rows use
    /// this one function.
    pub fn legacy_key(&self, id: &str) -> String {
        let clean = self.quiet(id);
        if clean == id {
            return clean;
        }
        let hash = sha256_hex(id.as_bytes());
        format!("{clean}~{}", &hash[..16])
    }
}

/// The id of part `i` (0 based) of the drawer filed under `key`.
fn part_id(key: &str, i: usize) -> String {
    if i == 0 {
        key.to_string()
    } else {
        format!("{key}#{}", i + 1)
    }
}

/// The project a stored record belongs to: the planner's slug, as clean as every other stored name.
fn store_slug(filter: &RedactFilter, slug: &str) -> String {
    let s = slugify(&filter.quiet(slug));
    if s.is_empty() {
        "global".to_string()
    } else {
        s
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
/// call into the planner) can end early. It also hands out every wing name already redacted:
/// project slugs are made from wing names, and a slug made from a raw `ghp_...` wing would be
/// lower-cased before any detector saw it.
struct StopReader<'a> {
    inner: Box<dyn PalaceReader>,
    stop: Arc<AtomicBool>,
    filter: &'a RedactFilter,
    wings: Mutex<HashMap<String, String>>,
}

impl StopReader<'_> {
    fn clean_wing(&self, wing: &str) -> String {
        let mut cache = self.wings.lock();
        if let Some(c) = cache.get(wing) {
            return c.clone();
        }
        let c = self.filter.quiet(wing);
        cache.insert(wing.to_string(), c.clone());
        c
    }

    fn clean(&self, mut drawers: Vec<Drawer>) -> Vec<Drawer> {
        for d in &mut drawers {
            d.meta.wing = self.clean_wing(&d.meta.wing);
        }
        drawers
    }
}

impl PalaceReader for StopReader<'_> {
    fn info(&self) -> &PalaceInfo {
        self.inner.info()
    }
    fn page(&self, after: Option<&str>, limit: usize, since: Option<&str>) -> Result<Vec<Drawer>> {
        check_stop(&self.stop)?;
        Ok(self.clean(self.inner.page(after, limit, since)?))
    }
    fn get(&self, ids: &[String]) -> Result<Vec<Drawer>> {
        check_stop(&self.stop)?;
        Ok(self.clean(self.inner.get(ids)?))
    }
    fn wing_counts(&self) -> Result<Vec<(String, u64)>> {
        let mut merged: BTreeMap<String, u64> = BTreeMap::new();
        for (w, n) in self.inner.wing_counts()? {
            *merged.entry(self.clean_wing(&w)).or_default() += n;
        }
        let mut out: Vec<(String, u64)> = merged.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
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
    // The form the chunks are stored under (the planner redacts the uri with the same detectors).
    let Some(stored) = filter.stored_form("source.uri", uri) else {
        return Ok(BTreeSet::new());
    };
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

fn wing_entry(wings: &WingMap, filter: &RedactFilter, wing: &str, drawers: u64) -> WingMapEntry {
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
    // Names come from the owner's wings and folders: as clean as every other name in the report.
    WingMapEntry {
        to: filter.quiet(&to),
        via: via.to_string(),
        agent: agent.map(|a| filter.quiet(&a)),
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

// ------------------------------------------------------------------------------------- batching

/// Groups units into batches: a batch closes at `BATCH_RECORDS` records or `BATCH_BYTES` bytes,
/// and only between two units (a unit is one drawer, so the parts of one drawer always share a
/// batch; a unit over the budget gets a batch of its own).
struct Batcher<T> {
    items: Vec<T>,
    records: usize,
    bytes: usize,
}

impl<T> Batcher<T> {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            records: 0,
            bytes: 0,
        }
    }

    /// Adds a unit; returns the batch that had to close to make room for it.
    fn push(&mut self, item: T, records: usize, bytes: usize) -> Option<Vec<T>> {
        let closed = if !self.items.is_empty()
            && (self.records + records > BATCH_RECORDS || self.bytes + bytes > BATCH_BYTES)
        {
            self.records = 0;
            self.bytes = 0;
            Some(std::mem::take(&mut self.items))
        } else {
            None
        };
        self.items.push(item);
        self.records += records;
        self.bytes += bytes;
        closed
    }

    fn finish(&mut self) -> Option<Vec<T>> {
        self.records = 0;
        self.bytes = 0;
        (!self.items.is_empty()).then(|| std::mem::take(&mut self.items))
    }
}

/// All the stored rows of one drawer (its parts), already redacted and keyed.
#[derive(Clone)]
enum Unit {
    Memories {
        slug: String,
        items: Vec<NewMemoryInput>,
        /// The drawer repeats one already planned in this run (counted as a duplicate).
        dup: bool,
    },
    Chunks {
        slug: String,
        source: SourceInput,
        chunks: Vec<ChunkInput>,
    },
}

impl Unit {
    fn weight(&self) -> (usize, usize) {
        match self {
            Unit::Memories { items, .. } => (
                items.len(),
                items.iter().map(|m| m.content.len() + 64).sum(),
            ),
            Unit::Chunks { chunks, .. } => {
                (chunks.len(), chunks.iter().map(|c| c.text.len() + 64).sum())
            }
        }
    }
}

// ---------------------------------------------------------------------------------------- the job

// Notes for the phases that store records:
// - Phases after `raw`: `graph` (facts), `collapsed` (repeated chunks point at their keeper),
//   `mentions` (links of the memories stored in this run to the entities that exist now) and
//   `done`. Tunnel links and review items travel in the closing batch(es), which also carry the
//   `import.mempalace.last` marker, so a rerun that finds nothing new writes one small batch.
// - The dry run only dedupes knowledge drawers against each other: a memory already stored in
//   Alice that an incoming drawer duplicates is reported by the real run as a warning.
// - Collapsed raw drawers are recorded at the end of the raw phase (they point at their keeper).

/// What the planning passes count.
#[derive(Default)]
struct Tally {
    /// Stored by an earlier run, by tier.
    already: u64,
    already_knowledge: u64,
    already_session: u64,
    already_file: u64,
    knowledge_duplicates: u64,
    /// Parts of those duplicates handed to the store (it answers "duplicate" for them too).
    duplicate_parts_sent: u64,
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
    /// From the store's answers.
    store_errors: Vec<String>,
    store_warnings: BTreeSet<String>,
    store_duplicate_memories: u64,
    /// Drawers the store refused (listed in the errors; they are accounted for there).
    rejected: u64,
    // The graph.
    facts_added: u64,
    facts_ended: u64,
    facts_skipped: u64,
    isolated: u64,
    kg_blocked: u64,
    kg_warnings: Vec<String>,
    links_made: u64,
    links_dropped: u64,
    tunnels_dropped: u64,
    tunnels_topic: u64,
    reviews_created: u64,
}

struct Ctx<'a> {
    env: &'a ImportEnv,
    filter: &'a RedactFilter,
    survey: &'a Survey,
    reader: &'a StopReader<'a>,
    planner: Planner<'a>,
    tally: Tally,
    done: u64,
    dry: bool,
    job_id: &'a str,
    batcher: Batcher<Unit>,
    /// Stored project slug -> the aliases to give it (sent with the first batch that names it).
    aliases: HashMap<String, Vec<String>>,
    sent_projects: HashSet<String>,
    /// (collapsed drawer key, keeper key), recorded in the `collapsed` phase.
    collapsed: Vec<(String, String)>,
    /// Batches the writer has taken (nothing was written while this is 0).
    submitted: u64,
    /// Tunnel links and review items wait for the closing batch.
    pending_links: Vec<(String, String)>,
    pending_reviews: Vec<NewReview>,
}

impl Ctx<'_> {
    fn slug(&self, slug: &str) -> String {
        store_slug(self.filter, slug)
    }

    /// Counts what `d` would add; in a real run also returns the rows to store.
    fn take(&mut self, d: &Drawer) -> Option<Unit> {
        if let Some(h) = &self.env.hook {
            h.planned.fetch_add(1, Ordering::Relaxed);
        }
        let planned = self.planner.plan(d);
        let key = self.filter.legacy_key(&d.id);
        match planned {
            Planned::Memories(parts) => {
                let first = parts.first()?;
                let slug = self.slug(&first.project.slug);
                let identity = memory_identity(&slug, &first.input);
                let duplicate = !self.tally.seen_identities.insert(identity);
                if duplicate {
                    self.tally.knowledge_duplicates += 1;
                    self.tally.duplicate_parts_sent += parts.len() as u64;
                    // The store records the old id against the memory that survives; a dry run
                    // has nothing to write.
                } else {
                    self.tally.memory_drawers += 1;
                    if parts.len() > 1 {
                        self.tally.split_memories += 1;
                    }
                    self.tally
                        .projects
                        .entry(slug.clone())
                        .or_default()
                        .memories += parts.len() as u64;
                }
                if self.dry {
                    return None;
                }
                let items = parts
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let mut input = p.input;
                        input.legacy_id = Some(part_id(&key, i));
                        input
                    })
                    .collect();
                Some(Unit::Memories {
                    slug,
                    items,
                    dup: duplicate,
                })
            }
            Planned::Chunks(parts) => {
                let first = parts.first()?;
                let slug = self.slug(&first.project.slug);
                self.tally.chunk_drawers += 1;
                if parts.len() > 1 {
                    self.tally.split_chunks += 1;
                }
                if first.source.source_type == SourceType::Session {
                    self.tally.transcripts.insert(first.source.uri.clone());
                }
                self.tally.projects.entry(slug.clone()).or_default().chunks += parts.len() as u64;
                if self.dry {
                    return None;
                }
                let source = first.source.clone();
                let chunks = parts
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let mut chunk = p.chunk;
                        chunk.legacy_id = Some(part_id(&key, i));
                        chunk
                    })
                    .collect();
                Some(Unit::Chunks {
                    slug,
                    source,
                    chunks,
                })
            }
            Planned::Skipped { reason } => {
                *self.tally.skipped.entry(reason.to_string()).or_default() += 1;
                None
            }
            Planned::Blocked { detectors } => {
                self.tally.blocked_drawers += 1;
                for d in detectors {
                    *self.tally.blocked.entry(d).or_default() += 1;
                }
                None
            }
            Planned::Collapsed { keeper } => {
                self.tally.raw_collapsed += 1;
                if !self.dry {
                    self.collapsed.push((key, self.filter.legacy_key(&keeper)));
                }
                None
            }
        }
    }

    fn progress(&self) {
        set_progress(&self.env.status, self.done, None);
    }

    /// Of `keys`, the ones an earlier run stored.
    fn stored(&self, keys: &[String]) -> Result<HashSet<String>> {
        self.env.readers.with(|c| legacy_existing(c, keys))
    }

    /// Adds a drawer's rows to the open batch; a batch that closes is written.
    fn push(&mut self, unit: Unit) -> Result<()> {
        let (records, bytes) = unit.weight();
        if let Some(closed) = self.batcher.push(unit, records, bytes) {
            self.write(closed)?;
        }
        Ok(())
    }

    fn finish_batch(&mut self) -> Result<()> {
        if let Some(closed) = self.batcher.finish() {
            self.write(closed)?;
        }
        Ok(())
    }

    fn batch(&self) -> ImportBatch {
        ImportBatch {
            job_id: self.job_id.to_string(),
            ..Default::default()
        }
    }

    /// Writes a batch. A batch the store refuses as invalid is retried drawer by drawer, and a
    /// drawer that is still refused goes to the report's errors: one odd record never fails
    /// the whole import (and so never fails every rerun).
    fn write(&mut self, units: Vec<Unit>) -> Result<()> {
        match self.try_write(&units) {
            Err(AliceError::Invalid(why)) => {
                if units.len() == 1 {
                    self.reject(&units[0], &why);
                    return Ok(());
                }
                for unit in units {
                    match self.try_write(std::slice::from_ref(&unit)) {
                        Err(AliceError::Invalid(why)) => self.reject(&unit, &why),
                        other => other?,
                    }
                }
                Ok(())
            }
            other => other,
        }
    }

    fn try_write(&mut self, units: &[Unit]) -> Result<()> {
        let mut batch = self.batch();
        let mut slugs: BTreeSet<String> = BTreeSet::new();
        let mut at: HashMap<(String, &'static str, String), usize> = HashMap::new();
        for unit in units.iter().cloned() {
            match unit {
                Unit::Memories { slug, items, .. } => {
                    slugs.insert(slug.clone());
                    batch
                        .memories
                        .extend(items.into_iter().map(|input| ImportMemory {
                            project_slug: slug.clone(),
                            input,
                        }));
                }
                Unit::Chunks {
                    slug,
                    source,
                    chunks,
                } => {
                    slugs.insert(slug.clone());
                    let k = (
                        slug.clone(),
                        source.source_type.as_str(),
                        source.uri.clone(),
                    );
                    match at.get(&k) {
                        Some(&i) => batch.sources[i].chunks.extend(chunks),
                        None => {
                            at.insert(k, batch.sources.len());
                            batch.sources.push(ImportSource {
                                project_slug: slug,
                                source,
                                chunks,
                            });
                        }
                    }
                }
            }
        }
        // Aliases go with the first batch that names a project, once it is stored.
        for slug in &slugs {
            let aliases = if self.sent_projects.contains(slug) {
                Vec::new()
            } else {
                self.aliases.get(slug).cloned().unwrap_or_default()
            };
            batch.projects.push(ImportProject {
                slug: slug.clone(),
                aliases,
            });
        }
        self.submit(batch)?;
        self.sent_projects.extend(slugs);
        Ok(())
    }

    /// The store refused this drawer: it is listed in the errors, and counted there instead of
    /// among the drawers that were stored.
    fn reject(&mut self, unit: &Unit, why: &str) {
        let (id, parts) = match unit {
            Unit::Memories { items, dup, slug } => {
                let n = items.len() as u64;
                if *dup {
                    self.tally.knowledge_duplicates -= 1;
                    self.tally.duplicate_parts_sent -= n;
                } else {
                    // Nothing was stored, so a later identical drawer is not a duplicate of it.
                    // (One already planned into the same batch was counted as one and is
                    // stored as new by the store; the totals still add up.)
                    if let Some(first) = items.first() {
                        self.tally
                            .seen_identities
                            .remove(&memory_identity(slug, first));
                    }
                    self.tally.memory_drawers -= 1;
                    if n > 1 {
                        self.tally.split_memories -= 1;
                    }
                    if let Some(c) = self.tally.projects.get_mut(slug) {
                        c.memories = c.memories.saturating_sub(n);
                    }
                }
                (items.first().and_then(|m| m.legacy_id.clone()), n)
            }
            Unit::Chunks { slug, chunks, .. } => {
                let n = chunks.len() as u64;
                self.tally.chunk_drawers -= 1;
                if n > 1 {
                    self.tally.split_chunks -= 1;
                }
                if let Some(c) = self.tally.projects.get_mut(slug) {
                    c.chunks = c.chunks.saturating_sub(n);
                }
                (chunks.first().and_then(|c| c.legacy_id.clone()), n)
            }
        };
        self.tally.rejected += 1;
        let why: String = self.filter.quiet(why).chars().take(300).collect();
        self.tally.store_errors.push(format!(
            "drawer {} ({parts} rows) was not stored: {why}",
            id.unwrap_or_default()
        ));
    }

    /// One writer transaction. The batch holds nothing the filter has not seen. Returns how many
    /// collapsed pairs the store could not record yet.
    fn submit(&mut self, batch: ImportBatch) -> Result<usize> {
        check_stop(&self.env.stop)?;
        let (records, bytes) = batch.weight();
        match self
            .env
            .writer
            .submit_blocking(WriteOp::ImportBatch(Box::new(batch)))?
        {
            WriteOutcome::Imported(out) => {
                self.submitted += 1;
                // A breath between batches: remembers that queued behind this one are served
                // before the next one is built, so the import never keeps the writer busy back to back.
                std::thread::sleep(BATCH_PAUSE);
                {
                    let mut st = self.env.status.lock();
                    st.batches += 1;
                    st.max_batch_records = st.max_batch_records.max(records as u64);
                    st.max_batch_bytes = st.max_batch_bytes.max(bytes as u64);
                }
                Ok(self.absorb(out))
            }
            other => Err(AliceError::Other(format!(
                "the writer answered an import batch with {}",
                other.name()
            ))),
        }
    }

    fn absorb(&mut self, out: BatchOutcome) -> usize {
        self.tally.store_errors.extend(out.errors);
        self.tally.store_warnings.extend(out.warnings);
        self.tally.store_duplicate_memories += out.memories_duplicate as u64;
        self.tally.facts_added += out.facts_added as u64;
        self.tally.facts_ended += out.facts_ended as u64;
        self.tally.facts_skipped += out.facts_skipped as u64;
        self.tally.links_made += out.links_made as u64;
        self.tally.links_dropped += out.links_dropped as u64;
        self.tally.reviews_created += out.reviews_created as u64;
        out.deferred_collapsed.len()
    }

    fn knowledge_pass(&mut self) -> Result<()> {
        let order: &[String] = &self.survey.knowledge_order;
        for group in order.chunks(KNOWLEDGE_GROUP) {
            check_stop(&self.env.stop)?;
            // Drawers stored before cost nothing more: no reading, no redaction, no planning.
            let keys: Vec<String> = group.iter().map(|id| self.filter.legacy_key(id)).collect();
            let stored = self.stored(&keys)?;
            let mut fresh: Vec<String> = Vec::new();
            for (id, key) in group.iter().zip(&keys) {
                if stored.contains(key) {
                    self.tally.already += 1;
                    self.tally.already_knowledge += 1;
                } else {
                    fresh.push(id.clone());
                }
            }
            let mut by_id: HashMap<String, Drawer> = self
                .reader
                .get(&fresh)?
                .into_iter()
                .map(|d| (d.id.clone(), d))
                .collect();
            // In survey order: of identical memories the earliest filed is the one kept.
            for id in &fresh {
                if let Some(d) = by_id.remove(id)
                    && let Some(unit) = self.take(&d)
                {
                    self.push(unit)?;
                }
            }
            self.done += group.len() as u64;
            self.progress();
        }
        self.finish_batch()
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
            let keys: Vec<String> = raw
                .iter()
                .map(|(d, _)| self.filter.legacy_key(&d.id))
                .collect();
            let stored = self.stored(&keys)?;
            let mut units: Vec<Unit> = Vec::new();
            for ((d, tier), key) in raw.iter().zip(&keys) {
                if stored.contains(key) {
                    self.tally.already += 1;
                    if *tier == Tier::RawSession {
                        self.tally.already_session += 1;
                    } else {
                        self.tally.already_file += 1;
                    }
                    continue;
                }
                if let Some(unit) = self.take(d) {
                    units.push(unit);
                }
            }
            // One source's chunks sit together, so a batch holds whole stretches of a source.
            units.sort_by(|a, b| unit_source(a).cmp(&unit_source(b)));
            for unit in units {
                self.push(unit)?;
            }
            self.finish_batch()?;
            self.done += raw.len() as u64;
            self.progress();
        }
        Ok(())
    }

    /// The `collapsed` phase. Collapsed drawers point where their keeper went; their keepers are stored by now. A
    /// repeat whose keeper was not stored (held back by block mode, or its insert failed) is left
    /// for the next run: it is accounted as skipped, not as a duplicate.
    fn collapsed_pass(&mut self) -> Result<()> {
        let pairs = std::mem::take(&mut self.collapsed);
        let mut deferred = 0usize;
        for part in pairs.chunks(COLLAPSED_PER_BATCH) {
            let mut batch = self.batch();
            batch.collapsed = part.to_vec();
            deferred += self.submit(batch)?;
        }
        if deferred > 0 {
            self.tally.raw_collapsed -= deferred as u64;
            *self
                .tally
                .skipped
                .entry(REPEAT_OF_UNSTORED.to_string())
                .or_default() += deferred as u64;
            self.tally.store_warnings.insert(format!(
                "{deferred} repeated chunks were left for the next run because the chunk they repeat was not stored"
            ));
        }
        Ok(())
    }
}

impl Ctx<'_> {
    /// The `graph` phase: the facts of the knowledge graph in batches; tunnel links and review
    /// items are held for the closing batch (they need the facts and the memories stored).
    fn graph_pass(
        &mut self,
        kg: Option<&Path>,
        tunnels: Option<&Path>,
        assume: chrono::FixedOffset,
    ) -> Result<()> {
        if let Some(path) = kg {
            self.graph_facts(path, assume)?;
        }
        if let Some(path) = tunnels {
            self.graph_tunnels(path);
        }
        Ok(())
    }

    fn graph_facts(&mut self, path: &Path, assume: chrono::FixedOffset) -> Result<()> {
        check_stop(&self.env.stop)?;
        let kg = match read_kg(path) {
            Ok(kg) => kg,
            Err(e) => {
                // One unreadable graph never blocks the drawers; the report says what was lost.
                self.tally.store_errors.push(format!(
                    "the knowledge graph could not be read and was not imported: {}",
                    self.filter.quiet(&e.to_string())
                ));
                return Ok(());
            }
        };
        self.tally.isolated = kg.isolated_entities;
        let existing: Vec<(String, u64)> = self.env.readers.with(|c| {
            let mut st = c.prepare("SELECT predicate, count(*) FROM relations r
                 WHERE NOT EXISTS (SELECT 1 FROM legacy_ids l WHERE l.target_type = 'relation' AND l.target_id = r.id)
                 GROUP BY 1")?;
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        // Only what Alice had of its own: the graph's own triples are counted by the planner, and
        // counting them again on a rerun would change which predicate is canonical.
        let plan = plan_kg(&kg, &existing, assume, self.filter);
        drop(kg);
        for detectors in &plan.blocked {
            self.tally.kg_blocked += 1;
            for d in detectors {
                *self.tally.blocked.entry(d.clone()).or_default() += 1;
            }
        }
        self.tally.kg_warnings = plan.warnings;
        // Validate before submitting: the store would refuse these one by one.
        let mut ready: Vec<ImportFact> = Vec::new();
        let mut invalid = 0u64;
        for f in plan.facts {
            if !fact_is_valid(&f) {
                invalid += 1;
                continue;
            }
            ready.push(ImportFact {
                legacy_id: f.legacy_id,
                add: f.add,
                end_at: f.end_at,
                source_drawer_id: f.source_drawer_id.map(|d| self.filter.legacy_key(&d)),
            });
        }
        if invalid > 0 {
            self.tally.facts_skipped += invalid;
            self.tally.kg_warnings.push(format!(
                "{invalid} triples have an empty subject, predicate or object and were not imported"
            ));
        }
        for r in plan.reviews {
            self.pending_reviews.push(self.clean_review(r));
        }
        // Facts an earlier run stored cost nothing more.
        let ids: Vec<String> = ready.iter().map(|f| f.legacy_id.clone()).collect();
        let stored = self.stored(&ids)?;
        let (known, fresh): (Vec<ImportFact>, Vec<ImportFact>) = ready
            .into_iter()
            .partition(|f| stored.contains(&f.legacy_id));
        self.tally.facts_skipped += known.len() as u64;
        if self.dry {
            self.tally.facts_added += fresh.len() as u64;
            self.tally.facts_ended += fresh.iter().filter(|f| f.end_at.is_some()).count() as u64;
            return Ok(());
        }
        let mut fresh: VecDeque<ImportFact> = fresh.into();
        while !fresh.is_empty() {
            let n = fresh.len().min(FACTS_PER_BATCH);
            let mut batch = self.batch();
            batch.facts = fresh.drain(..n).collect();
            self.submit(batch)?;
        }
        Ok(())
    }

    /// Review payloads are stored text like any other: through the same filter. The dedupe key
    /// follows the (normalised) predicate it names.
    fn clean_review(&self, mut r: NewReview) -> NewReview {
        let mut renamed = None;
        if let Some(obj) = r.payload.as_object_mut() {
            for key in ["predicate", "canonical"] {
                if let Some(v) = obj.get_mut(key)
                    && let Some(text) = v.as_str()
                {
                    let clean = self.filter.quiet(text);
                    if clean != text {
                        if key == "predicate" {
                            renamed = Some(clean.clone());
                        }
                        *v = serde_json::Value::String(clean);
                    }
                }
            }
        }
        if let Some(p) = renamed {
            r.dedupe_key = Some(format!("predicate:{}", normalize_predicate(&p)));
        }
        r
    }

    /// Explicit tunnels between two drawers become links; everything else is counted, not kept.
    /// (A tunnel's label and its wing and room names are not stored anywhere, so there is
    /// nothing in them to redact.)
    fn graph_tunnels(&mut self, path: &Path) {
        let tunnels = read_tunnels(path);
        self.tally.tunnels_topic += tunnels.dropped_topic;
        self.tally.tunnels_dropped += tunnels.dropped_malformed;
        for t in tunnels.explicit {
            match (t.source_drawer_id, t.target_drawer_id) {
                (Some(a), Some(b)) => {
                    let pair = (self.filter.legacy_key(&a), self.filter.legacy_key(&b));
                    self.pending_links.push(pair);
                }
                _ => self.tally.tunnels_dropped += 1,
            }
        }
    }

    /// The `mentions` phase: every imported knowledge memory (whichever run stored it) whose
    /// text names an entity it is not linked to yet. An interrupted run's rerun completes them;
    /// a finished palace sends nothing.
    fn mentions_pass(&mut self) -> Result<()> {
        if self.dry {
            return Ok(());
        }
        let entities: i64 = self
            .env
            .readers
            .with(|c| Ok(c.query_row("SELECT count(*) FROM entities", [], |r| r.get(0))?))?;
        if entities == 0 {
            return Ok(());
        }
        let matcher = self
            .env
            .readers
            .with(alice_core::store::kg::EntityMatcher::load)?;
        let mut todo: Vec<String> = Vec::new();
        let keys: Vec<String> = self
            .survey
            .knowledge_order
            .iter()
            .map(|id| self.filter.legacy_key(id))
            .collect();
        for group in keys.chunks(MENTIONS_PER_BATCH) {
            check_stop(&self.env.stop)?;
            // Part 0 of every drawer, then part 2, 3 ... of those that have them.
            let mut round: Vec<String> = group.to_vec();
            let mut part = 0usize;
            while !round.is_empty() {
                let ids: Vec<String> = round.iter().map(|k| part_id(k, part)).collect();
                let found = self
                    .env
                    .readers
                    .with(|c| unlinked_memories(c, &ids, &matcher))?;
                let present: HashSet<&String> = found.iter().map(|(id, _)| id).collect();
                todo.extend(
                    found
                        .iter()
                        .filter(|(_, miss)| *miss)
                        .map(|(id, _)| id.clone()),
                );
                round = round
                    .into_iter()
                    .zip(&ids)
                    .filter(|(_, id)| present.contains(id))
                    .map(|(k, _)| k)
                    .collect();
                part += 1;
            }
        }
        for group in todo.chunks(MENTIONS_PER_BATCH) {
            let mut batch = self.batch();
            batch.link_mentions = group.to_vec();
            self.submit(batch)?;
        }
        Ok(())
    }

    /// The closing batch(es): tunnel links and review items.
    fn closing_pass(&mut self) -> Result<()> {
        let links = std::mem::take(&mut self.pending_links);
        let reviews = std::mem::take(&mut self.pending_reviews);
        let seen_keys: Vec<String> = reviews
            .iter()
            .filter_map(|r| r.dedupe_key.as_ref().map(|k| format!("review:{k}")))
            .collect();
        let seen = self.stored(&seen_keys)?;
        let reviews: Vec<NewReview> = reviews
            .into_iter()
            .filter(|r| {
                !r.dedupe_key
                    .as_ref()
                    .is_some_and(|k| seen.contains(&format!("review:{k}")))
            })
            .collect();
        if self.dry {
            let mut open = 0u64;
            for r in &reviews {
                let rejected = match r.dedupe_key.as_deref() {
                    Some(k) => self
                        .env
                        .readers
                        .with(|c| alice_core::store::reviews::was_rejected(c, k))?,
                    None => false,
                };
                if !rejected {
                    open += 1;
                }
            }
            self.tally.reviews_created += open;
            // A link needs both ends among the memories this run would make or an earlier run made.
            let known: HashSet<String> = self
                .survey
                .knowledge_order
                .iter()
                .map(|id| self.filter.legacy_key(id))
                .collect();
            let ends: Vec<String> = links
                .iter()
                .flat_map(|(a, b)| [a.clone(), b.clone()])
                .collect();
            let stored = self.stored(&ends)?;
            let has = |k: &String| known.contains(k) || stored.contains(k);
            for (a, b) in &links {
                if a != b && has(a) && has(b) {
                    self.tally.links_made += 1;
                } else {
                    self.tally.links_dropped += 1;
                }
            }
            return Ok(());
        }
        let mut links: VecDeque<(String, String)> = links.into();
        let mut reviews: VecDeque<NewReview> = reviews.into();
        loop {
            let mut batch = self.batch();
            let mut n = 0;
            while n < TAIL_PER_BATCH {
                if let Some(l) = links.pop_front() {
                    batch.links.push(l);
                } else if let Some(r) = reviews.pop_front() {
                    batch.reviews.push(r);
                } else {
                    break;
                }
                n += 1;
            }
            let last = links.is_empty() && reviews.is_empty();
            if n > 0 {
                self.submit(batch)?;
            }
            if last {
                return Ok(());
            }
        }
    }

    /// The marker of this run, written last so its verdict is the final one.
    fn marker_pass(&mut self, marker: (String, String)) -> Result<()> {
        let mut batch = self.batch();
        batch.meta.push(marker);
        self.submit(batch)?;
        Ok(())
    }
}

/// Of `ids` (legacy ids), the ones that name a stored memory, each with whether its text names an
/// entity it has no mention link to.
fn unlinked_memories(
    conn: &rusqlite::Connection,
    ids: &[String],
    matcher: &alice_core::store::kg::EntityMatcher,
) -> Result<Vec<(String, bool)>> {
    let marks = vec!["?"; ids.len()].join(",");
    let mut st = conn.prepare(&format!(
        "SELECT l.legacy_id, m.id, m.title, m.body FROM legacy_ids l
         JOIN memories m ON m.id = l.target_id
         WHERE l.target_type = 'memory' AND l.legacy_id IN ({marks})"
    ))?;
    let rows = st.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (legacy, id, title, body) = row?;
        let wanted = matcher.find_in(&format!("{title}\n{body}"));
        let mut linked =
            conn.prepare_cached("SELECT entity_id FROM mentions WHERE memory_id = ?1")?;
        let have: HashSet<String> = linked
            .query_map([&id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        out.push((legacy, wanted.iter().any(|e| !have.contains(e))));
    }
    Ok(out)
}

/// A triple the store would refuse: an empty part (after redaction and normalising).
fn fact_is_valid(f: &PlannedFact) -> bool {
    !f.add.subject.trim().is_empty()
        && !normalize_predicate(&f.add.predicate).is_empty()
        && !f.add.object.trim().is_empty()
}

/// (project, source uri) of a chunk unit; knowledge sorts first.
fn unit_source(u: &Unit) -> (&str, &str) {
    match u {
        Unit::Chunks { slug, source, .. } => (slug, &source.uri),
        Unit::Memories { .. } => ("", ""),
    }
}

/// Runs one phase: the status names it while it runs and the report times it.
fn run_phase<T>(
    status: &Mutex<ImportStatus>,
    phases: &mut Vec<PhaseTiming>,
    name: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    set_phase(status, name);
    let t = Instant::now();
    let out = f();
    phases.push(PhaseTiming {
        phase: name.to_string(),
        seconds: (t.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    });
    out
}

/// Every drawer is imported, collapsed onto its keeper, skipped, held back, refused (and listed
/// in the errors) or already imported by an earlier run. `skipped_total` is the survey's and
/// the planner's skips together.
fn accounted(t: &Tally, skipped_total: u64) -> u64 {
    skipped_total
        + t.already
        + t.knowledge_duplicates
        + t.raw_collapsed
        + t.memory_drawers
        + t.chunk_drawers
        + t.blocked_drawers
        + t.rejected
}

/// What a job that ended early says. Nothing was written: the error alone. Otherwise how far it
/// got, and what to do; a stopped daemon keeps its own sentence.
fn failure(e: AliceError, ctx: &Ctx<'_>) -> AliceError {
    if ctx.submitted == 0 {
        return e;
    }
    let t = &ctx.tally;
    let stopped = matches!(&e, AliceError::Other(m) if m == STOPPED);
    let mut msg = format!(
        "{e}; {} drawers were stored before it ended",
        t.memory_drawers + t.chunk_drawers + t.knowledge_duplicates
    );
    if !t.store_errors.is_empty() {
        msg.push_str(&format!(
            ", {} could not be stored (first: {})",
            t.store_errors.len(),
            t.store_errors[0]
        ));
    }
    if !stopped {
        msg.push_str("; run `alice import mempalace` again to continue; what was imported is kept");
    }
    AliceError::Other(msg)
}

enum Compat {
    /// Already set by the owner (or by an earlier run).
    Left,
    Enabled,
    Warning(String),
}

/// A real run that found `compat.mempalace` at its default turns it on (a value the owner wrote,
/// even `false`, is left alone). Says what happened.
fn enable_compat(env: &ImportEnv, job_id: &str) -> Compat {
    const KEY: &str = "compat.mempalace";
    if env.settings.resolved().sources.get(KEY) != Some(&Source::Default) {
        return Compat::Left;
    }
    let mut change = serde_json::Map::new();
    change.insert(KEY.to_string(), serde_json::Value::Bool(true));
    if let Err(errors) = env.settings.patch(&change) {
        let why = errors
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_default();
        return Compat::Warning(format!(
            "the MemPalace compatibility layer could not be turned on ({why}); run `alice config set compat.mempalace true`"
        ));
    }
    // The settings follower records the `settings.change`; this row says why.
    match env.writer.submit_blocking(WriteOp::RecordEvent {
        actor: Actor::Import,
        action: "import.compat_enabled".into(),
        target_type: "settings".into(),
        target_id: KEY.into(),
        before: None,
        after: Some(serde_json::json!({ "job_id": job_id })),
    }) {
        Ok(_) => Compat::Enabled,
        Err(e) => Compat::Warning(format!(
            "the MemPalace compatibility layer is on, but the import could not record that: {e}"
        )),
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
    // The folder holds a raw, unredacted copy of the owner's history: private to its owner
    // (`snapshot` only sets the mode of a folder it creates itself).
    let prepare = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [dir.parent().unwrap_or(&dir), &dir] {
                std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    };
    prepare().map_err(|e| {
        AliceError::Other(format!(
            "cannot write the import's working folder {} ({e}); make it writable and check free disk space, then run the import again to continue",
            dir.parent().unwrap_or(&dir).display()
        ))
    })?;
    let _cleanup = SnapshotDir { dir: dir.clone() };
    let snap: Snapshot = snapshot(palace, &dir, env.mempalace_home.as_deref())?;
    if let Some(hook) = &env.hook {
        hook.run(&env.stop);
    }
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
        filter: &filter,
        wings: Mutex::new(HashMap::new()),
    };
    let info = reader.info().clone();
    set_progress(&env.status, 0, Some(info.drawers));
    // Wing names reach the map redacted (see `StopReader`), so the override keys and values do too.
    let overrides: BTreeMap<String, String> = req
        .map_wing
        .iter()
        .map(|(k, v)| (filter.quiet(k), filter.quiet(v)))
        .collect();
    let mut wings = WingMap::build(&reader.wing_counts()?, &overrides);
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

    // 3. plan every drawer that is not stored yet; unless this is a dry run, store what it adds
    let planner = Planner {
        mapper: Mapper {
            wings: &wings,
            assume,
            filter: &filter,
        },
        survey: &surveyed,
    };
    // The spellings that point at each project, as stored (a transcript folder is not a wing).
    let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
    for (slug, spellings) in wings.projects() {
        let stored = store_slug(&filter, &slug);
        for w in spellings.iter().filter(|w| !w.starts_with("transcript:")) {
            let alias = store_slug(&filter, w);
            let list = aliases.entry(stored.clone()).or_default();
            if alias != stored && !list.contains(&alias) {
                list.push(alias);
            }
        }
    }
    let skipped_by_survey: u64 = surveyed.skipped.values().sum();
    let mut ctx = Ctx {
        env,
        filter: &filter,
        survey: &surveyed,
        reader: &reader,
        planner,
        tally: Tally::default(),
        done: skipped_by_survey,
        dry: req.dry_run,
        job_id,
        batcher: Batcher::new(),
        aliases,
        sent_projects: HashSet::new(),
        collapsed: Vec::new(),
        submitted: 0,
        pending_links: Vec::new(),
        pending_reviews: Vec::new(),
    };
    if let Some(hook) = &env.hook {
        hook.park_before_planning(&env.stop);
    }
    let kg_path = snap.kg.clone();
    let tunnels_path = snap.tunnels.clone();
    let mut outcome = run_phase(&env.status, &mut phases, "knowledge", || {
        ctx.knowledge_pass()
    });
    if outcome.is_ok() {
        outcome = run_phase(&env.status, &mut phases, "raw", || ctx.raw_pass(since));
    }
    if outcome.is_ok() {
        outcome = run_phase(&env.status, &mut phases, "graph", || {
            ctx.graph_pass(kg_path.as_deref(), tunnels_path.as_deref(), assume)
        });
    }
    if outcome.is_ok() {
        outcome = run_phase(&env.status, &mut phases, "collapsed", || {
            ctx.collapsed_pass()
        });
    }
    if outcome.is_ok() {
        outcome = run_phase(&env.status, &mut phases, "mentions", || {
            if let Some(hook) = &env.hook {
                hook.park_before_mentions(&env.stop);
            }
            ctx.mentions_pass()
        });
    }
    let skipped_of = |ctx: &Ctx<'_>| {
        surveyed.skipped.values().sum::<u64>() + ctx.tally.skipped.values().sum::<u64>()
    };
    let finished_at = now_rfc3339();
    if outcome.is_ok() {
        outcome = run_phase(&env.status, &mut phases, "done", || {
            ctx.closing_pass()?;
            if req.dry_run {
                return Ok(());
            }
            // The marker is the last thing written, so its verdict covers every batch.
            let ok = ctx.tally.store_errors.is_empty()
                && accounted(&ctx.tally, skipped_of(&ctx)) == surveyed.total;
            let marker = serde_json::json!({
                "job_id": job_id,
                "palace": filter.quiet(&palace.display().to_string()),
                "finished_at": finished_at,
                "drawers_seen": surveyed.total,
                "ok": ok,
            });
            ctx.marker_pass(("import.mempalace.last".to_string(), marker.to_string()))
        });
    }
    let accounted_total = accounted(&ctx.tally, skipped_of(&ctx));
    if let Err(e) = outcome {
        return Err(failure(e, &ctx));
    }
    let mut tally = ctx.tally;
    let mut store_errors = std::mem::take(&mut tally.store_errors);
    if store_errors.len() > REPORT_LIST_CAP {
        let more = store_errors.len() - REPORT_LIST_CAP;
        store_errors.truncate(REPORT_LIST_CAP);
        store_errors.push(format!("and {more} more rows were not stored"));
    }
    let mut store_warnings: Vec<String> = std::mem::take(&mut tally.store_warnings)
        .into_iter()
        .collect();
    if store_warnings.len() > REPORT_LIST_CAP {
        store_warnings.truncate(REPORT_LIST_CAP);
    }
    let already_in_alice = tally
        .store_duplicate_memories
        .saturating_sub(tally.duplicate_parts_sent);
    if already_in_alice > 0 {
        store_warnings.push(format!(
            "{already_in_alice} imported memories were already in Alice (same project, kind, title and text); their old ids point at the memory Alice had"
        ));
    }
    warnings.extend(store_warnings);
    if tally.kg_blocked > 0 {
        warnings.push(format!(
            "{} knowledge graph triples were held back because they hold a secret (privacy.redact_mode is block)",
            tally.kg_blocked
        ));
    }
    warnings.extend(std::mem::take(&mut tally.kg_warnings));

    let mut skipped: BTreeMap<String, u64> = BTreeMap::new();
    for (k, v) in &surveyed.skipped {
        *skipped.entry((*k).to_string()).or_default() += v;
    }
    for (k, v) in tally.skipped {
        *skipped.entry(k).or_default() += v;
    }
    let wing_map: BTreeMap<String, WingMapEntry> = surveyed
        .wing_counts
        .iter()
        .map(|(w, n)| (filter.quiet(w), wing_entry(&wings, &filter, w, *n)))
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
            knowledge: surveyed
                .tiers
                .knowledge
                .saturating_sub(tally.already_knowledge),
            raw_session_chunks: surveyed
                .tiers
                .raw_session
                .saturating_sub(tally.already_session),
            raw_session_transcripts: tally.transcripts.len() as u64,
            raw_file_chunks: surveyed.tiers.raw_file.saturating_sub(tally.already_file),
        },
        projects: tally.projects,
        wing_map,
        duplicates: alice_core::import::report::DuplicateCounts {
            knowledge: tally.knowledge_duplicates,
            raw: tally.raw_collapsed,
        },
        drawers_seen: surveyed.total,
        drawers_accounted: accounted_total,
        facts: FactCounts {
            added: tally.facts_added,
            ended: tally.facts_ended,
            skipped: tally.facts_skipped,
        },
        tunnels: TunnelCounts {
            linked: tally.links_made,
            dropped: tally.tunnels_dropped + tally.links_dropped,
            ignored_topic: tally.tunnels_topic,
        },
        isolated_entities: tally.isolated,
        review_items: tally.reviews_created,
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
    report.errors.extend(store_errors);
    report.phases = phases;
    report.peak_rss_mb = peak_rss_mb();
    report.finished_at = Some(finished_at);
    if !req.dry_run {
        match enable_compat(env, job_id) {
            Compat::Enabled => report.compat_enabled = true,
            Compat::Warning(w) => report.warnings.push(w),
            Compat::Left => {}
        }
    }
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
    let ImportEnv { status, guard, .. } = env;
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
            // `STOPPED` is the stop signal's own message; any other failure keeps its own.
            s.error = Some(e.to_string());
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

    fn drain(
        b: &mut Batcher<&'static str>,
        items: &[(&'static str, usize, usize)],
    ) -> Vec<Vec<&'static str>> {
        let mut out = Vec::new();
        for (name, records, bytes) in items {
            out.extend(b.push(name, *records, *bytes));
        }
        out.extend(b.finish());
        out
    }

    #[test]
    fn a_batch_closes_on_a_drawer_boundary_and_never_inside_one() {
        let (r, by) = (BATCH_RECORDS, BATCH_BYTES);
        // r/2 + 3r/4 would cross the record limit: the second drawer opens a new batch, whole.
        let got = drain(
            &mut Batcher::new(),
            &[("d0", r / 2, 10), ("d1", r * 3 / 4, 10), ("d2", 1, 10)],
        );
        assert_eq!(got, vec![vec!["d0"], vec!["d1", "d2"]]);
        // A drawer over the whole budget is a batch of its own.
        let got = drain(
            &mut Batcher::new(),
            &[("a", 1, 10), ("huge", r * 5, 10), ("b", 1, 10)],
        );
        assert_eq!(got, vec![vec!["a"], vec!["huge"], vec!["b"]]);
        // Bytes close a batch too.
        let got = drain(
            &mut Batcher::new(),
            &[
                ("a", 1, by * 5 / 8),
                ("b", 1, by / 2),
                ("c", 1, by * 2),
                ("d", 1, 1),
            ],
        );
        assert_eq!(got, vec![vec!["a"], vec!["b"], vec!["c"], vec!["d"]]);
        // Exactly the limit still fits.
        let got = drain(
            &mut Batcher::new(),
            &[("a", r / 2, 10), ("b", r - r / 2, 10)],
        );
        assert_eq!(got, vec![vec!["a", "b"]]);
        assert!(Batcher::<u8>::new().finish().is_none());
    }

    #[test]
    fn a_drawer_id_that_holds_a_secret_is_filed_under_a_clean_distinct_key() {
        let f = filter();
        assert_eq!(f.legacy_key("drawer_plain_01"), "drawer_plain_01");
        let a = f.legacy_key(concat!(
            "diary_wing_gh",
            "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"
        ));
        let b = f.legacy_key(concat!(
            "diary_wing_gh",
            "p_zYxWvUtSrQpOnMlKjIhGfEdCbA9876543210"
        ));
        assert!(!a.contains("ghp_") && a.contains("[REDACTED:"), "{a}");
        assert_ne!(a, b, "two secrets stay two ids");
        assert_eq!(
            a,
            f.legacy_key(concat!(
                "diary_wing_gh",
                "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"
            ))
        );
        assert_eq!(part_id(&a, 0), a);
        assert_eq!(part_id(&a, 2), format!("{a}#3"));
        assert_eq!(f.redacted_fields(), 0, "keys are not counted");
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
