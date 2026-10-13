//! The service layer: every operation, used identically by the HTTP API, MCP and CLI.
//!
//! Writes go to the single writer thread; reads run on pooled read-only connections in
//! `spawn_blocking`, so neither ever waits for the other (spec §4.1).

use crate::paths::AliceHome;
use crate::spool;
use crate::worker::{WorkerConfig, spawn_worker};
use crate::writer::{ProjectChoice, WriteOp, WriteOutcome, WriterHandle, spawn_writer};
use alice_core::api::*;
use alice_core::embed::Embedder;
use alice_core::error::{AliceError, Result};
use alice_core::ids::new_id;
use alice_core::model::limits;
use alice_core::normalize::normalize_topic;
use alice_core::project::{ProjectRef, detect_project};
use alice_core::search::{self, SearchParams, fusion::Weights, pack};
use alice_core::store::embeddings;
use alice_core::store::kg::{self, FactOutcome};
use alice_core::store::memories::{self, InsertOutcome, NewMemoryInput};
use alice_core::store::projects::{self, ResolvedProject};
use alice_core::store::sources::{self, IngestOutcome};
use alice_core::store::{ReaderPool, open_reader, open_writer};
use alice_core::vector::VectorIndex;
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `None` while the index is being rebuilt; searches then use lexical candidates only.
pub type IndexSlot = RwLock<Option<Arc<VectorIndex>>>;

/// Texts per embedding batch in the worker.
const EMBED_BATCH: usize = 32;
/// How long a working directory's detected project is reused before git is asked again.
const DETECT_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ServiceOptions {
    /// Cosine similarity at which a vector-only match counts as relevant.
    pub relevance_floor: f32,
    /// Fewer relevant knowledge results than this bring in raw chunks (the raw fallback).
    pub raw_min_results: usize,
    pub weights: Weights,
    /// Most read connections open at once (each has its own page cache); further reads wait.
    pub reader_pool: usize,
    /// Maps a working directory to its project (runs git). Tests replace it.
    pub detect_project: fn(&Path) -> ProjectRef,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            relevance_floor: search::DEFAULT_RELEVANCE_FLOOR,
            raw_min_results: 3,
            weights: Weights::default(),
            reader_pool: 8,
            detect_project,
        }
    }
}

/// The project a read is about.
#[derive(Debug, Clone)]
pub struct ReadScope {
    /// Restrict results to this project id (only when the caller named a project).
    pub filter_id: Option<String>,
    /// The current project. For a project detected from the working directory that has never
    /// been written to, a placeholder with an empty `id` and the slug and name it would get.
    pub project: ResolvedProject,
}

impl ReadScope {
    /// The id of the current project, if it is stored.
    pub fn current_id(&self) -> Option<String> {
        (!self.project.id.is_empty()).then(|| self.project.id.clone())
    }
}

/// Maps (explicit project, working directory) to a project. Cheap to clone; caches git lookups for 60 s.
#[derive(Clone)]
pub struct ProjectResolver {
    cache: Arc<Mutex<HashMap<String, (ProjectRef, Instant)>>>,
    detect: fn(&Path) -> ProjectRef,
}

impl Default for ProjectResolver {
    fn default() -> Self {
        Self::new(detect_project)
    }
}

fn explicit_slug(explicit: Option<&str>) -> Option<&str> {
    explicit.map(str::trim).filter(|s| !s.is_empty())
}

impl ProjectResolver {
    /// A resolver that maps working directories with `detect`.
    pub fn new(detect: fn(&Path) -> ProjectRef) -> Self {
        Self {
            cache: Arc::default(),
            detect,
        }
    }

    fn cached(&self, cwd: &str) -> Option<ProjectRef> {
        let cache = self.cache.lock();
        let (p, at) = cache.get(cwd)?;
        (at.elapsed() < DETECT_TTL).then(|| p.clone())
    }

    /// May run git; call it off the async runtime.
    fn detect(&self, cwd: &str) -> ProjectRef {
        if let Some(p) = self.cached(cwd) {
            return p;
        }
        let p = (self.detect)(Path::new(cwd));
        self.cache
            .lock()
            .insert(cwd.to_string(), (p.clone(), Instant::now()));
        p
    }

    /// Write side: an explicit slug must exist; otherwise the working directory decides; otherwise `global`.
    pub fn choose(&self, explicit: Option<&str>, cwd: Option<&str>) -> ProjectChoice {
        match (explicit_slug(explicit), cwd) {
            (Some(s), _) => ProjectChoice::Slug(s.to_string()),
            (None, Some(c)) => ProjectChoice::Detected(self.detect(c)),
            (None, None) => ProjectChoice::Slug("global".into()),
        }
    }

    /// `choose` when it needs no git lookup; `None` when it does.
    fn choose_without_git(
        &self,
        explicit: Option<&str>,
        cwd: Option<&str>,
    ) -> Option<ProjectChoice> {
        match (explicit_slug(explicit), cwd) {
            (None, Some(c)) => self.cached(c).map(ProjectChoice::Detected),
            _ => Some(self.choose(explicit, cwd)),
        }
    }

