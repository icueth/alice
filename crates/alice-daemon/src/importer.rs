//! The import job: it runs on a thread of its own (never on the writer or on an async runtime
//! thread) and reports through a shared [`ImportStatus`].
//!
//! Phases: `snapshot` (copy the palace read-only into `ALICE_HOME/import-tmp/<job>/`), `survey`
//! (one pass over every drawer: duplicates, chunk positions), then the planning passes
//! (`knowledge`, `raw`) that turn drawers into what Alice would store. Without `dry_run` the
//! planning passes hand their batches to the writer (one transaction each, closing at 200
//! records or 64 KB of text and only between two drawers); a dry run does the same work and
//! only counts.
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
use alice_core::normalize::{content_hash, derive_title, nfc, sha256_hex, slugify};
use alice_core::redact::Redactor;
use alice_core::settings::{EmbeddingProvider, Settings};
use alice_core::store::ReaderPool;
use alice_core::store::import::{
    BatchOutcome, ImportBatch, ImportMemory, ImportProject, ImportSource, legacy_existing,
};
use alice_core::store::memories::NewMemoryInput;
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
/// A batch closes at this many records ...
const BATCH_RECORDS: usize = 200;
/// ... or this many bytes of text, whichever comes first, and only between two drawers.
const BATCH_BYTES: usize = 64 * 1024;
/// Collapsed pairs per batch (they carry no text).
const COLLAPSED_PER_BATCH: usize = 500;
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
#[derive(Default)]
pub struct ImportHook {
    pub hold: AtomicBool,
    pub panic: AtomicBool,
}

impl ImportHook {
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
enum Unit {
    Memories {
        slug: String,
        items: Vec<NewMemoryInput>,
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

// Notes for the phases that store records (Task 11 adds the graph, tunnels and mentions):
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
}

struct Ctx<'a> {
    env: &'a ImportEnv,
    filter: &'a RedactFilter,
    survey: &'a Survey,
    reader: &'a StopReader,
    planner: Planner<'a>,
    tally: Tally,
    done: u64,
    dry: bool,
    job_id: &'a str,
    batcher: Batcher<Unit>,
    /// Stored project slug -> the aliases to give it (sent with the first batch that names it).
    aliases: HashMap<String, Vec<String>>,
    sent_projects: HashSet<String>,
    /// (collapsed drawer key, keeper key), recorded when the raw phase ends.
    collapsed: Vec<(String, String)>,
}

impl Ctx<'_> {
    fn slug(&self, slug: &str) -> String {
        store_slug(self.filter, slug)
    }

    /// Counts what `d` would add; in a real run also returns the rows to store.
    fn take(&mut self, d: &Drawer) -> Option<Unit> {
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
                Some(Unit::Memories { slug, items })
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

    fn write(&mut self, units: Vec<Unit>) -> Result<()> {
        let mut batch = self.batch();
        let mut slugs: BTreeSet<String> = BTreeSet::new();
        let mut at: HashMap<(String, &'static str, String), usize> = HashMap::new();
        for unit in units {
            match unit {
                Unit::Memories { slug, items } => {
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
        for slug in slugs {
            let aliases = if self.sent_projects.insert(slug.clone()) {
                self.aliases.get(&slug).cloned().unwrap_or_default()
            } else {
                Vec::new()
            };
            batch.projects.push(ImportProject { slug, aliases });
        }
        self.submit(batch)
    }

    /// One writer transaction. The batch holds nothing the filter has not seen.
    fn submit(&mut self, batch: ImportBatch) -> Result<()> {
        check_stop(&self.env.stop)?;
        match self
            .env
            .writer
            .submit_blocking(WriteOp::ImportBatch(Box::new(batch)))?
        {
            WriteOutcome::Imported(out) => {
                self.absorb(out);
                Ok(())
            }
            other => Err(AliceError::Other(format!(
                "the writer answered an import batch with {}",
                other.name()
            ))),
        }
    }

    fn absorb(&mut self, out: BatchOutcome) {
        self.tally.store_errors.extend(out.errors);
        self.tally.store_warnings.extend(out.warnings);
        self.tally.store_duplicate_memories += out.memories_duplicate as u64;
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
        self.record_collapsed()
    }

    /// Collapsed drawers point where their keeper went; their keepers are stored by now.
    fn record_collapsed(&mut self) -> Result<()> {
        let pairs = std::mem::take(&mut self.collapsed);
        for part in pairs.chunks(COLLAPSED_PER_BATCH) {
            let mut batch = self.batch();
            batch.collapsed = part.to_vec();
            self.submit(batch)?;
        }
        Ok(())
    }
}

/// (project, source uri) of a chunk unit; knowledge sorts first.
fn unit_source(u: &Unit) -> (&str, &str) {
    match u {
        Unit::Chunks { slug, source, .. } => (slug, &source.uri),
        Unit::Memories { .. } => ("", ""),
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
    // The folder holds a raw, unredacted copy of the owner's history: private to its owner
    // (`snapshot` only sets the mode of a folder it creates itself).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for d in [dir.parent().unwrap_or(&dir), &dir] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
    }
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
    };
    set_phase(&env.status, "knowledge");
    let t = Instant::now();
    ctx.knowledge_pass()?;
    lap(&mut phases, "knowledge", t);
    set_phase(&env.status, "raw");
    let t = Instant::now();
    ctx.raw_pass(since)?;
    lap(&mut phases, "raw", t);
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
    report.errors.extend(store_errors);
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
        // 100 + 150 would cross 200 records: the second drawer opens a new batch, whole.
        let got = drain(
            &mut Batcher::new(),
            &[("d0", 100, 10), ("d1", 150, 10), ("d2", 1, 10)],
        );
        assert_eq!(got, vec![vec!["d0"], vec!["d1", "d2"]]);
        // A drawer over the whole budget is a batch of its own.
        let got = drain(
            &mut Batcher::new(),
            &[("a", 1, 10), ("huge", 250, 10), ("b", 1, 10)],
        );
        assert_eq!(got, vec![vec!["a"], vec!["huge"], vec!["b"]]);
        // Bytes close a batch too.
        let got = drain(
            &mut Batcher::new(),
            &[
                ("a", 1, 40_000),
                ("b", 1, 30_000),
                ("c", 1, 100_000),
                ("d", 1, 1),
            ],
        );
        assert_eq!(got, vec![vec!["a"], vec!["b"], vec!["c"], vec!["d"]]);
        // Exactly the limit still fits.
        let got = drain(&mut Batcher::new(), &[("a", 100, 10), ("b", 100, 10)]);
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