    /// Read side, for a `choice` made by `choose` (`explicit`: the caller named the project).
    /// Never creates projects and never runs git: a read resolves its project before it takes a
    /// connection.
    pub fn read_scope(
        conn: &Connection,
        choice: ProjectChoice,
        explicit: bool,
    ) -> Result<ReadScope> {
        match choice {
            ProjectChoice::Slug(s) => {
                let p = projects::resolve_slug(conn, &s)?.ok_or_else(|| {
                    AliceError::invalid(format!(
                        "unknown project \"{s}\"; omit project or use an existing slug"
                    ))
                })?;
                Ok(ReadScope {
                    filter_id: explicit.then(|| p.id.clone()),
                    project: p,
                })
            }
            ProjectChoice::Detected(r) => {
                let project = projects::find_by_key(conn, &r.key)?.unwrap_or(ResolvedProject {
                    id: String::new(),
                    slug: r.slug,
                    name: r.name,
                });
                Ok(ReadScope {
                    filter_id: None,
                    project,
                })
            }
        }
    }
}

pub struct Service {
    home: AliceHome,
    readers: Arc<ReaderPool>,
    writer: WriterHandle,
    index: Arc<IndexSlot>,
    /// Shared by the embedding worker and query embedding.
    embedder: Arc<dyn Embedder>,
    opts: ServiceOptions,
    instance: String,
    stop: Arc<AtomicBool>,
    wake_worker: crossbeam_channel::Sender<()>,
    /// (embedding worker, writer); taken by `shutdown`.
    threads: Mutex<Option<(JoinHandle<()>, JoinHandle<()>)>>,
    resolver: ProjectResolver,
}

fn device_id(conn: &Connection) -> Result<String> {
    if let Some(d) = conn
        .query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
    {
        return Ok(d);
    }
    let d = new_id()[20..].to_lowercase();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('device_id', ?1)",
        [&d],
    )?;
    Ok(d)
}

fn join_err(e: tokio::task::JoinError) -> AliceError {
    AliceError::Other(format!("internal task failed: {e}"))
}

fn unexpected(o: WriteOutcome) -> AliceError {
    AliceError::Other(format!("unexpected writer outcome {o:?}"))
}

/// Rebuild the vector index from the stored vectors on a background thread; the slot stays
/// `None` (lexical-only search, worker paused) until it is done. With `loaded` (an index
/// opened from its files after an unclean stop) only its chunk part is rebuilt, unless the
/// counts still disagree afterwards: then the whole index is.
fn spawn_rebuild(
    slot: Arc<IndexSlot>,
    db: std::path::PathBuf,
    model: String,
    dim: usize,
    loaded: Option<VectorIndex>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("alice-index-rebuild".into())
        .spawn(move || {
            let started = Instant::now();
            let rebuilt = open_reader(&db).and_then(|c| {
                if let Some(idx) = loaded {
                    idx.rebuild_chunks_from(&c)?;
                    let stored = embeddings::count(&c, &model)?;
                    if idx.len() == stored {
                        return Ok(idx);
                    }
                    tracing::info!(
                        indexed = idx.len(),
                        stored,
                        "memory vectors are out of date too; rebuilding the whole index"
                    );
                }
                VectorIndex::rebuild_from(&c, &model, dim)
            });
            match rebuilt {
                Ok(idx) => {
                    tracing::info!(
                        n = idx.len(),
                        secs = started.elapsed().as_secs_f32(),
                        "vector index rebuilt"
                    );
                    *slot.write() = Some(Arc::new(idx));
                }
                Err(e) => {
                    tracing::error!(error = %e, "vector index rebuild failed; starting with an empty index");
                    *slot.write() = VectorIndex::for_model(&model, dim).ok().map(Arc::new);
                }
            }
        })?;
    Ok(())
}

impl Service {
    pub fn start(
        home: AliceHome,
        make_embedder: &dyn Fn() -> Result<Arc<dyn Embedder>>,
        opts: ServiceOptions,
    ) -> Result<Arc<Service>> {
        home.ensure_dirs()?;
        let conn = open_writer(&home.db())?;
        let device = device_id(&conn)?;
        let readers = Arc::new(ReaderPool::new(home.db(), opts.reader_pool));
        // One instance for the worker and for queries: the local model costs ~600 MB each. It
        // embeds passages in short batches and lets a waiting query go next after each one.
        let embedder = make_embedder()?;
        let model = embedder.model_id().to_string();
        let dim = embedder.dim();
        let index: Arc<IndexSlot> = Arc::new(RwLock::new(None));
        let vector_path = home.vector_file(&model);

        let expected = embeddings::count(&conn, &model)?;
        // A clean stop marks the files it saved last. Without the mark (a crash, say while
        // embedding) the chunk file can miss chunks or hold stale ones even when the counts
        // agree, and rebuilding the chunk part takes seconds.
        let clean = VectorIndex::take_clean_mark(&vector_path);
        match VectorIndex::open(&vector_path, &model, dim) {
            Ok(idx) if !clean => {
                tracing::info!("the last stop was not clean; rebuilding the chunk vectors");
                spawn_rebuild(index.clone(), home.db(), model.clone(), dim, Some(idx))?;
            }
            Ok(idx) if idx.len() == expected => *index.write() = Some(Arc::new(idx)),
            Ok(idx) => {
                tracing::info!(
                    indexed = idx.len(),
                    stored = expected,
                    "vector index is out of date; rebuilding"
                );
                spawn_rebuild(index.clone(), home.db(), model.clone(), dim, None)?;
            }
            Err(e) => {
                tracing::info!(error = %e, "no usable vector index; rebuilding");
                spawn_rebuild(index.clone(), home.db(), model.clone(), dim, None)?;
            }
        }

        // bounded(1): repeated wake-ups coalesce.
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let (writer, writer_thread) = spawn_writer(conn, device, wake_tx.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_thread = spawn_worker(WorkerConfig {
            readers: readers.clone(),
            writer: writer.clone(),
            index: index.clone(),
            embedder: embedder.clone(),
            wake: wake_rx,
            stop: stop.clone(),
            vector_path,
            batch: EMBED_BATCH,
        });
        let resolver = ProjectResolver::new(opts.detect_project);
        Ok(Arc::new(Service {
            home,
            readers,
            writer,
            index,
            embedder,
            opts,
            instance: new_id(),
            stop,
            wake_worker: wake_tx,
            threads: Mutex::new(Some((worker_thread, writer_thread))),
            resolver,
        }))
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn home(&self) -> &AliceHome {
        &self.home
    }

    /// Why writes are refused (`None` = writable). No database access, so it is cheap to poll.
    pub fn read_only(&self) -> Option<String> {
        self.writer.read_only.reason()
    }

    /// Tests: refuse writes as a damaged database would.
    #[cfg(test)]
    pub(crate) fn force_read_only(&self, reason: &str) {
        self.writer.read_only.set(reason.to_string());
    }

    /// Stop the embedding worker, let the writer commit everything queued before this call,
    /// stop it, save the vector index and mark the saved files clean (the next start trusts
    /// them without rebuilding). Blocks until both threads have exited (normally well under a
    /// second; an embedding batch in flight is finished first). Later writes fail with "writer
    /// stopped"; reads keep working. Calling it again does nothing.
    pub fn shutdown(&self) {
        let Some((worker, writer)) = self.threads.lock().take() else {
            return;
        };
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake_worker.try_send(());
        if worker.join().is_err() {
            tracing::error!("the embedding worker panicked");
        }
        self.writer.send_nowait(WriteOp::Shutdown);
        if writer.join().is_err() {
            tracing::error!("the writer panicked");
        }
        // No index yet (still rebuilding): nothing is marked, so the next start rebuilds too.
        let Some(idx) = self.index.read().clone() else {
            return;
        };
        let path = self.home.vector_file(self.embedder.model_id());
        let saved = if idx.is_dirty() {
            idx.save(&path)
        } else {
            Ok(())
        };
        if let Err(e) = saved.and_then(|()| VectorIndex::mark_clean(&path)) {
            tracing::warn!(error = %e, "saving the vector index failed; the next start rebuilds it");
        }
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let readers = self.readers.clone();
        tokio::task::spawn_blocking(move || readers.with(f))
            .await
            .map_err(join_err)?
    }

    /// Project for a write. A working directory not seen in the last minute is resolved with
    /// git, which runs on the blocking pool rather than on the async runtime.
    async fn choose(&self, explicit: Option<&str>, cwd: Option<&str>) -> Result<ProjectChoice> {
        if let Some(choice) = self.resolver.choose_without_git(explicit, cwd) {
            return Ok(choice);
        }
        let resolver = self.resolver.clone();
        let (explicit, cwd) = (explicit.map(String::from), cwd.map(String::from));
        tokio::task::spawn_blocking(move || resolver.choose(explicit.as_deref(), cwd.as_deref()))
            .await
            .map_err(join_err)
    }

    pub async fn remember(&self, req: RememberRequest) -> Result<RememberResponse> {
        let project = self
            .choose(req.project.as_deref(), req.cwd.as_deref())
            .await?;
        let input = NewMemoryInput {
            content: req.content,
            kind: req.kind,
            title: req.title,
            topics: req.topics,
            importance: req.importance,
            supersedes: req.supersedes,
            idempotency_key: req.idempotency_key,
            author: req.author,
            ..Default::default()
        };
        match self
            .writer
            .submit(WriteOp::Remember { input, project })
            .await?
        {
            WriteOutcome::Remembered(InsertOutcome::New { id, .. }, p) => Ok(RememberResponse {
                id: Some(id),
                status: RememberStatus::New,
                project: p.slug,
                reason: None,
            }),
            WriteOutcome::Remembered(InsertOutcome::Duplicate { id }, p) => Ok(RememberResponse {
                id: Some(id),
                status: RememberStatus::Duplicate,
                project: p.slug,
                reason: None,
            }),
            other => Err(unexpected(other)),
        }
    }

    /// `remember`, or, while the database is read-only, the request queued in the spool and
    /// applied after a repair (spec §5.7), with a reply that says so. Every interface of the
    /// daemon (the JSON API, MCP over HTTP) remembers through this. If even the spool cannot be
    /// written (often the same cause, a full disk), the read-only error says what to do.
    pub async fn remember_or_spool(&self, req: RememberRequest) -> Result<RememberResponse> {
        let read_only = match self.remember(req.clone()).await {
            Err(AliceError::ReadOnly(reason)) => reason,
            other => return other,
        };
        let dir = self.home.spool_dir();
        let entry = spool::SpoolEntry::Remember(req);
        match tokio::task::spawn_blocking(move || spool::append(&dir, &entry)).await {
            Ok(Ok(_)) => Ok(RememberResponse {
                id: None,
                status: RememberStatus::Spooled,
                project: String::new(),
                reason: Some(spool::read_only_note(&read_only)),
            }),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "cannot spool a remember while read-only");
                Err(AliceError::ReadOnly(read_only))
            }
            Err(e) => {
                tracing::warn!(error = %e, "spooling a remember failed");
                Err(AliceError::ReadOnly(read_only))
            }
        }
    }

    pub async fn recall(&self, req: RecallRequest) -> Result<RecallResponse> {
        let limit = req
            .limit
            .unwrap_or(limits::DEFAULT_RECALL_LIMIT)
            .min(limits::MAX_RECALL_LIMIT);
        let max_tokens = req.max_tokens.unwrap_or(limits::DEFAULT_RECALL_TOKENS);
        let format = req.format;
        let opts = self.opts.clone();
        let index = self.index.read().clone();
        // Embedded before a reader connection is taken: the model is shared with the
        // embedding worker, and a recall waiting for it must not hold a connection.
        let text = search::query_text(&req.query)?;
        let vector = match &index {
            Some(idx) => {
                let (idx, embedder) = (idx.clone(), self.embedder.clone());
                tokio::task::spawn_blocking(move || {
                    search::embed_query(Some(&idx), embedder.as_ref(), &text)
                })
                .await
                .map_err(join_err)?
            }
            None => None,
        };
        // Resolved before a reader connection is taken too: it may run git.
        let explicit = explicit_slug(req.project.as_deref()).is_some();
        let choice = self
            .choose(req.project.as_deref(), req.cwd.as_deref())
            .await?;
        let (out, slug) = self
            .read(move |c| {
                let scope = ProjectResolver::read_scope(c, choice, explicit)?;
                let params = SearchParams {
                    query: req.query,
                    project_filter_id: scope.filter_id.clone(),
                    current_project_id: scope.current_id(),
                    topics: req
                        .topics
                        .iter()
                        .filter_map(|t| normalize_topic(t))
                        .collect(),
                    kinds: req.kinds,
                    since: req.since,
                    before: req.before,
                    include_raw: req.include_raw,
                    limit,
                    relevance_floor: opts.relevance_floor,
                    raw_min_results: opts.raw_min_results,
                };
                let out = search::run_with_vector(
                    c,
                    index.as_deref(),
                    vector,
                    &opts.weights,
                    &params,
                    chrono::Utc::now(),
                )?;
                Ok((out, scope.project.slug))
            })
            .await?;
        let (response, touched) = match format {
            OutputFormat::Text => {
                let (text, omitted) = pack::pack_text(&out, Some(&slug), max_tokens);
                let shown = &out.hits[..out.hits.len() - omitted];
                let touched = returned_memories(out.memory_seqs.clone(), shown);
                let response = RecallResponse {
                    project: Some(slug),
                    omitted,
                    raw_fallback: out.raw_fallback,
                    text: Some(text),
                    ..Default::default()
                };
                (response, touched)
            }
            OutputFormat::Json => {
                let mut out = out;
                let seqs = std::mem::take(&mut out.memory_seqs);
                let response = pack::pack_json(out, Some(slug), max_tokens);
                let touched = returned_memories(seqs, &response.results);
                (response, touched)
            }
        };
        if !touched.is_empty() {
            self.writer.send_nowait(WriteOp::TouchAccess(touched));
        }
        Ok(response)
    }

    pub async fn context(&self, req: ContextRequest) -> Result<ContextResponse> {
        let max_tokens = req.max_tokens.unwrap_or(limits::DEFAULT_CONTEXT_TOKENS);
        let explicit = explicit_slug(req.project.as_deref()).is_some();
        let choice = self
            .choose(req.project.as_deref(), req.cwd.as_deref())
            .await?;
        self.read(move |c| {
            let scope = ProjectResolver::read_scope(c, choice, explicit)?;
            // A placeholder project (never written to) gets the hint and the global preferences.
            let text =
                alice_core::context::build(c, &scope.project, max_tokens, chrono::Utc::now())?;
            Ok(ContextResponse {
                project: scope.project.slug,
                text,
            })
        })
        .await
    }

    pub async fn get(&self, req: GetRequest) -> Result<GetResponse> {
        self.read(move |c| {
            let found = memories::get_many(c, &req.ids)?;
            let mut out = GetResponse::default();
            for id in &req.ids {
                if !found
                    .iter()
                    .any(|m| &m.id == id || m.legacy_id.as_deref() == Some(id.as_str()))
                {
                    out.missing.push(id.clone());
                }
            }
            for m in found {
                out.memories.push(full(c, m, req.include_provenance)?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn update(&self, req: UpdateRequest) -> Result<MemoryFull> {
        let m = match self
            .writer
            .submit(WriteOp::Update {
                id: req.id,
                update: req.update,
            })
            .await?
        {
            WriteOutcome::Updated(m) => m,
            other => return Err(unexpected(other)),
        };
        self.read(move |c| full(c, m, false)).await
    }

    pub async fn facts(&self, req: FactsRequest) -> Result<FactsResponse> {
        self.read(move |c| {
            let facts = kg::query_facts(c, &req)?;
            let mut text = String::new();
            if facts.is_empty() {
                text.push_str(&format!("No facts about \"{}\".\n", req.entity));
            }
            for f in &facts {
                let day = |t: &str| t.chars().take(10).collect::<String>();
                let until = f
                    .valid_to
                    .as_deref()
                    .map(|t| format!(" until {}", day(t)))
                    .unwrap_or_default();
                text.push_str(&format!(
                    "- {} {} {} (since {}{}) [{}]\n",
                    f.subject,
                    f.predicate,
                    f.object,
                    day(&f.valid_from),
                    until,
                    f.id
                ));
            }
            Ok(FactsResponse {
                entity: req.entity.clone(),
                facts,
                text,
            })
        })
        .await
    }

    pub async fn assert_fact(&self, req: AssertFactRequest) -> Result<FactOutcome> {
        match self.writer.submit(WriteOp::AssertFact(req)).await? {
            WriteOutcome::Fact(f) => Ok(f),
            other => Err(unexpected(other)),
        }
    }

    pub async fn ingest(&self, req: IngestRequest) -> Result<IngestOutcome> {
        // Checked here so an oversized request never reaches (or delays) the writer.
        sources::validate_chunks(&req.chunks)?;
        let project = self
            .choose(req.project.as_deref(), req.cwd.as_deref())
            .await?;
        match self
            .writer
            .submit(WriteOp::Ingest {
                project,
                source: req.source,
                chunks: req.chunks,
            })
            .await?
        {
            WriteOutcome::Ingested(o) => Ok(o),
            other => Err(unexpected(other)),
        }
    }

    pub async fn projects(&self) -> Result<Vec<ResolvedProject>> {
        self.read(projects::list_projects).await
    }

    pub async fn health(&self) -> Result<Health> {
        let instance = self.instance.clone();
        let read_only = self.writer.read_only.reason();
        let index_ready = self.index.read().is_some();
        self.read(move |c| {
            let count = |sql: &str| -> Result<u64> {
                Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
            };
            Ok(Health {
                service: "alice".into(),
                instance,
                version: env!("CARGO_PKG_VERSION").into(),
                pid: std::process::id(),
                memories: count("SELECT count(*) FROM memories WHERE status <> 'trashed'")?,
                chunks: count("SELECT count(*) FROM chunks")?,
                projects: count("SELECT count(*) FROM projects")?,
                embed_backlog: embeddings::backlog(c)? as u64,
                read_only,
                index_ready,
            })
        })
        .await
    }
}

impl Drop for Service {
    /// Without an explicit `shutdown`, ask both threads to stop but do not wait for them.
    fn drop(&mut self) {
        if self.threads.get_mut().is_some() {
            self.stop.store(true, Ordering::SeqCst);
            let _ = self.wake_worker.try_send(());
            self.writer.send_nowait(WriteOp::Shutdown);
        }
    }
}

/// The memories a response returns, out of `seqs` (one per knowledge hit, in hit order): those
/// among `returned`, the hits it kept (packing keeps a prefix). Only they count an access
/// (spec §4.5), not hits the token budget left out.
fn returned_memories(mut seqs: Vec<i64>, returned: &[Hit]) -> Vec<i64> {
    seqs.truncate(
        returned
            .iter()
            .filter(|h| h.tier == Tier::Knowledge)
            .count(),
    );
    seqs
}

fn full(
    c: &Connection,
    m: alice_core::model::Memory,
    include_provenance: bool,
) -> Result<MemoryFull> {
    let mut links = Vec::new();
    let mut stmt = c.prepare_cached(
        "SELECT to_id, type, 'out' FROM links WHERE from_id = ?1 UNION ALL SELECT from_id, type, 'in' FROM links WHERE to_id = ?1",
    )?;
    for r in stmt.query_map([&m.id], |r| {
        Ok(LinkRef {
            id: r.get(0)?,
            link_type: r.get(1)?,
            direction: r.get(2)?,
        })
    })? {
        links.push(r?);
    }
    let provenance = if include_provenance {
        let mut stmt = c.prepare_cached("SELECT chunk_id FROM provenance WHERE memory_id = ?1")?;
        stmt.query_map([&m.id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?
    } else {
        Vec::new()
    };
    Ok(MemoryFull {
        memory: m,
        links,
        provenance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::embed::FakeEmbedder;
    use alice_core::model::Kind;
    use alice_core::store::memories::{MemoryUpdate, UpdateAction};
    use alice_core::store::sources::{ChunkInput, SourceInput};

    async fn svc() -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap();
        (d, s)
    }

    async fn wait_embedded(s: &Service) {
        for _ in 0..200 {
            if s.health().await.unwrap().embed_backlog == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("embedding backlog did not drain");
    }

    fn repo(d: &tempfile::TempDir, name: &str) -> Option<String> {
        let repo = d.path().join(name);
        std::fs::create_dir(&repo).unwrap();
        Some(repo.to_string_lossy().into_owned())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remember_recall_get_update_round_trip() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        let r = s
            .remember(RememberRequest {
                content: "Use WAL with one writer thread".into(),
                kind: Some(Kind::Decision),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.status, RememberStatus::New);
        assert_eq!(r.project, "proj");
        let dup = s
            .remember(RememberRequest {
                content: "Use WAL with one writer thread".into(),
                kind: Some(Kind::Decision),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(dup.status, RememberStatus::Duplicate);
        wait_embedded(&s).await;
        let found = s
            .recall(RecallRequest {
                query: "writer thread".into(),
                cwd: cwd.clone(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(found.results[0].id, r.id.clone().unwrap());
        let text = s
            .recall(RecallRequest {
                query: "writer thread".into(),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap()
            .text
            .unwrap();
        assert!(text.contains("Use WAL"));
        let got = s
            .get(GetRequest {
                ids: vec![r.id.clone().unwrap(), "MISSING".into()],
                include_provenance: false,
            })
            .await
            .unwrap();
        assert_eq!(got.memories.len(), 1);
        assert_eq!(got.missing, vec!["MISSING".to_string()]);
        assert_eq!(
            got.memories[0].memory.status,
            alice_core::model::Status::Active
        );
        let ctx = s
            .context(ContextRequest {
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(ctx.text.contains("Use WAL"), "{}", ctx.text);

        let edited = s
            .update(UpdateRequest {
                id: r.id.clone().unwrap(),
                update: MemoryUpdate {
                    action: UpdateAction::Edit,
                    body: Some("Use WAL so readers never block the lone writer".into()),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        assert!(edited.memory.body.contains("readers never block"));
        wait_embedded(&s).await;
        let found = s
            .recall(RecallRequest {
                query: "readers never block".into(),
                cwd: cwd.clone(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(found.results[0].id, r.id.clone().unwrap());
        assert!(
            found.results[0].matched.contains(&MatchVia::Vector),
            "the edited text was re-embedded: {:?}",
            found.results[0].matched
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn facts_round_trip_and_unknown_project_errors() {
        let (_d, s) = svc().await;
        s.assert_fact(AssertFactRequest {
            subject: "alice".into(),
            predicate: "written in".into(),
            object: "Rust".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let f = s
            .facts(FactsRequest {
                entity: "Alice".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(f.facts[0].predicate, "written_in");
        assert!(f.text.contains("written_in"));
        let err = s
            .remember(RememberRequest {
                content: "x".into(),
                project: Some("ghost".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown project"));
        let err = s
            .recall(RecallRequest {
                query: "x".into(),
                project: Some("ghost".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown project"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_keeps_data_and_rebuilds_a_missing_index() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let s = Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap();
        let id = s
            .remember(RememberRequest {
                content: "persist me across restarts".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
            .unwrap();
        wait_embedded(&s).await;
        s.shutdown();
        drop(s);
        // The index is two files next to the base path (memories and chunks).
        let base = home.vector_file("fake");
        for part in ["memories.usearch", "chunks.bq"] {
            std::fs::remove_file(base.with_extension(part)).unwrap();
        }
        let s2 = Service::start(home, &mk, ServiceOptions::default()).unwrap();
        for _ in 0..100 {
            if s2.health().await.unwrap().index_ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(s2.health().await.unwrap().index_ready);
        let out = s2
            .recall(RecallRequest {
                query: "persist restarts".into(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.results[0].id, id);
        assert!(
            out.results[0].matched.contains(&MatchVia::Vector),
            "the rebuilt index serves vector matches: {:?}",
            out.results[0].matched
        );
    }

    /// Review finding m-8 (spec §10): a damaged vector index (cut short, or the right size full
    /// of garbage) is rebuilt from the stored vectors at start, and vector search works again.
    /// Both parts damaged, or only the sign-bit chunk file.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_truncated_or_corrupted_index_is_rebuilt_at_start() {
        type Damage = fn(&Path);
        let truncate: Damage = |p| {
            let head = std::fs::read(p).unwrap()[..100].to_vec();
            std::fs::write(p, head).unwrap();
        };
        let garbage: Damage = |p| {
            let len = std::fs::metadata(p).unwrap().len() as usize;
            let junk: Vec<u8> = (0..len).map(|i| (i * 7 + 13) as u8).collect();
            std::fs::write(p, junk).unwrap();
        };
        let cases = [
            (
                "truncated",
                truncate,
                &["memories.usearch", "chunks.bq"][..],
            ),
            ("garbage", garbage, &["memories.usearch", "chunks.bq"][..]),
            ("truncated chunk file", truncate, &["chunks.bq"][..]),
            ("garbage chunk file", garbage, &["chunks.bq"][..]),
        ];
        for (how, damage, parts) in cases {
            let d = tempfile::tempdir().unwrap();
            let home = AliceHome::new(d.path().to_path_buf());
            let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
            let s = Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap();
            let id = s
                .remember(RememberRequest {
                    content: "vectors survive a damaged index file".into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .id
                .unwrap();
            s.ingest(IngestRequest {
                source: SourceInput {
                    uri: "s".into(),
                    ..Default::default()
                },
                chunks: vec![ChunkInput {
                    ord: 0,
                    text: "a raw chunk whose vector also survives".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .await
            .unwrap();
            wait_embedded(&s).await;
            tokio::task::spawn_blocking({
                let s = s.clone();
                move || s.shutdown()
            })
            .await
            .unwrap();
            drop(s);
            let base = home.vector_file("fake");
            for part in parts {
                damage(&base.with_extension(part));
            }
            let s = Service::start(home, &mk, ServiceOptions::default()).unwrap();
            for _ in 0..200 {
                if s.health().await.unwrap().index_ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(s.health().await.unwrap().index_ready, "{how}");
            let out = s
                .recall(RecallRequest {
                    query: "vectors survive a damaged index file".into(),
                    format: OutputFormat::Json,
                    include_raw: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            let hit = out.results.iter().find(|h| h.id == id).expect(how);
            assert!(hit.matched.contains(&MatchVia::Vector), "{how}: {hit:?}");
            assert!(
                out.results
                    .iter()
                    .any(|h| h.tier == Tier::Raw && h.matched.contains(&MatchVia::Vector)),
                "{how}: chunk vectors are back too: {:?}",
                out.results
            );
        }
    }

    /// Fix round 1 (M9): a clean stop marks the files it saved and the next start uses them as
    /// they are; a start without the mark (after a crash) rebuilds the chunk part from the
    /// stored vectors even though the counts agree.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_chunk_part_is_rebuilt_after_an_unclean_stop_and_trusted_after_a_clean_one() {
        use alice_core::bq::BinaryIndex;
        use alice_core::store::embeddings::VecKey;
        use alice_core::vector::KeyFilter;
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let s = Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap();
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            chunks: (0..3)
                .map(|i| ChunkInput {
                    ord: i,
                    text: format!("chunk number {i} of the session"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_embedded(&s).await;
        let stop = |s: Arc<Service>| tokio::task::spawn_blocking(move || s.shutdown());
        stop(s.clone()).await.unwrap();
        drop(s);
        let base = home.vector_file("fake");
        let mark = base.with_extension("clean");
        assert!(mark.exists(), "a clean stop marks the files it saved");
        let seqs: Vec<i64> = {
            let c = open_reader(&home.db()).unwrap();
            let mut stmt = c.prepare("SELECT seq FROM chunks ORDER BY seq").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        // A stale chunk file with the right count: the newest chunk missing, a gone one there.
        let write_stale = || {
            let e = FakeEmbedder::new(384);
            let mut stale = BinaryIndex::new(384);
            for seq in [seqs[0], seqs[1], 999] {
                stale
                    .upsert(seq, &e.embed_query(&format!("x{seq}")).unwrap())
                    .unwrap();
            }
            let f = std::fs::File::create(base.with_extension("chunks.bq")).unwrap();
            stale.write_to(std::io::BufWriter::new(f)).unwrap();
        };
        let chunk_seqs = |s: &Service| -> Vec<i64> {
            let q = FakeEmbedder::new(384).embed_query("chunk").unwrap();
            let idx = s.index.read().clone().expect("index ready");
            let mut out: Vec<i64> = idx
                .search(&q, 10, KeyFilter::Chunks)
                .unwrap()
                .into_iter()
                .map(|(k, _)| match k {
                    VecKey::Chunk(s) | VecKey::Memory(s) => s,
                })
                .collect();
            out.sort();
            out
        };
        let ready = |s: Arc<Service>| async move {
            for _ in 0..200 {
                if s.health().await.unwrap().index_ready {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the index never became ready");
        };

        write_stale();
        let s = ready(Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap()).await;
        assert!(!mark.exists(), "the mark is taken at start");
        assert_eq!(
            chunk_seqs(&s),
            vec![seqs[0], seqs[1], 999],
            "after a clean stop the saved chunk file is used as it is"
        );
        stop(s.clone()).await.unwrap();
        drop(s);

        write_stale();
        std::fs::remove_file(&mark).unwrap(); // as after a crash
        let s = ready(Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap()).await;
        assert_eq!(chunk_seqs(&s), seqs, "rebuilt from the stored vectors");
        stop(s.clone()).await.unwrap();
        assert!(mark.exists());
    }

    /// An index saved by M1 has `chunks.usearch` (an int8 HNSW) and no `chunks.bq`: the chunk
    /// part is rebuilt from the stored vectors at start, chunk vectors are found again, and the
    /// first save writes `chunks.bq` and deletes the M1 file.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_m1_chunk_index_is_replaced_by_a_rebuilt_sign_bit_index() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let s = Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap();
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            chunks: vec![ChunkInput {
                ord: 0,
                text: "a raw chunk written before the upgrade".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .unwrap();
        wait_embedded(&s).await;
        tokio::task::spawn_blocking({
            let s = s.clone();
            move || s.shutdown()
        })
        .await
        .unwrap();
        drop(s);
        let base = home.vector_file("fake");
        let (bq, legacy) = (
            base.with_extension("chunks.bq"),
            base.with_extension("chunks.usearch"),
        );
        std::fs::remove_file(&bq).unwrap();
        std::fs::write(&legacy, b"an M1 int8 HNSW of the chunks").unwrap();

        let s = Service::start(home, &mk, ServiceOptions::default()).unwrap();
        for _ in 0..200 {
            if s.health().await.unwrap().index_ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(s.health().await.unwrap().index_ready);
        let out = s
            .recall(RecallRequest {
                query: "a raw chunk written before the upgrade".into(),
                format: OutputFormat::Json,
                include_raw: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            out.results
                .iter()
                .any(|h| h.tier == Tier::Raw && h.matched.contains(&MatchVia::Vector)),
            "{:?}",
            out.results
        );
        tokio::task::spawn_blocking({
            let s = s.clone();
            move || s.shutdown()
        })
        .await
        .unwrap();
        assert!(bq.exists(), "the rebuilt chunk part is saved");
        assert!(!legacy.exists(), "the M1 chunk file is deleted");
    }

    /// Ruling: a project detected from the working directory but never written to still gets
    /// the global preferences, and reading it does not create it.
    #[tokio::test(flavor = "multi_thread")]
    async fn context_for_an_unwritten_project_keeps_global_preferences() {
        let (d, s) = svc().await;
        s.remember(RememberRequest {
            content: "Always answer in Thai".into(),
            kind: Some(Kind::Preference),
            ..Default::default()
        })
        .await
        .unwrap();
        let ctx = s
            .context(ContextRequest {
                cwd: repo(&d, "brand-new"),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ctx.project, "brand-new");
        assert!(
            ctx.text.contains("No memories yet for brand-new"),
            "{}",
            ctx.text
        );
        assert!(ctx.text.contains("Always answer in Thai"), "{}", ctx.text);
        let slugs: Vec<String> = s
            .projects()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.slug)
            .collect();
        assert_eq!(
            slugs,
            vec!["global".to_string()],
            "reads never create projects"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ingest_checks_limits_first_and_raw_recall_finds_chunks() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        let too_many = (0..1_001)
            .map(|i| ChunkInput {
                ord: i,
                text: "x".into(),
                ..Default::default()
            })
            .collect();
        let err = s
            .ingest(IngestRequest {
                source: SourceInput {
                    uri: "big".into(),
                    ..Default::default()
                },
                chunks: too_many,
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1000"), "{err}");
        assert_eq!(s.health().await.unwrap().projects, 1, "nothing was written");
        let out = s
            .ingest(IngestRequest {
                source: SourceInput {
                    uri: "session-1".into(),
                    ..Default::default()
                },
                chunks: vec![ChunkInput {
                    ord: 0,
                    text: "the lease expired because the writer held a long transaction".into(),
                    ..Default::default()
                }],
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.inserted, 1);
        wait_embedded(&s).await;
        let found = s
            .recall(RecallRequest {
                query: "lease expired".into(),
                cwd,
                include_raw: true,
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(found.results[0].tier, Tier::Raw);
        let h = s.health().await.unwrap();
        assert_eq!((h.chunks, h.projects, h.memories), (1, 2, 0));
    }

    /// Review finding M2: a recall embeds its query before it takes a reader connection, so a
    /// recall waiting for the (shared, possibly busy) model never holds up other reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recall_waiting_for_the_model_holds_no_reader_connection() {
        struct SlowQueries(FakeEmbedder);
        impl Embedder for SlowQueries {
            fn model_id(&self) -> &str {
                self.0.model_id()
            }
            fn dim(&self) -> usize {
                self.0.dim()
            }
            fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                self.0.embed_passages(texts)
            }
            fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
                std::thread::sleep(Duration::from_millis(600));
                self.0.embed_query(text)
            }
        }
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(SlowQueries(FakeEmbedder::new(384))) as Arc<dyn Embedder>),
            ServiceOptions {
                reader_pool: 1,
                ..Default::default()
            },
        )
        .unwrap();
        for _ in 0..200 {
            if s.health().await.unwrap().index_ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(s.health().await.unwrap().index_ready);
        let recall = {
            let s = s.clone();
            tokio::spawn(async move {
                s.recall(RecallRequest {
                    query: "anything".into(),
                    ..Default::default()
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t = Instant::now();
        s.context(ContextRequest::default()).await.unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(300),
            "context waited {:?} for the reader the recall held",
            t.elapsed()
        );
        recall.await.unwrap().unwrap();
    }

    /// Review finding m-10: a read resolves its project (git, for a working directory not seen
    /// in the last minute) before it takes a reader connection, so a slow git never holds up
    /// other reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn project_detection_holds_no_reader_connection() {
        fn slow_detect(p: &Path) -> ProjectRef {
            std::thread::sleep(Duration::from_millis(600));
            detect_project(p)
        }
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                reader_pool: 1,
                detect_project: slow_detect,
                ..Default::default()
            },
        )
        .unwrap();
        let reads = [
            ("recall", repo(&d, "slow-recall")),
            ("context", repo(&d, "slow-context")),
        ];
        for (what, cwd) in reads {
            let slow = {
                let s = s.clone();
                tokio::spawn(async move {
                    if what == "recall" {
                        s.recall(RecallRequest {
                            query: "anything".into(),
                            cwd,
                            ..Default::default()
                        })
                        .await
                        .map(|_| ())
                    } else {
                        s.context(ContextRequest {
                            cwd,
                            ..Default::default()
                        })
                        .await
                        .map(|_| ())
                    }
                })
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            let t = Instant::now();
            s.projects().await.unwrap();
            assert!(
                t.elapsed() < Duration::from_millis(300),
                "a read waited {:?} for the reader a {what} held while it ran git",
                t.elapsed()
            );
            slow.await.unwrap().unwrap();
        }
    }

    /// Review finding m-6 (spec §4.5): only memories a recall returns count an access; hits
    /// the token budget left out do not.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_returned_memories_count_an_access() {
        let (_d, s) = svc().await;
        let mut ids = Vec::new();
        for i in 0..6 {
            let content = format!("zebra crossing note {i}: {}", "padding words ".repeat(40));
            let r = s
                .remember(RememberRequest {
                    content,
                    ..Default::default()
                })
                .await
                .unwrap();
            ids.push(r.id.unwrap());
        }
        wait_embedded(&s).await;
        let mut expected: HashMap<String, i64> = ids.iter().map(|id| (id.clone(), 0)).collect();
        for format in [OutputFormat::Text, OutputFormat::Json] {
            let r = s
                .recall(RecallRequest {
                    query: "zebra crossing".into(),
                    max_tokens: Some(120),
                    format,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(r.omitted > 0, "{format:?}: the budget leaves some out");
            let shown: Vec<&String> = match format {
                OutputFormat::Text => {
                    let text = r.text.as_deref().unwrap();
                    ids.iter().filter(|id| text.contains(id.as_str())).collect()
                }
                OutputFormat::Json => r.results.iter().map(|h| &h.id).collect(),
            };
            assert_eq!(shown.len(), 6 - r.omitted, "{format:?}");
            for id in shown {
                *expected.get_mut(id).unwrap() += 1;
            }
        }
        // The counters go to the writer without a reply; a later write is committed after them.
        s.remember(RememberRequest {
            content: "barrier".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let got = s
            .get(GetRequest {
                ids: ids.clone(),
                include_provenance: false,
            })
            .await
            .unwrap();
        let counted: HashMap<String, i64> = got
            .memories
            .iter()
            .map(|m| (m.memory.id.clone(), m.memory.access_count))
            .collect();
        assert_eq!(counted, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_is_idempotent_and_refuses_later_writes() {
        let (_d, s) = svc().await;
        s.remember(RememberRequest {
            content: "before".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        s.shutdown();
        s.shutdown();
        let err = s
            .remember(RememberRequest {
                content: "after".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("writer stopped"), "{err}");
        assert_eq!(s.health().await.unwrap().memories, 1, "reads still work");
    }
}
