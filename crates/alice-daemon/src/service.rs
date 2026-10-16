//! The service layer: every operation, used identically by the HTTP API, MCP and CLI.
//!
//! Writes go to the single writer thread; reads run on pooled read-only connections in
//! `spawn_blocking`, so neither ever waits for the other (spec §4.1).

use crate::backup::{self, BackupInfo, BackupKind};
use crate::capture::{CaptureCtl, PassInput, PassReport};
use crate::config::SettingsHandle;
use crate::embedding::{self, Builder, EmbeddingCtl};
use crate::importer::{self, ImportEnv, ImportSlot};
use crate::judge_http::{DaemonJevPolicy, JudgeHub};
use crate::judge_worker::{JudgeWorkerConfig, spawn_judge_worker};
use crate::maintenance::{Activity, Job, JobReport, MaintenanceCtx, Scheduler};
use crate::paths::AliceHome;
use crate::precise_gate::Refused;
use crate::redaction::Redactors;
use crate::secrets::{self, SecretName, SecretStore};
use crate::spool;
use crate::worker::{SAVE_EVERY, WorkerConfig, spawn_worker};
use crate::writer::{
    CaptureInit, ProjectChoice, ResolveText, WriteOp, WriteOutcome, WriterHandle, spawn_writer_with,
};
use alice_core::api::*;
use alice_core::embed::Embedder;
use alice_core::error::{AliceError, Result};
use alice_core::ids::new_id;
use alice_core::judge::jev::{JevPolicy, JevTransport};
use alice_core::judge::{Judge, JudgeError, JudgeMemory, RELEVANCE_ANSWERS, RELEVANCE_PARTIAL};
use alice_core::model::{Author, Kind, limits};
use alice_core::normalize::normalize_topic;
use alice_core::project::{ProjectRef, detect_project};
use alice_core::redact::Redactor;
use alice_core::search::{self, SearchOutput, SearchParams, pack};
use alice_core::settings::{self, DefaultRecallMode, ProjectTree, SettingError, Settings};
use alice_core::store::embeddings;
use alice_core::store::events::{Actor, EventRow};
use alice_core::store::history::UndoOutcome;
use alice_core::store::kg::{self, FactOutcome};
use alice_core::store::memories::{
    self, InsertOutcome, MemoryUpdate, NewMemoryInput, ScreenedMove, UpdateAction,
};
use alice_core::store::projects::{self, DetectedForRead, ResolvedProject};
use alice_core::store::reviews::{self, NewReview, ReviewItem, ReviewQuery, ReviewType};
use alice_core::store::sources::{self, IngestOutcome};
use alice_core::store::{ReaderPool, VECTOR_READER_CACHE_KIB, open_reader, open_writer};
use alice_core::vector::VectorIndex;
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `None` while the index is being rebuilt; searches then use lexical candidates only.
pub type IndexSlot = RwLock<Option<Arc<VectorIndex>>>;

/// The embedding model in use: queries, the embedding worker and the judge read it per use;
/// a model switch replaces all three parts at once.
pub struct Active {
    pub embedder: Arc<dyn Embedder>,
    pub model: String,
    pub vector_path: PathBuf,
}

pub type ActiveSlot = RwLock<Arc<Active>>;

/// The model in use and the index that holds its vectors, read together. A model switch holds
/// both slots' write locks while it replaces them (taken in this order), so the pair is always
/// one model's embedder with that model's index.
pub fn snapshot(active: &ActiveSlot, index: &IndexSlot) -> (Arc<Active>, Option<Arc<VectorIndex>>) {
    let a = active.read();
    let i = index.read().clone();
    (a.clone(), i)
}

/// How long `shutdown` waits for the judge worker to leave a request to the judge.
const JUDGE_STOP_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(3)
};

/// How long `shutdown` waits for a maintenance job to reach the end of its slice.
const MAINTENANCE_STOP_WAIT: Duration = Duration::from_secs(3);
/// How long shutdown waits for an import that is inside one long step (a snapshot copy).
const IMPORT_STOP_WAIT: Duration = Duration::from_secs(30);

/// How long a working directory's detected project is reused before git is asked again.
const DETECT_TTL: Duration = Duration::from_secs(60);
/// How long `assert_fact` waits for the judge to match a new predicate before the heuristic
/// decides.
const PREDICATE_WAIT: Duration = Duration::from_secs(3);
/// Most similar existing predicates offered to the judge.
const PREDICATE_CANDIDATES: usize = 10;
/// Predicate names read to find the similar ones (most used first).
const PREDICATE_VOCABULARY: usize = 500;

/// Results the judge grades in precise mode (spec §4.4 step 4).
const PRECISE_TOP: usize = 20;
/// Why a precise recall answered as fast mode does (a note beside `reranked: false`).
const NOTE_NO_MODEL_JUDGE: &str =
    "precise mode needs a model judge (judge.provider = jev); this answer is the fast one";
const NOTE_DENIED: &str =
    "the cloud policy does not let the judge see this project; this answer is the fast one";
const NOTE_BUDGET: &str = "the judge's daily budget is spent; this answer is the fast one";
const NOTE_UNAVAILABLE: &str = "the judge is unavailable; this answer is the fast one";
const NOTE_UNEXPECTED: &str =
    "the judge's answer did not match the results sent; this answer is the fast one";
/// The judge graded the results and none clearly answers the query (precise mode).
const NOTE_NOTHING_ANSWERS: &str = "nothing found clearly answers this; the closest matches follow";
const NOTE_NO_GRADES: &str = "the judge gave no grade; this answer is the fast one";
const NOTE_BUSY: &str = "the judge is busy with other recalls; this answer is the fast one";
const NOTE_SLOW: &str =
    "the judge did not answer within search.precise_timeout_ms; this answer is the fast one";
/// How far below its vector floor a candidate may fall and still be shown to the judge in
/// precise mode (cosine; spec §4.4 step 4). Chosen before the evaluation that motivated it
/// (the paraphrase answers missed the floor by 0.004 to 0.011), not tuned afterwards.
const NEAR_FLOOR_BAND: f32 = 0.03;
/// A near-floor candidate is kept when the judge says it answers the query with at least this
/// probability (`P(answers) >= 0.5`, the relevance rule of the M2 floor evaluation,
/// `jev_eval.rs`).
const RESCUE_ANSWERS_MIN: f64 = 0.5;

/// Whether `hay` holds `needle` with no letter, digit or `_` on either side.
fn contains_bounded(hay: &str, needle: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    hay.match_indices(needle).any(|(i, m)| {
        !hay[..i].chars().next_back().is_some_and(word)
            && !hay[i + m.len()..].chars().next().is_some_and(word)
    })
}

/// Whether `hit` holds, verbatim (ignoring case) in its title, snippet or body, a distinctive
/// token of `query` (6 or more characters with a digit or a `_`: a code, id or identifier;
/// not an ordinary hyphenated word or filename) or the whole `query` as a phrase (3 or more
/// words; a query without spaces, as in Thai, 12 or more characters). Words and tokens match on
/// boundaries. Such a hit is no "nothing found".
fn exact_match(query: &str, hit: &alice_core::api::Hit) -> bool {
    let hay = format!(
        "{}\n{}\n{}",
        hit.title,
        hit.snippet,
        hit.body.as_deref().unwrap_or("")
    )
    .to_lowercase();
    let whole = query.trim().to_lowercase();
    let words = whole.split_whitespace().count();
    let phrase = if words >= 3 {
        contains_bounded(&hay, &whole)
    } else {
        words == 1 && whole.chars().count() >= 12 && hay.contains(&whole)
    };
    phrase
        || whole
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|t| t.chars().count() >= 6 && t.chars().any(|c| c.is_ascii_digit() || c == '_'))
            .any(|t| contains_bounded(&hay, t))
}

/// Said in a recall answer when its query was not sent to a remote embedding provider.
const QUERY_HELD_BACK: &str = "this search used word matches only: the cloud policy keeps this project's text away from the embedding provider, so the query was not sent";

/// The cloud decision for a query asked from `choice`'s project: whether it may leave, the
/// redactors that screen it, and whether the project is a detected one the store never saw.
/// A project that was never written to is judged by its working tree's own `.alice.toml` and
/// the global lists (every identity of it is asked; an empty slug says no). A stored project
/// asked from one of its trees is judged by that tree too, not only by the tree it was last
/// written from (final re-review N-I1). The tree carries the rules project merges carried into
/// the project a write from it lands in; a checkout read as an import placeholder is judged by
/// its tree and the placeholder's policy.
struct QueryCloud {
    allowed: bool,
    redactor: Arc<Redactor>,
    /// The rules as read in the tree the query comes from, when that is not the stored one.
    also: Option<Arc<Redactor>>,
    unstored: bool,
}

impl QueryCloud {
    /// `text` through every redactor of the decision (redaction is idempotent).
    fn redact(&self, text: &str) -> String {
        let once = self.redactor.apply(text).text;
        match &self.also {
            Some(r) => r.apply(&once).text,
            None => once,
        }
    }
}

fn query_cloud(
    cloud: &DaemonJevPolicy,
    readers: &ReaderPool,
    settings: &SettingsHandle,
    redactors: &Redactors,
    choice: &ProjectChoice,
) -> QueryCloud {
    let known = |allowed, redactor| QueryCloud {
        allowed,
        redactor,
        also: None,
        unstored: false,
    };
    match choice {
        ProjectChoice::Slug(s) => known(cloud.cloud_allowed(s), cloud.redactor(s)),
        ProjectChoice::Fallback => known(cloud.cloud_allowed("global"), cloud.redactor("global")),
        ProjectChoice::Detected(r) => {
            // The tree the query comes from, with the rules merges carried into the project a
            // write from it would land in (and that project's own tree when it is another one).
            // A store that cannot say which rules those are judges the tree closed: denied.
            let (found, tree) = readers
                .with(|c| {
                    Ok((
                        projects::detected_for_read(c, r)?,
                        projects::tree_of_detected(c, r)?,
                    ))
                })
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "a project's rules could not be read; the query is kept from cloud providers");
                    (
                        DetectedForRead::Unstored,
                        Some(ProjectTree::unreadable(r.key.clone(), r.tree())),
                    )
                });
            // What the working tree itself says (its `.alice.toml`, kept per project key, the
            // carried rules, and the global lists), for a project the store does not hold as the
            // detected one.
            let tree_allows = || {
                !r.slug.trim().is_empty()
                    && [r.slug.as_str(), r.name.as_str()]
                        .into_iter()
                        .filter(|n| !n.trim().is_empty())
                        .all(|n| settings.cloud_allowed(n, tree.as_ref()))
            };
            match found {
                DetectedForRead::Stored(p) => {
                    // The tree the query comes from first (with the rules merges carried into the
                    // project): its `.alice.toml` may deny what the stored tree (a worktree
                    // without the file, say) does not. The look lands in the project's one entry
                    // of kept rules, so the hits' checks and later looks from other trees keep
                    // what it read (final re-review N-I1).
                    let here = tree
                        .as_ref()
                        .is_none_or(|t| settings.cloud_allowed(&p.slug, Some(t)));
                    QueryCloud {
                        allowed: here && cloud.cloud_allowed(&p.slug),
                        redactor: cloud.redactor(&p.slug),
                        also: tree.as_ref().map(|t| redactors.for_root_always_on(Some(t))),
                        unstored: false,
                    }
                }
                // An import placeholder stands in for the project: its own policy applies, and
                // so does the tree's (a placeholder has no root of its own to carry the tree's
                // file). A tree that denies is judged as an unstored one is: denied.
                DetectedForRead::Placeholder(p) => {
                    let here = tree_allows();
                    QueryCloud {
                        allowed: here && cloud.cloud_allowed(&p.slug),
                        redactor: redactors.for_root_always_on(tree.as_ref()),
                        also: None,
                        unstored: !here,
                    }
                }
                DetectedForRead::Ambiguous(_) | DetectedForRead::Unstored => QueryCloud {
                    allowed: tree_allows(),
                    redactor: redactors.for_root_always_on(tree.as_ref()),
                    also: None,
                    unstored: true,
                },
            }
        }
    }
}

/// What to send a remote embedder for a query asked from `choice`'s project: the query with that
/// project's secrets redacted, or `None` when the cloud policy keeps the project at home.
fn query_for_remote(
    cloud: &DaemonJevPolicy,
    readers: &ReaderPool,
    settings: &SettingsHandle,
    redactors: &Redactors,
    choice: &ProjectChoice,
    text: &str,
) -> Option<String> {
    let q = query_cloud(cloud, readers, settings, redactors, choice);
    q.allowed.then(|| q.redact(text))
}

/// What the judge decided for a predicate that is new to the vocabulary.
#[derive(Default)]
struct PredicateVerdict {
    /// `(new, canonical)`: sure enough to map.
    alias: Option<(String, String)>,
    /// Similar but not sure: stored as given, and asked in the review inbox.
    review: Option<NewReview>,
    suggestion: Option<String>,
}

/// Reads one environment variable for the secrets' status (tests replace the process's).
pub type SecretEnv = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What a request says about its project (spec §4.1): an explicit slug, the client's working
/// directory (local daemons only) and the project a remote client detected.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scope<'a> {
    pub project: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub client_project: Option<&'a ClientProject>,
    /// A project name from a direct MCP caller; honoured in tenant mode only.
    pub project_hint: Option<&'a str>,
}

/// A request that names its project.
pub trait Scoped {
    fn scope(&self) -> Scope<'_>;
}

macro_rules! scoped {
    ($($t:ty),*) => {$(
        impl Scoped for $t {
            fn scope(&self) -> Scope<'_> {
                Scope {
                    project: self.project.as_deref(),
                    cwd: self.cwd.as_deref(),
                    client_project: self.client_project.as_ref(),
                    project_hint: self.project_hint.as_deref(),
                }
            }
        }
    )*};
}
scoped!(
    RememberRequest,
    RecallRequest,
    ContextRequest,
    IngestRequest
);

/// A tenant import must leave at least this many bytes free once its copy exists.
pub const IMPORT_MIN_FREE_AFTER: u64 = 2_000_000_000;

fn too_large_message(n: u64, cap: u64) -> String {
    format!(
        "the upload is {n} bytes, over the limit of {cap} bytes (import.max_upload_bytes); upload a smaller palace or ask the server's admin to raise the limit"
    )
}

/// Why an upload was not admitted or stopped.
#[derive(Debug)]
pub enum UploadError {
    /// Another import holds the slot (409).
    Conflict(String),
    /// Over `import.max_upload_bytes` (413).
    TooLarge(String),
    Other(AliceError),
}

impl From<AliceError> for UploadError {
    fn from(e: AliceError) -> Self {
        Self::Other(e)
    }
}

/// An admitted upload: it holds the one import slot until [`finish`](Self::finish) hands it to
/// the job thread. Dropped before that (a refusal, a broken connection), it deletes the
/// partial archive and leaves the status `failed` with the reason.
pub struct UploadTicket {
    svc: Arc<Service>,
    archive: PathBuf,
    dir: PathBuf,
    job_id: String,
    cap: u64,
    declared: Option<u64>,
    received: u64,
    guard: Option<crate::maintenance::OwnedActivityGuard>,
    allow_remote_embedding: bool,
    reason: Option<String>,
    armed: bool,
}

impl UploadTicket {
    /// Where the body is to be stored; the file must not exist yet.
    pub fn archive_path(&self) -> &Path {
        &self.archive
    }

    /// Counts `n` more bytes that arrived (and are being stored). Over the cap, or without
    /// room on the disk any more, the upload ends here: the error says why.
    pub fn account(&mut self, n: u64) -> std::result::Result<(), UploadError> {
        self.received = self.received.saturating_add(n);
        self.svc.import.lock().status.lock().bytes_received = self.received;
        if self.received > self.cap {
            let msg = too_large_message(self.received, self.cap);
            self.reason = Some(msg.clone());
            return Err(UploadError::TooLarge(msg));
        }
        let size = self.declared.unwrap_or(0).max(self.received);
        if let Err(e) = self.svc.check_upload_room(size, self.received) {
            self.reason = Some(e.to_string());
            return Err(e.into());
        }
        Ok(())
    }

    /// Records why the upload ends, when it is not one of [`account`](Self::account)'s errors.
    pub fn fail(&mut self, why: impl Into<String>) {
        self.reason = Some(why.into());
    }

    /// The whole body is stored: answers with the status (`unpacking`) and runs the rest on a
    /// thread of its own.
    pub fn finish(mut self) -> std::result::Result<alice_core::import::ImportStatus, UploadError> {
        use alice_core::import::ImportState;
        if self.received == 0 {
            self.fail("the upload was empty; send the palace as a gzip tar in the request body");
            return Err(AliceError::invalid(
                "the upload is empty; send the palace as a gzip tar in the request body",
            )
            .into());
        }
        let svc = self.svc.clone();
        let mut slot = svc.import.lock();
        {
            let mut st = slot.status.lock();
            st.state = ImportState::Unpacking;
            st.phase = "unpacking".to_string();
            st.bytes_received = self.received;
        }
        let guard = self.guard.take().expect("the guard is held until finish");
        let env = svc.import_env(&slot, guard);
        let req = alice_core::import::ImportRequest {
            allow_remote_embedding: self.allow_remote_embedding,
            ..Default::default()
        };
        let job = importer::UploadJob {
            archive: self.archive.clone(),
            archive_len: self.received,
            dir: self.dir.clone(),
            cap: self.cap,
        };
        match importer::spawn_upload(env, req, job, self.job_id.clone()) {
            Ok(handle) => {
                slot.handle = Some(handle);
                self.armed = false;
                let st = slot.status.lock().clone();
                Ok(st)
            }
            Err(e) => {
                self.fail("the server could not start the import");
                Err(AliceError::Other(format!("could not start the import thread: {e}")).into())
            }
        }
    }
}

impl Drop for UploadTicket {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = std::fs::remove_file(&self.archive);
        let _ = std::fs::remove_dir_all(&self.dir);
        let slot = self.svc.import.lock();
        let mut st = slot.status.lock();
        if st.state == alice_core::import::ImportState::Receiving
            || st.job_id.as_deref() == Some(self.job_id.as_str())
                && st.state == alice_core::import::ImportState::Unpacking
        {
            st.state = alice_core::import::ImportState::Failed;
            st.error = Some(self.reason.take().unwrap_or_else(|| {
                "the upload ended before the whole archive arrived; upload it again".to_string()
            }));
        }
    }
}

/// The total size of the regular files under `dir`; links are counted for nothing and not followed.
fn tree_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.is_dir() {
                todo.push(e.path());
            } else if m.is_file() {
                total = total.saturating_add(m.len());
            }
        }
    }
    total
}

#[derive(Clone)]
pub struct ServiceOptions {
    /// Most read connections open at once (each has its own page cache); further reads wait.
    pub reader_pool: usize,
    /// Connections (4 MiB page cache each) that read the stored chunk vectors a raw-tier
    /// recall rescores, while its main reader runs the word lists. A recall takes one main
    /// reader and then at most one of these, never two of either, so the pools cannot
    /// deadlock; with all of these in use a recall waits for one of them only.
    pub vector_reader_pool: usize,
    /// Maps a working directory to its project (runs git). Tests replace it.
    pub detect_project: fn(&Path) -> ProjectRef,
    /// Settings to use; `None` loads `config.toml` and `ALICE_*` from the home.
    pub settings: Option<Arc<SettingsHandle>>,
    /// The `ALICE_*` variables the settings see when `settings` is `None`; `None`: the
    /// process's. The default is an empty list, so a developer's shell cannot change a test;
    /// [`ServiceOptions::process_env`] is the one that reads the process's.
    pub settings_env: Option<Vec<(String, String)>>,
    /// How often `config.toml` is checked for edits.
    pub settings_poll: Duration,
    /// How often the event stream checks whether the health changed (tests shorten it).
    pub health_tick: Duration,
    /// The secrets store; `None` opens the one `secrets.backend` names. Tests pass a fake, so
    /// none of them touches the OS keychain.
    pub secrets: Option<Arc<dyn SecretStore>>,
    /// The environment the secrets' status is read from; `None`: the process's. The default
    /// sees no variable at all (a shell's `TYPESAFE_API_KEY` never reaches a test).
    pub secret_env: Option<SecretEnv>,
    /// How long a request waits for the secrets store (an OS keychain may wait for a prompt
    /// nobody answers) before it says so and carries on.
    pub secrets_timeout: Duration,
    /// The transport the Jev judge posts through; `None` makes the real HTTP client on first
    /// use. Tests pass a scripted one, so none of them reaches the network.
    pub jev_transport: Option<Arc<dyn JevTransport>>,
    /// How often memories whose judgement failed (the judge was down) are judged again.
    pub judge_retry_every: Duration,
    /// The first wait of a memory whose judgement failed; it doubles with every failure.
    pub judge_retry_base: Duration,
    /// False: the judge reads and decides but changes nothing (no merge, supersede, link, kind,
    /// move or review item) and only activates memories. For `alice bench`, whose generated
    /// corpus must stay as it was written.
    pub judge_applies: bool,
    /// MemPalace's own folder (`~/.mempalace`): an import never writes into it, and an import
    /// that names no palace reads `<this>/palace`. `None`: no palace is ever looked up on the
    /// machine (the default, so tests and embedded services touch nothing outside their home);
    /// [`ServiceOptions::process_env`] sets it from `$HOME`.
    pub mempalace_home: Option<PathBuf>,
    /// Tests only: pause or panic an import job.
    pub import_hook: Option<Arc<importer::ImportHook>>,
    /// Builds embedders from `[embedding]` settings. The daemon passes one; with it the model
    /// follows the settings (a change starts a re-embed). `None` (tests, `alice bench`, the
    /// fake embedder): the embedder `start` is given is the only one and nothing switches.
    pub embedder_factory: Option<alice_embed::EmbedderFactory>,
    /// The wait before a failed batch of a re-embed is tried again (it doubles each time).
    pub reembed_retry_wait: Duration,
    /// Tests: the clock of the breaker around the judge call of a precise recall.
    pub precise_clock: Option<crate::precise_gate::Clock>,
    /// Server mode (`alice daemon run --tenant`, spec §2.2): a request's `cwd` is never used
    /// (no git on a client's path), the settings follow the tenant rules, and a project root
    /// that is not there fails closed.
    pub tenant: bool,
    /// Free bytes at a path (tests inject a value); `None` reads the volume
    /// (`backup::free_space`). A tenant import checks it before it starts.
    pub free_space: Option<crate::maintenance::FreeSpace>,
    /// Server mode, the team store (`alice daemon run --tenant --team-store`, spec §2.10): this
    /// daemon holds the shared team memory, so it keeps a project hint from an HTTP body (its
    /// callers are the personal daemons, which name the project).
    pub team_store: bool,
    /// The team store's URL (`ALICE_TEAM_URL`, loopback http only); the daemon fills it from
    /// its environment, tests set it directly.
    pub team_url: Option<String>,
    /// This tenant's name (`ALICE_TENANT_NAME`), the author the team store records.
    pub tenant_name: Option<String>,
}

impl Default for ServiceOptions {
    /// Isolated from the process's environment: no `ALICE_*` variable reaches the settings and no
    /// secret variable (`TYPESAFE_API_KEY`) reaches the secrets' status. Only the daemon's own
    /// entry point (and tools that run a service for real, such as `alice bench`) asks for the
    /// process's environment, with [`ServiceOptions::process_env`].
    fn default() -> Self {
        Self {
            reader_pool: 8,
            vector_reader_pool: 4,
            detect_project,
            settings: None,
            settings_env: Some(Vec::new()),
            settings_poll: crate::config::SETTINGS_POLL,
            health_tick: Duration::from_secs(5),
            secrets: None,
            secret_env: Some(Arc::new(|_: &str| None)),
            secrets_timeout: Duration::from_secs(5),
            jev_transport: None,
            judge_retry_every: JudgeWorkerConfig::RETRY_EVERY,
            judge_retry_base: alice_core::store::judge::RETRY_BASE,
            judge_applies: true,
            mempalace_home: None,
            import_hook: None,
            embedder_factory: None,
            reembed_retry_wait: Duration::from_secs(2),
            precise_clock: None,
            tenant: false,
            free_space: None,
            team_store: false,
            team_url: None,
            tenant_name: None,
        }
    }
}

impl ServiceOptions {
    /// The defaults, reading the process's `ALICE_*` variables and secret variables. For code
    /// that runs a service for real; tests use [`Default`].
    pub fn process_env() -> Self {
        Self {
            settings_env: None,
            secret_env: None,
            mempalace_home: dirs::home_dir().map(|h| h.join(".mempalace")),
            ..Self::default()
        }
    }
}

impl std::fmt::Debug for ServiceOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceOptions")
            .field("reader_pool", &self.reader_pool)
            .field("vector_reader_pool", &self.vector_reader_pool)
            .field("settings_poll", &self.settings_poll)
            .field("health_tick", &self.health_tick)
            .field("secrets_timeout", &self.secrets_timeout)
            .finish_non_exhaustive()
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

/// The stand-in for a detected project the store does not hold: no id, the slug and name it
/// would get.
fn unstored_project(r: ProjectRef) -> ResolvedProject {
    ResolvedProject {
        id: String::new(),
        slug: r.slug,
        name: r.name,
    }
}

/// Said once per slug and run: several import placeholders match a checkout, so none is used.
fn warn_ambiguous_placeholders(slug: &str, candidates: &[String]) {
    static SAID: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let first = SAID
        .get_or_init(Mutex::default)
        .lock()
        .insert(slug.to_string());
    if first {
        tracing::warn!(
            slug,
            placeholders = ?candidates,
            "several imported projects match this checkout; reading as an unstored project (merge them with `alice projects merge`)"
        );
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
            (None, None) => ProjectChoice::Fallback,
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
        // Nothing named a project: reads see `global` as the current one, like a write does.
        let (choice, explicit) = match choice {
            ProjectChoice::Fallback => (ProjectChoice::Slug("global".into()), false),
            other => (other, explicit),
        };
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
                let project = match projects::detected_for_read(conn, &r)? {
                    DetectedForRead::Stored(p) | DetectedForRead::Placeholder(p) => p,
                    DetectedForRead::Ambiguous(slugs) => {
                        warn_ambiguous_placeholders(&r.slug, &slugs);
                        unstored_project(r)
                    }
                    DetectedForRead::Unstored => unstored_project(r),
                };
                Ok(ReadScope {
                    filter_id: None,
                    project,
                })
            }
            ProjectChoice::Fallback => unreachable!("mapped to the global slug above"),
        }
    }
}

/// The threads `shutdown` joins: the maintenance scheduler, the embedding worker, the judge
/// worker, the writer and the config watcher.
struct Threads {
    maintenance: JoinHandle<()>,
    embed: JoinHandle<()>,
    /// Compares `[embedding]` with the model in force and runs the re-embed.
    embedding: JoinHandle<()>,
    judge: JoinHandle<()>,
    writer: JoinHandle<()>,
    watcher: JoinHandle<()>,
}

pub struct Service {
    home: AliceHome,
    readers: Arc<ReaderPool>,
    /// Reads the stored chunk vectors for raw-tier rescoring (see `ServiceOptions`).
    vector_readers: Arc<ReaderPool>,
    writer: WriterHandle,
    index: Arc<IndexSlot>,
    /// The embedding model in force, shared by the embedding worker, queries and the judge.
    active: Arc<ActiveSlot>,
    /// Changes of the embedding model (re-embed) and the cloud gate of its texts.
    embedding: Arc<EmbeddingCtl>,
    /// The cloud policy: asked per project before text goes to a remote embedder.
    cloud: Arc<DaemonJevPolicy>,
    opts: ServiceOptions,
    /// The link to the team store; `None` when none is configured or it was refused.
    team: Option<Arc<crate::team::TeamLink>>,
    /// Why the link is disabled, when `team_url` was given and refused (health reports it).
    team_error: Option<String>,
    instance: String,
    stop: Arc<AtomicBool>,
    wake_worker: crossbeam_channel::Sender<()>,
    wake_judge: crossbeam_channel::Sender<()>,
    /// Taken by `shutdown`.
    threads: Mutex<Option<Threads>>,
    resolver: ProjectResolver,
    /// The health every event-stream client sends: read at most once per `health_tick`.
    stream_health: tokio::sync::Mutex<Option<(Instant, Health)>>,
    /// Live settings: read per request, never copied into the service.
    settings: Arc<SettingsHandle>,
    /// Compiled secret redactors, global and per project (rebuilt when the rules change).
    redactors: Arc<Redactors>,
    /// The settings the daemon started with, for the keys that apply only after a restart.
    started_with: Arc<Settings>,
    secrets: Arc<dyn SecretStore>,
    secret_env: SecretEnv,
    /// The judge in force (rebuilt when its settings or its key change) and why it is not the
    /// one the settings ask for.
    judge: Arc<JudgeHub>,
    /// The read of the secrets' status in flight (it may be stuck on a keychain prompt):
    /// requests that overlap it wait for it instead of starting another.
    secrets_read: Arc<Mutex<Option<SecretsRead>>>,
    /// Counts the changes that can alter what a settings view shows (a secret set or deleted, a
    /// settings write), bumped after each lands. A view never joins a secrets read that began
    /// before the latest change: it would show the state from before.
    settings_gen: std::sync::atomic::AtomicU64,
    /// Numbers the secrets reads, so a finishing read clears only its own record.
    secrets_read_seq: std::sync::atomic::AtomicU64,
    /// One write to the secrets store at a time; a second waits (up to the timeout) rather than
    /// parking another thread behind a stuck store.
    secrets_write: Arc<tokio::sync::Mutex<()>>,
    /// When requests last came (maintenance runs only while the daemon is idle).
    activity: Arc<Activity>,
    /// The guard around the judge call of a precise recall: calls in flight and a breaker.
    precise_gate: crate::precise_gate::PreciseGate,
    /// The maintenance jobs, shared with the scheduler thread so `run_job` can run one on demand.
    scheduler: Arc<Scheduler>,
    /// The one import job (a running one, or the last one's outcome).
    import: Mutex<ImportSlot>,
    /// The capture pipeline (SP2): transcript bytes into chunks, turn keys and cursors.
    capture: Arc<CaptureCtl>,
    /// The compatibility layer's short-lived overview cache.
    pub(crate) compat: crate::compat::CompatState,
    /// Tests: time added to the age of the shared health reading.
    #[cfg(test)]
    health_aged: Mutex<Duration>,
    /// Readings of the health (tests count them).
    #[cfg(test)]
    health_reads: std::sync::atomic::AtomicUsize,
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

/// `text` for `field` through `r`: redacted, or refused in `block` mode. Logs the field and
/// that something was redacted, never the text.
/// An update refuses what its action would otherwise drop without a word: a `move` takes only
/// `project`, and `project` belongs to `move` alone.
fn check_update_fields(u: &MemoryUpdate) -> Result<()> {
    if u.action == UpdateAction::Move {
        let extra: Vec<&str> = [
            ("title", u.title.is_some()),
            ("body", u.body.is_some()),
            ("kind", u.kind.is_some()),
            ("topics", u.topics.is_some()),
            ("importance", u.importance.is_some()),
            ("superseded_by", u.superseded_by.is_some()),
        ]
        .into_iter()
        .filter_map(|(field, given)| given.then_some(field))
        .collect();
        if !extra.is_empty() {
            return Err(AliceError::invalid(format!(
                "move takes only project ({} given too); move the memory first, then edit it",
                extra.join(", ")
            )));
        }
    } else if u.project.is_some() {
        return Err(AliceError::invalid(
            "project is only for action \"move\"; to move the memory, update it with action \"move\" and project alone",
        ));
    }
    Ok(())
}

fn redact_field(r: &Redactor, field: &str, text: &str) -> Result<String> {
    let out = r.check(field, text)?;
    if out != text {
        tracing::info!(field, "redacted a secret before storing it");
    }
    Ok(out)
}

impl Service {
    /// `text` as it may be shown back to a caller: redacted under the global rules, so an answer
    /// never echoes a secret the store did not keep. A text the rules refuse is hidden whole.
    pub(crate) fn redact_echo(&self, text: &str) -> String {
        match self.redactors.for_root(None).check("echo", text) {
            Ok(t) => t,
            Err(_) => "[REDACTED]".into(),
        }
    }
}

/// The working tree of the project `choice` names, keyed by the project, with the rules project
/// merges carried into it (for a detected checkout: into the stored project its writes land in,
/// whose own tree applies too when that is another project; see
/// [`projects::tree_of_detected`]).
fn project_root_in(readers: &ReaderPool, choice: &ProjectChoice) -> Option<ProjectTree> {
    project_tree_in(readers, choice).0
}

/// [`project_root_in`], and whether the store could not say which rules the project carries.
/// Such a project gets the closed rules (secrets blocked, no cloud; a checkout keeps its own
/// file on top), never the global ones alone: a rule that cannot be read must not loosen
/// anything (final re-review 2, I-1).
fn project_tree_in(readers: &ReaderPool, choice: &ProjectChoice) -> (Option<ProjectTree>, bool) {
    let looked = match choice {
        ProjectChoice::Detected(r) => readers
            .with(|c| projects::tree_of_detected(c, r))
            .map_err(|e| (e, r.key.clone(), r.tree())),
        // `global` has no working tree: the global privacy settings apply.
        ProjectChoice::Fallback => Ok(None),
        ProjectChoice::Slug(s) => readers
            .with(|c| projects::tree_of(c, s))
            .map_err(|e| (e, s.clone(), None)),
    };
    match looked {
        Ok(tree) => (tree, false),
        Err((e, key, own)) => {
            tracing::warn!(error = %e, "a project's rules could not be read; its writes are screened closed");
            (Some(ProjectTree::unreadable(key, own)), true)
        }
    }
}

/// Why a project merge cannot carry the rules of the project `slug` (its tree `tree`, its rules
/// as loaded `load`) right now, said plainly with what to do; `None` when they are its rules.
/// Never names a pattern or quotes the file (people write known secrets as patterns).
fn rules_not_known(
    slug: &str,
    tree: Option<&ProjectTree>,
    load: &alice_core::settings::ProjectLoad,
) -> Option<String> {
    let root = tree.filter(|t| t.has_root()).map(|t| t.root.clone());
    if load.error.is_some() {
        let because = match &root {
            Some(r) if !r.is_dir() => format!(
                "its working tree ({}) is gone or cannot be read, and its .alice.toml was never read on this machine. Bring the working tree back at that path (a clone, or an empty folder if the project had no .alice.toml), or write to the project once from another checkout of it, then merge",
                r.display()
            ),
            Some(r) => format!(
                "its .alice.toml ({}) is invalid or cannot be read. Fix the file, then merge",
                r.join(".alice.toml").display()
            ),
            None => "they cannot be read. Try the merge again; if it is refused again, run `alice doctor`".into(),
        };
        return Some(format!(
            "the privacy rules of project \"{slug}\" are not known, so the merge cannot carry them: {because}"
        ));
    }
    load.loosening_pending.then(|| format!(
        "the privacy rules of project \"{slug}\" are not in force yet, so the merge cannot carry them: its .alice.toml is blank or now protects less than before, and such a change applies only once it has been read the same three times, 2 seconds apart (each merge attempt reads it). Try the merge again in a few seconds"
    ))
}

fn unexpected(o: WriteOutcome) -> AliceError {
    AliceError::Other(format!("unexpected writer outcome {}", o.name()))
}

/// Rebuild the vector index from the stored vectors on a background thread; the slot stays
/// `None` (lexical-only search, worker paused) until it is done. With `loaded` (an index
/// opened from its files after an unclean stop) the index is only caught up by content (what the
/// crash lost is added, what a purge removed is dropped), unless that fails or the counts
/// still disagree afterwards: then the whole index is rebuilt.
pub(crate) fn spawn_rebuild(
    slot: Arc<IndexSlot>,
    active: Arc<ActiveSlot>,
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
                    match idx.catch_up(&c) {
                        Ok(done) => {
                            let stored = embeddings::count(&c, &model)?;
                            if idx.len() == stored {
                                tracing::info!(
                                    added = done.added,
                                    removed = done.removed,
                                    "vector index caught up with the stored vectors"
                                );
                                return Ok(idx);
                            }
                            tracing::info!(
                                indexed = idx.len(),
                                stored,
                                "vector index still differs after catching up; rebuilding the whole index"
                            );
                        }
                        Err(e) => {
                            tracing::info!(error = %e, "catching the vector index up failed; rebuilding the whole index");
                        }
                    }
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
                    install_rebuilt(&slot, &active, &model, Some(Arc::new(idx)));
                }
                Err(e) => {
                    tracing::error!(error = %e, "vector index rebuild failed; starting with an empty index");
                    install_rebuilt(
                        &slot,
                        &active,
                        &model,
                        VectorIndex::for_model(&model, dim).ok().map(Arc::new),
                    );
                }
            }
        })?;
    Ok(())
}

/// Puts a rebuilt index in the slot, unless the model changed while it was being built (a
/// model switch put the new model's index there; this one is the old model's).
fn install_rebuilt(
    slot: &IndexSlot,
    active: &ActiveSlot,
    model: &str,
    idx: Option<Arc<VectorIndex>>,
) {
    let a = active.read();
    if a.model == model {
        *slot.write() = idx;
    } else {
        tracing::info!(
            model,
            "dropped a rebuilt index of a model that is no longer in use"
        );
    }
}

/// Records every settings change that takes effect: a `settings.change` event naming the keys
/// (never a value) and a `settings` item on the stream. Ends when the handle is dropped.
fn spawn_config_events(
    settings: &Arc<SettingsHandle>,
    writer: &WriterHandle,
    judge: &Arc<JudgeHub>,
    embedding: &Arc<EmbeddingCtl>,
) -> Result<()> {
    let rx = settings.subscribe();
    let start = settings.get();
    // Weak: the loop must not keep the handle (and so the channel) alive after shutdown.
    let weak = Arc::downgrade(settings);
    let weak_judge = Arc::downgrade(judge);
    let weak_embedding = Arc::downgrade(embedding);
    let writer = writer.clone();
    std::thread::Builder::new()
        .name("alice-config-events".into())
        .spawn(move || {
            crate::config::follow_settings(
                rx,
                start,
                move || weak.upgrade().map(|h| h.get()).unwrap_or_default(),
                move |changed, loosened| {
                    // Keys only, never a value. `loosened` names the protective keys that got
                    // weaker, so the history shows a loosening whoever typed it.
                    let mut after = serde_json::json!({ "changed": changed });
                    if !loosened.is_empty() {
                        after["loosened"] = serde_json::json!(loosened);
                    }
                    writer.send_nowait(WriteOp::RecordEvent {
                        actor: Actor::User,
                        action: crate::writer::SETTINGS_CHANGE.into(),
                        target_type: "settings".into(),
                        target_id: "config".into(),
                        before: None,
                        after: Some(after),
                    });
                    // The judge reads its settings when it is built: rebuild it for a change
                    // that can matter (this also runs after a lag, with the keys re-derived).
                    if let Some(judge) = weak_judge.upgrade() {
                        judge.on_change(&changed);
                    }
                    // A new embedding model starts a re-embed; a new privacy rule or provider
                    // may free (or hold back) rows the cloud policy had decided about.
                    if let Some(embedding) = weak_embedding.upgrade()
                        && changed
                            .iter()
                            .any(|k| k.starts_with("embedding.") || k.starts_with("privacy."))
                    {
                        embedding.recheck_skipped.store(true, Ordering::SeqCst);
                        embedding.poke();
                    }
                },
            )
        })?;
    Ok(())
}

type SecretStatuses = BTreeMap<String, SecretStatus>;

/// A read of the secrets' status that is running: which one, when it began, which generation of
/// settings it began under, and where its answer arrives.
struct SecretsRead {
    id: u64,
    started: Instant,
    gen_at_start: u64,
    answer: tokio::sync::watch::Receiver<Option<SecretStatuses>>,
}

/// What a request got from the secrets' status read.
enum SecretsAnswer {
    Statuses(SecretStatuses),
    /// The store did not answer in time (a keychain waiting for a prompt).
    Silent,
    /// The read ended without an answer (it panicked).
    Failed,
}

/// Ends the "read in flight" record when the read ends, however it ends (unless a newer read has
/// already replaced it).
struct ReadDone(Arc<Mutex<Option<SecretsRead>>>, u64);

impl Drop for ReadDone {
    fn drop(&mut self) {
        let mut slot = self.0.lock();
        if slot.as_ref().is_some_and(|r| r.id == self.1) {
            *slot = None;
        }
    }
}

impl Service {
    pub fn start(
        home: AliceHome,
        make_embedder: &dyn Fn() -> Result<Arc<dyn Embedder>>,
        opts: ServiceOptions,
    ) -> Result<Arc<Service>> {
        home.ensure_dirs()?;
        // No import can be running at start: whatever an earlier one left is garbage.
        let _ = std::fs::remove_dir_all(home.import_tmp());
        // ... and so is an upload a crash cut short (archives and unpacked folders).
        if opts.tenant {
            let n = crate::upload::sweep(&home.import_inbox());
            if n > 0 {
                tracing::warn!(
                    removed = n,
                    "removed what an interrupted upload left behind"
                );
            }
        }
        // Settings first: everything below reads them (the model's idle unload and the port are
        // read by the daemon from the same handle).
        let settings = opts
            .settings
            .clone()
            .unwrap_or_else(|| match &opts.settings_env {
                Some(env) => SettingsHandle::load_with(&home, env.clone(), opts.tenant),
                None if opts.tenant => SettingsHandle::load_tenant(&home),
                None => SettingsHandle::load(&home),
            });
        let started_with = settings.get();
        let secrets = opts
            .secrets
            .clone()
            .unwrap_or_else(|| secrets::open_store(&home, started_with.secrets.backend));
        let secret_env: SecretEnv = opts
            .secret_env
            .clone()
            .unwrap_or_else(|| Arc::new(|var: &str| std::env::var(var).ok()));
        let conn = open_writer(&home.db())?;
        let device = device_id(&conn)?;
        // A tenant keeps no path of the person's machine (SP2 spec §9.4): an adopted database's
        // transcript paths and file identities are cleared before anything reads them.
        if opts.tenant {
            let cleared = crate::capture::forget_local_paths(&conn)?;
            if cleared > 0 {
                tracing::info!(
                    sources = cleared,
                    "cleared local transcript paths from an adopted database"
                );
            }
        }
        let readers = Arc::new(ReaderPool::new(home.db(), opts.reader_pool));
        let vector_readers = Arc::new(ReaderPool::with_cache(
            home.db(),
            opts.vector_reader_pool,
            VECTOR_READER_CACHE_KIB,
        ));
        // One instance for the worker and for queries: the local model costs ~600 MB each. It
        // embeds passages in short batches and lets a waiting query go next after each one.
        // With a factory the model in force is the one the database records (the settings may
        // ask for another: the control thread starts a re-embed below); without one it is
        // whatever `make_embedder` makes and nothing switches.
        let builder = opts.embedder_factory.clone().map(|factory| Builder {
            factory,
            secrets: secrets.clone(),
            env: secret_env.clone(),
        });
        let (embedder, embedding_error) = match &builder {
            Some(b) => {
                let plan = embedding::plan(b, &conn, &started_with.embedding)?;
                if let Some(rec) = &plan.record {
                    conn.execute(
                        "INSERT INTO meta (key, value) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        rusqlite::params![
                            crate::reembed::ACTIVE_KEY,
                            serde_json::to_string(rec).expect("settings serialise")
                        ],
                    )?;
                }
                (plan.active, plan.error)
            }
            None => (make_embedder()?, None),
        };
        let model = embedder.model_id().to_string();
        let dim = embedder.dim();
        let index: Arc<IndexSlot> = Arc::new(RwLock::new(None));
        let vector_path = home.vector_file(&model);
        let active: Arc<ActiveSlot> = Arc::new(RwLock::new(Arc::new(Active {
            embedder: embedder.clone(),
            model: model.clone(),
            vector_path: vector_path.clone(),
        })));

        let expected = embeddings::count(&conn, &model)?;
        // A clean stop marks the files it saved last. Without the mark (a crash, say while
        // embedding) the chunk file can miss chunks or hold stale ones even when the counts
        // agree, and rebuilding the chunk part takes seconds.
        let clean = VectorIndex::take_clean_mark(&vector_path);
        match VectorIndex::open(&vector_path, &model, dim) {
            Ok(idx) if !clean => {
                tracing::info!("the last stop was not clean; rebuilding the chunk vectors");
                spawn_rebuild(
                    index.clone(),
                    active.clone(),
                    home.db(),
                    model.clone(),
                    dim,
                    Some(idx),
                )?;
            }
            Ok(idx) if idx.len() == expected => *index.write() = Some(Arc::new(idx)),
            Ok(idx) => {
                tracing::info!(
                    indexed = idx.len(),
                    stored = expected,
                    "vector index is out of date; rebuilding"
                );
                spawn_rebuild(
                    index.clone(),
                    active.clone(),
                    home.db(),
                    model.clone(),
                    dim,
                    None,
                )?;
            }
            Err(e) => {
                tracing::info!(error = %e, "no usable vector index; rebuilding");
                spawn_rebuild(
                    index.clone(),
                    active.clone(),
                    home.db(),
                    model.clone(),
                    dim,
                    None,
                )?;
            }
        }

        // bounded(1): repeated wake-ups coalesce.
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let redactors = Redactors::new(settings.clone());
        let (writer, writer_thread) =
            spawn_writer_with(conn, device, wake_tx.clone(), redactors.clone());
        let stop = Arc::new(AtomicBool::new(false));
        // bounded(1): the embedding worker wakes the judge worker after each batch it stores.
        let (judge_wake_tx, judge_wake_rx) = crossbeam_channel::bounded(1);
        // The one cloud policy: the Jev judge, the embedding worker, a re-embed and the query
        // embedding of a recall all ask `SettingsHandle::cloud_allowed` through it.
        let cloud = Arc::new(DaemonJevPolicy::new(
            settings.clone(),
            readers.clone(),
            redactors.clone(),
        ));
        let policy: Arc<dyn JevPolicy> = cloud.clone();
        let embedding_error: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(embedding_error));
        let (main_paused, parked, recheck_skipped) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let worker_thread = spawn_worker(WorkerConfig {
            readers: readers.clone(),
            writer: writer.clone(),
            index: index.clone(),
            active: active.clone(),
            wake: wake_rx,
            stop: stop.clone(),
            // `embedding.batch_size` (a restart key: the worker is built once).
            batch: started_with.embedding.batch_size,
            judge_wake: Some(judge_wake_tx.clone()),
            save_every: SAVE_EVERY,
            paused: main_paused.clone(),
            parked: parked.clone(),
            policy: Some(policy.clone()),
            recheck_skipped: recheck_skipped.clone(),
            error: embedding_error.clone(),
        });
        let judge = JudgeHub::new(
            settings.clone(),
            secrets.clone(),
            secret_env.clone(),
            readers.clone(),
            writer.clone(),
            redactors.clone(),
            opts.jev_transport.clone(),
            opts.secrets_timeout,
        );
        let judge_active = active.clone();
        let judge_index = index.clone();
        let judge_thread = spawn_judge_worker(JudgeWorkerConfig {
            readers: readers.clone(),
            writer: writer.clone(),
            index: index.clone(),
            judge: judge.clone(),
            settings: settings.clone(),
            redactors: redactors.clone(),
            model: Arc::new(move || {
                let (a, i) = snapshot(&judge_active, &judge_index);
                (a.model.clone(), i)
            }),
            wake: judge_wake_rx,
            stop: stop.clone(),
            retry_every: opts.judge_retry_every,
            retry_base: opts.judge_retry_base,
            applies: opts.judge_applies,
        });
        let activity = Activity::new();
        let scheduler = Arc::new(Scheduler::new(MaintenanceCtx {
            home: home.clone(),
            readers: readers.clone(),
            writer: writer.clone(),
            index: index.clone(),
            active: active.clone(),
            settings: settings.clone(),
            activity: activity.clone(),
            judge_wake: judge_wake_tx.clone(),
            stop: stop.clone(),
            free_space: Arc::new(|p: &Path| backup::free_space(p)),
        }));
        let maintenance_thread = scheduler.clone().spawn(stop.clone());
        let (embedding_ctl, embedding_rx) = EmbeddingCtl::new(
            builder,
            home.clone(),
            active.clone(),
            index.clone(),
            readers.clone(),
            writer.clone(),
            settings.clone(),
            policy,
            activity.clone(),
            main_paused,
            parked,
            recheck_skipped,
            embedding_error,
            stop.clone(),
            opts.reembed_retry_wait,
        );
        let embedding_thread = embedding_ctl.spawn(embedding_rx)?;
        // A model change made while the daemon was stopped (or a re-embed it was in the middle
        // of) is carried out now.
        embedding_ctl.poke();
        if let Err(e) = spawn_config_events(&settings, &writer, &judge, &embedding_ctl) {
            // The workers and the writer are running already: stop them, do not leak them.
            stop.store(true, Ordering::SeqCst);
            let _ = wake_tx.try_send(());
            let _ = judge_wake_tx.try_send(());
            embedding_ctl.stop_run();
            writer.send_nowait(WriteOp::Shutdown);
            return Err(e);
        }
        let watcher = settings.spawn_watcher(opts.settings_poll, stop.clone());
        let resolver = ProjectResolver::new(opts.detect_project);
        let precise_gate = crate::precise_gate::PreciseGate::new(opts.precise_clock.clone());
        let capture = CaptureCtl::new(
            readers.clone(),
            writer.clone(),
            settings.clone(),
            redactors.clone(),
        );
        // The team store never links to itself.
        let (team, team_error) = if opts.team_store {
            (None, None)
        } else {
            match crate::team::TeamLink::from_env(&home, opts.team_url.as_deref()) {
                Ok(link) => (link, None),
                Err(reason) => {
                    tracing::warn!("the team link is off: {reason}");
                    (None, Some(reason))
                }
            }
        };
        Ok(Arc::new(Service {
            home,
            readers,
            vector_readers,
            writer,
            index,
            active,
            embedding: embedding_ctl,
            cloud,
            opts,
            team,
            team_error,
            instance: new_id(),
            stop,
            wake_worker: wake_tx,
            wake_judge: judge_wake_tx,
            threads: Mutex::new(Some(Threads {
                maintenance: maintenance_thread,
                embed: worker_thread,
                embedding: embedding_thread,
                judge: judge_thread,
                writer: writer_thread,
                watcher,
            })),
            resolver,
            stream_health: tokio::sync::Mutex::new(None),
            settings,
            redactors,
            started_with,
            secrets,
            secret_env,
            judge,
            secrets_read: Arc::new(Mutex::new(None)),
            settings_gen: Default::default(),
            secrets_read_seq: Default::default(),
            secrets_write: Arc::new(tokio::sync::Mutex::new(())),
            activity,
            precise_gate,
            scheduler,
            import: Mutex::new(ImportSlot::new()),
            capture,
            compat: crate::compat::CompatState::default(),
            #[cfg(test)]
            health_aged: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            health_reads: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn home(&self) -> &AliceHome {
        &self.home
    }

    /// The embedder in force (it changes when the model does: read it per use).
    pub fn embedder(&self) -> Arc<dyn Embedder> {
        self.active.read().embedder.clone()
    }

    /// The id of the embedding model in force.
    pub fn model(&self) -> String {
        self.active.read().model.clone()
    }

    /// Why writes are refused (`None` = writable). No database access, so it is cheap to poll.
    pub fn read_only(&self) -> Option<String> {
        self.writer.read_only.reason()
    }

    /// Tests: a handle to the writer, for ops the service has no method for.
    #[cfg(test)]
    pub(crate) fn writer_for_tests(&self) -> WriterHandle {
        self.writer.clone()
    }

    /// Tests: refuse writes as a damaged database would.
    #[doc(hidden)]
    pub fn force_read_only(&self, reason: &str) {
        self.writer.read_only.set(reason.to_string());
    }

    /// Stop the embedding worker, let the writer commit everything queued before this call,
    /// stop it, save the vector index and mark the saved files clean (the next start trusts
    /// them without rebuilding). Blocks until both threads have exited (normally well under a
    /// second; an embedding batch in flight is finished first). Later writes fail with "writer
    /// stopped"; reads keep working. Calling it again does nothing.
    pub fn shutdown(&self) {
        let Some(Threads {
            maintenance,
            embed: worker,
            embedding,
            judge,
            writer,
            watcher,
        }) = self.threads.lock().take()
        else {
            return;
        };
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake_worker.try_send(());
        let _ = self.wake_judge.try_send(());
        // The import first: it reads and, later, writes through the pieces stopped below.
        let import = self.import.lock().handle.take();
        if let Some(h) = import {
            let waited = Instant::now();
            while !h.is_finished() && waited.elapsed() < IMPORT_STOP_WAIT {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !h.is_finished() {
                tracing::warn!("the import is inside a long step; stopping without it");
                let slot = self.import.lock();
                let mut st = slot.status.lock();
                if st.state.is_active() {
                    st.state = alice_core::import::ImportState::Failed;
                    st.error = Some(importer::STOPPED.to_string());
                }
            } else if h.join().is_err() {
                tracing::error!("the import thread panicked");
            }
        }
        // No new re-embed after this; the one running stops at its next batch and saves its
        // progress (the next start resumes it). Before the writer stops: it writes through it.
        self.embedding.poke();
        let waited = Instant::now();
        while !embedding.is_finished() && waited.elapsed() < JUDGE_STOP_WAIT {
            std::thread::sleep(Duration::from_millis(10));
        }
        if embedding.is_finished() && embedding.join().is_err() {
            tracing::error!("the embedding control thread panicked");
        }
        self.embedding.stop_run();
        // An integrity check in progress is interrupted; a backup stops at its next step.
        self.scheduler.interrupt_running();
        // A job ends at its next slice. One inside a long single step (a backup, a check of the
        // whole database) is left to finish on its own, as with the judge below; its writes
        // fail once the writer has stopped.
        let waited = Instant::now();
        while !maintenance.is_finished() && waited.elapsed() < MAINTENANCE_STOP_WAIT {
            std::thread::sleep(Duration::from_millis(10));
        }
        let scheduler_stopped = maintenance.is_finished();
        if !scheduler_stopped {
            tracing::warn!("a maintenance job is still running; stopping without it");
        } else if maintenance.join().is_err() {
            tracing::error!("the maintenance scheduler panicked");
        }
        if watcher.join().is_err() {
            tracing::error!("the config watcher panicked");
        }
        if worker.join().is_err() {
            tracing::error!("the embedding worker panicked");
        }
        // It may be inside a request to a judge that is slow to answer: wait a moment, then
        // leave it. Its writes fail once the writer has stopped.
        let waited = Instant::now();
        while !judge.is_finished() && waited.elapsed() < JUDGE_STOP_WAIT {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !judge.is_finished() {
            tracing::warn!("the judge worker is waiting for an answer; stopping without it");
        } else if judge.join().is_err() {
            tracing::error!("the judge worker panicked");
        }
        self.writer.send_nowait(WriteOp::Shutdown);
        if writer.join().is_err() {
            tracing::error!("the writer panicked");
        }
        // No index yet (still rebuilding): nothing is marked, so the next start rebuilds too.
        let (active, idx) = snapshot(&self.active, &self.index);
        let Some(idx) = idx.filter(|_| active.model != embedding::PLACEHOLDER_MODEL) else {
            return;
        };
        let path = active.vector_path.clone();
        // Saved even when nothing changed since start: an index that never came from these
        // files (an empty one after a failed rebuild, say) must replace them before the mark
        // vouches for them. Each part skips the write when its file already holds it.
        // A maintenance job that did not stop in time may still change the index (a purge
        // drops vectors): the files are saved but not marked, and the next start catches up.
        let saved = idx.save(&path);
        let marked = if scheduler_stopped {
            saved.and_then(|()| VectorIndex::mark_clean(&path))
        } else {
            saved
        };
        if let Err(e) = marked {
            tracing::warn!(error = %e, "saving the vector index failed; the next start rebuilds it");
        }
    }

    pub(crate) async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let readers = self.readers.clone();
        tokio::task::spawn_blocking(move || readers.with(f))
            .await
            .map_err(join_err)?
    }

    /// Project for a request (precedence: spec §4.1, then §2.8's project hint on a tenant). A `client_project` is mirrored first
    /// (on the blocking pool), whatever wins; a working directory not seen in the last minute is
    /// resolved with git on the blocking pool, and never on a server tenant.
    async fn choose(&self, scope: Scope<'_>) -> Result<ProjectChoice> {
        let mirrored = match scope.client_project {
            Some(cp) => {
                let (home, cp) = (self.home.clone(), cp.clone());
                Some(
                    tokio::task::spawn_blocking(move || {
                        crate::client_projects::sync_mirror(&home, &cp)
                    })
                    .await
                    .map_err(join_err)??,
                )
            }
            None => None,
        };
        if let Some(slug) = explicit_slug(scope.project) {
            if slug.chars().count() > limits::MAX_PROJECT_SLUG_CHARS {
                return Err(AliceError::Invalid(
                    "project names are at most 100 characters".into(),
                ));
            }
            return Ok(ProjectChoice::Slug(slug.to_string()));
        }
        if let Some(r) = mirrored {
            return Ok(ProjectChoice::Detected(r));
        }
        if self.opts.tenant
            && let Some(hint) = scope.project_hint
        {
            let slug = alice_core::normalize::slugify(hint);
            if hint.trim().chars().count() > limits::MAX_PROJECT_NAME_CHARS
                || slug.chars().count() > limits::MAX_PROJECT_SLUG_CHARS
            {
                return Err(AliceError::Invalid(
                    "a project name is at most 200 characters and its slug at most 100".into(),
                ));
            }
            if !slug.is_empty() {
                let name = hint.trim().to_string();
                let lookup = slug.clone();
                let found = self
                    .read(move |conn| alice_core::store::projects::resolve_slug(conn, &lookup))
                    .await?;
                return Ok(match found {
                    Some(p) => ProjectChoice::Slug(p.slug),
                    None => ProjectChoice::Detected(ProjectRef {
                        key: format!(
                            "{}{slug}",
                            alice_core::store::projects::PLACEHOLDER_KEY_PREFIX
                        ),
                        slug,
                        name,
                        root: None,
                        remote: None,
                    }),
                });
            }
        }
        let cwd = if self.opts.tenant { None } else { scope.cwd };
        if let Some(choice) = self.resolver.choose_without_git(None, cwd) {
            return Ok(choice);
        }
        let resolver = self.resolver.clone();
        let cwd = cwd.map(String::from);
        tokio::task::spawn_blocking(move || resolver.choose(None, cwd.as_deref()))
            .await
            .map_err(join_err)
    }

    /// Whether this service runs in server mode (`--tenant`).
    pub fn is_tenant(&self) -> bool {
        self.opts.tenant
    }

    /// Whether this service is the team store (`--team-store`).
    pub fn is_team_store(&self) -> bool {
        self.opts.team_store
    }

    /// This tenant's name, when the daemon was given one (`ALICE_TENANT_NAME`).
    pub fn tenant_name(&self) -> Option<&str> {
        self.opts.tenant_name.as_deref()
    }

    /// The link to the team store, when one is configured and usable.
    pub fn team(&self) -> Option<&Arc<crate::team::TeamLink>> {
        self.team.as_ref()
    }

    /// One import batch, written by the writer in a commit of its own (the importer's only way
    /// in; no logic here). The caller must have redacted every text in the batch already:
    /// nothing below redacts it. Facts and other rows that cannot be stored come back in
    /// `BatchOutcome::errors`; a batch-level failure rolls the whole batch back.
    #[allow(dead_code)] // the importer (a later task) is the caller
    pub(crate) async fn submit_import(
        &self,
        batch: alice_core::store::import::ImportBatch,
    ) -> Result<alice_core::store::import::BatchOutcome> {
        let _busy = self.activity.enter();
        match self
            .writer
            .submit(WriteOp::ImportBatch(Box::new(batch)))
            .await?
        {
            WriteOutcome::Imported(out) => Ok(out),
            other => Err(AliceError::Other(format!(
                "import batch answered {}",
                other.name()
            ))),
        }
    }

    pub async fn remember(&self, req: RememberRequest) -> Result<RememberResponse> {
        // A shared write goes to the team store (`remember_shared` holds the activity guard itself).
        if req.shared {
            return self.remember_shared(req).await;
        }
        let _busy = self.activity.enter();
        self.remember_spooled(req).await
    }

    /// [`remember`](Self::remember) for a write the spool queued: the drain is the daemon's own
    /// timer, not a request, so it does not keep maintenance (backups included) from starting.
    pub async fn remember_spooled(&self, req: RememberRequest) -> Result<RememberResponse> {
        // Clients never queue a shared write; one found in the spool still goes to the team.
        if req.shared {
            return self.remember_shared(req).await;
        }
        let project = self.choose(req.scope()).await?;
        // Before anything else touches the text: the writer, the spool and the logs only ever
        // see the redacted request (spec §5.1 step 2).
        let req = self.redact_remember_in(&project, req).await?;
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
                shared: false,
            }),
            WriteOutcome::Remembered(InsertOutcome::Duplicate { id }, p) => Ok(RememberResponse {
                id: Some(id),
                status: RememberStatus::Duplicate,
                project: p.slug,
                reason: None,
                shared: false,
            }),
            other => Err(unexpected(other)),
        }
    }

    /// The working tree of the project a write goes to, if it has one. A named project is
    /// looked up in the database, so call this off the async runtime.
    pub fn project_root(&self, choice: &ProjectChoice) -> Option<ProjectTree> {
        project_root_in(&self.readers, choice)
    }

    /// Whether a write to this project (named, or detected from `cwd`) would be judged under
    /// fail-closed rules at the moment (see [`SettingsHandle::project_fail_closed`]; a project
    /// whose rules the store cannot read is too). The spool asks before it discards a request
    /// that block mode refused.
    pub async fn rules_fail_closed(&self, scope: Scope<'_>) -> bool {
        let Ok(choice) = self.choose(scope).await else {
            return false;
        };
        let (readers, settings) = (self.readers.clone(), self.settings.clone());
        tokio::task::spawn_blocking(move || {
            let (tree, unread) = project_tree_in(&readers, &choice);
            unread || settings.project_fail_closed(tree.as_ref())
        })
        .await
        .unwrap_or(false)
    }

    /// The compiled redactor for a write to `choice`: the global rules with the project's
    /// `.alice.toml` on top. Looks at the database and the file system, so it runs on the
    /// blocking pool.
    async fn redactor(&self, choice: &ProjectChoice) -> Result<Arc<Redactor>> {
        let (readers, redactors, choice) =
            (self.readers.clone(), self.redactors.clone(), choice.clone());
        tokio::task::spawn_blocking(move || {
            let tree = project_root_in(&readers, &choice);
            redactors.for_root(tree.as_ref())
        })
        .await
        .map_err(join_err)
    }

    /// The redactor for the project a memory (an id or legacy id) belongs to; the global rules
    /// for one that is not there (the writer then reports it as unknown), the closed ones when
    /// the store cannot say which rules apply.
    async fn redactor_for_memory(&self, id: String) -> Result<Arc<Redactor>> {
        let (readers, redactors) = (self.readers.clone(), self.redactors.clone());
        tokio::task::spawn_blocking(move || {
            let tree = readers
                .with(|c| projects::tree_of_memory(c, &id))
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "a project's rules could not be read; the edit is screened closed");
                    Some(ProjectTree::unreadable(id.clone(), None))
                });
            redactors.for_root(tree.as_ref())
        })
        .await
        .map_err(join_err)
    }

    async fn redact_remember_in(
        &self,
        choice: &ProjectChoice,
        mut req: RememberRequest,
    ) -> Result<RememberRequest> {
        let r = self.redactor(choice).await?;
        req.content = redact_field(&r, "content", &req.content)?;
        if let Some(t) = &req.title {
            req.title = Some(redact_field(&r, "title", t)?);
        }
        req.topics = req
            .topics
            .iter()
            .map(|t| redact_field(&r, "topic", t))
            .collect::<Result<_>>()?;
        Ok(req)
    }

    /// `req` with secrets redacted, or an error naming the detector in `block` mode. Idempotent:
    /// text that is redacted already comes back as it is. What a request is spooled as.
    pub async fn redact_remember(&self, req: RememberRequest) -> Result<RememberRequest> {
        let choice = self.choose(req.scope()).await?;
        self.redact_remember_in(&choice, req).await
    }

    /// `remember`, or, while the database is read-only, the request queued in the spool and
    /// applied after a repair (spec §5.7), with a reply that says so. Every interface of the
    /// daemon (the JSON API, MCP over HTTP) remembers through this. If even the spool cannot be
    /// written (often the same cause, a full disk), the read-only error says what to do.
    pub async fn remember_or_spool(&self, req: RememberRequest) -> Result<RememberResponse> {
        // A shared write goes to the team store and never touches this database, so there is
        // nothing to spool and nothing to fall back to.
        if req.shared {
            return self.remember_shared(req).await;
        }
        let read_only = match self.remember(req.clone()).await {
            Err(AliceError::ReadOnly(reason)) => reason,
            other => return other,
        };
        let dir = self.home.spool_dir();
        // The spool holds only redacted text; in `block` mode a request with a secret is
        // refused (the error names the detector) rather than queued.
        let entry = spool::SpoolEntry::Remember(self.redact_remember(req).await?);
        match tokio::task::spawn_blocking(move || spool::append(&dir, &entry)).await {
            Ok(Ok(_)) => Ok(RememberResponse {
                id: None,
                status: RememberStatus::Spooled,
                project: String::new(),
                reason: Some(spool::read_only_note(&read_only)),
                shared: false,
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

    // ------------------------------------------------------------------ shared team memory

    /// The team link and this tenant's name, or the error that says why a shared call cannot be made.
    fn team_parts(&self) -> Result<(&Arc<crate::team::TeamLink>, &str)> {
        let (Some(team), Some(tenant)) = (self.team(), self.tenant_name()) else {
            return Err(AliceError::Invalid(
                "no team memory is configured on this server; ask the admin".into(),
            ));
        };
        Ok((team, tenant))
    }

    /// The author the team store records (R34): `<tenant>/<client agent>`, or the tenant alone.
    /// Whatever the client sent, the part before the first `/` is this tenant, and the session
    /// is dropped.
    fn shared_author(tenant: &str, sent: &Author) -> Author {
        let agent = match sent
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            Some(a) => format!("{tenant}/{a}"),
            None => tenant.to_string(),
        };
        Author {
            agent: Some(agent),
            model: sent.model.clone(),
            session: None,
        }
    }

    /// A call to the team store that is a read or a change of an existing memory: its errors
    /// already say what failed.
    async fn team_json<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Req>,
    ) -> Result<Resp> {
        let (team, _) = self.team_parts()?;
        team.call(method, path, body, SHARED_TIMEOUT).await
    }

    async fn team_get(&self, req: GetRequest) -> Result<GetResponse> {
        self.team_json(reqwest::Method::POST, "/api/v1/get", Some(&req))
            .await
    }

    /// `remember` with `shared: true`: redacted with this person's rules, then written to the
    /// team store only, as this tenant (spec §2.10). It is never spooled and never saved here.
    async fn remember_shared(&self, req: RememberRequest) -> Result<RememberResponse> {
        let (team, tenant) = self.team_parts()?;
        let _busy = self.activity.enter();
        let project = self.choose(req.scope()).await?;
        let req = self.redact_remember_in(&project, req).await?;
        let slug = choice_slug(&project);
        // Superseding marks the older memory too: only its author may (spec §2.10).
        let supersedes: Vec<String> = req
            .supersedes
            .iter()
            .map(|id| id.strip_prefix(TEAM_PREFIX).unwrap_or(id).to_string())
            .collect();
        for id in &supersedes {
            self.require_shared_author(id).await?;
        }
        let body = RememberRequest {
            content: req.content,
            kind: req.kind,
            title: req.title,
            topics: req.topics,
            importance: req.importance,
            supersedes,
            // Keys are per tenant: another tenant's retry key must not return this memory.
            idempotency_key: req.idempotency_key.map(|k| format!("{tenant}:{k}")),
            project_hint: (slug != "global").then_some(slug),
            author: Self::shared_author(tenant, &req.author),
            shared: false,
            ..Default::default()
        };
        let r: RememberResponse = team
            .call(
                reqwest::Method::POST,
                "/api/v1/remember",
                Some(&body),
                SHARED_TIMEOUT,
            )
            .await
            .map_err(shared_write_error)?;
        Ok(RememberResponse {
            id: r.id.map(|id| format!("{TEAM_PREFIX}{id}")),
            status: r.status,
            project: r.project,
            reason: r.reason,
            shared: true,
        })
    }

    /// The memory `id` from the team store, when this tenant is its author; otherwise the refusal.
    async fn require_shared_author(&self, id: &str) -> Result<()> {
        let (_, tenant) = self.team_parts()?;
        let got = self
            .team_get(GetRequest {
                ids: vec![id.to_string()],
                include_provenance: false,
            })
            .await?;
        let memory = got
            .memories
            .iter()
            .find(|m| m.memory.id == id)
            .ok_or_else(|| AliceError::NotFound(format!("shared memory {TEAM_PREFIX}{id}")))?;
        if author_tenant(&memory.memory.author) == Some(tenant) {
            Ok(())
        } else {
            Err(AliceError::Invalid(SHARED_REFUSAL.into()))
        }
    }

    /// `update` of a `team:` id, for its author only.
    async fn update_shared(&self, mut req: UpdateRequest) -> Result<MemoryFull> {
        let (_, tenant) = self.team_parts()?;
        let id = req.id[TEAM_PREFIX.len()..].to_string();
        self.require_shared_author(&id).await?;
        if req.update.title.is_some() || req.update.body.is_some() || req.update.topics.is_some() {
            let r = self.redactor(&ProjectChoice::Fallback).await?;
            let u = &mut req.update;
            if let Some(t) = &u.title {
                u.title = Some(redact_field(&r, "title", t)?);
            }
            if let Some(b) = &u.body {
                u.body = Some(redact_field(&r, "body", b)?);
            }
            if let Some(topics) = &u.topics {
                u.topics = Some(
                    topics
                        .iter()
                        .map(|t| redact_field(&r, "topic", t))
                        .collect::<Result<_>>()?,
                );
            }
        }
        if let Some(by) = &req.update.superseded_by {
            req.update.superseded_by = Some(by.strip_prefix(TEAM_PREFIX).unwrap_or(by).to_string());
        }
        let body = UpdateRequest {
            id,
            update: req.update,
            author: Self::shared_author(tenant, &req.author),
        };
        let out: MemoryFull = self
            .team_json(reqwest::Method::POST, "/api/v1/update", Some(&body))
            .await?;
        let mut out = into_team_view(out);
        out.event_id = out.event_id.map(|e| format!("{TEAM_PREFIX}{e}"));
        Ok(out)
    }

    /// `undo` of a `team:<event id>`, when that event was made by this tenant. The team store's
    /// events API filters by exact actor, and an actor is `agent:<tenant>/<client>` with any
    /// client, so the events are listed newest first (a page of 500, up to
    /// [`SHARED_UNDO_PAGES`] pages) until the one asked for is found, then its actor is checked.
    async fn undo_shared(&self, req: UndoRequest) -> Result<UndoResponse> {
        let (_, tenant) = self.team_parts()?;
        let id = req.event_id[TEAM_PREFIX.len()..].to_string();
        let mut before: Option<String> = None;
        let mut mine = false;
        'pages: for _ in 0..SHARED_UNDO_PAGES {
            let path = match &before {
                Some(b) => format!("/api/v1/events?limit=500&before={b}"),
                None => "/api/v1/events?limit=500".to_string(),
            };
            let page: Vec<EventRow> = self
                .team_json(reqwest::Method::GET, &path, None::<&()>)
                .await?;
            for e in &page {
                if e.id == id {
                    mine = e.actor == format!("agent:{tenant}")
                        || e.actor.starts_with(&format!("agent:{tenant}/"));
                    break 'pages;
                }
            }
            match page.last() {
                Some(last) if page.len() >= 500 => before = Some(last.id.clone()),
                _ => break,
            }
        }
        if !mine {
            return Err(AliceError::Invalid(SHARED_REFUSAL.into()));
        }
        let body = UndoRequest {
            event_id: id,
            author: Self::shared_author(tenant, &req.author),
        };
        let mut out: UndoResponse = self
            .team_json(reqwest::Method::POST, "/api/v1/undo", Some(&body))
            .await?;
        out.reverted = out
            .reverted
            .into_iter()
            .map(|e| format!("{TEAM_PREFIX}{e}"))
            .collect();
        Ok(out)
    }

    /// Starts the team store's recall for `req` (spec §2.10): the same query, filters and limit,
    /// fast mode, JSON, no raw chunks, asked about the project this recall resolved to. It runs
    /// beside the personal search; `None` when there is no team link (or this is the team store).
    fn start_team_recall(
        &self,
        req: &RecallRequest,
        choice: &ProjectChoice,
        limit: usize,
    ) -> Option<tokio::task::JoinHandle<Result<Vec<Hit>>>> {
        if self.is_team_store() {
            return None;
        }
        let team = self.team()?.clone();
        let slug = choice_slug(choice);
        let body = RecallRequest {
            query: req.query.clone(),
            project: explicit_slug(req.project.as_deref()).map(str::to_string),
            topics: req.topics.clone(),
            kinds: req.kinds.clone(),
            since: req.since.clone(),
            before: req.before.clone(),
            include_raw: false,
            mode: Some(RecallMode::Fast),
            max_tokens: Some(TEAM_RECALL_TOKENS),
            format: OutputFormat::Json,
            limit: Some(limit),
            project_hint: (slug != "global").then_some(slug),
            ..Default::default()
        };
        Some(tokio::spawn(async move { team_recall(&team, body).await }))
    }

    /// Starts the team store's context for the same project, with a third of the budget.
    fn start_team_context(
        &self,
        req: &ContextRequest,
        choice: &ProjectChoice,
        max_tokens: usize,
    ) -> Option<tokio::task::JoinHandle<Result<Option<String>>>> {
        if self.is_team_store() {
            return None;
        }
        let team = self.team()?.clone();
        let slug = choice_slug(choice);
        let body = ContextRequest {
            project: explicit_slug(req.project.as_deref()).map(str::to_string),
            max_tokens: Some(max_tokens / 3),
            project_hint: (slug != "global").then_some(slug),
            ..Default::default()
        };
        Some(tokio::spawn(async move {
            let asked = tokio::time::timeout(
                TEAM_DEADLINE,
                team.call::<_, ContextResponse>(
                    reqwest::Method::POST,
                    "/api/v1/context",
                    Some(&body),
                    TEAM_DEADLINE,
                ),
            )
            .await
            .unwrap_or_else(|_| {
                Err(AliceError::Other(
                    "the team store did not answer in time".into(),
                ))
            });
            match asked {
                Ok(c) => Ok(Some(c.text)),
                // A project only this person has: the team has nothing to add, and nothing failed.
                Err(AliceError::Invalid(_)) => Ok(None),
                Err(e) => Err(e),
            }
        }))
    }

    pub async fn recall(&self, req: RecallRequest) -> Result<RecallResponse> {
        let _busy = self.activity.enter();
        let limit = req
            .limit
            .unwrap_or(limits::DEFAULT_RECALL_LIMIT)
            .min(limits::MAX_RECALL_LIMIT);
        // The settings of this request: a change applies to the next one.
        let cfg = self.settings();
        let max_tokens = req.max_tokens.unwrap_or(cfg.search.default_max_tokens);
        let format = req.format;
        let include_raw = req.include_raw;
        // The model in force and its index, as one pair (a model switch replaces both at once).
        let (active, index) = snapshot(&self.active, &self.index);
        // Embedded before a reader connection is taken: the model is shared with the
        // embedding worker, and a recall waiting for it must not hold a connection.
        let text = search::recall_query_text(&req.query)?;
        // The mode: the request's own, else `search.default_mode`; `auto` is precise exactly
        // when the judge in force is a model judge (the heuristic cannot grade).
        let can_grade = self.judge().provider() != "heuristic";
        let mode = req.mode.unwrap_or(match cfg.search.default_mode {
            DefaultRecallMode::Fast => RecallMode::Fast,
            DefaultRecallMode::Precise => RecallMode::Precise,
            DefaultRecallMode::Auto if can_grade => RecallMode::Precise,
            DefaultRecallMode::Auto => RecallMode::Fast,
        });
        let (precise, judged_query) = (mode == RecallMode::Precise, text.clone());
        // Precise mode grades PRECISE_TOP results and keeps `limit` of them, so it searches
        // for at least that many; with a judge that can grade it also asks for the candidates
        // just below the vector floor.
        let search_limit = if precise {
            limit.max(PRECISE_TOP)
        } else {
            limit
        };
        let (near_band, near_limit) = if precise && can_grade {
            (NEAR_FLOOR_BAND, PRECISE_TOP)
        } else {
            (0.0, 0)
        };
        // Resolved before a reader connection is taken: it may run git. (And before the query is
        // embedded: a remote embedder gets the query only if the project may use it.)
        let explicit = explicit_slug(req.project.as_deref()).is_some();
        let choice = self.choose(req.scope()).await?;
        let team_task = self.start_team_recall(&req, &choice, limit);
        let mut notes: Vec<String> = Vec::new();
        let vector = match &index {
            Some(idx) if idx.model() == active.model => {
                let embedder = active.embedder.clone();
                // A remote embedder: the cloud policy decides for the recall's project, and the
                // query is redacted with that project's rules before it leaves.
                let send = if embedder.is_remote() {
                    let (cloud, readers, settings, redactors) = (
                        self.cloud.clone(),
                        self.readers.clone(),
                        self.settings.clone(),
                        self.redactors.clone(),
                    );
                    let (choice, text) = (choice.clone(), text.clone());
                    tokio::task::spawn_blocking(move || {
                        query_for_remote(&cloud, &readers, &settings, &redactors, &choice, &text)
                    })
                    .await
                    .map_err(join_err)?
                } else {
                    Some(text.clone())
                };
                match send {
                    Some(q) => {
                        let idx = idx.clone();
                        let v = tokio::task::spawn_blocking(move || {
                            search::embed_query_or_why(Some(&idx), embedder.as_ref(), &q)
                        })
                        .await
                        .map_err(join_err)?;
                        // A recall that used word matches only says so (the first one after
                        // the model was unloaded for being idle, a provider that is down).
                        v.map_err(|why| notes.push(why)).ok()
                    }
                    None => {
                        notes.push(QUERY_HELD_BACK.into());
                        None
                    }
                }
            }
            _ => {
                notes.push(search::INDEX_REBUILDING.into());
                None
            }
        };
        // Whether meaning-search ran: without it, "nothing answers" would claim too much.
        let vector_ran = vector.is_some();
        // The raw tier's chunk vector list reads on this pool, beside the main reader's word
        // lists: one connection of each, main first (`ServiceOptions::vector_reader_pool`).
        let vector_readers = self.vector_readers.clone();
        // A precise recall from a tree the store never saw: the judge's cloud decision for the
        // query (decided here, before `choice` is spent on the read).
        let unstored_query = if precise && can_grade && matches!(choice, ProjectChoice::Detected(_))
        {
            let (cloud, readers, settings, redactors) = (
                self.cloud.clone(),
                self.readers.clone(),
                self.settings.clone(),
                self.redactors.clone(),
            );
            let (choice, text) = (choice.clone(), text.clone());
            tokio::task::spawn_blocking(move || {
                let q = query_cloud(&cloud, &readers, &settings, &redactors, &choice);
                if q.unstored {
                    Some(q.allowed.then(|| q.redact(&text)))
                } else if !q.allowed {
                    // A stored project that a tree of it denies (the one the query comes from):
                    // no project name is asked of the judge, as for a tree that denies itself.
                    Some(None)
                } else {
                    None
                }
            })
            .await
            .map_err(join_err)?
        } else {
            None
        };
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
                    limit: search_limit,
                    // The one place the f64 setting becomes the f32 the search compares with.
                    relevance_floor: cfg.search.relevance_floor as f32,
                    relevance_sure: Some(cfg.search.relevance_sure as f32),
                    relevance_margin: cfg.search.relevance_margin as f32,
                    chunk_floor: Some(search::CHUNK_RELEVANCE_FLOOR),
                    raw_min_results: cfg.search.raw_fallback_min_results,
                    near_band,
                    near_limit,
                    half_lives: cfg.search.half_life.clone(),
                    fusion: search::fusion::Fusion {
                        rrf_k: cfg.search.rrf_k,
                        boost_exponent: cfg.search.boost_exponent,
                    },
                    lists: search::Lists::Hybrid,
                };
                let out = search::run_with_chunk_reads(
                    c,
                    index.as_deref(),
                    vector,
                    search::ChunkReads::Pool(&vector_readers),
                    &cfg.search.weights,
                    &params,
                    chrono::Utc::now(),
                )?;
                Ok((out, scope.project.slug))
            })
            .await?;
        let mut out = out;
        // The team's hits join the personal ones before the person's judge sees the top: in
        // fast mode they are ordered by score with the rest; in precise mode the ones the team
        // store marked `cloud_ok` do the same (`team_ok` names them for `rerank`) and the others
        // wait in `team_rest` to follow the graded hits.
        let mut team_ok = std::collections::HashSet::new();
        let mut team_rest = Vec::new();
        if let Some(task) = team_task {
            match task.await {
                Ok(Ok(mut hits)) => {
                    if precise {
                        withhold_from_judge(&self.settings, &mut hits);
                    }
                    (team_ok, team_rest) = merge_team(&mut out, hits, precise);
                    if !precise {
                        out.hits.truncate(limit);
                        let kept = out
                            .hits
                            .iter()
                            .filter(|h| h.tier == Tier::Knowledge)
                            .count();
                        out.memory_seqs.truncate(kept);
                        out.memory_importance.truncate(kept);
                    }
                }
                _ => notes.push(NOTE_TEAM_LATE.into()),
            }
        }
        let mut reranked = false;
        let mut low_confidence = false;
        if precise {
            let why;
            (out, reranked, low_confidence, why) = self
                .rerank(
                    out,
                    &slug,
                    unstored_query,
                    &judged_query,
                    limit,
                    include_raw,
                    &team_ok,
                )
                .await;
            notes.extend(why);
            // The judge doubts it, but the top result holds a code or the whole phrase verbatim.
            if low_confidence
                && out
                    .hits
                    .first()
                    .is_some_and(|h| exact_match(&judged_query, h))
            {
                low_confidence = false;
            }
            // Only words were searched: what was not found by meaning may still be there.
            if !vector_ran {
                low_confidence = false;
            }
            if low_confidence {
                notes.push(NOTE_NOTHING_ANSWERS.into());
            }
            append_team(&mut out, team_rest, limit);
        }
        if self.is_team_store() {
            // What a federated caller needs to know before it grades these hits (R35): the cloud
            // policy of this store for each hit's project. Any doubt, a failed task included, is `false`.
            let mut projects: Vec<String> = Vec::new();
            for h in &out.hits {
                if !projects.contains(&h.project) {
                    projects.push(h.project.clone());
                }
            }
            let cloud = self.cloud.clone();
            let allowed: std::collections::HashSet<String> =
                tokio::task::spawn_blocking(move || {
                    projects
                        .into_iter()
                        .filter(|p| cloud.cloud_allowed(p))
                        .collect()
                })
                .await
                .unwrap_or_default();
            for h in &mut out.hits {
                h.cloud_ok = Some(allowed.contains(&h.project));
            }
        }
        let (response, touched) = match format {
            OutputFormat::Text => {
                let (text, omitted) = pack::pack_text(&out, Some(&slug), max_tokens);
                let shown = &out.hits[..out.hits.len() - omitted];
                let touched = returned_memories(out.memory_seqs.clone(), shown);
                // The notes come first: a reader going top-down meets the doubt before the results.
                let mut head = String::new();
                for n in &notes {
                    head.push_str(&format!("({n})\n"));
                }
                let text = if head.is_empty() {
                    text
                } else {
                    format!("{head}\n{text}")
                };
                let response = RecallResponse {
                    project: Some(slug),
                    omitted,
                    raw_fallback: out.raw_fallback,
                    reranked,
                    low_confidence,
                    text: Some(text),
                    notes: notes.clone(),
                    ..Default::default()
                };
                (response, touched)
            }
            OutputFormat::Json => {
                let seqs = std::mem::take(&mut out.memory_seqs);
                let mut response = pack::pack_json(out, Some(slug), max_tokens);
                response.reranked = reranked;
                response.low_confidence = low_confidence;
                response.notes = notes.clone();
                let touched = returned_memories(seqs, &response.results);
                (response, touched)
            }
        };
        if !touched.is_empty() {
            self.writer.send_nowait(WriteOp::TouchAccess(touched));
        }
        Ok(response)
    }

    /// Precise mode (spec §4.4 step 4): the judge grades the top `PRECISE_TOP` results of every
    /// tier, raw chunks included, together with the near-floor candidates (those that missed
    /// their vector floor by at most `NEAR_FLOOR_BAND`), in one batched call. The graded
    /// results are re-ordered by P(relevant), in the places the graded results held (results
    /// of a project the cloud may not see, and results the judge gave no grade, keep theirs);
    /// a near-floor candidate is added when the judge says it answers the query
    /// (`RESCUE_ANSWERS_MIN`), among the graded results by its grade. Then `limit` results are
    /// kept. Without a model judge, or when it answers `Denied`, `Budget` or an error, or too
    /// slowly, the order is the search's, no near-floor candidate is returned, and the answer
    /// says so (`false`). The call is scoped to the project the query was asked from.
    ///
    /// `unstored_query` is set when that project is a detected one the store never saw: `None`
    /// inside means its working tree's own file or the global lists deny the cloud (the answer
    /// is the fast one); `Some(text)` is the query redacted with the tree's rules, and the call
    /// then runs under `global`, a project the policy knows, whose gate and redaction apply on
    /// top. It is `Some(None)` too for a stored project that the tree the query comes from
    /// denies, and for an import placeholder read for the checkout that its policy denies.
    ///
    /// `team_ok` holds the ids of team hits the team store marked `cloud_ok`. The person's
    /// policy knows nothing of a team project, so those hits are allowed by the team store's mark
    /// instead of by their project, and the judge is told they belong to the query's project (the
    /// project whose gate and redaction this call runs under). The other switches (a heuristic
    /// judge, a degraded configuration, the budget, the breaker) apply to them as to any hit.
    #[allow(clippy::too_many_arguments)] // one call site; a struct would only rename them
    async fn rerank(
        &self,
        mut out: SearchOutput,
        query_project: &str,
        unstored_query: Option<Option<String>>,
        query: &str,
        limit: usize,
        include_raw: bool,
        team_ok: &std::collections::HashSet<String>,
    ) -> (SearchOutput, bool, bool, Option<String>) {
        let judge = self.judge();
        let mut reranked = false;
        let mut low_confidence = false;
        // Why the answer is the fast one, when precise mode could not grade.
        let mut why: Option<String> = None;
        if judge.provider() == "heuristic" {
            why = Some(NOTE_NO_MODEL_JUDGE.into());
        }
        let (query_project, query) = match unstored_query {
            Some(Some(redacted)) => ("global", redacted),
            // Denied by the tree itself: no project name is asked of the policy for it.
            Some(None) => ("", query.to_string()),
            None => (query_project, query.to_string()),
        };
        let query = query.as_str();
        let near = std::mem::take(&mut out.near);
        let window = out.hits.len().min(PRECISE_TOP);
        // The cloud policy is asked once per distinct project, off the runtime's threads (each
        // answer reads the store).
        let allowed: std::collections::HashSet<String> = if judge.provider() == "heuristic" {
            Default::default()
        } else {
            let mut projects = vec![query_project.to_string()];
            let seen = out.hits[..window]
                .iter()
                .filter(|h| !team_ok.contains(&h.id))
                .chain(near.iter().map(|n| &n.hit))
                .map(|h| h.project.clone());
            for p in seen {
                if !projects.contains(&p) {
                    projects.push(p);
                }
            }
            let hub = self.judge.clone();
            tokio::task::spawn_blocking(move || {
                projects
                    .into_iter()
                    .filter(|p| hub.cloud_allowed(p))
                    .collect()
            })
            .await
            .unwrap_or_default()
        };
        if allowed.contains(query_project) {
            // What the judge is shown, with the place of each: results among the top (their
            // index in the hit list), then near-floor candidates (index `hits.len() + j`).
            let judged = |h: &Hit, importance: u8| JudgeMemory {
                id: h.id.clone(),
                project: if team_ok.contains(&h.id) {
                    query_project.to_string()
                } else {
                    h.project.clone()
                },
                // A raw chunk has no kind; its title ("raw · <source>") says what it is. The
                // text is the stored (redacted) one, bounded like a memory's by the judge.
                kind: h.kind.unwrap_or(Kind::Note),
                title: h.title.clone(),
                body: h.body.clone().unwrap_or_else(|| h.snippet.clone()),
                importance,
                created_at: h.created_at.clone(),
                similarity: None,
                raw: h.tier == Tier::Raw,
                status: h.status,
                origin: h
                    .origin
                    .as_deref()
                    .and_then(alice_core::model::Origin::parse)
                    .unwrap_or_default(),
            };
            let mut shown: Vec<(usize, JudgeMemory)> = Vec::new();
            let mut ordinal = 0;
            for (i, h) in out.hits.iter().enumerate().take(window) {
                let importance = if h.tier == Tier::Knowledge {
                    ordinal += 1;
                    out.memory_importance.get(ordinal - 1).copied().unwrap_or(3)
                } else {
                    3
                };
                if allowed.contains(&h.project) || team_ok.contains(&h.id) {
                    shown.push((i, judged(h, importance)));
                }
            }
            let near_start = out.hits.len();
            for (j, n) in near.iter().enumerate() {
                if allowed.contains(&n.hit.project) {
                    let importance = n.memory.map_or(3, |(_, imp)| imp);
                    shown.push((near_start + j, judged(&n.hit, importance)));
                }
            }
            // One result alone has nothing to be ordered against, but a near-floor candidate
            // alone still has a ruling to get.
            if shown.len() > 1 || shown.iter().any(|(i, _)| *i >= near_start) {
                // The whole budget of the call: made once, with no retries, and the judge's own
                // request timeout is this budget, so the blocking task ends with the deadline.
                let budget = Duration::from_millis(self.settings().search.precise_timeout_ms);
                let permit = match self.precise_gate.admit() {
                    Ok(p) => Some(p),
                    Err(Refused::Busy) => {
                        why = Some(NOTE_BUSY.into());
                        None
                    }
                    Err(Refused::Open) => {
                        why = Some(NOTE_UNAVAILABLE.into());
                        None
                    }
                };
                if let Some(permit) = permit {
                    let (q, p) = (query.to_string(), query_project.to_string());
                    let ms: Vec<JudgeMemory> = shown.iter().map(|(_, m)| m.clone()).collect();
                    let asked = tokio::time::timeout(
                        budget,
                        tokio::task::spawn_blocking(move || {
                            // The permit goes with the call: the slot is freed, and the outcome
                            // recorded, when the judge call ends, whether or not this recall is
                            // still waiting for it (its future may have been dropped).
                            let mut permit = permit;
                            let r = judge.grade_relevance_within(&p, &q, &ms, budget);
                            // Budget and Denied are answers; a failure is not.
                            permit.settle(matches!(
                                &r,
                                Ok(_) | Err(JudgeError::Budget(_) | JudgeError::Denied)
                            ));
                            r
                        }),
                    )
                    .await;
                    match asked {
                        Ok(Ok(Ok(grades))) if grades.len() == shown.len() => {
                            // (index, P(relevant)) of what was graded; near-floor candidates
                            // only when they answer the query.
                            let mut graded: Vec<(usize, f64)> = Vec::new();
                            for ((i, _), d) in shown.iter().zip(&grades) {
                                if d.0.is_empty() {
                                    continue;
                                }
                                if *i < near_start || d.p(RELEVANCE_ANSWERS) >= RESCUE_ANSWERS_MIN {
                                    graded.push((
                                        *i,
                                        d.p(RELEVANCE_ANSWERS) + 0.5 * d.p(RELEVANCE_PARTIAL),
                                    ));
                                }
                            }
                            // Ordered by the grade with the status prior of the search kept
                            // (`weight^λ`, as in fusion): an outdated fact still "answers", and
                            // must not lead the memory that replaced it.
                            let cfg = self.settings();
                            let all_hits = |i: usize| {
                                if i < near_start {
                                    &out.hits[i]
                                } else {
                                    &near[i - near_start].hit
                                }
                            };
                            let prior = |i: usize| {
                                all_hits(i).status.map_or(1.0, |st| {
                                    search::fusion::status_weight(&cfg.search.weights, st)
                                        .powf(cfg.search.boost_exponent)
                                })
                            };
                            graded.sort_by(|a, b| {
                                (b.1 * prior(b.0))
                                    .total_cmp(&(a.1 * prior(a.0)))
                                    .then(a.0.cmp(&b.0))
                            });
                            reranked = !graded.is_empty();
                            // Graded, and not one of them clearly answers the query.
                            low_confidence = reranked
                                && !shown.iter().zip(&grades).any(|(_, d)| {
                                    !d.0.is_empty() && d.p(RELEVANCE_ANSWERS) >= RESCUE_ANSWERS_MIN
                                });
                            if graded.is_empty() {
                                why = Some(NOTE_NO_GRADES.into());
                            }
                            let rescued = graded.iter().filter(|(i, _)| *i >= near_start).count();
                            // The near-floor candidates join the list after the search's own
                            // hits, so that `reorder` finds them by index.
                            for n in &near {
                                out.hits.push(n.hit.clone());
                                if let Some((seq, importance)) = n.memory {
                                    out.memory_seqs.push(seq);
                                    out.memory_importance.push(importance);
                                }
                            }
                            for (i, p) in &graded {
                                out.hits[*i].relevance = Some(*p as f32);
                            }
                            // Ungraded results of the top keep their places; the graded ones
                            // (and the rescued) fill the others, best first; the rest follows.
                            let held: std::collections::HashSet<usize> = (0..window)
                                .filter(|i| !graded.iter().any(|(g, _)| g == i))
                                .collect();
                            let top = window + rescued;
                            let mut stream = graded.iter().map(|(i, _)| *i);
                            let mut order: Vec<usize> = Vec::with_capacity(top);
                            for place in 0..top {
                                if held.contains(&place) {
                                    order.push(place);
                                } else if let Some(from) = stream.next() {
                                    order.push(from);
                                }
                            }
                            order.extend(window..near_start);
                            // A raw chunk that came in through the near-floor channel of a recall
                            // that did not ask for raw results is a raw fallback like any other:
                            // the answer says so.
                            if !include_raw
                                && graded.iter().any(|(i, _)| {
                                    *i >= near_start && out.hits[*i].tier == Tier::Raw
                                })
                            {
                                out.raw_fallback = true;
                            }
                            out = reorder(out, &order);
                        }
                        Ok(Ok(Ok(_))) => {
                            tracing::warn!("the judge graded another number of results");
                            why = Some(NOTE_UNEXPECTED.into());
                        }
                        Ok(Ok(Err(e))) => {
                            tracing::debug!(error = %e, "precise recall: the judge did not grade; the search order stands");
                            why = Some(
                                match e {
                                    JudgeError::Budget(_) => NOTE_BUDGET,
                                    JudgeError::Denied => NOTE_DENIED,
                                    _ => NOTE_UNAVAILABLE,
                                }
                                .into(),
                            );
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "precise recall: the grading task failed");
                            why = Some(NOTE_UNAVAILABLE.into());
                        }
                        Err(_) => {
                            tracing::debug!("precise recall: the judge was too slow");
                            why = Some(NOTE_SLOW.into());
                        }
                    }
                }
            }
        } else if judge.provider() != "heuristic" {
            why = Some(NOTE_DENIED.into());
        }
        // Back to what was asked for; the related memories follow the final top results.
        out.hits.truncate(limit);
        let shown = out
            .hits
            .iter()
            .filter(|h| h.tier == Tier::Knowledge)
            .count();
        out.memory_seqs.truncate(shown);
        out.memory_importance.truncate(shown);
        // `related` followed the unlimited search; it is recomputed for the results kept, which
        // also puts back memories that only the cut-off results shadowed.
        {
            let ids: Vec<String> = out
                .hits
                .iter()
                .filter(|h| h.tier == Tier::Knowledge)
                .map(|h| h.id.clone())
                .collect();
            if let Ok(related) = self.read(move |c| search::related_for(c, &ids)).await {
                out.related = related;
            }
        }
        (out, reranked, low_confidence, why)
    }

    pub async fn context(&self, req: ContextRequest) -> Result<ContextResponse> {
        let _busy = self.activity.enter();
        let max_tokens = req.max_tokens.unwrap_or(limits::DEFAULT_CONTEXT_TOKENS);
        let explicit = explicit_slug(req.project.as_deref()).is_some();
        let choice = self.choose(req.scope()).await?;
        let team_task = self.start_team_context(&req, &choice, max_tokens);
        let mut out = self
            .read(move |c| {
                let scope = ProjectResolver::read_scope(c, choice, explicit)?;
                // A placeholder project (never written to) gets the hint and the global preferences.
                let text =
                    alice_core::context::build(c, &scope.project, max_tokens, chrono::Utc::now())?;
                Ok(ContextResponse {
                    project: scope.project.slug,
                    text,
                })
            })
            .await?;
        if let Some(task) = team_task {
            match task.await {
                Ok(Ok(Some(team))) => {
                    out.text.push_str(&format!("\n## Team memory\n{team}"));
                }
                Ok(Ok(None)) => {}
                _ => out.text.push_str(&format!("\n({NOTE_TEAM_LATE})\n")),
            }
        }
        Ok(out)
    }

    /// Memories by id. A duplicate that was merged into another memory is answered with the
    /// survivor, and `redirects` says so (spec §5.2).
    pub async fn get(&self, mut req: GetRequest) -> Result<GetResponse> {
        let _busy = self.activity.enter();
        let (team_ids, own): (Vec<String>, Vec<String>) = std::mem::take(&mut req.ids)
            .into_iter()
            .partition(|id| id.starts_with(TEAM_PREFIX));
        if team_ids.is_empty() {
            req.ids = own;
            return self.get_personal(req).await;
        }
        let team_req = GetRequest {
            ids: team_ids
                .iter()
                .map(|id| id[TEAM_PREFIX.len()..].to_string())
                .collect(),
            include_provenance: req.include_provenance,
        };
        let mut out = if own.is_empty() {
            GetResponse::default()
        } else {
            req.ids = own;
            self.get_personal(req).await?
        };
        let shared = self.team_get(team_req).await?;
        out.memories
            .extend(shared.memories.into_iter().map(into_team_view));
        out.missing
            .extend(shared.missing.iter().map(|id| format!("{TEAM_PREFIX}{id}")));
        out.redirects
            .extend(shared.redirects.into_iter().map(|r| Redirect {
                from: format!("{TEAM_PREFIX}{}", r.from),
                to: format!("{TEAM_PREFIX}{}", r.to),
            }));
        out.notes.extend(shared.notes);
        Ok(out)
    }

    async fn get_personal(&self, req: GetRequest) -> Result<GetResponse> {
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
            let found_ids: Vec<String> = found.iter().map(|m| m.id.clone()).collect();
            let mut shown: Vec<String> = Vec::new();
            for m in found {
                let asked = req
                    .ids
                    .iter()
                    .find(|id| **id == m.id || m.legacy_id.as_deref() == Some(id.as_str()))
                    .cloned()
                    .unwrap_or_else(|| m.id.clone());
                let Some(followed) = follow_merges(c, &m)? else {
                    if !shown.contains(&m.id) {
                        shown.push(m.id.clone());
                        out.memories.push(full(c, m, req.include_provenance)?);
                    }
                    continue;
                };
                for (i, hop) in followed.hops.into_iter().enumerate() {
                    let hop = if i == 0 {
                        Redirect { from: asked.clone(), to: hop.to }
                    } else {
                        hop
                    };
                    if !out.redirects.contains(&hop) {
                        out.redirects.push(hop);
                    }
                }
                let survivor = followed.last;
                if followed.truncated {
                    out.notes.push(format!(
                        "the merge chain from {asked} is longer than {MAX_MERGE_HOPS} steps; showing {}, which may itself be a merged duplicate",
                        survivor.id
                    ));
                }
                // A survivor that was asked for too is answered on its own turn.
                if !found_ids.contains(&survivor.id) && !shown.contains(&survivor.id) {
                    shown.push(survivor.id.clone());
                    out.memories.push(full(c, survivor, req.include_provenance)?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// The memory after the change, with the event that recorded it (`undo` takes its id).
    pub async fn update(&self, mut req: UpdateRequest) -> Result<MemoryFull> {
        let _busy = self.activity.enter();
        if req.id.starts_with(TEAM_PREFIX) {
            return self.update_shared(req).await;
        }
        let actor = Actor::from_author(&req.author);
        check_update_fields(&req.update)?;
        if req.update.action == UpdateAction::Move {
            req.update.screened = Some(self.screen_move(&req.id, &req.update).await?);
        } else if req.update.title.is_some()
            || req.update.body.is_some()
            || req.update.topics.is_some()
        {
            let r = self.redactor_for_memory(req.id.clone()).await?;
            let u = &mut req.update;
            if let Some(t) = &u.title {
                u.title = Some(redact_field(&r, "title", t)?);
            }
            if let Some(b) = &u.body {
                u.body = Some(redact_field(&r, "body", b)?);
            }
            if let Some(topics) = &u.topics {
                u.topics = Some(
                    topics
                        .iter()
                        .map(|t| redact_field(&r, "topic", t))
                        .collect::<Result<_>>()?,
                );
            }
        }
        let (m, event_id) = match self
            .writer
            .submit(WriteOp::Update {
                id: req.id,
                update: req.update,
                actor,
            })
            .await?
        {
            WriteOutcome::Updated(m, event_id) => (m, event_id),
            other => return Err(unexpected(other)),
        };
        let mut out = self.read(move |c| full(c, m, false)).await?;
        out.event_id = event_id;
        Ok(out)
    }

    /// What a `move` is allowed to do, decided before the writer is asked: a memory of a project
    /// kept away from cloud providers never moves into one that is not (the rule of the project
    /// merge; a slug no project has yet is judged by the settings alone, as the placeholder the
    /// move would make has no `.alice.toml`), and its text is screened with the destination's
    /// privacy rules as the review move does (`block` refuses a text they would change, `redact`
    /// cleans it). The answer carries what the memory looked like, so the writer refuses a
    /// memory edited in between.
    async fn screen_move(&self, id: &str, u: &MemoryUpdate) -> Result<ScreenedMove> {
        let raw = u.project.as_deref().ok_or_else(|| {
            AliceError::invalid("move needs project: the slug of the project to move it to")
        })?;
        let slug = projects::move_slug(raw)?;
        let (asked, wanted) = (id.to_string(), slug.clone());
        let (m, content_hash, updated_at, target, tree) = self
            .read(move |c| {
                let own = memories::id_of(c, &asked)?.ok_or_else(|| {
                    AliceError::NotFound(format!(
                        "memory {asked}; check the id returned by recall or remember"
                    ))
                })?;
                let (m, hash, at) = stamped_memory(c, &own)?
                    .ok_or_else(|| AliceError::NotFound(format!("memory {asked}")))?;
                let target = projects::resolve_move_target(c, &wanted)?;
                let tree = match &target {
                    Some(t) => projects::tree_of(c, &t.id)?,
                    None => None,
                };
                Ok((m, hash, at, target, tree))
            })
            .await?;
        let (dest, dest_allowed) = match &target {
            Some(t) => (t.slug.clone(), self.cloud.cloud_allowed(&t.slug)),
            None => (slug.clone(), self.settings.cloud_allowed(&slug, None)),
        };
        if dest != m.project && dest_allowed && !self.cloud.cloud_allowed(&m.project) {
            return Err(AliceError::invalid(format!(
                "project \"{}\" is kept away from cloud providers and \"{dest}\" is not: deny \"{dest}\" first (add it to privacy.cloud_denied_projects or set cloud_denied in its .alice.toml), or remove the denial of \"{}\", then move memory {}",
                m.project, m.project, m.id
            )));
        }
        let r = self.redactors.for_root(tree.as_ref());
        let screen = |field: &str, t: &str| -> Result<String> {
            let red = r.apply(t);
            if red.found.is_empty() {
                return Ok(t.to_string());
            }
            if r.blocks() {
                return Err(AliceError::invalid(format!(
                    "cannot move memory {}: the privacy rules of project {dest} (block mode) refuse its {field}, which contains what looks like a secret ({}); keep it where it is",
                    m.id,
                    red.found.join(", ")
                )));
            }
            tracing::info!(
                field,
                "redacted a moved memory with the destination's rules"
            );
            Ok(red.text)
        };
        Ok(ScreenedMove {
            title: screen("title", &m.title)?,
            body: screen("body", &m.body)?,
            topics: m
                .topics
                .iter()
                .map(|t| screen("topic", t))
                .collect::<Result<_>>()?,
            content_hash,
            updated_at,
        })
    }

    pub async fn facts(&self, req: FactsRequest) -> Result<FactsResponse> {
        let _busy = self.activity.enter();
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

    pub async fn assert_fact(&self, mut req: AssertFactRequest) -> Result<FactOutcome> {
        let _busy = self.activity.enter();
        let actor = Actor::from_author(&req.author);
        // Facts belong to no project; the one a fact cites as its source (if any) sets the rules.
        let r = match req.source_memory_id.clone() {
            Some(id) => self.redactor_for_memory(id).await?,
            None => self.redactors.for_root(None),
        };
        req.subject = redact_field(&r, "subject", &req.subject)?;
        req.predicate = redact_field(&r, "predicate", &req.predicate)?;
        req.object = redact_field(&r, "object", &req.object)?;
        let verdict = self.match_new_predicate(&req).await;
        match self
            .writer
            .submit(WriteOp::AssertFact {
                fact: req,
                actor,
                alias: verdict.alias,
                review: verdict.review,
            })
            .await?
        {
            WriteOutcome::Fact(mut f) => {
                f.suggestion = f.suggestion.or(verdict.suggestion);
                Ok(f)
            }
            other => Err(unexpected(other)),
        }
    }

    /// What the judge makes of a predicate the vocabulary has not seen (spec §5.2): the same
    /// as an existing one (mapped when sure), or only similar (suggested, and asked in the
    /// review inbox). Never fails: without an answer the fact is stored as given.
    async fn match_new_predicate(&self, req: &AssertFactRequest) -> PredicateVerdict {
        let none = PredicateVerdict::default();
        let name = kg::normalize_predicate(&req.predicate);
        if name.is_empty() || req.action == kg::FactAction::End {
            return none;
        }
        let (n, source) = (name.clone(), req.source_memory_id.clone());
        let looked = self
            .read(move |c| {
                if kg::predicate_exists(c, &n)? {
                    return Ok(None);
                }
                let names = kg::predicate_names(c, PREDICATE_VOCABULARY)?;
                // The project of the memory the fact cites: a cloud judge sees the names only
                // if that project may go to the cloud.
                let project: Option<String> = match source {
                    Some(id) => c
                        .query_row(
                            "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id
                             WHERE m.id = ?1 OR m.legacy_id = ?1 ORDER BY m.seq LIMIT 1",
                            [id],
                            |r| r.get(0),
                        )
                        .optional()?,
                    None => None,
                };
                Ok(Some((names, project)))
            })
            .await;
        let Ok(Some((names, project))) = looked else {
            return none;
        };
        // The most similar names go to the judge; the heuristic only acts on close ones anyway.
        let mut close: Vec<(String, f64)> = names
            .into_iter()
            .map(|p| {
                let sim = alice_core::judge::text::predicate_similarity(&name, &p);
                (p, sim)
            })
            .collect();
        close.sort_by(|a, b| b.1.total_cmp(&a.1));
        close.truncate(PREDICATE_CANDIDATES);
        let candidates: Vec<String> = close.into_iter().map(|(p, _)| p).collect();
        if candidates.is_empty() {
            return none;
        }
        let settings = self.settings();
        let judge = self.judge();
        let (n, c) = (name.clone(), candidates.clone());
        let asked = tokio::time::timeout(
            PREDICATE_WAIT,
            tokio::task::spawn_blocking(move || {
                let scope: Vec<&str> = project.as_deref().into_iter().collect();
                let d = judge.match_predicate_in(&scope, &n, &c);
                (judge.provider().to_string(), d)
            }),
        )
        .await;
        let (provider, decision) = match asked {
            Ok(Ok((provider, Ok(d)))) => (provider, d),
            // Denied, over budget, failing, slow: the heuristic decides, locally.
            _ => (
                "heuristic".to_string(),
                alice_core::judge::heuristic::HeuristicJudge::default()
                    .match_predicate(&name, &candidates)
                    .unwrap_or_default(),
            ),
        };
        let Some((canonical, p)) = decision.best().filter(|(c, _)| c != "none") else {
            return none;
        };
        if !candidates.contains(&canonical) {
            return none;
        }
        let j = &settings.judge;
        // The same words the other way round (`depends_on`, `depended_on_by`) are a different
        // predicate: never mapped without a person looking.
        let reversed = alice_core::judge::text::directions_differ(&name, &canonical);
        if p >= j.auto_apply_threshold && !reversed {
            return PredicateVerdict {
                alias: Some((name, canonical)),
                ..none
            };
        }
        if p >= j.review_threshold {
            return PredicateVerdict {
                suggestion: Some(canonical.clone()),
                review: Some(NewReview {
                    kind: ReviewType::Predicate,
                    payload: serde_json::json!({
                        "predicate": name,
                        "canonical": canonical,
                        "p": p,
                        "provider": provider,
                        "reversed": reversed,
                    }),
                    confidence: Some(p),
                    dedupe_key: Some(format!("predicate:{name}")),
                }),
                ..none
            };
        }
        none
    }

    /// Recorded changes, newest first (spec §5.5). The actor may be a bare agent name
    /// (`codex` means `agent:codex`), and a memory may be named by its legacy id.
    pub async fn events(&self, mut req: EventsRequest) -> Result<Vec<EventRow>> {
        let _busy = self.activity.enter();
        req.actor = req
            .actor
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|a| Actor::parse(a).as_string());
        self.read(move |c| {
            if let Some(target) = req.target_id.as_deref()
                && let Some(own) = memories::id_of(c, target)?
            {
                req.target_id = Some(own);
            }
            alice_core::store::events::list(c, &req)
        })
        .await
    }

    /// Reverse a change by its event id. A target that changed since is never overwritten:
    /// the answer names the review item that asks what to do instead.
    pub async fn undo(&self, req: UndoRequest) -> Result<UndoResponse> {
        let _busy = self.activity.enter();
        if req.event_id.starts_with(TEAM_PREFIX) {
            return self.undo_shared(req).await;
        }
        let actor = Actor::from_author(&req.author);
        match self
            .writer
            .submit(WriteOp::Undo {
                event_id: req.event_id,
                actor,
            })
            .await?
        {
            WriteOutcome::Undo(UndoOutcome::Undone { reverted, .. }) => Ok(UndoResponse {
                undone: true,
                message: format!("Undone: {} change(s) reverted.", reverted.len()),
                reverted,
                review_id: None,
            }),
            WriteOutcome::Undo(UndoOutcome::Conflict {
                review_id, reason, ..
            }) => Ok(UndoResponse {
                undone: false,
                reverted: Vec::new(),
                message: format!(
                    "Not undone: {reason}. Review item {review_id} asks what to do (alice review)."
                ),
                review_id: Some(review_id),
            }),
            other => Err(unexpected(other)),
        }
    }

    // ------------------------------------------------------------------ review inbox

    /// Review items, newest first (open ones unless the query says otherwise).
    pub async fn reviews(&self, q: ReviewQuery) -> Result<Vec<ReviewItem>> {
        let _busy = self.activity.enter();
        self.read(move |c| reviews::list(c, &q)).await
    }

    /// Open items in all and by type, narrowed to a type and provider.
    pub async fn review_counts(
        &self,
        review_type: Option<reviews::ReviewType>,
        provider: Option<String>,
    ) -> Result<reviews::ReviewCounts> {
        let _busy = self.activity.enter();
        self.read(move |c| reviews::counts_open(c, review_type, provider.as_deref()))
            .await
    }

    /// Close every open item of a type (and provider) without changing any memory.
    pub async fn dismiss_reviews(&self, req: DismissRequest) -> Result<DismissResponse> {
        let _busy = self.activity.enter();
        match self
            .writer
            .submit(WriteOp::DismissReviews {
                review_type: req.review_type,
                provider: req.provider.filter(|p| !p.trim().is_empty()),
                actor: Actor::from_author(&req.author),
            })
            .await?
        {
            WriteOutcome::Count(n) => Ok(DismissResponse {
                dismissed: n as u64,
            }),
            other => Err(unexpected(other)),
        }
    }

    pub async fn review(&self, id: String) -> Result<ReviewItem> {
        let _busy = self.activity.enter();
        self.read(move |c| {
            reviews::get(c, &id)?.ok_or_else(|| {
                AliceError::NotFound(format!("review item {id}; list them with `alice review`"))
            })
        })
        .await
    }

    /// Apply one of a review item's choices and record the decision.
    pub async fn resolve_review(&self, req: ResolveRequest) -> Result<ResolveResponse> {
        let _busy = self.activity.enter();
        let actor = Actor::from_author(&req.author);
        let text = {
            let (id, choice, redactors) =
                (req.id.clone(), req.choice.clone(), self.redactors.clone());
            self.read(move |c| screen_resolution(c, &redactors, &id, &choice))
                .await?
        };
        match self
            .writer
            .submit(WriteOp::ResolveReview {
                id: req.id,
                choice: req.choice.clone(),
                actor,
                text,
            })
            .await?
        {
            WriteOutcome::Resolved(review) => Ok(ResolveResponse {
                message: format!("Resolved: {}.", req.choice),
                review: *review,
            }),
            other => Err(unexpected(other)),
        }
    }

    /// Make `others` duplicates of `survivor` (trashed, linked to it).
    pub async fn merge(&self, req: MergeRequest) -> Result<MergeResponse> {
        let _busy = self.activity.enter();
        if req.others.len() > MAX_MERGE_OTHERS {
            return Err(AliceError::invalid(format!(
                "{} memories to merge is over the limit of {MAX_MERGE_OTHERS} per request; merge them in smaller groups",
                req.others.len()
            )));
        }
        let actor = Actor::from_author(&req.author);
        match self
            .writer
            .submit(WriteOp::Merge {
                survivor: req.survivor,
                others: req.others,
                actor,
            })
            .await?
        {
            WriteOutcome::Merged(merged) => Ok(MergeResponse { merged }),
            other => Err(unexpected(other)),
        }
    }

    /// Counts, review and judge backlogs, and the judge's cost (UTC days).
    pub async fn stats(&self) -> Result<Stats> {
        let _busy = self.activity.enter();
        let provider = self.judge().provider().to_string();
        let judge_warning = self.judge_warning();
        self.read(move |c| {
            let group = |sql: &str| -> Result<BTreeMap<String, u64>> {
                let mut stmt = c.prepare(sql)?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
                })?;
                Ok(rows.collect::<std::result::Result<_, _>>()?)
            };
            let count = |sql: &str| -> Result<u64> {
                Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
            };
            let now = chrono::Utc::now();
            let today = now.format("%Y-%m-%d").to_string();
            let week = (now - chrono::Duration::days(6))
                .format("%Y-%m-%d")
                .to_string();
            let costs = alice_core::store::judge::cost_since(c, &week)?;
            // The judge's spend is every one of its ledger rows: its own and distillation's.
            use alice_core::judge::jev::{LEDGER_DISTILL, LEDGER_ROWS};
            let judge_costs = |keep: &dyn Fn(&alice_core::store::judge::CostRow) -> bool| {
                // From +0.0: an empty f64 `sum()` is -0.0, which would show as `-0.0`.
                costs
                    .iter()
                    .filter(|r| LEDGER_ROWS.contains(&r.provider.as_str()) && keep(r))
                    .fold(0.0, |usd, r| usd + r.usd)
            };
            let mut top = c.prepare(
                "SELECT p.slug, count(*) FROM memories m JOIN projects p ON p.id = m.project_id
                 WHERE m.status <> 'trashed' GROUP BY p.slug ORDER BY 2 DESC, p.slug LIMIT 20",
            )?;
            let top_projects = top
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(Stats {
                memories_by_status: group("SELECT status, count(*) FROM memories GROUP BY status")?,
                memories_by_kind: group(
                    "SELECT kind, count(*) FROM memories WHERE status <> 'trashed' GROUP BY kind",
                )?,
                top_projects,
                chunks: count("SELECT count(*) FROM chunks")?,
                reviews_open: reviews::count_open(c)?,
                judge_provider: provider,
                judge_warning,
                judge_backlog: alice_core::store::judge::judge_backlog(c)?,
                judge_retry: count("SELECT count(*) FROM memories WHERE judge_state = 'retry'")?,
                embed_backlog: embeddings::backlog(c)? as u64,
                cost_today_usd: judge_costs(&|r| r.day == today),
                cost_7d_usd: judge_costs(&|_| true),
                distill_cost_today_usd: judge_costs(&|r| {
                    r.day == today && r.provider == LEDGER_DISTILL
                }),
                // Filled by the distiller (SP2 Task 12).
                distilled_today: 0,
                distill_dropped_today: 0,
            })
        })
        .await
    }

    /// Changes committed from now on (the SSE stream); a slow receiver misses old items.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StreamItem> {
        self.writer.subscribe()
    }

    /// How often the SSE stream checks whether health changed.
    pub fn health_tick(&self) -> Duration {
        self.opts.health_tick
    }

    /// The health for event-stream clients: one reading per `health_tick` shared by all of
    /// them (the counts scan tables), so it is at most one tick old.
    pub async fn stream_health(&self) -> Result<Health> {
        let mut cached = self.stream_health.lock().await;
        // Fresh for half a tick: a client's tick can land just before this one's reading is a
        // tick old, and must not then wait a whole tick for the next one.
        if let Some((at, h)) = cached.as_ref()
            && self.age_of(*at) < self.opts.health_tick / 2
        {
            return Ok(h.clone());
        }
        let h = self.health().await?;
        *cached = Some((Instant::now(), h.clone()));
        #[cfg(test)]
        {
            *self.health_aged.lock() = Duration::ZERO;
        }
        Ok(h)
    }

    // ------------------------------------------------------------------ settings

    /// The settings in effect: a shared pointer to a snapshot. Read them once per request; a
    /// change applies to the next request.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.get()
    }

    pub fn settings_handle(&self) -> &Arc<SettingsHandle> {
        &self.settings
    }

    pub fn secrets(&self) -> &Arc<dyn SecretStore> {
        &self.secrets
    }

    /// What the store's backend is called to a person.
    fn secrets_store_name(&self) -> &'static str {
        match self.secrets.backend() {
            "keychain" => "OS keychain",
            _ => "secrets file",
        }
    }

    /// The status of every secret. **Blocks** when the store does (an OS keychain may wait for
    /// a permission prompt): request handlers use `settings_view_async`.
    fn secret_statuses(&self) -> BTreeMap<String, SecretStatus> {
        SecretName::ALL
            .iter()
            .map(|n| {
                (
                    n.as_str().to_string(),
                    secrets::status_with(self.secrets.as_ref(), *n, self.secret_env.as_ref()),
                )
            })
            .collect()
    }

    fn unknown_secrets() -> BTreeMap<String, SecretStatus> {
        SecretName::ALL
            .iter()
            .map(|n| (n.as_str().to_string(), SecretStatus::unset()))
            .collect()
    }

    fn secrets_silent_note(&self) -> String {
        format!(
            "the {} did not answer within {} ms; its secrets show as not set until it does (unlock it or approve its prompt)",
            self.secrets_store_name(),
            self.opts.secrets_timeout.as_millis()
        )
    }

    fn secrets_failed_note(&self) -> String {
        format!(
            "reading the {} failed (the daemon log has the cause); its secrets show as not set until it can be read",
            self.secrets_store_name()
        )
    }

    /// The view without the secrets' status (which may wait on the store), plus notes.
    /// Entries of `privacy.cloud_denied_projects` that name no stored project by slug, alias or
    /// display name. Only a hint for typos: the entries are never changed or dropped.
    fn unmatched_cloud_denied(&self) -> Vec<String> {
        let list = self.settings.get().privacy.cloud_denied_projects.clone();
        if list.is_empty() {
            return Vec::new();
        }
        let known: std::collections::HashSet<String> = self
            .readers
            .with(|c| {
                let mut names = Vec::new();
                let mut stmt = c.prepare("SELECT slug, name FROM projects")?;
                for r in
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                {
                    let (slug, name) = r?;
                    names.push(slug);
                    names.push(name);
                }
                let mut stmt = c.prepare("SELECT alias FROM project_aliases")?;
                for a in stmt.query_map([], |r| r.get::<_, String>(0))? {
                    names.push(a?);
                }
                Ok(names)
            })
            .unwrap_or_default()
            .into_iter()
            .map(|n| alice_core::normalize::slugify(&n))
            .collect();
        if known.is_empty() {
            return Vec::new();
        }
        list.into_iter()
            .filter(|e| !known.contains(&alice_core::normalize::slugify(e.trim())))
            .collect()
    }

    fn view_with(
        &self,
        mut secrets: BTreeMap<String, SecretStatus>,
        mut notes: Vec<String>,
    ) -> SettingsView {
        // On a server the Jev key is the owner's (addendum 5): a person sees only that it is set.
        if self.opts.tenant {
            for s in secrets.values_mut() {
                s.hint = None;
            }
        }
        // The errors before the settings: a reload swaps the settings first and clears the
        // errors second, so an empty list read here is followed by settings that are new.
        let errors = self.settings.errors();
        let r = self.settings.resolved();
        let running = &self.started_with;
        let restart_pending: Vec<String> = settings::RESTART_KEYS
            .iter()
            .filter(|k| settings::get_key(&r.settings, k) != settings::get_key(running, k))
            .map(|k| k.to_string())
            .collect();
        if restart_pending.iter().any(|k| k == "secrets.backend") {
            let name = |s: &Settings| {
                settings::get_key(s, "secrets.backend")
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default()
            };
            let (now, was) = (name(&r.settings), name(running));
            notes.push(format!(
                "secrets.backend is {now} but this daemon started with {was}; the change applies after `alice daemon stop` and the next start, and the secrets are not copied: under {now} they show as not set until you set them again with `alice config set-secret <name>`"
            ));
        }
        for entry in self.unmatched_cloud_denied() {
            notes.push(format!(
                "privacy.cloud_denied_projects lists \"{entry}\", which matches no project (by slug, alias or name); it is kept and denies any project later called that, but check it for a typo"
            ));
        }
        if self.settings.loosening_waiting() {
            notes.push(format!(
                "config.toml loosens a protection (redaction, a cloud denylist or a budget); it applies once it reads the same on {} polls in a row (about {} s), so a half-written save never switches protections off. The settings in force are unchanged until then",
                settings::LOOSEN_POLLS,
                self.opts.settings_poll.as_secs().max(1) * u64::from(settings::LOOSEN_POLLS)
            ));
        }
        SettingsView {
            settings: serde_json::to_value(&r.settings).expect("settings serialise"),
            sources: r.sources.clone(),
            keys: settings::key_infos(),
            errors,
            warnings: self.settings.warnings(),
            config_degraded: self.settings.config_degraded(),
            notes,
            secrets,
            restart_pending,
        }
    }

    /// Every setting with its source, the problems with them and the secrets' status. May
    /// block on the secrets store: a request handler uses `settings_view_async`.
    pub fn settings_view(&self) -> SettingsView {
        self.view_with(self.secret_statuses(), Vec::new())
    }

    /// The view with every secret shown as not set, without asking the secrets store (which may
    /// wait for a prompt): for callers that need only the errors, warnings and pending restarts.
    pub fn settings_view_without_secrets(&self) -> SettingsView {
        self.view_with(Self::unknown_secrets(), Vec::new())
    }

    /// One number from a read-only query that returns a single integer.
    pub(crate) async fn count(&self, sql: &'static str) -> Result<u64> {
        self.read(move |c| Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))?.max(0) as u64))
            .await
    }

    /// The secrets' status for a request handler, read off the runtime's threads. Requests that
    /// overlap share one read, unless that read began before the latest change to a secret or a
    /// setting (a view after a write must show the write). A read that has been running longer
    /// than `secrets_timeout` (a keychain waiting for a prompt) is not waited for: `Silent`.
    async fn secret_statuses_shared(self: &Arc<Self>) -> SecretsAnswer {
        let (started, mut answer) = {
            let mut running = self.secrets_read.lock();
            // Read under the lock and before the read starts: a change that lands later only
            // makes the next view start its own read.
            let gen_now = self.settings_gen.load(Ordering::SeqCst);
            match running.as_ref().filter(|r| r.gen_at_start == gen_now) {
                Some(r) => (r.started, r.answer.clone()),
                None => {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    let started = Instant::now();
                    let id = self.secrets_read_seq.fetch_add(1, Ordering::SeqCst) + 1;
                    *running = Some(SecretsRead {
                        id,
                        started,
                        gen_at_start: gen_now,
                        answer: rx.clone(),
                    });
                    let me = self.clone();
                    let done = ReadDone(self.secrets_read.clone(), id);
                    tokio::task::spawn_blocking(move || {
                        let _done = done;
                        let statuses = me.secret_statuses();
                        let _ = tx.send(Some(statuses));
                    });
                    (started, rx)
                }
            }
        };
        let wait = self.opts.secrets_timeout.saturating_sub(started.elapsed());
        match tokio::time::timeout(wait, answer.wait_for(|a| a.is_some())).await {
            Ok(Ok(statuses)) => match statuses.clone() {
                Some(s) => SecretsAnswer::Statuses(s),
                None => SecretsAnswer::Failed,
            },
            // The reader ended without sending: it panicked.
            Ok(Err(_)) => {
                tracing::warn!("reading the secrets' status failed");
                SecretsAnswer::Failed
            }
            Err(_) => SecretsAnswer::Silent,
        }
    }

    /// A change that a settings view shows has landed: later views read afresh.
    fn settings_changed(&self) {
        self.settings_gen.fetch_add(1, Ordering::SeqCst);
    }

    /// `settings_view` for a request handler: the secrets are read off the runtime's threads
    /// and given `secrets_timeout`; a store that does not answer (a keychain waiting for a
    /// prompt) leaves them as not set, with a note, and overlapping requests share one read.
    pub async fn settings_view_async(self: &Arc<Self>) -> SettingsView {
        match self.secret_statuses_shared().await {
            SecretsAnswer::Statuses(secrets) => self.view_with(secrets, Vec::new()),
            SecretsAnswer::Silent => {
                self.view_with(Self::unknown_secrets(), vec![self.secrets_silent_note()])
            }
            SecretsAnswer::Failed => {
                self.view_with(Self::unknown_secrets(), vec![self.secrets_failed_note()])
            }
        }
    }

    /// Apply dotted-key changes (`null` removes a key from config.toml); all or nothing.
    /// Blocking: a request handler uses `patch_settings_async`.
    pub fn patch_settings(
        &self,
        changes: serde_json::Map<String, serde_json::Value>,
    ) -> std::result::Result<PatchResponse, Vec<SettingError>> {
        let before = self.settings.get();
        let out = self.settings.patch(&changes)?;
        self.settings_changed();
        Ok(self.patch_response(&before, out, self.settings_view()))
    }

    /// The answer to a change, with the notes it earns: a protective setting loosened (applied at
    /// once, where the same edit of config.toml waits), keys saved but not used yet.
    fn patch_response(
        &self,
        before: &Settings,
        out: PatchOutcome,
        view: SettingsView,
    ) -> PatchResponse {
        let mut notes = Vec::new();
        let loosened = settings::loosened_protections(before, &self.settings.get());
        if !loosened.is_empty() {
            notes.push(format!(
                "{} loosened and applied at once: a change through the API or `alice config set` is a complete, deliberate write, where the same edit typed into config.toml waits to be read the same on {} polls in a row",
                loosened.join(", "),
                settings::LOOSEN_POLLS
            ));
        }
        if out.changed.iter().any(|k| {
            matches!(
                k.as_str(),
                "embedding.provider" | "embedding.model" | "embedding.endpoint"
            )
        }) {
            notes.extend(self.reembed_note());
        }
        PatchResponse {
            changed: out.changed,
            restart_required: out.restart_required,
            overridden_by_env: out.overridden_by_env,
            notes,
            view,
        }
    }

    /// What a change of the embedding model will do: that everything is embedded again in the
    /// background while the current model serves, and, for a remote provider, how many texts
    /// will leave the machine (and how many the cloud policy keeps at home).
    fn reembed_note(&self) -> Option<String> {
        self.opts.embedder_factory.as_ref()?;
        let want = self.settings.get().embedding.clone();
        let want_id = alice_embed::expected_model_id(&want);
        let current = self.model();
        if want_id == current {
            return None;
        }
        let Some((_, host)) = want.remote_destination() else {
            return Some(format!(
                "the embedding model changes to {want_id}: every memory and chunk is embedded again in the background, on this machine; {current} answers searches until the new index is complete, then the switch is atomic"
            ));
        };
        let by_project = self.readers.with(embeddings::embeddable_by_project).ok()?;
        let policy: Arc<dyn JevPolicy> = self.cloud.clone();
        let mut cache = HashMap::new();
        let (mut sent, mut kept) = ((0usize, 0usize), (0usize, 0usize));
        for (project, memories, chunks) in by_project {
            let slot = if crate::worker::project_allowed(
                &self.readers,
                Some(&policy),
                &project,
                &mut cache,
            ) {
                &mut sent
            } else {
                &mut kept
            };
            slot.0 += memories;
            slot.1 += chunks;
        }
        Some(format!(
            "the embedding model changes to {want_id}: about {} memories and {} chunks will be sent to {host} to be embedded again, in the background. The switch is gated by the cloud policy project by project: {} memories and {} chunks of projects it keeps at home (privacy.cloud_denied_projects, a project's cloud_denied) are not sent and stay searchable by words only. {current} answers searches until the new index is complete, then the switch is atomic",
            sent.0, sent.1, kept.0, kept.1
        ))
    }

    /// `patch_settings` for a request handler: the file work and the secrets' status stay off
    /// the runtime's threads.
    pub async fn patch_settings_async(
        self: &Arc<Self>,
        changes: serde_json::Map<String, serde_json::Value>,
    ) -> std::result::Result<PatchResponse, Vec<SettingError>> {
        let me = self.clone();
        let before = self.settings.get();
        let out = tokio::task::spawn_blocking(move || me.settings.patch(&changes))
            .await
            .map_err(|e| {
                vec![SettingError {
                    key: "config.toml".into(),
                    message: format!("internal task failed: {e}"),
                }]
            })??;
        self.settings_changed();
        let view = self.settings_view_async().await;
        Ok(self.patch_response(&before, out, view))
    }

    /// Store a secret. Blocking when the store is (see `set_secret_async`). The event records
    /// that it was set, never the value.
    pub fn set_secret(&self, name: SecretName, value: String) -> Result<SecretStatus> {
        self.secrets.set(name, &value)?;
        self.settings_changed();
        self.writer.send_nowait(WriteOp::RecordEvent {
            actor: Actor::User,
            action: "secret.set".into(),
            target_type: "secret".into(),
            target_id: name.as_str().into(),
            before: None,
            after: None,
        });
        if name == SecretName::JevApiKey {
            self.judge.rebuild_now();
        }
        if name == SecretName::EmbeddingApiKey {
            // The embedder is built again with the new key (on its own thread: it may call the
            // provider).
            self.embedding.rebuild.store(true, Ordering::SeqCst);
            self.embedding.poke();
        }
        Ok(secrets::status_with(
            self.secrets.as_ref(),
            name,
            self.secret_env.as_ref(),
        ))
    }

    /// `true` when there was a value to delete.
    pub fn delete_secret(&self, name: SecretName) -> Result<bool> {
        let had = self.secrets.delete(name)?;
        self.settings_changed();
        if had {
            self.writer.send_nowait(WriteOp::RecordEvent {
                actor: Actor::User,
                action: "secret.delete".into(),
                target_type: "secret".into(),
                target_id: name.as_str().into(),
                before: None,
                after: None,
            });
            if name == SecretName::JevApiKey {
                self.judge.rebuild_now();
            }
            if name == SecretName::EmbeddingApiKey {
                self.embedding.rebuild.store(true, Ordering::SeqCst);
                self.embedding.poke();
            }
        }
        Ok(had)
    }

    /// Record that the API token was replaced (the event never holds the token).
    pub fn record_token_rotation(&self) {
        self.writer.send_nowait(WriteOp::RecordEvent {
            actor: Actor::User,
            action: "token.rotate".into(),
            target_type: "token".into(),
            target_id: "api".into(),
            before: None,
            after: None,
        });
    }

    /// The judge in force: the one `judge.provider` names, or the heuristic when Jev cannot be
    /// used (no key, no HTTP client).
    pub fn judge(&self) -> Arc<dyn Judge> {
        self.judge.current()
    }

    /// Why the judge is not working as set: it fell back to the heuristic, or Jev is paused
    /// (its spend cannot be read, or the day's budget is used up).
    pub fn judge_warning(&self) -> Option<String> {
        self.judge.warning()
    }

    /// Runs `f` (a call into the secrets store) off the runtime's threads, for at most
    /// `secrets_timeout` in all. One call at a time: a second waits for the first, and gives up
    /// with the same error when the store stays stuck, without parking a thread of its own.
    async fn secrets_call<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let silent = || {
            AliceError::Other(format!(
                "the {} did not answer within {} ms; unlock it or approve its prompt, then try again (a change that was asked for may still land: see `alice config list`)",
                self.secrets_store_name(),
                self.opts.secrets_timeout.as_millis()
            ))
        };
        let started = Instant::now();
        let turn = tokio::time::timeout(
            self.opts.secrets_timeout,
            self.secrets_write.clone().lock_owned(),
        )
        .await
        .map_err(|_| silent())?;
        let wait = self.opts.secrets_timeout.saturating_sub(started.elapsed());
        let task = tokio::task::spawn_blocking(move || {
            let _turn = turn;
            f()
        });
        match tokio::time::timeout(wait, task).await {
            Ok(done) => done.map_err(join_err)?,
            Err(_) => Err(silent()),
        }
    }

    /// `set_secret` for a request handler.
    pub async fn set_secret_async(
        self: &Arc<Self>,
        name: SecretName,
        value: String,
    ) -> Result<SecretStatus> {
        let me = self.clone();
        self.secrets_call(move || me.set_secret(name, value)).await
    }

    /// `delete_secret` for a request handler.
    pub async fn delete_secret_async(self: &Arc<Self>, name: SecretName) -> Result<bool> {
        let me = self.clone();
        self.secrets_call(move || me.delete_secret(name)).await
    }

    /// Connection test for a provider (`judge`, `embedding`: the embedder in force).
    pub async fn test_provider(&self, which: &str) -> Result<ProviderTest> {
        let started = Instant::now();
        let took = || started.elapsed().as_millis() as u64;
        match which {
            "embedding" => {
                let e = self.embedder();
                let id = e.model_id().to_string();
                if e.is_remote() && self.settings.config_degraded() {
                    return Ok(ProviderTest {
                        ok: false,
                        message: "config.toml has not loaded as a valid file since the daemon started, so no text is sent to a cloud provider; fix it first (alice config list)".into(),
                        latency_ms: took(),
                    });
                }
                let r = tokio::task::spawn_blocking(move || {
                    e.embed_passages(&["connection test".to_string()])
                })
                .await
                .map_err(join_err)?;
                Ok(match r {
                    Ok(v) if v.len() == 1 => ProviderTest {
                        ok: true,
                        message: format!("{id} answered with {} dimensions", v[0].len()),
                        latency_ms: took(),
                    },
                    Ok(_) => ProviderTest {
                        ok: false,
                        message: "the embedder returned no vector".into(),
                        latency_ms: took(),
                    },
                    Err(e) => ProviderTest {
                        ok: false,
                        message: e.to_string(),
                        latency_ms: took(),
                    },
                })
            }
            "judge" => {
                let j = self.judge();
                let provider = j.provider().to_string();
                if provider == "heuristic" {
                    // Jev wanted but not usable: the test fails and says why.
                    let fell_back = self.judge.fell_back();
                    return Ok(ProviderTest {
                        ok: !fell_back,
                        message: self
                            .judge_warning()
                            .unwrap_or_else(|| "the heuristic judge needs no connection".into()),
                        latency_ms: took(),
                    });
                }
                use alice_core::judge::jev::scenarios;
                let r = tokio::task::spawn_blocking(move || {
                    j.classify_constant(scenarios::CLASSIFY_STATE, scenarios::CLASSIFY_OPTIONS)
                })
                .await
                .map_err(join_err)?;
                Ok(match r {
                    Ok(d) => ProviderTest {
                        ok: true,
                        message: format!(
                            "Jev answered ({})",
                            d.best()
                                .map_or("no choice".into(), |(c, p)| format!("{c}, {p:.2}"))
                        ),
                        latency_ms: took(),
                    },
                    Err(e) => ProviderTest {
                        ok: false,
                        message: e.to_string(),
                        latency_ms: took(),
                    },
                })
            }
            other => Err(AliceError::invalid(format!(
                "unknown provider \"{other}\"; test judge or embedding"
            ))),
        }
    }

    /// Tests: how many times the health was read.
    #[cfg(test)]
    pub(crate) fn health_reads(&self) -> usize {
        self.health_reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Tests: make the shared health reading `by` older (a clock offset: it works however
    /// long the machine has been up).
    #[cfg(test)]
    pub(crate) async fn age_stream_health(&self, by: Duration) {
        *self.health_aged.lock() += by;
    }

    /// How old a reading taken at `at` is.
    fn age_of(&self, at: Instant) -> Duration {
        #[cfg(test)]
        {
            at.elapsed() + *self.health_aged.lock()
        }
        #[cfg(not(test))]
        {
            at.elapsed()
        }
    }

    pub async fn ingest(&self, req: IngestRequest) -> Result<IngestOutcome> {
        let _busy = self.activity.enter();
        self.ingest_spooled(req).await
    }

    /// [`ingest`](Self::ingest) for a request the spool queued (no activity, as
    /// [`remember_spooled`](Self::remember_spooled)).
    pub async fn ingest_spooled(&self, mut req: IngestRequest) -> Result<IngestOutcome> {
        // Checked here so an oversized request never reaches (or delays) the writer.
        sources::validate_chunks(&req.chunks)?;
        let project = self.choose(req.scope()).await?;
        // The whole request is redacted (or refused) before its first slice is written, off the
        // runtime's threads: a request can hold 32 MB of text.
        let r = self.redactor(&project).await?;
        req = tokio::task::spawn_blocking(move || -> Result<IngestRequest> {
            req.source.uri = redact_field(&r, "source uri", &req.source.uri)?;
            let mut redacted = 0usize;
            // In order: a private key cut across chunks is redacted in every one of them.
            let mut order: Vec<usize> = (0..req.chunks.len()).collect();
            order.sort_by_key(|&i| req.chunks[i].ord);
            let mut open = false;
            for i in order {
                let c = &mut req.chunks[i];
                let text = r.check_piece("chunk text", &c.text, &mut open)?;
                if text != c.text {
                    redacted += 1;
                    c.text = text;
                }
            }
            if redacted > 0 {
                // One line per request, not per chunk.
                tracing::info!(
                    chunks = redacted,
                    "redacted secrets in raw chunks before storing them"
                );
            }
            Ok(req)
        })
        .await
        .map_err(join_err)??;
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
        let _busy = self.activity.enter();
        self.read(projects::list_projects).await
    }

    /// Merge project `from` into `into` (or, with `dry_run`, say what that would move). The
    /// answer carries the event id that `alice undo` reverses.
    ///
    /// The privacy rules of `from` (its `.alice.toml`, and what earlier merges carried into it)
    /// are carried into `into`, which keeps the stricter of both from then on; undo takes back
    /// exactly what this merge carried.
    pub async fn merge_projects(&self, req: ProjectMergeRequest) -> Result<ProjectMergeResponse> {
        use alice_core::store::project_merge;
        let _busy = self.activity.enter();
        let (from, into) = (req.from.clone(), req.into.clone());
        let (plan, from_tree) = self
            .read(move |c| {
                let plan = project_merge::plan(c, &from, &into)?;
                let tree = projects::tree_of(c, &plan.from.id)?;
                Ok((plan, tree))
            })
            .await?;
        // What `from`'s own file puts in force now. While that is only the closed stand-in (its
        // tree is gone and was never read here, its file is invalid) or not in force yet (a
        // loosening waiting for its polls), the merge would carry block mode and a cloud denial
        // `from` never had into `into` for good: refused, with the reason. Asked before the
        // cloud check, whose "kept away from cloud providers" would not be true.
        let from_load = self.settings.project_rules(from_tree.as_ref());
        if let Some(why) = rules_not_known(&plan.from.slug, from_tree.as_ref(), &from_load) {
            return Err(AliceError::invalid(why));
        }
        // Privacy first: rows of a project kept away from cloud providers must not become
        // eligible for them by moving into one that is not. The one policy the judge and the
        // remote embedder ask (every name of the project, its `.alice.toml`, degraded config).
        // The merge carries `from`'s own denial into `into`, but `into` must be denied already:
        // the person decides that, not the merge.
        let from_denied = !self.cloud.cloud_allowed(&plan.from.slug);
        if from_denied && self.cloud.cloud_allowed(&plan.into.slug) {
            return Err(AliceError::invalid(format!(
                "project \"{}\" is kept away from cloud providers and \"{}\" is not: deny \"{}\" first (add it to privacy.cloud_denied_projects or set cloud_denied in its .alice.toml), or remove the denial of \"{}\", then merge",
                plan.from.slug, plan.into.slug, plan.into.slug, plan.from.slug
            )));
        }
        let mut from_rules = from_load.settings;
        // A denial of `from` that is not in its file (the global list naming its slug, an alias
        // or its display name, which do not all become names of `into`) is carried too, as the
        // refusal above treats `from` as denied. Not while the configuration is degraded: that
        // denies every project, and would be carried for good (final re-review 2, M-3).
        if from_denied && !self.settings.config_degraded() {
            from_rules.privacy.cloud_denied = true;
        }
        let answer = |merged: bool,
                      event_id: Option<String>,
                      c: &project_merge::MergeCounts,
                      carried: &alice_core::settings::ProjectSettings| {
            let rules = carried.rule_names();
            let kept = if rules.is_empty() {
                String::new()
            } else {
                format!(
                    " \"{}\" {} the privacy rules of \"{}\": {}.",
                    plan.into.slug,
                    if merged {
                        "now also keeps"
                    } else {
                        "would also keep"
                    },
                    plan.from.slug,
                    rules.join(", ")
                )
            };
            ProjectMergeResponse {
                from: plan.from.slug.clone(),
                into: plan.into.slug.clone(),
                merged,
                memories: c.memories,
                chunks: c.chunks,
                sources: c.sources,
                facts: c.facts,
                message: if merged {
                    format!(
                        "Merged \"{}\" into \"{}\": {} memories, {} chunks, {} sources, {} facts moved. \"{}\" now names \"{}\".{kept}{}",
                        plan.from.slug,
                        plan.into.slug,
                        c.memories,
                        c.chunks,
                        c.sources,
                        c.facts,
                        plan.from.slug,
                        plan.into.slug,
                        event_id
                            .as_deref()
                            .map(|e| format!(" To undo: alice undo {e}"))
                            .unwrap_or_default()
                    )
                } else {
                    format!(
                        "Would merge \"{}\" into \"{}\": {} memories, {} chunks, {} sources, {} facts.{kept}",
                        plan.from.slug, plan.into.slug, c.memories, c.chunks, c.sources, c.facts
                    )
                },
                event_id,
                rules_carried: rules,
            }
        };
        if req.dry_run {
            let carried = project_merge::rules_to_carry(&plan, &from_rules);
            return Ok(answer(false, None, &plan.counts, &carried));
        }
        match self
            .writer
            .submit(WriteOp::MergeProjects {
                from: req.from,
                into: req.into,
                actor: Actor::from_author(&req.author),
                rules: Box::new(from_rules),
            })
            .await?
        {
            WriteOutcome::ProjectMerged(done) => {
                let (done, event) = *done;
                Ok(answer(true, Some(event), &done.counts, &done.carried))
            }
            other => Err(unexpected(other)),
        }
    }

    // ------------------------------------------------------------------ import

    /// The tenant rules of an import (spec §2.9): the palace lies strictly inside
    /// `import-inbox/` (placed there by the admin's `alice-server import`), and the disk keeps
    /// room for twice its size plus [`IMPORT_MIN_FREE_AFTER`].
    fn check_tenant_import(&self, req: &alice_core::import::ImportRequest) -> Result<()> {
        let outside = || {
            AliceError::invalid(
                "a server tenant imports only a palace the admin placed under import-inbox/ with `alice-server import`; it cannot read other paths on the server",
            )
        };
        let inbox = std::fs::canonicalize(self.home.import_inbox()).map_err(|_| outside())?;
        let palace = req.palace.as_deref().ok_or_else(outside)?;
        let palace = std::fs::canonicalize(palace).map_err(|_| outside())?;
        if palace == inbox || !palace.starts_with(&inbox) {
            return Err(outside());
        }
        let size = tree_size(&palace);
        // An unknown free size cannot prove there is room.
        let free = self.free_bytes().unwrap_or(0);
        if free < size.saturating_mul(2).saturating_add(IMPORT_MIN_FREE_AFTER) {
            return Err(AliceError::invalid(format!(
                "the server has {free} bytes free; importing this {size}-byte palace needs about twice its size plus 2 GB; ask the server's admin"
            )));
        }
        Ok(())
    }

    /// Free bytes on the volume of the data directory (`None`: unknown).
    fn free_bytes(&self) -> Option<u64> {
        let home = self.home.root();
        match &self.opts.free_space {
            Some(f) => f(home),
            None => backup::free_space(home),
        }
    }

    /// The pieces a job thread needs, taken from the service while `slot` is held.
    fn import_env(
        &self,
        slot: &ImportSlot,
        guard: crate::maintenance::OwnedActivityGuard,
    ) -> ImportEnv {
        ImportEnv {
            readers: self.readers.clone(),
            writer: self.writer.clone(),
            redactors: self.redactors.clone(),
            settings: self.settings.clone(),
            stop: self.stop.clone(),
            home: self.home.clone(),
            mempalace_home: self.opts.mempalace_home.clone(),
            guard,
            status: slot.status.clone(),
            hook: self.opts.import_hook.clone(),
        }
    }

    /// Starts an import of a MemPalace palace on a thread of its own. The refusals that need
    /// no work (one already running, no palace, a missing or foreign folder, a bad `since`, a
    /// remote embedding provider) come back at once; everything else shows in
    /// [`import_status`](Self::import_status).
    pub fn start_import(
        self: &Arc<Self>,
        req: alice_core::import::ImportRequest,
    ) -> Result<alice_core::import::ImportStatus> {
        use alice_core::import::{ImportState, ImportStatus};
        let stopping =
            || AliceError::invalid("the daemon is stopping; start it again and rerun the import");
        if self.stop.load(Ordering::SeqCst) {
            return Err(stopping());
        }
        if self.opts.tenant {
            self.check_tenant_import(&req)?;
        }
        let running = |slot: &ImportSlot| {
            let st = slot.status.lock();
            st.state.is_active().then(|| {
                AliceError::invalid(format!(
                    "an import is already running (phase {}, {} of {} drawers); wait for it, or follow it with `alice import status`",
                    st.phase, st.done, st.total
                ))
            })
        };
        if let Some(e) = running(&self.import.lock()) {
            return Err(e);
        }
        // Disk checks run without the slot lock, so a status read never waits for them.
        let palace = importer::preflight(
            &req,
            self.opts.mempalace_home.as_deref(),
            &self.settings.get(),
        )?;
        let mut slot = self.import.lock();
        // Under the lock: shutdown takes the handle under it too, so a job started here is
        // always one shutdown can see and join.
        if self.stop.load(Ordering::SeqCst) {
            return Err(stopping());
        }
        if let Some(e) = running(&slot) {
            return Err(e);
        }
        // The last job has finished (its status is no longer `Running`).
        if let Some(old) = slot.handle.take() {
            let _ = old.join();
        }
        let job_id = new_id();
        let first = ImportStatus {
            state: ImportState::Running,
            job_id: Some(job_id.clone()),
            phase: "starting".to_string(),
            done: 0,
            total: 0,
            report: None,
            error: None,
            batches: 0,
            max_batch_records: 0,
            max_batch_bytes: 0,
            max_wal_bytes: 0,
            bytes_received: 0,
            embedding_backlog: 0,
        };
        *slot.status.lock() = first.clone();
        let env = self.import_env(&slot, self.activity.enter_owned());
        match importer::spawn(env, req, palace, job_id) {
            Ok(handle) => {
                slot.handle = Some(handle);
                Ok(first)
            }
            Err(e) => {
                *slot.status.lock() = ImportStatus::idle();
                Err(AliceError::Other(format!(
                    "could not start the import thread: {e}"
                )))
            }
        }
    }

    /// The running import's progress, or the last one's outcome. Cheap: no I/O.
    pub fn import_status(&self) -> alice_core::import::ImportStatus {
        // Slot, then status: the order `start_import` takes them in.
        self.import.lock().status.lock().clone()
    }

    /// Chunks still waiting for a vector (what `stats` and `health` call the embed backlog).
    pub async fn embed_backlog(&self) -> Result<u64> {
        self.read(|c| Ok(embeddings::backlog(c)? as u64)).await
    }

    /// Whether the data directory has room for an upload of `size` bytes, `received` of which
    /// are already on the disk: about three times its size (the archive, the unpacked copy and
    /// the database growth) must remain free above [`IMPORT_MIN_FREE_AFTER`].
    fn check_upload_room(&self, size: u64, received: u64) -> Result<()> {
        // An unknown free size cannot prove there is room.
        let free = self.free_bytes().unwrap_or(0);
        if free.saturating_add(received)
            < size.saturating_mul(3).saturating_add(IMPORT_MIN_FREE_AFTER)
        {
            return Err(AliceError::invalid(format!(
                "the server has {free} bytes free; importing an upload of {size} bytes needs about three times its size plus 2 GB; ask the server's admin"
            )));
        }
        Ok(())
    }

    /// Admits an upload (`POST /api/v1/import/upload`, tenant mode): refuses at once what the
    /// headers already show (over the cap, no room on the disk, a remote embedding provider
    /// not allowed) and takes the one import slot BEFORE a byte of the body is read.
    pub fn upload_begin(
        self: &Arc<Self>,
        content_length: Option<u64>,
        allow_remote_embedding: bool,
    ) -> std::result::Result<UploadTicket, UploadError> {
        use alice_core::import::{ImportState, ImportStatus};
        if self.stop.load(Ordering::SeqCst) {
            return Err(AliceError::invalid(
                "the daemon is stopping; try the upload again in a minute",
            )
            .into());
        }
        let settings = self.settings.get();
        let cap = settings.import.max_upload_bytes;
        if let Some(n) = content_length.filter(|n| *n > cap) {
            return Err(UploadError::TooLarge(too_large_message(n, cap)));
        }
        importer::check_remote_embedding(&settings, allow_remote_embedding)?;
        if let Some(n) = content_length {
            self.check_upload_room(n, 0)?;
        }
        let mut slot = self.import.lock();
        {
            let st = slot.status.lock();
            if st.state.is_active() {
                return Err(UploadError::Conflict(format!(
                    "an import is already in progress ({}); wait until `GET /api/v1/import/status` says done or failed, then upload again",
                    match st.state {
                        ImportState::Receiving => "receiving an upload".to_string(),
                        ImportState::Unpacking => "unpacking an upload".to_string(),
                        _ => format!("phase {}", st.phase),
                    }
                )));
            }
        }
        if self.stop.load(Ordering::SeqCst) {
            return Err(AliceError::invalid(
                "the daemon is stopping; try the upload again in a minute",
            )
            .into());
        }
        if let Some(old) = slot.handle.take() {
            let _ = old.join();
        }
        let inbox = self.home.import_inbox();
        std::fs::create_dir_all(&inbox).map_err(AliceError::Io)?;
        let job_id = new_id();
        *slot.status.lock() = ImportStatus {
            state: ImportState::Receiving,
            job_id: Some(job_id.clone()),
            phase: "receiving".to_string(),
            ..ImportStatus::idle()
        };
        let name = format!("{}{job_id}", crate::upload::PREFIX);
        Ok(UploadTicket {
            svc: self.clone(),
            archive: inbox.join(format!("{name}.tar.gz")),
            dir: inbox.join(&name),
            job_id,
            cap,
            declared: content_length,
            received: 0,
            guard: Some(self.activity.enter_owned()),
            allow_remote_embedding,
            reason: None,
            armed: true,
        })
    }

    // ------------------------------------------------------------------ maintenance

    /// When requests last came. A long task (an import) holds `activity().enter()` for its whole
    /// length, so maintenance does not start under it.
    pub fn activity(&self) -> &Arc<Activity> {
        &self.activity
    }

    /// Run one maintenance job now, to its end, whether or not the daemon is idle.
    pub async fn run_job(&self, job: Job) -> Result<JobReport> {
        let _busy = self.activity.enter();
        let sched = self.scheduler.clone();
        tokio::task::spawn_blocking(move || sched.run_job(job, chrono::Utc::now(), true))
            .await
            .map_err(join_err)
    }

    /// Copy the database into the backup directory now (`alice backup now`).
    pub async fn backup_now(&self) -> Result<BackupInfo> {
        if self.opts.tenant {
            return Err(AliceError::invalid(
                "on a server tenant backups are scheduled (the last 2 are kept); ask the server's admin for an extra copy",
            ));
        }
        let _busy = self.activity.enter();
        let (db, dir) = (
            self.home.db(),
            backup::backup_dir(&self.home, &self.settings().backup),
        );
        let policy = {
            let b = &self.settings().backup;
            (b.daily, b.weekly)
        };
        let made = tokio::task::spawn_blocking(move || {
            let now = chrono::Utc::now();
            // The same guard as the scheduled job: no room fails with a clear message.
            let mut warnings = Vec::new();
            backup::ensure_room(&db, &dir, now, policy, &backup::free_space, &mut warnings)?;
            let info = backup::create(&db, &dir, BackupKind::Manual, now)?;
            Ok::<_, AliceError>(info)
        })
        .await
        .map_err(join_err)??;
        Ok(made)
    }

    /// The backups in the backup directory, newest first.
    pub async fn backups(&self) -> Result<Vec<BackupInfo>> {
        let dir = backup::backup_dir(&self.home, &self.settings().backup);
        tokio::task::spawn_blocking(move || backup::list(&dir))
            .await
            .map_err(join_err)?
    }

    /// When each maintenance job last ran.
    pub async fn maintenance_status(&self) -> Result<Vec<JobStatus>> {
        self.read(|c| {
            let meta = |job: Job, what: &str| {
                alice_core::store::maintenance::get_meta(c, &format!("maint.{}.{what}", job.name()))
            };
            Job::ALL
                .iter()
                .map(|&j| {
                    Ok(JobStatus {
                        job: j.name().into(),
                        every_secs: j.every().as_secs(),
                        last_run: meta(j, "last_run")?,
                        last_ok: meta(j, "last_ok")?,
                        last_error: meta(j, "last_error")?,
                    })
                })
                .collect()
        })
        .await
    }

    pub async fn health(&self) -> Result<Health> {
        #[cfg(test)]
        self.health_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let settings_warnings = self.settings.warnings();
        let settings_errors = self.settings.errors();
        let config_degraded = self.settings.config_degraded();
        let instance = self.instance.clone();
        let read_only = self.writer.read_only.reason();
        // The model first, then the progress of a re-embed: the progress is hidden before the
        // model changes, so an answer never shows a finished re-embed beside the old model.
        let (active, index) = snapshot(&self.active, &self.index);
        let index_ready = index.is_some();
        let embedding_model = active.model.clone();
        let reembed = self.embedding.status();
        let embedding_error = self.embedding.problem();
        let judge_provider = self.judge().provider().to_string();
        let judge_warning = self.judge_warning();
        let team_warning = self
            .team_error
            .clone()
            .or_else(|| self.team.as_ref().and_then(|t| t.problem()));
        let model_loaded = self.embedder().is_loaded();
        let memory_bytes = crate::doctor::self_memory_bytes();
        let unrecognised_lines = self.capture.unrecognised_lines_7d();
        let capture_settings = self.settings();
        let tenant = self.opts.tenant;
        // Read the backup directory, the spool and the database: off the runtime's threads.
        let scheduler = self.scheduler.clone();
        let backup_dir = backup::backup_dir(&self.home, &self.settings().backup);
        let spool_dir = self.home.spool_dir();
        let (maintenance_warning, last_backup, spool_pending) =
            tokio::task::spawn_blocking(move || {
                let warning = scheduler.backup_warning(chrono::Utc::now());
                (
                    warning,
                    newest_backup_time(&backup_dir),
                    spool::pending_count(&spool_dir) as u64,
                )
            })
            .await
            .map_err(join_err)?;
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
                judge_backlog: alice_core::store::judge::judge_backlog(c)?,
                judge_provider,
                judge_warning,
                embedding_model,
                reembed,
                embedding_error,
                embed_skipped_cloud: embeddings::skipped_cloud_count(c)? as u64,
                maintenance_warning,
                read_only,
                reviews_open: reviews::count_open(c)?,
                model_loaded,
                spool_pending,
                last_backup,
                memory_bytes,
                index_ready,
                settings_warnings,
                settings_errors,
                config_degraded,
                team_warning,
                capture: crate::capture::health(c, &capture_settings, unrecognised_lines)?,
                tenant,
            })
        })
        .await
    }

    /// A transcript upload (SP2 spec §5.2): `Err(cursor)` when `q.offset` is below the source's
    /// cursor (the route answers `409 {"next_offset": cursor}`), else what was captured.
    pub async fn capture_transcript(
        &self,
        q: TranscriptQuery,
        body: axum::body::Bytes,
    ) -> Result<std::result::Result<TranscriptAnswer, u64>> {
        let report = self.capture_upload(q, body).await?;
        Ok(match report.behind {
            Some(cursor) => Err(cursor),
            None => Ok(report.answer()),
        })
    }

    /// [`capture_transcript`](Self::capture_transcript) with the whole report of the pass.
    ///
    /// - refused (`Invalid`, nothing written): an unknown agent or event, a bad id, an offset
    ///   over [`crate::capture::MAX_OFFSET`], and Codex (no reader until Task 17);
    /// - `no_capture=1` is a no-op: nothing is read or written, and the client moves past the
    ///   body's last complete line;
    /// - the project: on a server tenant, the hint (`project_header`, the `X-Alice-Project`
    ///   header, else `project`), resolved as a direct MCP caller's is (an existing slug or
    ///   alias, else the `import:<slug>` placeholder); on a local daemon, `cwd` (plan decision
    ///   P8). An existing source keeps its own project;
    /// - `session-end` closes the trailing turn whatever it waits for and ends the source;
    ///   every other event closes it unless its last tool call has no result yet. With
    ///   `more=1` (a capped body) the trailing turn stays open and nothing ends.
    pub async fn capture_upload(
        &self,
        q: TranscriptQuery,
        body: axum::body::Bytes,
    ) -> Result<PassReport> {
        let _busy = self.activity.enter();
        let agent = alice_core::capture::AgentKind::parse(&q.agent).ok_or_else(|| {
            AliceError::invalid(format!(
                "unknown agent \"{}\"; use claude-code or codex",
                q.agent
            ))
        })?;
        // Controller ruling: nothing reads a Codex rollout yet (Task 17). Accepted, its offset
        // would never move and the client would send a growing body on every event.
        if agent == alice_core::capture::AgentKind::Codex {
            return Err(AliceError::invalid(
                "Codex transcripts are not captured by this server yet; capture for Codex arrives in a later version",
            ));
        }
        crate::capture::check_ids(&q.session, q.subagent.as_deref())?;
        crate::capture::check_event(&q.event)?;
        crate::capture::check_offset(q.offset)?;
        if q.no_capture.is_some_and(|v| v != 0) {
            let uri = agent.source_uri(&q.session, q.subagent.as_deref());
            return PassReport::not_captured(uri, q.offset, &body);
        }
        let nonempty = |s: &Option<String>| s.clone().filter(|v| !v.trim().is_empty());
        let hint = nonempty(&q.project_header).or_else(|| nonempty(&q.project));
        let project = self
            .choose(Scope {
                project: None,
                cwd: q.cwd.as_deref(),
                client_project: None,
                project_hint: hint.as_deref(),
            })
            .await?;
        let session_end = q.event == "session-end";
        // A capped body (`more=1`): the transcript goes on, so its trailing turn stays open.
        let more = q.more.is_some_and(|v| v != 0);
        let input = PassInput {
            agent,
            session: q.session,
            subagent: q.subagent,
            init: CaptureInit {
                no_distill: q.no_distill.is_some_and(|v| v != 0),
                ..Default::default()
            },
            project,
            bytes: body,
            offset: q.offset,
            at_file_start: q.offset == 0,
            close_trailing: !session_end && !more,
            force_close: session_end && !more,
            event: q.event,
            end: session_end && !more,
        };
        let capture = self.capture.clone();
        tokio::task::spawn_blocking(move || capture.apply(input))
            .await
            .map_err(join_err)?
    }

    /// `GET /api/v1/capture/briefing` (SP2 spec §5.7, §8.2): the SessionStart briefing, the
    /// `context` text of the session's project cut to [`BRIEFING_MAX_CHARS`] at a line end.
    /// `None` when `capture.briefing` is off, or when `source` is `resume` or `fork` (the
    /// session already has its context).
    ///
    /// The project: on a server tenant, the hint (`project_header`, the `X-Alice-Project`
    /// header, else `project`), resolved as a transcript upload's is (an existing slug or alias,
    /// or the `import:<slug>` placeholder when it is stored); a hint no stored project answers
    /// to gives the global briefing, because a read creates nothing. On a local daemon, `cwd`.
    pub async fn capture_briefing(&self, q: BriefingQuery) -> Result<Option<String>> {
        let _busy = self.activity.enter();
        if !self.settings().capture.briefing
            || matches!(q.source.as_deref().map(str::trim), Some("resume" | "fork"))
        {
            return Ok(None);
        }
        let req = if self.opts.tenant {
            let nonempty = |s: &Option<String>| s.clone().filter(|v| !v.trim().is_empty());
            let project = match nonempty(&q.project_header).or_else(|| nonempty(&q.project)) {
                None => None,
                Some(hint) => {
                    let choice = self
                        .choose(Scope {
                            project_hint: Some(&hint),
                            ..Scope::default()
                        })
                        .await?;
                    self.read(move |c| {
                        Ok(match choice {
                            ProjectChoice::Slug(slug) => Some(slug),
                            ProjectChoice::Detected(r) => match projects::detected_for_read(c, &r)?
                            {
                                DetectedForRead::Stored(p) | DetectedForRead::Placeholder(p) => {
                                    Some(p.slug)
                                }
                                DetectedForRead::Ambiguous(_) | DetectedForRead::Unstored => None,
                            },
                            ProjectChoice::Fallback => None,
                        })
                    })
                    .await?
                }
            };
            ContextRequest {
                project,
                ..ContextRequest::default()
            }
        } else {
            ContextRequest {
                cwd: q.cwd,
                ..ContextRequest::default()
            }
        };
        let text = self.context(req).await?.text;
        Ok(Some(
            crate::capture::cut_at_line_end(&text, BRIEFING_MAX_CHARS).to_string(),
        ))
    }

    /// `GET /api/v1/capture/status`: the capture switch, distillation and the 50 sessions
    /// captured most recently.
    pub async fn capture_status(&self) -> Result<CaptureStatus> {
        let _busy = self.activity.enter();
        let (tenant, settings) = (self.opts.tenant, self.settings());
        self.read(move |c| crate::capture::status(c, tenant, &settings))
            .await
    }
}

/// When the newest scheduled or manual backup was made (RFC 3339), if there is one. The
/// copies kept before a migration or a restore are not backups of the current data.
fn newest_backup_time(dir: &Path) -> Option<String> {
    backup::list(dir)
        .ok()?
        .into_iter()
        .find(|b| matches!(b.kind, BackupKind::Scheduled | BackupKind::Manual))
        .map(|b| {
            b.created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
}

impl Drop for Service {
    /// Without an explicit `shutdown`, ask both threads to stop but do not wait for them.
    fn drop(&mut self) {
        if self.threads.get_mut().is_some() {
            self.stop.store(true, Ordering::SeqCst);
            let _ = self.wake_worker.try_send(());
            let _ = self.wake_judge.try_send(());
            self.embedding.signal_stop();
            self.writer.send_nowait(WriteOp::Shutdown);
        }
    }
}

/// `out` with its hits in the order `order` says (`order[i]` is the old index of the hit now at
/// `i`; a permutation), the knowledge hits' `memory_seqs` and importances following their hits.
fn reorder(mut out: SearchOutput, order: &[usize]) -> SearchOutput {
    let ordinals: Vec<Option<usize>> = {
        let mut n = 0;
        out.hits
            .iter()
            .map(|h| {
                (h.tier == Tier::Knowledge).then(|| {
                    n += 1;
                    n - 1
                })
            })
            .collect()
    };
    let hits = std::mem::take(&mut out.hits);
    let (seqs, importance) = (
        std::mem::take(&mut out.memory_seqs),
        std::mem::take(&mut out.memory_importance),
    );
    for &from in order {
        out.hits.push(hits[from].clone());
        if let Some(k) = ordinals[from] {
            out.memory_seqs.push(seqs[k]);
            out.memory_importance.push(importance[k]);
        }
    }
    out
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
    // A team hit has no personal memory to count an access for.
    seqs.retain(|s| *s != TEAM_SEQ);
    seqs
}

/// Most duplicates one merge request may name.
const MAX_MERGE_OTHERS: usize = 50;

/// Longest chain of merges `get` follows.
const MAX_MERGE_HOPS: usize = 8;

/// Where a trashed duplicate leads.
struct Followed {
    /// The last memory reached: the live survivor, or where the walk stopped.
    last: alice_core::model::Memory,
    /// One entry per step, from the duplicate on.
    hops: Vec<Redirect>,
    /// The walk stopped at `MAX_MERGE_HOPS` with more links ahead.
    truncated: bool,
}

/// Follow `duplicate_of` links from the trashed memory `m` (A merged into B, B into C...) to the
/// last memory reached; `None` when `m` is not trashed or has no such link.
fn follow_merges(c: &Connection, m: &alice_core::model::Memory) -> Result<Option<Followed>> {
    use alice_core::model::Status;
    if m.status != Status::Trashed {
        return Ok(None);
    }
    let next_of = |id: &str| -> Result<Option<String>> {
        Ok(c.prepare_cached(
            "SELECT to_id FROM links WHERE from_id = ?1 AND type = 'duplicate_of' ORDER BY created_at DESC, to_id LIMIT 1",
        )?
        .query_row([id], |r| r.get(0))
        .optional()?)
    };
    let mut visited = vec![m.id.clone()];
    let mut hops = Vec::new();
    let mut last = m.clone();
    let mut truncated = false;
    while last.status == Status::Trashed {
        let Some(next) = next_of(&last.id)?.filter(|n| !visited.contains(n)) else {
            break;
        };
        if hops.len() == MAX_MERGE_HOPS {
            truncated = true;
            break;
        }
        let Some(target) = memories::get_many(c, std::slice::from_ref(&next))?.pop() else {
            break;
        };
        hops.push(Redirect {
            from: last.id.clone(),
            to: target.id.clone(),
        });
        visited.push(next);
        last = target;
    }
    Ok((!hops.is_empty()).then_some(Followed {
        last,
        hops,
        truncated,
    }))
}

/// The memory `id` (or legacy id) with the `(content_hash, updated_at)` stamp a guarded write (a
/// move) is checked against. `None`: no memory has this id.
///
/// The stamp is read before the text, as the judge worker reads its items: two autocommit reads
/// are two snapshots, and an edit landing between them must leave the writer a stamp older than
/// the text (it refuses the move), never the edit's stamp with the text from before it (the move
/// would put that old text back over the edit).
fn stamped_memory(
    c: &Connection,
    id: &str,
) -> Result<Option<(alice_core::model::Memory, String, String)>> {
    let Some((own, hash, at)) = c
        .query_row(
            "SELECT id, content_hash, updated_at FROM memories WHERE id = ?1 OR legacy_id = ?1 ORDER BY seq LIMIT 1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    #[cfg(test)]
    tests::between_stamp_and_text(&own);
    let Some(m) = memories::get_many(c, &[own])?.pop() else {
        return Ok(None);
    };
    Ok(Some((m, hash, at)))
}

/// What the privacy rules say about the text a review resolution writes, before it is sent to
/// the writer (which does not scan content). `Predicate`/`map`: the two names, redacted (or
/// refused in `block` mode). `Project`/`move`: the memory arrives in a project whose rules may
/// be stricter; in `block` mode a text those rules would change is refused, in `redact` mode it
/// is re-redacted with the destination's rules and stored with the move. Anything else:
/// nothing to screen.
fn screen_resolution(
    c: &Connection,
    redactors: &Redactors,
    id: &str,
    choice: &str,
) -> Result<Option<ResolveText>> {
    use alice_core::store::reviews::{ReviewStatus, ReviewType};
    let Some(item) = reviews::get(c, id)?.filter(|i| i.status == ReviewStatus::Open) else {
        return Ok(None); // the writer says: not found, or already resolved
    };
    let text = |k: &str| {
        item.payload
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    match (item.review_type, choice) {
        (ReviewType::Predicate, "map") => {
            let r = redactors.for_root(None);
            Ok(Some(ResolveText::Predicate(
                redact_field(&r, "predicate", &text("predicate"))?,
                redact_field(&r, "predicate", &text("canonical"))?,
            )))
        }
        (ReviewType::Project, "move") => {
            let Some(target) = projects::resolve_slug(c, &text("to"))? else {
                return Ok(None); // the writer says the project is gone
            };
            // The hash and time of the memory screened below: the writer refuses the move if
            // either changed.
            let Some((m, content_hash, updated_at)) = stamped_memory(c, &text("memory_id"))? else {
                return Ok(None);
            };
            let tree = projects::tree_of(c, &target.id)?;
            let r = redactors.for_root(tree.as_ref());
            let apply = |field: &str, t: &str| -> Result<(String, bool)> {
                let red = r.apply(t);
                if red.found.is_empty() {
                    return Ok((t.to_string(), false));
                }
                if r.blocks() {
                    return Err(AliceError::invalid(format!(
                        "cannot move memory {}: the privacy rules of project {} (block mode) refuse its {field}, which contains what looks like a secret ({}); keep it where it is",
                        m.id,
                        target.slug,
                        red.found.join(", ")
                    )));
                }
                tracing::info!(
                    field,
                    "redacted a moved memory with the destination's rules"
                );
                Ok((red.text, true))
            };
            let (title, _) = apply("title", &m.title)?;
            let (body, _) = apply("body", &m.body)?;
            let mut topics = Vec::new();
            for t in &m.topics {
                let (t, _) = apply("topic", t)?;
                topics.push(t);
            }
            // Always sent, changed or not: it carries the hash that makes the move refuse a
            // memory edited after it was screened.
            Ok(Some(ResolveText::Moved {
                content_hash,
                updated_at,
                title,
                body,
                topics,
            }))
        }
        _ => Ok(None),
    }
}

/// The prefix of an id that lives in the team store.
const TEAM_PREFIX: &str = "team:";

/// What a person is told when a shared write fails. The team store's own answer (its redaction
/// rules' refusal, an invalid request, a missing memory) is passed on as it is; no answer, or a
/// broken one, after the request went out means it may have been saved; only a store that could
/// not be reached means nothing was.
fn shared_write_error(e: AliceError) -> AliceError {
    let text = e.to_string();
    match e {
        e if alice_core::redact::is_refusal(&e) => e,
        e @ (AliceError::Invalid(_) | AliceError::NotFound(_)) => e,
        _ if text.contains("did not answer in time")
            || text.contains("broke off its answer")
            || text.contains("sent an answer this daemon cannot read") =>
        {
            AliceError::Other(format!(
                "the team memory did not confirm the write ({text}); it may have been saved: recall it before saving it again"
            ))
        }
        _ => AliceError::Other(format!(
            "the team memory is unreachable ({text}); nothing was saved. Retry, or save it personally without shared"
        )),
    }
}
/// The team store's total time to answer a recall or a context, the batch `get` for authors
/// included (spec §2.10).
const TEAM_DEADLINE: Duration = Duration::from_millis(800);
/// The budget a recall asks of the team store; the person's own `max_tokens` packs the merged answer.
const TEAM_RECALL_TOKENS: usize = 4_000;
/// The answer when the team store did not answer within [`TEAM_DEADLINE`].
const NOTE_TEAM_LATE: &str = "the team memory did not answer in time; this answer is personal only";
/// The place of a team hit in `SearchOutput::memory_seqs`: it has no personal sequence number,
/// so the parallel arrays stay one entry per knowledge hit, and no access is counted for it.
const TEAM_SEQ: i64 = -1;
/// The importance a team hit has for the judge (the team's own is not in a hit).
const TEAM_IMPORTANCE: u8 = 3;

/// The slug of the project a read or write resolved to (`global` when nothing named one).
fn choice_slug(c: &ProjectChoice) -> String {
    match c {
        ProjectChoice::Slug(s) => s.clone(),
        ProjectChoice::Detected(r) => r.slug.clone(),
        ProjectChoice::Fallback => "global".into(),
    }
}

/// What the team store answers to a recall, as this tenant shows it: `team:` ids, `source` and
/// the `author` (one batch `get`), all inside [`TEAM_DEADLINE`]. A project the team store does
/// not know (a refusal, not a failure) is an empty answer.
async fn team_recall(team: &crate::team::TeamLink, body: RecallRequest) -> Result<Vec<Hit>> {
    let asked = tokio::time::timeout(TEAM_DEADLINE, async {
        let found: RecallResponse = team
            .call(
                reqwest::Method::POST,
                "/api/v1/recall",
                Some(&body),
                TEAM_DEADLINE,
            )
            .await?;
        let mut hits = found.results;
        hits.retain(|h| h.tier == Tier::Knowledge);
        if hits.is_empty() {
            return Ok(hits);
        }
        let got: GetResponse = team
            .call(
                reqwest::Method::POST,
                "/api/v1/get",
                Some(&GetRequest {
                    ids: hits.iter().map(|h| h.id.clone()).collect(),
                    include_provenance: false,
                }),
                TEAM_DEADLINE,
            )
            .await?;
        for h in &mut hits {
            h.author = got
                .memories
                .iter()
                .find(|m| m.memory.id == h.id)
                .and_then(|m| author_tenant(&m.memory.author))
                .map(str::to_string);
            h.id = format!("{TEAM_PREFIX}{}", h.id);
            h.source = Some("team".into());
        }
        Ok(hits)
    })
    .await
    .unwrap_or_else(|_| {
        Err(AliceError::Other(
            "the team store did not answer in time".into(),
        ))
    });
    match asked {
        Err(AliceError::Invalid(_)) => Ok(Vec::new()),
        other => other,
    }
}

/// Takes the team store's `cloud_ok` mark off every team hit whose project the person's own
/// cloud policy does not allow (the person's `privacy.cloud_denied_projects`, or a degraded
/// configuration): a team hit reaches the person's judge only when the team store and the
/// person both allow its project. The policy is asked once per distinct project slug, with no
/// project root, so it reads only the settings snapshot.
fn withhold_from_judge(settings: &SettingsHandle, hits: &mut [Hit]) {
    let mut asked: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for h in hits.iter_mut().filter(|h| h.cloud_ok == Some(true)) {
        let allowed = *asked
            .entry(h.project.clone())
            .or_insert_with(|| settings.cloud_allowed(&h.project, None));
        if !allowed {
            h.cloud_ok = Some(false);
        }
    }
}

/// The hash two hits are the same memory by: title and snippet (R36).
fn hit_hash(h: &Hit) -> String {
    alice_core::normalize::content_hash("", Kind::Note, &h.title, &h.snippet)
}

/// Adds the team's hits to the personal search `out`, dropping the ones a personal hit equals.
/// Fast mode (`precise == false`): all join and the list is ordered by score. Precise mode: the
/// ones the team store marked `cloud_ok` join (their ids are returned) so the judge can grade
/// them with the rest, and the others are returned to follow the graded hits. The team's mark
/// is not part of the answer, so it is cleared here.
fn merge_team(
    out: &mut SearchOutput,
    team: Vec<Hit>,
    precise: bool,
) -> (std::collections::HashSet<String>, Vec<Hit>) {
    let personal: std::collections::HashSet<String> = out.hits.iter().map(hit_hash).collect();
    let mut ok = std::collections::HashSet::new();
    let (mut joined, mut rest) = (Vec::new(), Vec::new());
    for mut h in team {
        if personal.contains(&hit_hash(&h)) {
            continue;
        }
        let gradable = h.cloud_ok == Some(true);
        h.cloud_ok = None;
        if precise && !gradable {
            rest.push(h);
            continue;
        }
        if precise {
            ok.insert(h.id.clone());
        }
        joined.push(h);
    }
    rest.sort_by(|a, b| b.score.total_cmp(&a.score));
    if !joined.is_empty() {
        let hits = std::mem::take(&mut out.hits);
        let (seqs, importance) = (
            std::mem::take(&mut out.memory_seqs),
            std::mem::take(&mut out.memory_importance),
        );
        let mut knowledge = 0;
        let mut entries: Vec<(Hit, Option<(i64, u8)>)> = hits
            .into_iter()
            .map(|h| {
                let memory = (h.tier == Tier::Knowledge).then(|| {
                    knowledge += 1;
                    (seqs[knowledge - 1], importance[knowledge - 1])
                });
                (h, memory)
            })
            .collect();
        entries.extend(
            joined
                .into_iter()
                .map(|h| (h, Some((TEAM_SEQ, TEAM_IMPORTANCE)))),
        );
        entries.sort_by(|a, b| b.0.score.total_cmp(&a.0.score));
        for (h, memory) in entries {
            out.hits.push(h);
            if let Some((seq, imp)) = memory {
                out.memory_seqs.push(seq);
                out.memory_importance.push(imp);
            }
        }
    }
    (ok, rest)
}

/// Puts the team hits the judge did not see after the graded results, up to `limit`.
fn append_team(out: &mut SearchOutput, rest: Vec<Hit>, limit: usize) {
    for h in rest {
        if out.hits.len() >= limit {
            break;
        }
        out.hits.push(h);
        out.memory_seqs.push(TEAM_SEQ);
        out.memory_importance.push(TEAM_IMPORTANCE);
    }
}
/// A call to the team store for one shared write, read or change.
const SHARED_TIMEOUT: Duration = Duration::from_secs(5);
/// How many pages of 500 events `undo` of a shared change looks through for the event.
const SHARED_UNDO_PAGES: usize = 20;
/// The refusal for a shared memory (or event) that this tenant did not write.
const SHARED_REFUSAL: &str = "only the author or the admin can change a shared memory";

/// The tenant part of a shared memory's author: the text before the first `/`.
fn author_tenant(a: &Author) -> Option<&str> {
    a.agent
        .as_deref()
        .and_then(|s| s.split('/').next())
        .filter(|t| !t.is_empty())
}

/// A memory of the team store as this tenant shows it: `team:` ids, its source and author.
fn into_team_view(mut m: MemoryFull) -> MemoryFull {
    m.source = Some("team".into());
    m.author_name = author_tenant(&m.memory.author).map(str::to_string);
    m.memory.id = format!("{TEAM_PREFIX}{}", m.memory.id);
    for l in &mut m.links {
        l.id = format!("{TEAM_PREFIX}{}", l.id);
    }
    m
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
        event_id: None,
        source: None,
        author_name: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::embed::FakeEmbedder;
    use alice_core::model::Kind;
    use alice_core::store::memories::{MemoryUpdate, UpdateAction};
    use alice_core::store::sources::{ChunkInput, SourceInput};

    /// Options whose secret status ignores the machine's environment (a developer's own
    /// `TYPESAFE_API_KEY` must not change what a test sees).
    fn quiet() -> ServiceOptions {
        ServiceOptions {
            secret_env: Some(Arc::new(|_| None)),
            // No `ALICE_*` variable of the developer's shell reaches the settings.
            settings_env: Some(Vec::new()),
            ..Default::default()
        }
    }

    async fn svc() -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            quiet(),
        )
        .unwrap();
        (d, s)
    }

    /// Until the embedding and judge stages have finished every memory.
    async fn wait_settled(s: &Service) {
        for _ in 0..400 {
            let h = s.health().await.unwrap();
            if h.embed_backlog == 0 && h.judge_backlog == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("background work did not settle");
    }

    fn repo(d: &tempfile::TempDir, name: &str) -> Option<String> {
        let repo = d.path().join(name);
        std::fs::create_dir(&repo).unwrap();
        Some(repo.to_string_lossy().into_owned())
    }

    type Hooks = parking_lot::Mutex<HashMap<String, Box<dyn FnMut() + Send>>>;

    /// Run by [`stamped_memory`] between its two reads of the memory with that id (tests only).
    /// Keyed by id: the reads run on the reader pool's threads, beside other tests.
    static BETWEEN_STAMP_AND_TEXT: std::sync::LazyLock<Hooks> =
        std::sync::LazyLock::new(Default::default);

    pub(super) fn between_stamp_and_text(id: &str) {
        if let Some(f) = BETWEEN_STAMP_AND_TEXT.lock().get_mut(id) {
            f()
        }
    }

    /// From now on (until its entry is removed), another connection to `db` edits memory `id`
    /// between the two reads of [`stamped_memory`].
    fn edit_between_the_reads(db: &std::path::Path, id: &str) {
        let other = rusqlite::Connection::open(db).unwrap();
        let edit_id = id.to_string();
        let edit = move || {
            other
                .execute(
                    "UPDATE memories SET body = 'the deploy runs on mondays', content_hash = 'edited', updated_at = '2999-01-01T00:00:00Z' WHERE id = ?1",
                    [&edit_id],
                )
                .unwrap();
        };
        BETWEEN_STAMP_AND_TEXT
            .lock()
            .insert(id.to_string(), Box::new(edit));
    }

    /// A move is screened from the memory's text and checked by the writer against the stamp
    /// read with it. An edit that lands between the two reads must leave the writer a stamp
    /// older than the text (it then refuses the move), never the edit's stamp with the text from
    /// before it (the move would then put the old text back over the edit). `screen_move` and
    /// the review move both read through `stamped_memory`.
    #[test]
    fn an_edit_between_the_reads_of_a_move_leaves_a_stamp_the_writer_refuses() {
        use alice_core::project::ProjectRef;
        use alice_core::store::memories::{InsertOutcome, NewMemoryInput};
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("a.db");
        let c = alice_core::store::open_writer(&db).unwrap();
        let p = projects::upsert_project(
            &c,
            &ProjectRef {
                key: "k/app".into(),
                slug: "app".into(),
                name: "app".into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
        let m = memories::prepare(
            NewMemoryInput {
                content: "the deploy runs on fridays".into(),
                kind: Some(Kind::Fact),
                ..Default::default()
            },
            &p,
            "0000000001000-0000-dev".into(),
        )
        .unwrap();
        let InsertOutcome::New { id, .. } = memories::insert(&c, &m).unwrap() else {
            panic!("duplicate")
        };
        let stamp = |c: &Connection| -> (String, String) {
            c.query_row(
                "SELECT content_hash, updated_at FROM memories WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let before = stamp(&c);
        // Another writer edits the memory while the move reads it.
        edit_between_the_reads(&db, &id);
        let (read, hash, at) = stamped_memory(&c, &id).unwrap().unwrap();
        BETWEEN_STAMP_AND_TEXT.lock().remove(&id);
        assert_ne!(stamp(&c), before, "the edit landed");
        assert_eq!(
            (hash, at),
            before,
            "the stamp is the one from before the edit, so the writer refuses the move"
        );
        assert!(
            read.body.contains("mondays"),
            "the text read is the edited one"
        );
    }

    /// Final re-review 2, M-6: the same through `update`, which pins `screen_move` to
    /// [`stamped_memory`]: the writer refuses the move, and the edit stays where it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_screened_across_an_edit_is_refused_and_the_edit_stays() {
        let (d, s) = svc().await;
        let mut ids = Vec::new();
        for (content, dir) in [("the deploy runs on fridays", "a"), ("a note in b", "b")] {
            let (content, cwd) = (content.into(), repo(&d, dir));
            let r = s.remember(RememberRequest {
                content,
                cwd,
                ..Default::default()
            });
            ids.push(r.await.unwrap().id.unwrap());
        }
        wait_settled(&s).await;
        let id = ids.swap_remove(0);
        edit_between_the_reads(&s.home().db(), &id);
        let (action, project) = (UpdateAction::Move, Some("b".into()));
        let update = MemoryUpdate {
            action,
            project,
            ..Default::default()
        };
        let moved = s.update(UpdateRequest {
            id: id.clone(),
            update,
            ..Default::default()
        });
        let moved = moved.await;
        BETWEEN_STAMP_AND_TEXT.lock().remove(&id);
        let e = moved.expect_err("the writer refuses the move").to_string();
        assert!(e.contains("was edited while its move"), "{e}");
        let got = s.get(GetRequest {
            ids: vec![id],
            include_provenance: false,
        });
        let m = got.await.unwrap().memories.remove(0).memory;
        assert_eq!(
            (m.project.as_str(), m.body.as_str()),
            ("a", "the deploy runs on mondays")
        );
    }

    /// A `remember` queued because the database is read-only holds no secret, and in `block`
    /// mode a request with one is refused rather than queued.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_only_spool_holds_the_redacted_request_and_block_mode_refuses() {
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        s.force_read_only("disk full");
        let req = |content: String| RememberRequest {
            content,
            cwd: cwd.clone(),
            ..Default::default()
        };
        let r = s
            .remember_or_spool(req(format!("deploy with {token}")))
            .await
            .unwrap();
        assert_eq!(r.status, RememberStatus::Spooled);
        let spooled = |s: &Service| -> String {
            std::fs::read_dir(s.home().spool_dir())
                .unwrap()
                .flatten()
                .map(|e| std::fs::read_to_string(e.path()).unwrap())
                .collect()
        };
        let text = spooled(&s);
        assert!(
            text.contains("[REDACTED:github_token]") && !text.contains(token),
            "{text}"
        );
        std::fs::write(
            std::path::Path::new(cwd.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\nblock_secrets = true\n",
        )
        .unwrap();
        let err = s
            .remember_or_spool(req(format!("again {token}")))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("github_token") && !err.contains(token),
            "{err}"
        );
        assert_eq!(
            spooled(&s).matches("deploy with").count(),
            1,
            "only the first is queued"
        );
    }

    /// `ServiceOptions::default()` sees nothing of the process: not an `ALICE_*` setting, not a
    /// secret variable. Checked in a child process that has the variable (set here, it would
    /// race the other tests' reads of the environment).
    #[test]
    fn a_default_service_ignores_the_environment_of_its_process() {
        const CHILD: &str = "ALICE_TEST_ENV_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let d = tempfile::tempdir().unwrap();
            let s = Service::start(
                AliceHome::new(d.path().to_path_buf()),
                &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
                ServiceOptions::default(),
            )
            .unwrap();
            assert_eq!(
                s.settings().search.default_max_tokens,
                Settings::default().search.default_max_tokens,
                "ALICE_SEARCH_DEFAULT_MAX_TOKENS must not reach a default-built service"
            );
            assert!(s.settings_handle().warnings().is_empty());
            let see = ServiceOptions::default().secret_env.unwrap();
            assert_eq!(see("TYPESAFE_API_KEY"), None);
            assert_eq!(see("PATH"), None);
            // The real entry points ask for the process's.
            assert!(ServiceOptions::process_env().settings_env.is_none());
            assert!(ServiceOptions::process_env().secret_env.is_none());
            s.shutdown();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service::tests::a_default_service_ignores_the_environment_of_its_process",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("ALICE_SEARCH_DEFAULT_MAX_TOKENS", "777")
            .env("ALICE_NOT_A_SETTING_AT_ALL", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `alice history --actor codex` and `alice history drawer_7` mean what they say: a bare
    /// agent name is that agent, and a legacy id finds the memory it names.
    #[tokio::test(flavor = "multi_thread")]
    async fn history_takes_bare_agent_names_and_legacy_ids() {
        let (_d, s) = svc().await;
        let id = match s
            .writer
            .submit(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "imported from the palace".into(),
                    legacy_id: Some("drawer_7".into()),
                    author: alice_core::model::Author {
                        agent: Some("codex".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .await
            .unwrap()
        {
            WriteOutcome::Remembered(InsertOutcome::New { id, .. }, _) => id,
            o => panic!("{o:?}"),
        };
        let q = |target: Option<&str>, actor: Option<&str>| EventsRequest {
            target_id: target.map(Into::into),
            actor: actor.map(Into::into),
            ..Default::default()
        };
        let by_legacy = s.events(q(Some("drawer_7"), None)).await.unwrap();
        assert_eq!(by_legacy.len(), 1);
        assert_eq!(by_legacy[0].target_id, id);
        for actor in ["codex", "agent:codex", " codex "] {
            assert_eq!(
                s.events(q(None, Some(actor))).await.unwrap().len(),
                1,
                "{actor:?}"
            );
        }
        assert!(s.events(q(None, Some("user"))).await.unwrap().is_empty());
        assert_eq!(
            s.events(q(None, Some(""))).await.unwrap().len(),
            1,
            "blank is no filter"
        );
    }

    /// A remember queued while the daemon was away is recorded as its writer's, not the
    /// daemon's, when the spool is replayed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_spooled_remember_is_recorded_as_its_original_agent() {
        let (_d, s) = svc().await;
        spool::append(
            &s.home.spool_dir(),
            &spool::SpoolEntry::Remember(RememberRequest {
                content: "written while the daemon was down".into(),
                author: alice_core::model::Author {
                    agent: Some("codex".into()),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .unwrap();
        let r = spool::drain(&s.home.spool_dir(), &s).await;
        assert_eq!(r.processed, 1);
        let evs = s.events(EventsRequest::default()).await.unwrap();
        assert_eq!(
            (evs[0].action.as_str(), evs[0].actor.as_str()),
            ("memory.create", "agent:codex")
        );
    }

    /// Every event-stream client shares one health reading per tick: the counts are read once,
    /// not once per connected client. A reading counts as fresh for half a tick. No sleeping:
    /// the tick is long and the cached reading is aged by hand.
    #[tokio::test(flavor = "multi_thread")]
    async fn stream_health_is_read_once_per_tick_for_all_clients() {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                health_tick: Duration::from_secs(600),
                ..quiet()
            },
        )
        .unwrap();
        let (a, b) = tokio::join!(s.stream_health(), s.stream_health());
        assert_eq!(a.unwrap().memories, 0);
        assert_eq!(b.unwrap().memories, 0);
        assert_eq!(s.health_reads(), 1, "two clients, one reading");
        s.remember(RememberRequest {
            content: "counted on the next tick".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(
            s.stream_health().await.unwrap().memories,
            0,
            "the reading of this tick is shared"
        );
        assert_eq!(s.health_reads(), 1);
        // Younger than half a tick: still fresh. Older: read again.
        s.age_stream_health(Duration::from_secs(290)).await;
        assert_eq!(
            s.stream_health().await.unwrap().memories,
            0,
            "290 s < 300 s"
        );
        s.age_stream_health(Duration::from_secs(20)).await;
        assert_eq!(
            s.stream_health().await.unwrap().memories,
            1,
            "310 s > 300 s"
        );
        assert_eq!(s.health_reads(), 2);
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
        wait_settled(&s).await;
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
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(edited.memory.body.contains("readers never block"));
        wait_settled(&s).await;
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
        wait_settled(&s).await;
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
            wait_settled(&s).await;
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

    /// Fix round 2: a raw-tier recall holds one main-pool reader and reads the chunk vectors on
    /// a connection of the separate vector pool. With every main reader held by such recalls
    /// and more waiting, none waits for a second main reader, so they all finish.
    #[test]
    fn raw_tier_recalls_never_deadlock_on_the_reader_pools() {
        // Its own runtime: a deadlocked recall never returns, and dropping a runtime waits for
        // its blocking threads, so a regression must abandon them to fail instead of hanging.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let finished = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(30),
                sixteen_raw_recalls_on_two_readers(),
            )
            .await
        });
        rt.shutdown_background();
        assert_eq!(
            finished.expect("raw-tier recalls deadlocked on the reader pools"),
            16
        );
    }

    async fn sixteen_raw_recalls_on_two_readers() -> usize {
        let d = tempfile::tempdir().unwrap();
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &mk,
            ServiceOptions {
                reader_pool: 2,
                vector_reader_pool: 1,
                ..Default::default()
            },
        )
        .unwrap();
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            chunks: (0..40)
                .map(|i| ChunkInput {
                    ord: i,
                    text: format!("session chunk {i} about the frobnicator calibration"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_settled(&s).await;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..(2 + 14) {
            let s = s.clone();
            set.spawn(async move {
                s.recall(RecallRequest {
                    query: format!("frobnicator calibration {i}"),
                    include_raw: true,
                    format: OutputFormat::Json,
                    ..Default::default()
                })
                .await
            });
        }
        let mut done = 0;
        while let Some(r) = set.join_next().await {
            let out = r.unwrap().unwrap();
            assert!(
                out.results
                    .iter()
                    .any(|h| h.tier == Tier::Raw && h.matched.contains(&MatchVia::Vector)),
                "the chunk vector list ran: {:?}",
                out.results
            );
            done += 1;
        }
        tokio::task::spawn_blocking(move || s.shutdown())
            .await
            .unwrap();
        done
    }

    /// Task 14: backups, the job list and an on-demand job through the service.
    #[tokio::test(flavor = "multi_thread")]
    async fn backups_and_maintenance_jobs_run_through_the_service() {
        use crate::maintenance::JobOutcome;
        let (d, s) = svc().await;
        s.remember(RememberRequest {
            content: "a memory the backup must hold".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(s.backups().await.unwrap().is_empty());
        let made = s.backup_now().await.unwrap();
        assert_eq!(made.kind, BackupKind::Manual);
        assert!(made.path.starts_with(d.path().join("backups")) && made.path.exists());
        let listed = s.backups().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, made.name);

        let status = s.maintenance_status().await.unwrap();
        assert_eq!(
            status.iter().map(|j| j.job.as_str()).collect::<Vec<_>>(),
            Job::names()
        );
        assert!(
            status
                .iter()
                .all(|j| j.last_run.is_none() && j.last_error.is_none())
        );
        let report = s.run_job(Job::TrashPurge).await.unwrap();
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        let purge = s
            .maintenance_status()
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.job == "trash_purge")
            .unwrap();
        assert!(purge.last_run.is_some() && purge.last_ok.is_some());
        assert_eq!(purge.every_secs, 86_400);
        // The on-demand backup job rotates and keeps the manual one.
        let report = s.run_job(Job::Backup).await.unwrap();
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        // The manual backup has its own name: both stay.
        let kept = s.backups().await.unwrap();
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|b| b.path.exists()));
        tokio::task::spawn_blocking(move || s.shutdown())
            .await
            .unwrap();
    }

    /// The timer: with the shortest quiet period (5 s), nothing runs while something holds the
    /// daemon busy, and once it is quiet the due jobs run on their own (a backup among them).
    #[tokio::test(flavor = "multi_thread")]
    async fn due_jobs_run_by_themselves_once_the_daemon_is_quiet() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(home.config_file(), "[maintenance]\nidle_secs = 5\n").unwrap();
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let s = Service::start(home.clone(), &mk, quiet()).unwrap();
        let ran = |s: &Arc<Service>| {
            let s = s.clone();
            async move {
                s.maintenance_status()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|j| j.last_run.is_some())
                    .count()
            }
        };
        {
            let _busy = s.activity().enter();
            tokio::time::sleep(Duration::from_millis(6_500)).await;
            assert_eq!(ran(&s).await, 0, "nothing runs under a held guard");
        }
        let mut waited = 0;
        while ran(&s).await < Job::ALL.len() && waited < 400 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += 1;
        }
        let status = s.maintenance_status().await.unwrap();
        assert!(
            status.iter().all(|j| j.last_run.is_some()),
            "every due job ran: {status:?}"
        );
        assert!(status.iter().all(|j| j.last_error.is_none()), "{status:?}");
        assert_eq!(s.backups().await.unwrap().len(), 1, "the scheduled backup");
        tokio::task::spawn_blocking(move || s.shutdown())
            .await
            .unwrap();
    }

    /// M-6: backups that are behind show in the health (and so in `alice status`).
    #[tokio::test(flavor = "multi_thread")]
    async fn overdue_backups_are_a_health_warning() {
        let (d, s) = svc().await;
        assert!(s.health().await.unwrap().maintenance_warning.is_none());
        std::fs::create_dir_all(d.path().join("backups")).unwrap();
        let old = chrono::Utc::now() - chrono::Duration::days(5);
        std::fs::write(
            d.path()
                .join("backups")
                .join(format!("alice-{}.db", old.format("%Y%m%dT%H%M%SZ"))),
            b"x",
        )
        .unwrap();
        let w = s
            .health()
            .await
            .unwrap()
            .maintenance_warning
            .expect("warning");
        assert!(w.contains("5 days old"), "{w}");
        s.backup_now().await.unwrap();
        assert!(s.health().await.unwrap().maintenance_warning.is_none());
        tokio::task::spawn_blocking(move || s.shutdown())
            .await
            .unwrap();
    }

    /// Requests are what maintenance yields to: each public request restarts the quiet period,
    /// and a held guard (a long import) keeps the daemon busy.
    #[tokio::test(flavor = "multi_thread")]
    async fn requests_and_held_guards_keep_the_daemon_busy() {
        let (_d, s) = svc().await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(s.activity().idle_for() >= Duration::from_millis(300));
        s.projects().await.unwrap();
        assert!(
            s.activity().idle_for() < Duration::from_millis(300),
            "a request restarts the quiet period: {:?}",
            s.activity().idle_for()
        );
        {
            let _import = s.activity().enter();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(s.activity().idle_for(), Duration::ZERO);
        }
        assert!(s.activity().idle_for() < Duration::from_millis(300));
        // Polling the health (the UI's stream does, every few seconds) is not activity.
        tokio::time::sleep(Duration::from_millis(400)).await;
        s.health().await.unwrap();
        assert!(s.activity().idle_for() >= Duration::from_millis(300));
        tokio::task::spawn_blocking(move || s.shutdown())
            .await
            .unwrap();
    }

    /// B4: after a crash the saved index files lack the vectors stored since; the next start
    /// adds exactly those (and drops what a purge removed) instead of rebuilding.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_after_a_crash_catches_the_index_up_by_content() {
        use alice_core::store::embeddings::VecKey;
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let stop = |s: Arc<Service>| tokio::task::spawn_blocking(move || s.shutdown());
        let s = Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap();
        let mut ids = Vec::new();
        for t in [
            "first kept memory",
            "second kept memory",
            "third purged memory",
        ] {
            ids.push(
                s.remember(RememberRequest {
                    content: t.into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .id
                .unwrap(),
            );
        }
        wait_settled(&s).await;
        stop(s.clone()).await.unwrap();
        drop(s);
        // What happened after the last save, and a crash: one more vector was stored, one
        // memory was purged. The files still hold the purged one and lack the new one.
        let (late, purged) = {
            let c = open_writer(&home.db()).unwrap();
            let seq = |id: &str| memories::seq_of(&c, id).unwrap().unwrap();
            let purged = seq(&ids[2]);
            c.execute(
                "DELETE FROM embeddings WHERE owner_type = 'memory' AND owner_seq = ?1",
                [purged],
            )
            .unwrap();
            c.execute("DELETE FROM memories WHERE seq = ?1", [purged])
                .unwrap();
            c.execute(
                "INSERT INTO memories (id, project_id, kind, title, body, status, content_hash, created_at, updated_at, hlc, embed_state, judge_state)
                 VALUES ('LATE', '00000000000000000000GLOBAL', 'note', 'late', 'late', 'active', 'late', '2026-10-01T00:00:00.000Z', '2026-10-01T00:00:00.000Z', 'h', 'done', 'done')",
                [],
            )
            .unwrap();
            let late = c.last_insert_rowid();
            let v = FakeEmbedder::new(384)
                .embed_passages(&["late".to_string()])
                .unwrap()
                .remove(0);
            embeddings::put(&c, VecKey::Memory(late), "fake", &v).unwrap();
            (late, purged)
        };
        let mark = home.vector_file("fake").with_extension("clean");
        std::fs::remove_file(&mark).unwrap();
        let s = Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap();
        for _ in 0..200 {
            if s.health().await.unwrap().index_ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let idx = s.index.read().clone().expect("index ready");
        assert!(
            idx.contains(VecKey::Memory(late)),
            "the late vector was added"
        );
        assert!(
            !idx.contains(VecKey::Memory(purged)),
            "the purged one was dropped"
        );
        assert_eq!(idx.len(), 3, "two kept and the late one");
        stop(s.clone()).await.unwrap();
    }

    /// Fix round 1 (M9): a clean stop marks the files it saved and the next start uses them as
    /// they are; a start without the mark (after a crash) catches the chunk part up from the
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
        wait_settled(&s).await;
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

    /// A stale index file is opened after an unclean stop, its rebuild fails, and the daemon
    /// runs on an empty index that was never written. A clean stop must then save that index
    /// before it marks the files clean: otherwise the mark vouches for the stale files, and the
    /// next start trusts them whenever their count happens to match the table.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_rebuild_never_leaves_stale_files_marked_clean() {
        use alice_core::bq::BinaryIndex;
        use alice_core::store::embeddings::VecKey;
        use alice_core::vector::KeyFilter;
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let stop = |s: Arc<Service>| tokio::task::spawn_blocking(move || s.shutdown());
        let ready = |s: Arc<Service>| async move {
            for _ in 0..200 {
                if s.health().await.unwrap().index_ready {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the index never became ready");
        };
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
        wait_settled(&s).await;
        stop(s.clone()).await.unwrap();
        drop(s);
        let base = home.vector_file("fake");
        let mark = base.with_extension("clean");
        let seqs: Vec<i64> = {
            let c = open_reader(&home.db()).unwrap();
            let mut stmt = c.prepare("SELECT seq FROM chunks ORDER BY seq").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        // A stale chunk file with the right count, left by a crash (no mark)...
        let e = FakeEmbedder::new(384);
        let mut stale = BinaryIndex::new(384);
        for seq in [seqs[0], seqs[1], 999] {
            stale
                .upsert(seq, &e.embed_query(&format!("x{seq}")).unwrap())
                .unwrap();
        }
        let f = std::fs::File::create(base.with_extension("chunks.bq")).unwrap();
        stale.write_to(std::io::BufWriter::new(f)).unwrap();
        std::fs::remove_file(&mark).unwrap();
        // ...and a stored vector the rebuild cannot read, so the rebuild fails.
        let bad = "INSERT INTO embeddings (owner_type, owner_seq, model, dim, vector) \
                   VALUES ('chunk', 424242, 'fake', 384, 'not a vector')";
        open_writer(&home.db()).unwrap().execute(bad, []).unwrap();

        let s = ready(Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap()).await;
        let in_memory = s.index.read().clone().unwrap().len();
        assert_eq!(in_memory, 0, "the failed rebuild left an empty index");
        stop(s.clone()).await.unwrap();
        drop(s);
        assert!(mark.exists(), "the stop was clean");
        let on_disk = VectorIndex::open(&base, "fake", 384).unwrap().len();
        assert_eq!(
            on_disk, in_memory,
            "the files marked clean hold the index that was in memory"
        );

        // With the unreadable row gone the counts agree with the stale file: only the saved
        // (empty) index makes the next start rebuild instead of trusting it.
        open_writer(&home.db())
            .unwrap()
            .execute("DELETE FROM embeddings WHERE owner_seq = 424242", [])
            .unwrap();
        let s = ready(Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap()).await;
        let q = FakeEmbedder::new(384).embed_query("chunk").unwrap();
        let idx = s.index.read().clone().unwrap();
        let mut found: Vec<i64> = idx
            .search(&q, 10, KeyFilter::Chunks)
            .unwrap()
            .into_iter()
            .map(|(k, _)| match k {
                VecKey::Chunk(s) | VecKey::Memory(s) => s,
            })
            .collect();
        found.sort();
        assert_eq!(
            found, seqs,
            "rebuilt from the stored vectors, not the stale file"
        );
        stop(s.clone()).await.unwrap();
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
        wait_settled(&s).await;
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
        wait_settled(&s).await;
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
        wait_settled(&s).await;
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

    // ------------------------------------------------------------------ Task 6: live settings

    use crate::config::SettingsHandle;
    use crate::secrets::{SecretName, SecretStore};
    use alice_core::settings::SecretValue;

    /// Events for `target`, polled until `ok` holds (at most 5 s): writes queued with
    /// `send_nowait` land a moment after the call that caused them returns.
    async fn wait_events(
        s: &Service,
        target: &str,
        ok: impl Fn(&[EventRow]) -> bool,
    ) -> Vec<EventRow> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ev = s
                .events(EventsRequest {
                    target_id: Some(target.into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            if ok(&ev) {
                return ev;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no matching event for {target}: {ev:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn patch_one(
        key: &str,
        value: serde_json::Value,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(key.into(), value);
        m
    }

    /// A secret store in memory. `delay_ms` makes every call slow, as a keychain that waits
    /// for a permission prompt is (and can be set back to 0 when the prompt is answered).
    struct MemSecrets {
        map: Mutex<HashMap<SecretName, String>>,
        delay_ms: std::sync::atomic::AtomicU64,
        /// Calls that reached the store.
        gets: std::sync::atomic::AtomicUsize,
        sets: std::sync::atomic::AtomicUsize,
        /// Reads fail as a locked keychain's would.
        fail_get: AtomicBool,
        /// The first read answers this instead of the stored value (a stale read).
        first_get: Mutex<Option<String>>,
        /// Reads by the judge build worker running now, and the most that ever ran at once.
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
        /// A request's read answers only once this is false (it has read the value by then);
        /// only as many reads as `hold_budget` allows wait, the rest answer at once.
        hold: AtomicBool,
        hold_budget: std::sync::atomic::AtomicUsize,
        /// Request reads waiting on `hold` right now.
        held: std::sync::atomic::AtomicUsize,
        /// A request's read panics, as a crashing keychain library would.
        panic_get: AtomicBool,
    }

    impl MemSecrets {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                map: Mutex::default(),
                delay_ms: std::sync::atomic::AtomicU64::new(delay.as_millis() as u64),
                gets: Default::default(),
                sets: Default::default(),
                fail_get: AtomicBool::new(false),
                first_get: Mutex::new(None),
                active: Default::default(),
                max_active: Default::default(),
                hold: AtomicBool::new(false),
                hold_budget: Default::default(),
                held: Default::default(),
                panic_get: AtomicBool::new(false),
            })
        }
        fn wait(&self) {
            let ms = self.delay_ms.load(Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(ms));
        }
    }

    impl SecretStore for MemSecrets {
        fn get(&self, name: SecretName) -> Result<Option<SecretValue>> {
            let first = self.gets.fetch_add(1, Ordering::SeqCst) == 0;
            // Only reads by the judge build worker count (status reads come from requests).
            let building = std::thread::current().name() == Some("alice-judge-build");
            if building {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(now, Ordering::SeqCst);
            }
            self.wait();
            if building {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            if !building && self.panic_get.load(Ordering::SeqCst) {
                panic!("the keychain library crashed");
            }
            let answer = if first && let Some(stale) = self.first_get.lock().take() {
                Ok(Some(SecretValue::new(stale)))
            } else if self.fail_get.load(Ordering::SeqCst) {
                Err(AliceError::Other("the keychain is locked".into()))
            } else {
                Ok(self.map.lock().get(&name).map(SecretValue::new))
            };
            // The value is read; a request's answer waits here (the judge worker never does).
            if !building
                && self.hold.load(Ordering::SeqCst)
                && self
                    .hold_budget
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |b| b.checked_sub(1))
                    .is_ok()
            {
                self.held.fetch_add(1, Ordering::SeqCst);
                while self.hold.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.held.fetch_sub(1, Ordering::SeqCst);
            }
            answer
        }
        fn set(&self, name: SecretName, value: &str) -> Result<()> {
            self.sets.fetch_add(1, Ordering::SeqCst);
            self.wait();
            self.map.lock().insert(name, value.trim().to_string());
            Ok(())
        }
        fn delete(&self, name: SecretName) -> Result<bool> {
            self.wait();
            Ok(self.map.lock().remove(&name).is_some())
        }
        fn backend(&self) -> &'static str {
            "keychain"
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_settings_apply_to_the_next_recall_without_a_restart() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        s.remember(RememberRequest {
            content: "lease timeouts in the hub writer".into(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_settled(&s).await;
        let q = || RecallRequest {
            query: "writer hub lease timeout problem".into(),
            cwd: cwd.clone(),
            format: OutputFormat::Json,
            ..Default::default()
        };
        let before = s.recall(q()).await.unwrap().results;
        assert!(
            !before.is_empty() && before.iter().all(|h| h.score > 0.0),
            "found, with a score, before the change: {before:?}"
        );
        let before = before.len();
        let out = s
            .patch_settings(patch_one(
                "search.weights.knowledge",
                serde_json::json!(0.0),
            ))
            .unwrap();
        assert_eq!(out.changed, vec!["search.weights.knowledge".to_string()]);
        let after = s.recall(q()).await.unwrap();
        assert!(
            !after.results.is_empty(),
            "the memory is still returned (weights scale scores; they do not filter)"
        );
        assert!(
            after.results.iter().all(|h| h.score == 0.0),
            "knowledge weight 0 zeroes knowledge scores: {before} → {:?}",
            after.results
        );
        // The settings event is written fire-and-forget by the config-events thread: wait for it.
        let ev = wait_events(&s, "config", |ev| {
            ev.iter().any(|e| e.action == "settings.change")
        })
        .await;
        assert!(
            ev[0]
                .after
                .as_ref()
                .unwrap()
                .to_string()
                .contains("search.weights.knowledge")
        );
    }

    /// Task 19: `search.rrf_k` and `search.boost_exponent` apply to the next recall like the
    /// other `search.*` keys. With the exponent 0 the score is the fused rank score alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn rrf_k_and_boost_exponent_apply_to_the_next_recall_without_a_restart() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        s.remember(RememberRequest {
            content: "lease timeouts in the hub writer".into(),
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_settled(&s).await;
        let top = |s: &Arc<Service>| {
            let s = s.clone();
            let req = RecallRequest {
                query: "writer hub lease timeout problem".into(),
                cwd: cwd.clone(),
                format: OutputFormat::Json,
                ..Default::default()
            };
            async move { s.recall(req).await.unwrap().results[0].score }
        };
        let base = top(&s).await;
        let out = s
            .patch_settings(patch_one("search.boost_exponent", serde_json::json!(0.0)))
            .unwrap();
        assert_eq!(out.changed, vec!["search.boost_exponent".to_string()]);
        let flat = top(&s).await;
        assert_ne!(flat, base, "the boosts no longer weigh: {base} -> {flat}");
        s.patch_settings(patch_one("search.rrf_k", serde_json::json!(1)))
            .unwrap();
        let sharp = top(&s).await;
        assert!(
            sharp > flat,
            "a smaller k gives the first place more: {flat} -> {sharp}"
        );
    }

    /// The token budget of a recall and the half-lives come from the settings too.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_default_token_budget_is_a_setting() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        for i in 0..5 {
            s.remember(RememberRequest {
                content: format!("budget memory {i}: {}", "lease timeout ".repeat(30)),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        wait_settled(&s).await;
        let q = || RecallRequest {
            query: "lease timeout".into(),
            cwd: cwd.clone(),
            format: OutputFormat::Json,
            ..Default::default()
        };
        let roomy = s.recall(q()).await.unwrap();
        assert_eq!(roomy.omitted, 0, "{roomy:?}");
        s.patch_settings(patch_one(
            "search.default_max_tokens",
            serde_json::json!(100),
        ))
        .unwrap();
        let tight = s.recall(q()).await.unwrap();
        assert!(
            tight.omitted > 0,
            "100 tokens hold fewer results: {tight:?}"
        );
        // A request that names its own budget is not changed by the setting.
        let own = s
            .recall(RecallRequest {
                max_tokens: Some(1500),
                ..q()
            })
            .await
            .unwrap();
        assert_eq!(own.omitted, 0);
    }

    /// An edit of config.toml applies to the running service by itself (the watcher), and the
    /// stop joins the watcher.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_edit_of_config_toml_applies_live_and_shutdown_stops_the_watcher() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let s = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                settings_poll: Duration::from_millis(50),
                ..quiet()
            },
        )
        .unwrap();
        std::fs::write(home.config_file(), "[search]\nrelevance_floor = 0.81\n").unwrap();
        for _ in 0..200 {
            if s.settings().search.relevance_floor == 0.81 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(s.settings().search.relevance_floor, 0.81);
        assert_eq!(
            s.settings_view().sources["search.relevance_floor"],
            alice_core::settings::Source::File
        );
        let started = Instant::now();
        s.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secrets_are_write_only_and_their_events_carry_no_value() {
        let (_d, s) = svc().await;
        let st = s
            .set_secret(
                SecretName::EmbeddingApiKey,
                concat!("sk-", "test-0123456789abcd").into(),
            )
            .unwrap();
        assert_eq!((st.set, st.hint.as_deref()), (true, Some("••••abcd")));
        let view = s.settings_view();
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("0123456789"), "{json}");
        assert!(view.secrets["embedding_api_key"].set);
        let ev = wait_events(&s, "embedding_api_key", |ev| {
            ev.iter().any(|e| e.action == "secret.set")
        })
        .await;
        assert_eq!(ev[0].action, "secret.set");
        assert_eq!(ev[0].actor, "user");
        assert!(!serde_json::to_string(&ev).unwrap().contains("0123456789"));
        assert!(s.delete_secret(SecretName::EmbeddingApiKey).unwrap());
        assert!(!s.settings_view().secrets["embedding_api_key"].set);
        let ev = wait_events(&s, "embedding_api_key", |ev| {
            ev.iter().any(|e| e.action == "secret.delete")
        })
        .await;
        assert!(ev.iter().all(|e| e.after.is_none() && e.before.is_none()));
        // Deleting what is not there records nothing and says so.
        assert!(!s.delete_secret(SecretName::EmbeddingApiKey).unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_config_edit_keeps_the_running_settings_and_shows_the_error() {
        let (d, s) = svc().await;
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(
            home.config_file(),
            "[judge]\nreview_threshold = 0.9\nauto_apply_threshold = 0.6\n",
        )
        .unwrap();
        assert!(s.settings_handle().reload_if_changed().is_none());
        assert_eq!(s.settings().judge.review_threshold, 0.5);
        let view = s.settings_view();
        assert!(
            view.errors
                .iter()
                .any(|e| e.key == "judge.review_threshold"),
            "{:?}",
            view.errors
        );
        // The daemon's health carries the same errors.
        let h = s.health().await.unwrap();
        assert!(
            h.settings_errors
                .iter()
                .any(|e| e.key == "judge.review_threshold"),
            "{:?}",
            h.settings_errors
        );
        assert!(
            !h.config_degraded,
            "a good config loaded before; the last one stays"
        );
        // A good edit clears them.
        std::fs::write(home.config_file(), "[judge]\nreview_threshold = 0.6\n").unwrap();
        assert!(s.settings_handle().reload_if_changed().is_some());
        assert!(s.settings_view().errors.is_empty());
        assert!(s.health().await.unwrap().settings_errors.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_config_that_never_parsed_is_flagged_in_health_and_the_view() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        std::fs::write(home.config_file(), "[judge\n").unwrap();
        let s = Service::start(
            home,
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            quiet(),
        )
        .unwrap();
        let h = s.health().await.unwrap();
        assert!(h.config_degraded);
        assert!(h.settings_errors.iter().any(|e| e.key == "config.toml"));
        let v = s.settings_view();
        assert!(v.config_degraded);
        assert!(v.errors.iter().any(|e| e.key == "config.toml"));
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ignored_variables_show_in_health_and_the_settings_view() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        let handle = SettingsHandle::load_with_env(&home, vec![("ALICE_NOPE".into(), "1".into())]);
        let s = Service::start(
            home,
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                settings: Some(handle),
                ..quiet()
            },
        )
        .unwrap();
        assert!(
            s.health()
                .await
                .unwrap()
                .settings_warnings
                .iter()
                .any(|w| w.contains("ALICE_NOPE"))
        );
        assert!(
            s.settings_view()
                .warnings
                .iter()
                .any(|w| w.contains("ALICE_NOPE"))
        );
        s.shutdown();
    }

    /// A settings change is one `settings.change` event by the user, naming the keys and no
    /// value, and one `settings` stream item.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_change_is_an_event_with_keys_only_and_a_stream_item() {
        let (_d, s) = svc().await;
        let mut rx = s.subscribe();
        s.patch_settings(patch_one("judge.review_threshold", serde_json::json!(0.65)))
            .unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(StreamItem::Settings { changed }) => break changed,
                    Ok(_) => continue,
                    Err(e) => panic!("stream ended: {e}"),
                }
            }
        })
        .await
        .expect("a settings item within 5 s");
        assert_eq!(changed, vec!["judge.review_threshold".to_string()]);
        let ev = wait_events(&s, "config", |ev| !ev.is_empty()).await;
        assert_eq!(
            (
                ev[0].action.as_str(),
                ev[0].actor.as_str(),
                ev[0].target_type.as_str()
            ),
            ("settings.change", "user", "settings")
        );
        assert_eq!(
            ev[0].after,
            Some(serde_json::json!({"changed": ["judge.review_threshold"]}))
        );
        assert_eq!(ev[0].before, None);
        // Settings are not memories: the history refuses to undo them, and says why.
        let err = s
            .undo(UndoRequest {
                event_id: ev[0].id.clone(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be undone"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_keys_show_as_pending_until_the_value_is_back() {
        let (_d, s) = svc().await;
        let out = s
            .patch_settings(patch_one("embedding.idle_unload", serde_json::json!("5m")))
            .unwrap();
        assert_eq!(
            out.restart_required,
            vec!["embedding.idle_unload".to_string()]
        );
        assert_eq!(
            out.view.restart_pending,
            vec!["embedding.idle_unload".to_string()]
        );
        // Live keys never wait for a restart.
        let out = s
            .patch_settings(patch_one("judge.review_threshold", serde_json::json!(0.6)))
            .unwrap();
        assert_eq!(
            out.view.restart_pending,
            vec!["embedding.idle_unload".to_string()]
        );
        assert!(out.restart_required.is_empty());
        let out = s
            .patch_settings(patch_one("embedding.idle_unload", serde_json::json!("10m")))
            .unwrap();
        assert!(
            out.view.restart_pending.is_empty(),
            "{:?}",
            out.view.restart_pending
        );
    }

    /// Secrets do not move with the backend: say so wherever the change shows.
    #[tokio::test(flavor = "multi_thread")]
    async fn switching_the_secrets_backend_says_that_secrets_are_not_copied() {
        let (_d, s) = svc().await;
        assert!(
            s.settings_view().notes.is_empty(),
            "{:?}",
            s.settings_view().notes
        );
        let out = s
            .patch_settings(patch_one("secrets.backend", serde_json::json!("keychain")))
            .unwrap();
        assert_eq!(out.restart_required, vec!["secrets.backend".to_string()]);
        let note = out
            .view
            .notes
            .iter()
            .find(|n| n.contains("secrets.backend"))
            .unwrap_or_else(|| panic!("{:?}", out.view.notes));
        assert!(
            note.contains("not copied") && note.contains("set-secret"),
            "{note}"
        );
        let out = s
            .patch_settings(patch_one("secrets.backend", serde_json::json!("file")))
            .unwrap();
        assert!(
            out.view.notes.is_empty(),
            "back to the running backend: {:?}",
            out.view.notes
        );
    }

    /// A loosening typed into config.toml waits for three polls; the view says so.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_loosening_in_the_file_is_announced_while_it_waits() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let s = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                // The test polls by hand; the watcher must not count polls of its own.
                settings_poll: Duration::from_secs(3_600),
                ..quiet()
            },
        )
        .unwrap();
        std::fs::write(home.config_file(), "[privacy]\nredact = false\n").unwrap();
        assert!(s.settings_handle().reload_if_changed().is_none());
        let notes = s.settings_view().notes;
        assert!(
            notes
                .iter()
                .any(|n| n.contains("3 polls") && n.contains("config.toml")),
            "{notes:?}"
        );
        assert!(s.settings().privacy.redact);
        s.settings_handle().reload_if_changed();
        assert!(s.settings_handle().reload_if_changed().is_some());
        assert!(s.settings_view().notes.is_empty());
    }

    /// A keychain that waits for a prompt must not hold up a request or pile up threads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_keychain_that_does_not_answer_never_blocks_a_request() {
        let d = tempfile::tempdir().unwrap();
        let store = MemSecrets::new(Duration::from_millis(400));
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                secrets_timeout: Duration::from_millis(100),
                ..quiet()
            },
        )
        .unwrap();
        let started = Instant::now();
        let view = s.settings_view_async().await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert!(view.secrets.values().all(|x| !x.set));
        assert!(
            view.notes
                .iter()
                .any(|n| n.contains("keychain") && n.contains("did not answer")),
            "{:?}",
            view.notes
        );
        // The first read is still stuck: a second request does not start another one.
        let started = Instant::now();
        let again = s.settings_view_async().await;
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "{:?}",
            started.elapsed()
        );
        assert!(again.notes.iter().any(|n| n.contains("keychain")));
        // Writes give up with an error that says what to do.
        let err = s
            .set_secret_async(
                SecretName::JevApiKey,
                concat!("ts_", "secret_value_1234").into(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not answer"), "{err}");
        // Once the prompt is answered and the stuck read has ended, the view is back to normal.
        store.delay_ms.store(0, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = s.settings_view_async().await;
            if view.notes.iter().all(|n| !n.contains("did not answer")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the view never recovered: {:?}",
                view.notes
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A view asked for after a secret was written must show the write, even when a read of the
    /// store that began before it is still in flight (and would show the old state).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_after_a_secret_write_never_joins_a_read_that_began_before_it() {
        let d = tempfile::tempdir().unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                secrets_timeout: Duration::from_secs(20),
                ..quiet()
            },
        )
        .unwrap();
        // The first read has read the key (not set) and waits.
        store.hold_budget.store(1, Ordering::SeqCst);
        store.hold.store(true, Ordering::SeqCst);
        let first = {
            let s = s.clone();
            tokio::spawn(async move { s.settings_view_async().await })
        };
        until("the first read to wait", || {
            (store.held.load(Ordering::SeqCst) > 0).then_some(())
        })
        .await;
        s.set_secret_async(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .await
            .unwrap();
        // Asked for while the first read is still in flight, after the write.
        let second = {
            let s = s.clone();
            tokio::spawn(async move { s.settings_view_async().await })
        };
        // The second view must finish on its own read while the first is still held (it may
        // not join the held one). If it does not, release the hold after a bounded wait so the
        // assertions below fail instead of hanging.
        let until = Instant::now() + Duration::from_secs(5);
        while !second.is_finished() && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let finished_while_held = second.is_finished();
        store.hold.store(false, Ordering::SeqCst);
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert!(
            finished_while_held,
            "the second view waited for the held read"
        );
        assert!(
            !first.secrets["jev_api_key"].set,
            "the first read began before the write"
        );
        assert!(
            second.secrets["jev_api_key"].set,
            "a view after the write shows it: {:?}",
            second.secrets
        );
        // Without a change in between, overlapping views still share one read.
        store.gets.store(0, Ordering::SeqCst);
        store.delay_ms.store(200, Ordering::SeqCst);
        let (a, b) = tokio::join!(s.settings_view_async(), s.settings_view_async());
        assert!(a.secrets["jev_api_key"].set && b.secrets["jev_api_key"].set);
        assert_eq!(store.gets.load(Ordering::SeqCst), SecretName::ALL.len());
    }

    /// A read of the secrets' status that crashes is reported as having failed, not as a store
    /// that did not answer, and the next view reads again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_crashed_status_read_says_failed_not_silent() {
        let d = tempfile::tempdir().unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                secrets_timeout: Duration::from_secs(20),
                ..quiet()
            },
        )
        .unwrap();
        store.panic_get.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let view = s.settings_view_async().await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a crashed read does not wait for the timeout"
        );
        assert!(view.secrets.values().all(|x| !x.set));
        let note = view.notes.join("; ");
        assert!(
            note.contains("failed") && !note.contains("did not answer"),
            "{note}"
        );
        store.panic_get.store(false, Ordering::SeqCst);
        let view = s.settings_view_async().await;
        assert!(view.notes.is_empty(), "{:?}", view.notes);
    }

    /// `alice doctor` on a fresh service is clean; a read-only database fails with what to do;
    /// Jev asked for without a key warns and names `set-secret jev_api_key`.
    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_checks_are_clean_when_fresh_and_name_a_read_only_database_and_a_missing_key() {
        use crate::doctor::{CheckStatus, daemon_checks};
        let (_d, s) = svc().await;
        let checks = daemon_checks(&s).await.unwrap();
        assert!(
            checks.iter().all(|c| c.status != CheckStatus::Fail),
            "{checks:?}"
        );
        assert!(
            checks
                .iter()
                .any(|c| c.name == "database" && c.status == CheckStatus::Ok)
        );
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        until("the no-key warning", || {
            s.judge_warning().filter(|w| w.contains("no TypeSafe key"))
        })
        .await;
        let checks = daemon_checks(&s).await.unwrap();
        let judge = checks.iter().find(|c| c.name == "judge").unwrap();
        assert_eq!(judge.status, CheckStatus::Warn, "{judge:?}");
        assert!(
            judge
                .fix
                .as_deref()
                .unwrap()
                .contains("set-secret jev_api_key"),
            "{judge:?}"
        );
        // No key value, hint or provider response reaches a check.
        let all = serde_json::to_string(&checks).unwrap();
        assert!(!all.contains(JEV_TEST_KEY), "{all}");
        s.force_read_only("the disk is full");
        let checks = daemon_checks(&s).await.unwrap();
        let db = checks.iter().find(|c| c.name == "database").unwrap();
        assert_eq!(db.status, CheckStatus::Fail);
        assert!(db.detail.contains("the disk is full"));
        assert!(db.fix.as_deref().unwrap().contains("alice restore"));
    }

    /// Two requests that overlap share one read of the store: both see the stored key, and
    /// neither is told the store did not answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn overlapping_reads_share_one_read_of_the_store() {
        let d = tempfile::tempdir().unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        store
            .set(SecretName::JevApiKey, concat!("ts_", "secret_value_1234"))
            .unwrap();
        store.delay_ms.store(300, Ordering::SeqCst);
        store.gets.store(0, Ordering::SeqCst);
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                secrets_timeout: Duration::from_secs(20),
                ..quiet()
            },
        )
        .unwrap();
        let (a, b) = tokio::join!(s.settings_view_async(), s.settings_view_async());
        for v in [&a, &b] {
            assert!(v.secrets["jev_api_key"].set, "{:?}", v.secrets);
            assert!(
                v.notes.iter().all(|n| !n.contains("did not answer")),
                "{:?}",
                v.notes
            );
        }
        assert_eq!(
            store.gets.load(Ordering::SeqCst),
            SecretName::ALL.len(),
            "one read for both requests"
        );
        // A PATCH answer next to a GET is the same case.
        let (p, g) = tokio::join!(
            s.patch_settings_async(patch_one("judge.review_threshold", serde_json::json!(0.6))),
            s.settings_view_async()
        );
        assert!(p.unwrap().view.secrets["jev_api_key"].set);
        assert!(g.secrets["jev_api_key"].set);
    }

    /// Writes against a stuck store wait for the one in flight (up to the timeout) instead of
    /// parking a thread each.
    #[tokio::test(flavor = "multi_thread")]
    async fn writes_against_a_stuck_store_do_not_pile_up() {
        let d = tempfile::tempdir().unwrap();
        let store = MemSecrets::new(Duration::from_millis(600));
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                secrets_timeout: Duration::from_millis(100),
                ..quiet()
            },
        )
        .unwrap();
        for _ in 0..3 {
            let err = s
                .set_secret_async(
                    SecretName::JevApiKey,
                    concat!("ts_", "secret_value_1234").into(),
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("did not answer"), "{err}");
        }
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert_eq!(
            store.sets.load(Ordering::SeqCst),
            1,
            "only the first write reached the store"
        );
    }

    /// A change through the API applies at once, where the same edit of config.toml waits for
    /// three polls: the answer says so for a loosening, and not for a tightening.
    #[tokio::test(flavor = "multi_thread")]
    async fn loosening_through_the_api_is_announced_and_a_tightening_is_not() {
        let (_d, s) = svc().await;
        let out = s
            .patch_settings_async(patch_one("privacy.redact", serde_json::json!(false)))
            .await
            .unwrap();
        assert!(!s.settings().privacy.redact, "applied at once");
        let note = out
            .notes
            .iter()
            .find(|n| n.contains("privacy.redact"))
            .unwrap_or_else(|| panic!("{:?}", out.notes));
        assert!(
            note.contains("at once") && note.contains("3 polls"),
            "{note}"
        );
        let ev = wait_events(&s, "config", |ev| {
            ev.iter().any(|e| e.action == "settings.change")
        })
        .await;
        assert_eq!(ev[0].actor, "user");
        assert_eq!(
            ev[0].after,
            Some(
                serde_json::json!({"changed": ["privacy.redact"], "loosened": ["privacy.redact"]})
            )
        );
        let out = s
            .patch_settings_async(patch_one("privacy.redact", serde_json::json!(true)))
            .await
            .unwrap();
        assert!(out.notes.is_empty(), "{:?}", out.notes);
        let out = s
            .patch_settings_async(patch_one("judge.review_threshold", serde_json::json!(0.6)))
            .await
            .unwrap();
        assert!(out.notes.is_empty(), "{:?}", out.notes);
    }

    /// R3: turning the admin MCP tools on is a loosening, announced like the privacy ones; turning
    /// them off is not.
    #[tokio::test(flavor = "multi_thread")]
    async fn switching_the_admin_tools_on_is_announced_as_a_loosening() {
        let (_d, s) = svc().await;
        let out = s
            .patch_settings_async(patch_one("mcp.admin_tools", serde_json::json!(true)))
            .await
            .unwrap();
        assert!(s.settings().mcp.admin_tools, "applied at once");
        let note = out
            .notes
            .iter()
            .find(|n| n.contains("mcp.admin_tools"))
            .unwrap_or_else(|| panic!("{:?}", out.notes));
        assert!(
            note.contains("loosened") && note.contains("3 polls"),
            "{note}"
        );
        let ev = wait_events(&s, "config", |ev| {
            ev.iter().any(|e| e.action == "settings.change")
        })
        .await;
        assert_eq!(
            ev[0].after,
            Some(
                serde_json::json!({"changed": ["mcp.admin_tools"], "loosened": ["mcp.admin_tools"]})
            )
        );
        let out = s
            .patch_settings_async(patch_one("mcp.admin_tools", serde_json::json!(false)))
            .await
            .unwrap();
        assert!(out.notes.is_empty(), "{:?}", out.notes);
    }

    /// The embedding provider, model and endpoint are read (a change starts a re-embed): they
    /// are not marked unused and a change needs no restart. The batch size still does.
    #[tokio::test(flavor = "multi_thread")]
    async fn embedding_keys_are_used_and_apply_without_a_restart() {
        let (_d, s) = svc().await;
        let view = s.settings_view();
        for k in [
            "embedding.provider",
            "embedding.model",
            "embedding.endpoint",
        ] {
            assert!(
                view.keys
                    .iter()
                    .any(|i| i.key == k && !i.not_used_yet && !i.restart),
                "{k}"
            );
        }
        assert!(
            view.keys
                .iter()
                .any(|i| i.key == "embedding.batch_size" && i.restart)
        );
        let out = s
            .patch_settings_async(patch_one(
                "embedding.endpoint",
                serde_json::json!("http://127.0.0.1:11434"),
            ))
            .await
            .unwrap();
        assert!(
            out.restart_required.is_empty(),
            "{:?}",
            out.restart_required
        );
        assert!(
            out.notes.iter().all(|n| !n.contains("not used yet")),
            "{:?}",
            out.notes
        );
        let out = s
            .patch_settings_async(patch_one("embedding.batch_size", serde_json::json!(16)))
            .await
            .unwrap();
        assert_eq!(out.restart_required, vec!["embedding.batch_size"]);
    }

    /// A stand-in for the Jev API: always answers yes, counts the requests, and checks the key.
    struct ScriptedJev {
        sent: std::sync::atomic::AtomicUsize,
    }

    const JEV_TEST_KEY: &str = concat!("ts_", "live_service_test_0001");

    impl alice_core::judge::jev::JevTransport for ScriptedJev {
        fn post(
            &self,
            _endpoint: &str,
            key: &str,
            _body: &[u8],
            _timeout: Duration,
        ) -> std::result::Result<
            alice_core::judge::jev::HttpReply,
            alice_core::judge::jev::TransportError,
        > {
            assert_eq!(key, JEV_TEST_KEY);
            self.sent.fetch_add(1, Ordering::SeqCst);
            let ok = serde_json::json!({"answers": {"q": {"choice": "yes",
                "probabilities": {"yes": 0.9, "no": 0.1}}}, "usage": {"input_tokens": 80, "output_tokens": 2}});
            Ok(alice_core::judge::jev::HttpReply {
                status: 200,
                body: serde_json::to_vec(&ok).unwrap(),
                retry_after: None,
            })
        }
    }

    async fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        for _ in 0..250 {
            if let Some(v) = f() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_jev_judge_follows_the_settings_and_the_key_and_records_its_cost() {
        let d = tempfile::tempdir().unwrap();
        let transport = Arc::new(ScriptedJev {
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                jev_transport: Some(transport.clone()),
                ..quiet()
            },
        )
        .unwrap();
        assert_eq!(s.judge().provider(), "heuristic");
        assert_eq!(s.judge_warning(), None);
        // Jev asked for, no key: the heuristic judges, and health says why.
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        // The final text, not the transient "reading the TypeSafe key" while the judge builds.
        let w = until("the no-key warning", || {
            s.judge_warning().filter(|w| w.contains("no TypeSafe key"))
        })
        .await;
        assert!(
            w.contains("no TypeSafe key") && w.contains("set-secret"),
            "{w}"
        );
        let t = s.test_provider("judge").await.unwrap();
        assert!(!t.ok && t.message.contains("no TypeSafe key"), "{t:?}");
        // The key arrives: the judge is Jev at once.
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        assert_eq!(s.judge().provider(), "jev");
        assert_eq!(s.judge_warning(), None);
        let t = s.test_provider("judge").await.unwrap();
        assert!(t.ok && t.message.contains("yes"), "{t:?}");
        assert!(!t.message.contains("service_test"), "{t:?}");
        assert_eq!(transport.sent.load(Ordering::SeqCst), 1);
        // The call is in the ledger and the cache (written fire-and-forget through the writer).
        let (calls, usd): (i64, f64) = until("the ledger row", || {
            futures::executor::block_on(s.read(|c| {
                Ok(c.query_row(
                    "SELECT coalesce(sum(calls), 0), coalesce(sum(usd), 0) FROM cost_ledger WHERE provider = 'jev'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            }))
            .ok()
            .filter(|(calls, _): &(i64, f64)| *calls == 1)
        })
        .await;
        assert_eq!(calls, 1);
        assert!((usd - 80.0 * 0.042 / 1e6).abs() < 1e-12, "{usd}");
        let cached: i64 = s
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM judge_cache WHERE provider = 'jev'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(cached, 1);
        // The same test again is answered from the cache: no request, no cost.
        assert!(s.test_provider("judge").await.unwrap().ok);
        assert_eq!(transport.sent.load(Ordering::SeqCst), 1);
        // A privacy change applies to the very next call: `global` is denied, nothing is sent.
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["global"]),
        ))
        .unwrap();
        let t = s.test_provider("judge").await.unwrap();
        assert!(!t.ok && t.message.contains("may not be sent"), "{t:?}");
        assert_eq!(transport.sent.load(Ordering::SeqCst), 1);
        // Back to the heuristic by setting; the key stays stored and unused.
        s.patch_settings(patch_one("judge.provider", serde_json::json!("heuristic")))
            .unwrap();
        until("the heuristic judge", || {
            (s.judge().provider() == "heuristic").then_some(())
        })
        .await;
        assert!(s.test_provider("judge").await.unwrap().ok);
        // Deleting the key while Jev is wanted falls back with a warning.
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        until("the jev judge", || {
            (s.judge().provider() == "jev").then_some(())
        })
        .await;
        s.delete_secret(SecretName::JevApiKey).unwrap();
        assert_eq!(s.judge().provider(), "heuristic");
        assert!(s.judge_warning().unwrap().contains("no TypeSafe key"));
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_secrets_store_never_holds_up_start_and_the_warning_says_so() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        std::fs::write(home.config_file(), "[judge]\nprovider = \"jev\"\n").unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        store.set(SecretName::JevApiKey, JEV_TEST_KEY).unwrap();
        store.delay_ms.store(2_000, Ordering::SeqCst);
        let transport = Arc::new(ScriptedJev {
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let started = Instant::now();
        let s = Service::start(
            home,
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                secrets_timeout: Duration::from_millis(150),
                jev_transport: Some(transport),
                ..quiet()
            },
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1_500),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            s.judge().provider(),
            "heuristic",
            "the heuristic answers meanwhile"
        );
        let w = s.judge_warning().expect("a warning while the key is read");
        assert!(w.contains("reading the TypeSafe key"), "{w}");
        let t = s.test_provider("judge").await.unwrap();
        assert!(!t.ok, "{t:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let w = s.judge_warning().expect("still reading");
        assert!(w.contains("taking") || w.contains("has taken"), "{w}");
        until("the Jev judge", || {
            (s.judge().provider() == "jev").then_some(())
        })
        .await;
        assert_eq!(s.judge_warning(), None);
        s.shutdown();
    }

    /// A stored Jev key reaches no answer of the admin surface: stats, the review list, a merge.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stored_jev_key_never_appears_in_stats_or_review_output() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        std::fs::write(home.config_file(), "[judge]\nprovider = \"jev\"\n").unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        store.set(SecretName::JevApiKey, JEV_TEST_KEY).unwrap();
        let s = Service::start(
            home,
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store),
                jev_transport: Some(Arc::new(ScriptedJev {
                    sent: std::sync::atomic::AtomicUsize::new(0),
                })),
                ..quiet()
            },
        )
        .unwrap();
        until("the Jev judge", || {
            (s.judge().provider() == "jev").then_some(())
        })
        .await;
        let stats = s.stats().await.unwrap();
        assert_eq!(stats.judge_provider, "jev");
        let all = serde_json::to_string(&(
            &stats,
            s.reviews(ReviewQuery::default()).await.unwrap(),
            s.judge_warning(),
        ))
        .unwrap();
        assert_eq!(all.matches(JEV_TEST_KEY).count(), 0, "{all}");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_older_slow_build_never_replaces_a_newer_one_and_builds_never_pile_up() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        std::fs::write(home.config_file(), "[judge]\nprovider = \"jev\"\n").unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        store.set(SecretName::JevApiKey, JEV_TEST_KEY).unwrap();
        // The first read (the start-up build) is slow and returns an out-of-date key.
        *store.first_get.lock() = Some(concat!("ts_", "live_stale_key_0001").into());
        store.delay_ms.store(300, Ordering::SeqCst);
        let transport = Arc::new(ScriptedJev {
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let s = Service::start(
            home,
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                jev_transport: Some(transport.clone()),
                ..quiet()
            },
        )
        .unwrap();
        // Three more builds are asked for while the first one is still reading.
        for budget in [2.0, 3.0, 4.0] {
            s.patch_settings(patch_one(
                "judge.daily_budget_usd",
                serde_json::json!(budget),
            ))
            .unwrap();
        }
        until("the judge", || {
            (s.judge().provider() == "jev" && s.judge_warning().is_none()).then_some(())
        })
        .await;
        assert_eq!(
            store.max_active.load(Ordering::SeqCst),
            1,
            "one build at a time"
        );
        // The installed judge holds the current key: the transport asserts it on every request.
        let t = s.test_provider("judge").await.unwrap();
        assert!(t.ok, "{t:?}");
        assert_eq!(transport.sent.load(Ordering::SeqCst), 1);
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_that_cannot_be_read_falls_back_with_a_warning_and_privacy_changes_keep_the_judge()
     {
        let d = tempfile::tempdir().unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        store.set(SecretName::JevApiKey, JEV_TEST_KEY).unwrap();
        let transport = Arc::new(ScriptedJev {
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                secrets: Some(store.clone()),
                jev_transport: Some(transport),
                ..quiet()
            },
        )
        .unwrap();
        store.fail_get.store(true, Ordering::SeqCst);
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        let w = until("the key warning", || {
            s.judge_warning().filter(|w| w.contains("cannot be read"))
        })
        .await;
        assert!(
            w.contains("keychain is locked") && !w.contains("service_test"),
            "{w}"
        );
        assert_eq!(s.judge().provider(), "heuristic");
        store.fail_get.store(false, Ordering::SeqCst);
        s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(2.0)))
            .unwrap();
        until("the Jev judge", || {
            (s.judge().provider() == "jev").then_some(())
        })
        .await;
        let judge = s.judge();
        // A privacy change is read per call: the judge is not rebuilt.
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["other"]),
        ))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(Arc::ptr_eq(&judge, &s.judge()));
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_denylist_entry_that_matches_no_project_is_flagged_but_kept() {
        let (_d, s) = svc().await;
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["Global", "clinet-x"]),
        ))
        .unwrap();
        let v = s.settings_view();
        let notes: Vec<_> = v
            .notes
            .iter()
            .filter(|n| n.contains("matches no project"))
            .collect();
        assert_eq!(notes.len(), 1, "{:?}", v.notes);
        assert!(
            notes[0].contains("clinet-x") && !notes[0].contains("Global"),
            "{notes:?}"
        );
        assert_eq!(
            s.settings().privacy.cloud_denied_projects,
            ["Global", "clinet-x"],
            "what the user typed is kept"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_provider_test_answers_for_embedding_and_judge_and_refuses_others() {
        let (_d, s) = svc().await;
        let e = s.test_provider("embedding").await.unwrap();
        assert!(e.ok && e.message.contains("384"), "{e:?}");
        let j = s.test_provider("judge").await.unwrap();
        assert!(j.ok, "{j:?}");
        let err = s.test_provider("jev2").await.unwrap_err();
        assert!(matches!(err, AliceError::Invalid(_)), "{err}");
        assert!(err.to_string().contains("judge or embedding"));
    }

    // ------------------------------------------------------------------ the judge pipeline

    async fn svc_with_jev_opts(
        t: Arc<crate::judge_http::ScriptedJev>,
        opts: ServiceOptions,
    ) -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                jev_transport: Some(t),
                ..opts
            },
        )
        .unwrap();
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        (d, s)
    }

    async fn svc_with_jev(
        t: Arc<crate::judge_http::ScriptedJev>,
    ) -> (tempfile::TempDir, Arc<Service>) {
        svc_with_jev_opts(t, quiet()).await
    }

    fn remember_req(content: &str, kind: Option<Kind>, cwd: &Option<String>) -> RememberRequest {
        RememberRequest {
            content: content.into(),
            kind,
            cwd: cwd.clone(),
            ..Default::default()
        }
    }

    async fn remember_id(
        s: &Service,
        content: &str,
        kind: Option<Kind>,
        cwd: &Option<String>,
    ) -> String {
        s.remember(remember_req(content, kind, cwd))
            .await
            .unwrap()
            .id
            .unwrap()
    }

    /// `(judge_state, status)` of a memory.
    async fn state_of(s: &Service, id: &str) -> (String, String) {
        let id = id.to_string();
        s.read(move |c| {
            Ok(c.query_row(
                "SELECT judge_state, status FROM memories WHERE id = ?1",
                [&id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?)
        })
        .await
        .unwrap()
    }

    async fn memory_of(s: &Service, id: &str) -> alice_core::api::MemoryFull {
        s.get(GetRequest {
            ids: vec![id.to_string()],
            include_provenance: false,
        })
        .await
        .unwrap()
        .memories
        .remove(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_heuristic_trashes_a_near_identical_memory_and_get_returns_the_survivor() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        let text = "Use WAL mode so readers never block the writer";
        let a = remember_id(&s, text, Some(Kind::Decision), &cwd).await;
        wait_settled(&s).await;
        // Same text under another kind: a new row (the content hash includes the kind) with an
        // identical vector, so cosine 1.0 is over 0.97.
        let b = remember_id(&s, text, Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        let got = s
            .get(GetRequest {
                ids: vec![b.clone()],
                include_provenance: false,
            })
            .await
            .unwrap();
        assert_eq!(
            got.memories[0].memory.id, a,
            "the survivor answers for the duplicate"
        );
        assert_eq!(got.redirects[0].from, b);
        let ev = s
            .events(EventsRequest {
                target_id: Some(b.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ev.iter()
                .any(|e| e.action == "memory.merge" && e.actor == "judge:heuristic"),
            "{ev:?}"
        );
        let h = s.health().await.unwrap();
        assert_eq!(
            (h.judge_provider.as_str(), h.judge_backlog),
            ("heuristic", 0)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jev_updates_supersede_contradictions_link_and_ask() {
        for (rel, expect_status, expect_review) in [
            (("updates", 0.95), "superseded", None),
            (("contradicts", 0.9), "active", Some("contradiction")),
        ] {
            let (d, s) = svc_with_jev(crate::judge_http::ScriptedJev::new(rel)).await;
            let cwd = repo(&d, "proj");
            let old = remember_id(
                &s,
                "Staging Postgres listens on port 5432",
                Some(Kind::Fact),
                &cwd,
            )
            .await;
            wait_settled(&s).await;
            let new = remember_id(
                &s,
                "Staging Postgres listens on port 5433 now",
                Some(Kind::Fact),
                &cwd,
            )
            .await;
            wait_settled(&s).await;
            assert_eq!(
                memory_of(&s, &old).await.memory.status.as_str(),
                expect_status,
                "{rel:?}"
            );
            let open = s.reviews(ReviewQuery::default()).await.unwrap();
            assert_eq!(
                open.first().map(|r| r.review_type.as_str()),
                expect_review,
                "{rel:?}"
            );
            // The new memory is live, and what the judge did is its work, in one group.
            assert_eq!(
                state_of(&s, &new).await,
                ("done".to_string(), "active".to_string())
            );
            let ev = s
                .events(EventsRequest {
                    target_id: Some(old.clone()),
                    ..Default::default()
                })
                .await
                .unwrap();
            let by_judge = ev.iter().filter(|e| e.actor == "judge:jev").count();
            assert_eq!(by_judge, usize::from(rel.0 == "updates"), "{ev:?}");
            s.shutdown();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_jev_falls_back_to_the_heuristic_and_retries_later_without_failing_the_write()
    {
        let t = crate::judge_http::ScriptedJev::failing(503);
        let (d, s) = svc_with_jev(t.clone()).await;
        // A: an explicit kind and no neighbour: nothing to ask, so no request at all.
        let a = s
            .remember(remember_req(
                "anything at all",
                Some(Kind::Note),
                &repo(&d, "proj"),
            ))
            .await
            .unwrap();
        assert_eq!(
            a.status,
            RememberStatus::New,
            "a failing judge never fails the write"
        );
        wait_settled(&s).await;
        assert_eq!(
            state_of(&s, &a.id.unwrap()).await,
            ("done".to_string(), "active".to_string())
        );
        assert_eq!(t.sent(), 0);
        // B: an explicit kind and a neighbour: the comparison's 503s exhaust the retries.
        let cwd = repo(&d, "proj3");
        remember_id(&s, "deploy notes for staging", Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        assert_eq!(t.sent(), 0, "the first memory of proj3 has no neighbour");
        let b = remember_id(
            &s,
            "deploy notes for staging servers",
            Some(Kind::Note),
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        assert_eq!(t.sent(), 4, "one try and three retries for the comparison");
        assert_eq!(
            state_of(&s, &b).await,
            ("retry".to_string(), "active".to_string()),
            "the heuristic decides meanwhile"
        );
        // C: no kind and no neighbour: the kind question fails; that is retried later too.
        let c = remember_id(&s, "something else entirely", None, &repo(&d, "proj2")).await;
        wait_settled(&s).await;
        assert_eq!(
            t.sent(),
            8,
            "one try and three retries for the kind question"
        );
        assert_eq!(
            state_of(&s, &c).await,
            ("retry".to_string(), "active".to_string()),
            "a failed kind classification is asked again (spec 5.2)"
        );
        // Nothing asked again by itself: the retry interval is long here.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(t.sent(), 8);
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_memory_judged_during_an_outage_is_judged_again_when_the_judge_is_back() {
        let t = crate::judge_http::ScriptedJev::new(("updates", 0.95));
        let (d, s) = svc_with_jev_opts(
            t.clone(),
            ServiceOptions {
                judge_retry_every: Duration::from_millis(100),
                judge_retry_base: Duration::from_millis(10),
                ..quiet()
            },
        )
        .await;
        let cwd = repo(&d, "proj");
        let old = remember_id(
            &s,
            "Staging Postgres listens on port 5432",
            Some(Kind::Fact),
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        t.set_status(Some(503));
        let new = remember_id(
            &s,
            "Staging Postgres listens on port 5433 now",
            Some(Kind::Fact),
            &cwd,
        )
        .await;
        // Judged, failed, and queued again by itself (4 calls per try).
        until("a second try during the outage", || {
            (t.sent() >= 8).then_some(())
        })
        .await;
        assert_ne!(
            memory_of(&s, &old).await.memory.status.as_str(),
            "superseded"
        );
        // The judge comes back: the memory is judged again and now supersedes.
        t.set_status(None);
        until("the supersede", || {
            (futures::executor::block_on(memory_of(&s, &old))
                .memory
                .status
                .as_str()
                == "superseded")
                .then_some(())
        })
        .await;
        assert_eq!(
            state_of(&s, &new).await,
            ("done".to_string(), "active".to_string())
        );
        s.shutdown();
    }

    // ------------------------------------------------- distilled memories (SP2 spec §6.6)

    /// A distilled memory, written as the distiller writes it (SP2 spec §6.5): the service's
    /// project choice, `origin = distill`.
    async fn remember_distilled(
        s: &Service,
        content: &str,
        kind: Kind,
        cwd: &Option<String>,
    ) -> String {
        let project = s
            .choose(remember_req(content, Some(kind), cwd).scope())
            .await
            .unwrap();
        match s
            .writer
            .submit(WriteOp::Remember {
                input: NewMemoryInput {
                    content: content.into(),
                    kind: Some(kind),
                    origin: alice_core::model::Origin::Distill,
                    ..Default::default()
                },
                project,
            })
            .await
            .unwrap()
        {
            WriteOutcome::Remembered(InsertOutcome::New { id, .. }, _) => id,
            o => panic!("{o:?}"),
        }
    }

    /// `(to_id, type)` of the links from `id`.
    async fn links_from(s: &Service, id: &str) -> Vec<(String, String)> {
        let id = id.to_string();
        s.read(move |c| {
            let mut st =
                c.prepare("SELECT to_id, type FROM links WHERE from_id = ?1 ORDER BY type")?;
            let rows = st
                .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_memory_replaces_an_older_distilled_duplicate() {
        let (d, s) = svc_with_jev(crate::judge_http::ScriptedJev::new(("duplicate", 0.95))).await;
        let cwd = repo(&d, "proj");
        let auto = remember_distilled(
            &s,
            "Staging Postgres listens on port 5432",
            Kind::Fact,
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        let mine = remember_id(
            &s,
            "The staging Postgres server listens on port 5432",
            Some(Kind::Fact),
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        // The explicit memory survives, though it is the newer one.
        assert_eq!(
            state_of(&s, &mine).await,
            ("done".to_string(), "active".to_string())
        );
        assert_eq!(state_of(&s, &auto).await.1, "trashed");
        assert_eq!(
            links_from(&s, &auto).await,
            [(mine.clone(), "duplicate_of".to_string())]
        );
        assert_eq!(
            memory_of(&s, &auto).await.memory.id,
            mine,
            "the survivor answers for the distilled copy"
        );
        assert!(s.reviews(ReviewQuery::default()).await.unwrap().is_empty());
        let ev = s
            .events(EventsRequest {
                target_id: Some(auto.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ev.iter()
                .any(|e| e.action == "memory.merge" && e.actor == "judge:jev"),
            "{ev:?}"
        );
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_distilled_memory_never_supersedes_an_agent_memory() {
        let (d, s) = svc_with_jev(crate::judge_http::ScriptedJev::new(("updates", 0.95))).await;
        let cwd = repo(&d, "proj");
        let mine = remember_id(
            &s,
            "Staging Postgres listens on port 5432",
            Some(Kind::Fact),
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        let auto = remember_distilled(
            &s,
            "Staging Postgres listens on port 5433 now",
            Kind::Fact,
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        let live = ("done".to_string(), "active".to_string());
        assert_eq!(state_of(&s, &mine).await.1, "active", "both stay");
        assert_eq!(state_of(&s, &auto).await, live);
        assert_eq!(
            links_from(&s, &auto).await,
            [(mine.clone(), "relates_to".to_string())]
        );
        assert!(s.reviews(ReviewQuery::default()).await.unwrap().is_empty());
        // A distilled update of an older distilled memory supersedes it, as an agent's would.
        let newer = remember_distilled(
            &s,
            "Staging Postgres listens on port 5434 now",
            Kind::Fact,
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        assert_eq!(state_of(&s, &newer).await, live);
        assert_eq!(state_of(&s, &auto).await.1, "superseded");
        assert_eq!(state_of(&s, &mine).await.1, "active");
        assert_eq!(
            links_from(&s, &newer).await,
            [
                (mine.clone(), "relates_to".to_string()),
                (auto.clone(), "supersedes".to_string()),
            ]
        );
        assert!(s.reviews(ReviewQuery::default()).await.unwrap().is_empty());
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unjudged_distilled_memories_are_hidden_from_recall_and_the_briefing() {
        let t = crate::judge_http::ScriptedJev::failing(503);
        let (d, s) = svc_with_jev_opts(
            t.clone(),
            ServiceOptions {
                judge_retry_every: Duration::from_millis(50),
                judge_retry_base: Duration::from_millis(1),
                ..quiet()
            },
        )
        .await;
        let cwd = repo(&d, "proj");
        // A neighbour, so the judge has a question to ask about the distilled memory.
        remember_id(
            &s,
            "Staging Postgres listens on port 5432",
            Some(Kind::Fact),
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        let text = "We release the billing service with blue green deploys";
        let auto = remember_distilled(&s, text, Kind::Decision, &cwd).await;
        let recall = || {
            s.recall(RecallRequest {
                query: "billing blue green deploys".into(),
                cwd: cwd.clone(),
                format: OutputFormat::Json,
                ..Default::default()
            })
        };
        // Pending, then waiting for the judge that cannot answer: hidden all along.
        for _ in 0..2 {
            assert_ne!(state_of(&s, &auto).await.0, "done");
            let r = recall().await.unwrap();
            assert!(r.results.iter().all(|h| h.id != auto), "{:?}", r.results);
            let ctx = context_of(&s, &cwd).await.text;
            assert!(!ctx.contains("blue green"), "{ctx}");
            assert_eq!(memory_of(&s, &auto).await.memory.id, auto, "get finds it");
            until("a failed judgement", || (t.sent() >= 4).then_some(())).await;
        }
        until("waiting for the judge", || {
            (futures::executor::block_on(state_of(&s, &auto)).0 == "retry").then_some(())
        })
        .await;
        // The judge is back: once judged, the memory is shown, marked.
        t.set_status(None);
        until("the judgement", || {
            (futures::executor::block_on(state_of(&s, &auto)).0 == "done").then_some(())
        })
        .await;
        let r = recall().await.unwrap();
        let hit = r.results.iter().find(|h| h.id == auto).unwrap();
        assert_eq!(hit.origin.as_deref(), Some("distill"));
        let ctx = context_of(&s, &cwd).await.text;
        assert!(
            ctx.contains(&format!("- [decision] {text} ({auto}) (auto)\n")),
            "{ctx}"
        );
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kind_is_set_only_when_the_writer_gave_none() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            kind: Some(("bugfix", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        let cwd = repo(&d, "proj");
        let a = remember_id(
            &s,
            "Fixed: the lease expired; mining now runs off the writer",
            None,
            &cwd,
        )
        .await;
        let b = remember_id(&s, "We chose WAL", Some(Kind::Decision), &cwd).await;
        wait_settled(&s).await;
        assert_eq!(memory_of(&s, &a).await.memory.kind, Kind::Bugfix);
        assert_eq!(
            memory_of(&s, &b).await.memory.kind,
            Kind::Decision,
            "an explicit kind is never changed"
        );
        let ev = s
            .events(EventsRequest {
                target_id: Some(a),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ev.iter()
                .any(|e| e.action == "memory.edit" && e.actor == "judge:jev"),
            "{ev:?}"
        );
        s.shutdown();
    }

    /// P1: the cloud decision is the judge's own. A project denied by its `.alice.toml`, one
    /// denied through an alias, and a used-up budget all leave the heuristic to decide, send
    /// nothing, and do not ask again later.
    #[tokio::test(flavor = "multi_thread")]
    async fn denied_projects_and_a_used_up_budget_are_judged_locally_and_not_retried() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("updates", 0.95),
            kind: Some(("bugfix", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        // Denied by the project's own file.
        let cwd = repo(&d, "filed");
        std::fs::write(
            std::path::Path::new(cwd.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        let first = remember_id(&s, "Staging Postgres listens on port 5432", None, &cwd).await;
        let second = remember_id(&s, "Staging Postgres listens on port 5433 now", None, &cwd).await;
        wait_settled(&s).await;
        assert_eq!(
            t.sent(),
            0,
            "nothing from a project its file denies is sent"
        );
        for id in [&first, &second] {
            assert_eq!(
                state_of(&s, id).await,
                ("done".to_string(), "active".to_string()),
                "judged locally, not retried"
            );
        }
        assert_eq!(
            memory_of(&s, &first).await.memory.kind,
            Kind::Note,
            "the heuristic does not reclassify"
        );
        // Denied through an alias of the project, by the global list.
        let cwd = repo(&d, "aliased");
        remember_id(&s, "Alias project first note", Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        let home = AliceHome::new(d.path().to_path_buf());
        alice_core::store::open_writer(&home.db())
            .unwrap()
            .execute(
                "INSERT INTO project_aliases (alias, project_id) SELECT 'client-x', id FROM projects WHERE slug = 'aliased'",
                [],
            )
            .unwrap();
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["client-x"]),
        ))
        .unwrap();
        let aliased = remember_id(&s, "Alias project second note", None, &cwd).await;
        wait_settled(&s).await;
        assert_eq!(t.sent(), 0, "an alias cannot get around the denylist");
        assert_eq!(
            state_of(&s, &aliased).await,
            ("done".to_string(), "active".to_string())
        );
        // A used-up budget is a decision too: the heuristic decides and nothing is retried.
        s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(0.0)))
            .unwrap();
        let cwd = repo(&d, "open");
        remember_id(&s, "Open project first note", Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        let broke = remember_id(&s, "Open project second note", None, &cwd).await;
        wait_settled(&s).await;
        assert_eq!(t.sent(), 0, "no request goes out against a budget of zero");
        assert_eq!(
            state_of(&s, &broke).await,
            ("done".to_string(), "active".to_string())
        );
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_memory_with_no_project_is_routed_by_the_judge() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            project: Some(("proj", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t).await;
        remember_id(&s, "proj uses WAL", Some(Kind::Note), &repo(&d, "proj")).await;
        wait_settled(&s).await;
        // Neither project nor cwd: global for now, and the judge may route it.
        let id = remember_id(
            &s,
            "the proj deploy runs on Fridays",
            Some(Kind::Note),
            &None,
        )
        .await;
        wait_settled(&s).await;
        assert_eq!(memory_of(&s, &id).await.memory.project, "proj");
        let ev = s
            .events(EventsRequest {
                target_id: Some(id),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ev.iter()
                .any(|e| e.action == "memory.move" && e.actor == "judge:jev"),
            "{ev:?}"
        );
        s.shutdown();
    }

    /// P2: a model is never told about a project it may not see: not its slug, its name, nor
    /// the titles that describe it. The heuristic, which runs here, may still route to it.
    #[tokio::test(flavor = "multi_thread")]
    async fn routing_never_offers_a_denied_project_to_the_model() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            project: Some(("allowedproj", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        let denied = repo(&d, "deniedproj");
        std::fs::write(
            std::path::Path::new(denied.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        remember_id(
            &s,
            "Zebracorn roadmap for the denied project",
            Some(Kind::Note),
            &denied,
        )
        .await;
        remember_id(
            &s,
            "allowedproj keeps its schedule in a wiki",
            Some(Kind::Note),
            &repo(&d, "allowedproj"),
        )
        .await;
        wait_settled(&s).await;
        assert_eq!(
            t.sent(),
            0,
            "so far only local judging: no neighbours, explicit kinds"
        );
        let id = remember_id(
            &s,
            "the allowedproj deploy runs on Fridays",
            Some(Kind::Note),
            &None,
        )
        .await;
        wait_settled(&s).await;
        assert_eq!(memory_of(&s, &id).await.memory.project, "allowedproj");
        assert!(t.sent() >= 1, "the router asked");
        for needle in ["deniedproj", "Zebracorn", "denied project"] {
            assert!(!t.mentioned(needle), "{needle} reached the model");
        }
        assert!(
            t.mentioned("allowedproj"),
            "the allowed project was offered"
        );
        assert!(t.mentioned("wiki"), "with its recent titles");
        // Every candidate denied: the model is not asked to route at all; the heuristic is, and
        // a project named in the text is only suggested.
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["allowedproj"]),
        ))
        .unwrap();
        let before = t.sent();
        let id2 = remember_id(&s, "notes about deniedproj", Some(Kind::Note), &None).await;
        wait_settled(&s).await;
        assert_eq!(
            t.sent(),
            before,
            "no project left to offer a model: it is not asked"
        );
        assert!(!t.mentioned("deniedproj"));
        let open = s.reviews(ReviewQuery::default()).await.unwrap();
        let suggested = open.iter().find(|r| {
            r.review_type.as_str() == "project" && r.payload["memory_id"] == id2.as_str()
        });
        assert!(suggested.is_some(), "{open:?}");
        assert_eq!(suggested.unwrap().payload["provider"], "heuristic");
        s.shutdown();
    }

    /// P4: the judge's own move is not allowed to write text the destination's rules would
    /// change; it asks instead, and the person's answer goes through the usual screening.
    #[tokio::test(flavor = "multi_thread")]
    async fn routing_into_a_stricter_project_is_left_to_a_person() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            project: Some(("strictproj", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        let strict = repo(&d, "strictproj");
        std::fs::write(
            std::path::Path::new(strict.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\n",
        )
        .unwrap();
        remember_id(
            &s,
            "strictproj tracks its work in a board",
            Some(Kind::Note),
            &strict,
        )
        .await;
        wait_settled(&s).await;
        let id = remember_id(
            &s,
            "the strictproj ticket TCK-12345 is still open",
            Some(Kind::Note),
            &None,
        )
        .await;
        wait_settled(&s).await;
        assert_eq!(
            memory_of(&s, &id).await.memory.project,
            "global",
            "not moved on its own"
        );
        let open = s.reviews(ReviewQuery::default()).await.unwrap();
        let item = open
            .iter()
            .find(|r| r.review_type.as_str() == "project" && r.payload["memory_id"] == id.as_str())
            .unwrap_or_else(|| panic!("{open:?}"));
        assert_eq!(item.payload["to"], "strictproj");
        assert_eq!(item.payload["provider"], "jev");
        // A person says yes: the move re-redacts the text with the destination's rules.
        s.resolve_review(ResolveRequest {
            id: item.id.clone(),
            choice: "move".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let moved = memory_of(&s, &id).await.memory;
        assert_eq!(moved.project, "strictproj");
        assert!(!moved.body.contains("TCK-12345"), "{}", moved.body);
        s.shutdown();
    }

    /// The titles that describe a project to a model are redacted like every other request
    /// text, even when the person stores raw text.
    #[tokio::test(flavor = "multi_thread")]
    async fn route_hints_are_redacted_before_they_reach_the_model() {
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            project: Some(("hintproj", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        s.patch_settings(patch_one("privacy.redact", serde_json::json!(false)))
            .unwrap();
        let title = format!("hintproj deploy token {token}");
        remember_id(&s, &title, Some(Kind::Note), &repo(&d, "hintproj")).await;
        wait_settled(&s).await;
        let stored = s
            .read({
                let title = title.clone();
                move |c| {
                    Ok(c.query_row(
                        "SELECT count(*) FROM memories WHERE title = ?1",
                        [&title],
                        |r| r.get::<_, i64>(0),
                    )?)
                }
            })
            .await
            .unwrap();
        assert_eq!(stored, 1, "stored raw, as the person asked");
        remember_id(
            &s,
            "the hintproj release is on Friday",
            Some(Kind::Note),
            &None,
        )
        .await;
        wait_settled(&s).await;
        assert!(
            t.mentioned("hintproj"),
            "the project and its title were offered"
        );
        assert!(
            t.mentioned("hintproj deploy token"),
            "a plain word of the hint title reaches the model"
        );
        assert!(t.mentioned("[REDACTED"), "where the token was");
        assert!(!t.mentioned(token), "the token reached the model");
        s.shutdown();
    }

    // ------------------------------------------------------------------ Jev for distillation

    /// One distillation question, `c1`, about `passage`.
    fn distill_question(passage: &str) -> alice_core::judge::jev::Question {
        alice_core::judge::jev::Question {
            id: "c1".into(),
            instructions: format!(
                "Passage 1 from this session: «{passage}». What lasting knowledge, if any, does it hold for future work?"
            ),
            options: vec![
                alice_core::judge::Choice::new(
                    "decision",
                    "a choice that was made, with its reason",
                ),
                alice_core::judge::Choice::new("ephemeral", "progress narration"),
            ],
        }
    }

    /// `ask_distill` on the judge in force, off the async runtime (as the distiller calls it).
    async fn ask_distill(
        s: &Service,
        project: &str,
        state: &str,
        passage: &str,
        share: f64,
    ) -> alice_core::judge::JudgeResult<BTreeMap<String, alice_core::judge::Distribution>> {
        let judge = s.judge();
        let (project, state, q) = (
            project.to_string(),
            state.to_string(),
            distill_question(passage),
        );
        tokio::task::spawn_blocking(move || {
            judge.ask_distill(&[project.as_str()], &state, &[q], share)
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distill_calls_are_recorded_in_their_own_ledger_row() {
        use crate::doctor::{CheckStatus, daemon_checks};
        use alice_core::judge::jev::{JevStore, LEDGER_DISTILL, LEDGER_JUDGE, scenarios};
        let t = crate::judge_http::ScriptedJev::new(("unrelated", 1.0));
        let (d, s) = svc_with_jev(t.clone()).await;
        remember_id(
            &s,
            "proj keeps its notes in WAL mode",
            Some(Kind::Note),
            &repo(&d, "proj"),
        )
        .await;
        wait_settled(&s).await;
        // One call of the judge itself and one of distillation.
        let judge = s.judge();
        tokio::task::spawn_blocking(move || {
            judge.classify_constant(scenarios::CLASSIFY_STATE, scenarios::CLASSIFY_OPTIONS)
        })
        .await
        .unwrap()
        .unwrap();
        let answers = ask_distill(
            &s,
            "proj",
            "Project: proj\nwe chose WAL",
            "we chose WAL",
            0.5,
        )
        .await
        .unwrap();
        assert!(answers.contains_key("c1"), "{answers:?}");
        // Both rows are written fire-and-forget through the writer.
        let rows = until("both ledger rows", || {
            let rows = futures::executor::block_on(
                s.read(|c| alice_core::store::judge::cost_since(c, "2000-01-01")),
            )
            .ok()?;
            let has = |p: &str| rows.iter().any(|r| r.provider == p);
            (has(LEDGER_JUDGE) && has(LEDGER_DISTILL)).then_some(rows)
        })
        .await;
        let row = |p: &str| rows.iter().find(|r| r.provider == p).unwrap().clone();
        let (judge_row, distill_row) = (row(LEDGER_JUDGE), row(LEDGER_DISTILL));
        assert_eq!(distill_row.calls, 1, "{rows:?}");
        assert!(judge_row.usd > 0.0 && distill_row.usd > 0.0, "{rows:?}");
        let total = judge_row.usd + distill_row.usd;
        let stats = s.stats().await.unwrap();
        assert!((stats.cost_today_usd - total).abs() < 1e-12, "{stats:?}");
        assert!((stats.cost_7d_usd - total).abs() < 1e-12, "{stats:?}");
        assert!(
            (stats.distill_cost_today_usd - distill_row.usd).abs() < 1e-12,
            "{stats:?}"
        );
        assert_eq!((stats.distilled_today, stats.distill_dropped_today), (0, 0));
        // The cache keeps one provider: the distill answer is a `jev` entry like any other.
        let cache: Vec<String> = s
            .read(|c| {
                let mut stmt = c.prepare("SELECT provider FROM judge_cache")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<std::result::Result<_, _>>()?)
            })
            .await
            .unwrap();
        assert!(
            cache.len() >= 2 && cache.iter().all(|p| p == "jev"),
            "{cache:?}"
        );
        // The judge's store reads the day's total, and each row on its own.
        let store = crate::judge_http::DaemonJevStore {
            readers: s.readers.clone(),
            writer: s.writer.clone(),
        };
        let today = judge_row.day.clone();
        assert!((store.spent(&today).unwrap() - total).abs() < 1e-12);
        assert!((store.spent_by(&today, LEDGER_DISTILL).unwrap() - distill_row.usd).abs() < 1e-12);
        assert!((store.spent_by(&today, LEDGER_JUDGE).unwrap() - judge_row.usd).abs() < 1e-12);
        // The doctor's 80 % warning counts both rows: neither reaches it alone, together they do.
        let budget = total / 0.85;
        assert!(judge_row.usd < 0.8 * budget && distill_row.usd < 0.8 * budget);
        s.patch_settings(patch_one(
            "judge.daily_budget_usd",
            serde_json::json!(budget),
        ))
        .unwrap();
        let checks = daemon_checks(&s).await.unwrap();
        let warn = checks
            .iter()
            .find(|c| c.name == "judge budget")
            .unwrap_or_else(|| panic!("no budget warning: {checks:?}"));
        assert_eq!(warn.status, CheckStatus::Warn, "{warn:?}");
        assert!(warn.detail.contains("(distillation $"), "{warn:?}");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ask_distill_passes_the_cloud_gate_and_redacts() {
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let t = crate::judge_http::ScriptedJev::new(("unrelated", 1.0));
        let (d, s) = svc_with_jev(t.clone()).await;
        remember_id(
            &s,
            "openproj first note",
            Some(Kind::Note),
            &repo(&d, "openproj"),
        )
        .await;
        remember_id(
            &s,
            "closedproj first note",
            Some(Kind::Note),
            &repo(&d, "closedproj"),
        )
        .await;
        wait_settled(&s).await;
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["closedproj"]),
        ))
        .unwrap();
        let before = t.sent();
        let e = ask_distill(
            &s,
            "closedproj",
            "Project: closedproj\nthe closed plan",
            "the closed plan",
            0.5,
        )
        .await
        .unwrap_err();
        assert_eq!(e, JudgeError::Denied);
        assert_eq!(t.sent(), before, "a denied project is never sent");
        assert!(!t.mentioned("the closed plan"));
        // An allowed project goes, redacted: the state and the question alike.
        let state = format!("Project: openproj\nthe deploy token is {token}");
        let answers = ask_distill(&s, "openproj", &state, &format!("deploy with {token}"), 0.5)
            .await
            .unwrap();
        assert!(answers.contains_key("c1"), "{answers:?}");
        assert_eq!(t.sent(), before + 1);
        assert!(t.mentioned("the deploy token is"), "the state was sent");
        assert!(t.mentioned("[REDACTED"), "where the token was");
        assert!(!t.mentioned(token), "the token reached the model");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ask_distill_is_denied_for_the_heuristic() {
        let d = tempfile::tempdir().unwrap();
        let t = crate::judge_http::ScriptedJev::new(("unrelated", 1.0));
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                jev_transport: Some(t.clone()),
                ..quiet()
            },
        )
        .unwrap();
        // A Jev key is stored, but the settings ask for the heuristic.
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        remember_id(&s, "proj first note", Some(Kind::Note), &repo(&d, "proj")).await;
        wait_settled(&s).await;
        assert_eq!(s.judge().provider(), "heuristic");
        let e = ask_distill(
            &s,
            "proj",
            "Project: proj\nwe chose WAL",
            "we chose WAL",
            0.5,
        )
        .await
        .unwrap_err();
        assert_eq!(e, JudgeError::Denied);
        assert_eq!(t.sent(), 0, "the heuristic sends nothing");
        // Nothing was spent, and nothing shows as `-0.0` (an empty f64 sum is negative zero).
        let st = s.stats().await.unwrap();
        for usd in [st.cost_today_usd, st.cost_7d_usd, st.distill_cost_today_usd] {
            assert!(usd == 0.0 && usd.is_sign_positive(), "{st:?}");
        }
        s.shutdown();
    }

    // ------------------------------------------------------------------ fix round 1

    async fn open_reviews(s: &Service, kind: &str) -> Vec<ReviewItem> {
        s.reviews(ReviewQuery::default())
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.review_type.as_str() == kind)
            .collect()
    }

    fn near_miss(port: u32) -> String {
        format!("deploy checklist port {port} {}", "common step ".repeat(30))
    }

    /// C1: the heuristic merges the same text only; a number that differs is a question.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_near_miss_number_is_asked_about_not_merged() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        let a = remember_id(&s, &near_miss(5432), Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        let b = remember_id(&s, &near_miss(5433), Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        assert_eq!(state_of(&s, &a).await.1, "active");
        assert_eq!(
            state_of(&s, &b).await,
            ("done".to_string(), "active".to_string())
        );
        let asks = open_reviews(&s, "merge").await;
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert_eq!(asks[0].payload["survivor"], a.as_str());
        assert_eq!(asks[0].payload["provider"], "heuristic");
        s.shutdown();
    }

    /// C1/M7: a heuristic that answers because the judge could not (an outage, or no key) only
    /// suggests, even for the same text.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_outage_fallback_and_a_stand_in_heuristic_apply_nothing() {
        let text = "Use WAL mode so readers never block the writer";
        // Outage.
        let t = crate::judge_http::ScriptedJev::failing(503);
        let (d, s) = svc_with_jev(t.clone()).await;
        let cwd = repo(&d, "proj");
        let a = remember_id(&s, text, Some(Kind::Decision), &cwd).await;
        wait_settled(&s).await;
        let b = remember_id(&s, text, Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        assert!(t.sent() >= 1, "the comparison was tried");
        assert_eq!(state_of(&s, &a).await.1, "active");
        assert_eq!(
            state_of(&s, &b).await,
            ("retry".to_string(), "active".to_string())
        );
        assert_eq!(open_reviews(&s, "merge").await.len(), 1);
        s.shutdown();
        // No key: the heuristic stands in for Jev.
        let (d, s) = svc().await;
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        until("the stand-in", || {
            s.judge_warning().filter(|w| w.contains("no TypeSafe key"))
        })
        .await;
        let cwd = repo(&d, "proj");
        let a = remember_id(&s, text, Some(Kind::Decision), &cwd).await;
        wait_settled(&s).await;
        let b = remember_id(&s, text, Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        assert_eq!(state_of(&s, &a).await.1, "active");
        assert_eq!(
            state_of(&s, &b).await,
            ("retry".to_string(), "active".to_string())
        );
        assert_eq!(open_reviews(&s, "merge").await.len(), 1);
        s.shutdown();
    }

    /// I1: two twins judged in one batch meet once, and the older survives.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_older_of_two_twins_judged_together_survives() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        let text = "Use WAL mode so readers never block the writer";
        let a = remember_id(&s, text, Some(Kind::Decision), &cwd).await;
        let b = remember_id(&s, text, Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        assert_eq!(state_of(&s, &a).await.1, "active", "the older survives");
        assert_eq!(state_of(&s, &b).await.1, "trashed");
        assert!(s.reviews(ReviewQuery::default()).await.unwrap().is_empty());
        s.shutdown();
    }

    /// M10: a judge that is told not to change anything only activates.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_judge_that_does_not_apply_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                judge_applies: false,
                ..quiet()
            },
        )
        .unwrap();
        let cwd = repo(&d, "proj");
        let text = "Use WAL mode so readers never block the writer";
        let a = remember_id(&s, text, Some(Kind::Decision), &cwd).await;
        let b = remember_id(&s, text, Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        assert_eq!(state_of(&s, &a).await.1, "active");
        assert_eq!(
            state_of(&s, &b).await,
            ("done".to_string(), "active".to_string())
        );
        assert!(s.reviews(ReviewQuery::default()).await.unwrap().is_empty());
        s.shutdown();
    }

    /// M8: the local router looks first; a memory that names a project the cloud may not see
    /// stays on this machine.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_memory_naming_a_denied_project_is_not_sent_to_a_model_to_route() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            project: Some(("allowedproj", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        let denied = repo(&d, "deniedproj");
        std::fs::write(
            std::path::Path::new(denied.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        remember_id(&s, "deniedproj keeps notes", Some(Kind::Note), &denied).await;
        remember_id(
            &s,
            "allowedproj keeps a wiki",
            Some(Kind::Note),
            &repo(&d, "allowedproj"),
        )
        .await;
        wait_settled(&s).await;
        let id = remember_id(
            &s,
            "release notes about deniedproj",
            Some(Kind::Note),
            &None,
        )
        .await;
        wait_settled(&s).await;
        assert_eq!(t.sent(), 0, "the memory never left");
        assert_eq!(memory_of(&s, &id).await.memory.project, "global");
        let asks = open_reviews(&s, "project").await;
        assert!(
            asks.iter().any(|r| r.payload["to"] == "deniedproj"),
            "{asks:?}"
        );
        s.shutdown();
    }

    /// M4: neighbours in the project are found even when the nearest vectors are all in others.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_search_widens_when_the_nearest_memories_belong_to_other_projects() {
        let t = crate::judge_http::ScriptedJev::new(("updates", 0.95));
        let (d, s) = svc_with_jev(t).await;
        let mine = repo(&d, "mine");
        let theirs = repo(&d, "theirs");
        let old = remember_id(
            &s,
            "Staging Postgres listens on port 5432",
            Some(Kind::Fact),
            &mine,
        )
        .await;
        wait_settled(&s).await;
        let text = "Staging Postgres listens on port 5433 now";
        for kind in [
            Kind::Note,
            Kind::Fact,
            Kind::Decision,
            Kind::Bugfix,
            Kind::Howto,
            Kind::Warning,
        ] {
            remember_id(&s, text, Some(kind), &theirs).await;
            wait_settled(&s).await;
        }
        // Identical to all six of the other project's, so they are its nearest; the search
        // starts two deep in tests.
        remember_id(&s, text, Some(Kind::Note), &mine).await;
        wait_settled(&s).await;
        assert_eq!(
            memory_of(&s, &old).await.memory.status.as_str(),
            "superseded"
        );
        s.shutdown();
    }

    /// M3: a judge that is slow to answer does not hold up shutdown.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_does_not_wait_for_a_slow_judge() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            delay: Duration::from_secs(8),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_id(&s, "deploy notes for staging", Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        remember_id(
            &s,
            "deploy notes for staging servers",
            Some(Kind::Note),
            &cwd,
        )
        .await;
        until("the judge inside its request", || {
            (t.sent() >= 1).then_some(())
        })
        .await;
        let started = Instant::now();
        let s2 = s.clone();
        tokio::task::spawn_blocking(move || s2.shutdown())
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(s.read_only().is_none(), "a clean stop, not a failure");
    }

    // ------------------------------------------------------------------ fix round 2

    /// N3: when the local router says a memory is about a project the cloud may not see, its
    /// comparison and its kind question stay local too.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_memory_about_a_denied_project_stays_local_for_comparison_and_kind() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("updates", 0.95),
            kind: Some(("bugfix", 0.95)),
            project: Some(("allowedproj", 0.95)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        let denied = repo(&d, "deniedproj");
        std::fs::write(
            std::path::Path::new(denied.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        remember_id(&s, "deniedproj keeps notes", Some(Kind::Note), &denied).await;
        remember_id(
            &s,
            "allowedproj keeps a wiki",
            Some(Kind::Note),
            &repo(&d, "allowedproj"),
        )
        .await;
        wait_settled(&s).await;
        let old = remember_id(
            &s,
            "release notes about deniedproj",
            Some(Kind::Note),
            &None,
        )
        .await;
        wait_settled(&s).await;
        let new = remember_id(&s, "release notes about deniedproj again", None, &None).await;
        wait_settled(&s).await;
        assert_eq!(
            t.sent(),
            0,
            "nothing about it was sent: no comparison, kind or route"
        );
        assert!(!t.mentioned("deniedproj"));
        assert_eq!(memory_of(&s, &new).await.memory.kind, Kind::Note);
        assert_eq!(
            memory_of(&s, &old).await.memory.status.as_str(),
            "active",
            "no model update"
        );
        s.shutdown();
    }

    // ------------------------------------------------------------------ predicates

    fn fact_req(subject: &str, predicate: &str, object: &str) -> AssertFactRequest {
        AssertFactRequest {
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_predicate_maps_to_an_identical_stem_or_is_suggested() {
        let (_d, s) = svc().await;
        let f = |p: &str, o: &str| fact_req("alice", p, o);
        s.assert_fact(f("uses", "SQLite")).await.unwrap();
        s.assert_fact(f("deployed_on", "Coolify")).await.unwrap();
        // Same stem: the heuristic maps it (an identical predicate is a duplicate).
        let out = s.assert_fact(f("use", "Rust")).await.unwrap();
        assert_eq!(
            (out.predicate.as_str(), out.suggestion.as_deref()),
            ("uses", None)
        );
        // Similar but not the same: stored as given, suggested, and asked in the review inbox.
        let out = s.assert_fact(f("deployed_to", "Railway")).await.unwrap();
        assert_eq!(
            (out.predicate.as_str(), out.suggestion.as_deref()),
            ("deployed_to", Some("deployed_on"))
        );
        let open = s.reviews(ReviewQuery::default()).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].review_type.as_str(), "predicate");
        assert_eq!(open[0].payload["canonical"], "deployed_on");
        // Asking again opens no second item.
        s.assert_fact(f("deployed_to", "Fly")).await.unwrap();
        assert_eq!(s.reviews(ReviewQuery::default()).await.unwrap().len(), 1);
        // An unrelated new predicate: nothing to say.
        let out = s.assert_fact(f("owns", "a cat")).await.unwrap();
        assert_eq!(out.suggestion, None);
        // The mapping is a recorded, undoable event with the fact.
        let events = s
            .events(EventsRequest {
                limit: Some(50),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(events.iter().any(|e| e.action == "predicate.map"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jev_maps_a_paraphrased_predicate_when_sure() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            predicate: Some(("runs_on", 0.93)),
            ..Default::default()
        });
        let (_d, s) = svc_with_jev(t.clone()).await;
        let f = |p: &str, o: &str| fact_req("api", p, o);
        s.assert_fact(f("runs_on", "k8s")).await.unwrap();
        assert_eq!(t.sent(), 0, "a known predicate is not asked about");
        let out = s.assert_fact(f("is_hosted_on", "fly.io")).await.unwrap();
        assert_eq!(out.predicate, "runs_on");
        assert_eq!(t.sent(), 1);
        let facts = s
            .facts(FactsRequest {
                entity: "api".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(facts.facts.iter().all(|f| f.predicate == "runs_on"));
        assert_eq!(facts.facts.len(), 2);
        // Later uses of the new name resolve without asking again.
        s.assert_fact(f("is_hosted_on", "aws")).await.unwrap();
        assert_eq!(t.sent(), 1);
    }

    /// F3: the same words the other way round are a different predicate: even a sure judge
    /// only gets a review item, and the heuristic never reaches the auto-apply threshold.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_predicate_that_reads_the_other_way_round_is_never_mapped_without_a_person() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            predicate: Some(("depends_on", 0.97)),
            ..Default::default()
        });
        let (_d, s) = svc_with_jev(t.clone()).await;
        s.assert_fact(fact_req("api", "depends_on", "postgres"))
            .await
            .unwrap();
        let out = s
            .assert_fact(fact_req("postgres", "depended_on_by", "api"))
            .await
            .unwrap();
        assert_eq!(t.sent(), 1, "the judge was asked");
        assert_eq!(out.predicate, "depended_on_by", "stored as given");
        assert_eq!(out.suggestion.as_deref(), Some("depends_on"));
        let open = s.reviews(ReviewQuery::default()).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].review_type.as_str(), "predicate");
        assert_eq!(open[0].payload["reversed"], true);
        // The offline heuristic gets the same answer.
        let (_d2, h) = svc().await;
        h.assert_fact(fact_req("api", "depends_on", "postgres"))
            .await
            .unwrap();
        let out = h
            .assert_fact(fact_req("postgres", "depended_on_by", "api"))
            .await
            .unwrap();
        assert_eq!(out.predicate, "depended_on_by");
        assert_eq!(out.suggestion.as_deref(), Some("depends_on"));
    }

    /// A fact that cites a memory of a project the cloud may not see keeps its predicate names
    /// away from the cloud judge; the heuristic decides instead.
    #[tokio::test(flavor = "multi_thread")]
    async fn predicate_names_of_a_denied_project_stay_local() {
        let t = Arc::new(crate::judge_http::ScriptedJev {
            relation: ("unrelated", 1.0),
            predicate: Some(("runs_on", 0.93)),
            ..Default::default()
        });
        let (d, s) = svc_with_jev(t.clone()).await;
        let cwd = repo(&d, "filed");
        std::fs::write(
            std::path::Path::new(cwd.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        let cited = remember_id(&s, "Client X runs its API on Fly", Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        s.assert_fact(fact_req("api", "runs_on", "k8s"))
            .await
            .unwrap();
        let before = t.sent();
        let mut req = fact_req("api", "is_hosted_on", "fly.io");
        req.source_memory_id = Some(cited);
        let out = s.assert_fact(req).await.unwrap();
        assert_eq!(t.sent(), before, "nothing from a denied project is sent");
        assert_eq!(
            out.predicate, "is_hosted_on",
            "the heuristic did not match it"
        );
        // Without such a source the judge is asked.
        let out = s
            .assert_fact(fact_req("api", "is_run_on", "aws"))
            .await
            .unwrap();
        assert_eq!(t.sent(), before + 1);
        assert_eq!(out.predicate, "runs_on");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_judge_that_fails_or_is_denied_never_fails_the_fact() {
        let t = crate::judge_http::ScriptedJev::failing(503);
        let (_d, s) = svc_with_jev(t.clone()).await;
        s.assert_fact(fact_req("api", "deployed_on", "Coolify"))
            .await
            .unwrap();
        let out = s
            .assert_fact(fact_req("api", "deploys_on", "Fly"))
            .await
            .unwrap();
        assert_eq!(out.predicate, "deployed_on", "the heuristic decided");
        s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(0.0)))
            .unwrap();
        // The failed calls above may still be retrying in the background: count from when the
        // transport has been quiet for a while, so only a request of the next call is counted.
        let mut before = t.sent();
        let quiet_by = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(
                Instant::now() < quiet_by,
                "the failing transport never went quiet"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = t.sent();
            if now == before {
                break;
            }
            before = now;
        }
        let out = s
            .assert_fact(fact_req("api", "deploying_on", "Railway"))
            .await
            .unwrap();
        assert_eq!(out.predicate, "deployed_on");
        assert_eq!(t.sent(), before, "no request against a budget of zero");
    }

    // ------------------------------------------------------------------ precise recall

    /// Grades results that contain `keyword` as answering the query, the rest as unrelated;
    /// every other question (the judge worker's) is answered `unrelated`. Counts requests.
    struct KeywordGrader {
        keyword: &'static str,
        grading_calls: std::sync::atomic::AtomicUsize,
        fail_grading: AtomicBool,
        /// Milliseconds a grading request takes before it answers.
        grading_delay_ms: std::sync::atomic::AtomicU64,
        /// The request timeout the judge passed with the last grading request, in milliseconds.
        last_timeout_ms: std::sync::atomic::AtomicU64,
        /// The states of the grading requests.
        graded_states: Mutex<Vec<String>>,
    }

    impl KeywordGrader {
        fn new(keyword: &'static str) -> Arc<Self> {
            Arc::new(Self {
                keyword,
                grading_calls: Default::default(),
                fail_grading: AtomicBool::new(false),
                grading_delay_ms: Default::default(),
                last_timeout_ms: Default::default(),
                graded_states: Default::default(),
            })
        }

        fn graded(&self) -> usize {
            self.grading_calls.load(Ordering::SeqCst)
        }
    }

    impl alice_core::judge::jev::JevTransport for KeywordGrader {
        fn post(
            &self,
            _endpoint: &str,
            _key: &str,
            body: &[u8],
            timeout: Duration,
        ) -> std::result::Result<
            alice_core::judge::jev::HttpReply,
            alice_core::judge::jev::TransportError,
        > {
            let req: serde_json::Value = serde_json::from_slice(body).unwrap();
            let state = req["state"].as_str().unwrap();
            let mut answers = serde_json::Map::new();
            let mut grading = false;
            for (qid, q) in req["questions"].as_object().unwrap() {
                let options: Vec<String> =
                    q["criteria"].as_object().unwrap().keys().cloned().collect();
                let (choice, other) = if let Some(k) = qid.strip_prefix("r_") {
                    grading = true;
                    let k: usize = k.parse().unwrap();
                    let block = state
                        .split(&format!("Result {k} ("))
                        .nth(1)
                        .unwrap_or("")
                        .split("\nResult ")
                        .next()
                        .unwrap_or("");
                    if block.contains(self.keyword) {
                        ("answers", "unrelated")
                    } else {
                        ("unrelated", "answers")
                    }
                } else {
                    ("unrelated", "duplicate")
                };
                let probs: serde_json::Map<String, serde_json::Value> = options
                    .iter()
                    .map(|o| {
                        let p = if o == choice {
                            0.9
                        } else if o == other {
                            0.05
                        } else {
                            0.05 / (options.len().max(3) - 2) as f64
                        };
                        (o.clone(), serde_json::json!(p))
                    })
                    .collect();
                answers.insert(
                    qid.clone(),
                    serde_json::json!({"type": "choice", "choice": choice, "confidence": 0.9, "probabilities": probs}),
                );
            }
            if grading {
                self.graded_states.lock().push(state.to_string());
                self.grading_calls.fetch_add(1, Ordering::SeqCst);
                self.last_timeout_ms
                    .store(timeout.as_millis() as u64, Ordering::SeqCst);
                let delay = self.grading_delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    std::thread::sleep(Duration::from_millis(delay));
                }
                if self.fail_grading.load(Ordering::SeqCst) {
                    return Ok(alice_core::judge::jev::HttpReply {
                        status: 503,
                        body: b"{}".to_vec(),
                        retry_after: Some(Duration::ZERO),
                    });
                }
            }
            let resp = serde_json::json!({"model": "jev-1.13.0", "answers": answers,
                "usage": {"input_tokens": 800, "output_tokens": 0}});
            Ok(alice_core::judge::jev::HttpReply {
                status: 200,
                body: serde_json::to_vec(&resp).unwrap(),
                retry_after: None,
            })
        }
    }

    async fn svc_with_grader(t: Arc<KeywordGrader>) -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                jev_transport: Some(t),
                ..quiet()
            },
        )
        .unwrap();
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        (d, s)
    }

    const DEPLOY_NOTES: [&str; 3] = [
        "deploy notes: staging deploy runs make deploy",
        "deploy notes: the ZEBRA cluster deploy uses blue green",
        "deploy notes: deploy freeze on Fridays",
    ];

    fn recall_req(query: &str, mode: RecallMode, cwd: &Option<String>) -> RecallRequest {
        RecallRequest {
            query: query.into(),
            mode: Some(mode),
            cwd: cwd.clone(),
            format: OutputFormat::Json,
            ..Default::default()
        }
    }

    async fn remember_deploy_notes(s: &Service, cwd: &Option<String>) {
        for text in DEPLOY_NOTES {
            remember_id(s, text, Some(Kind::Howto), cwd).await;
        }
        wait_settled(s).await;
    }

    /// Grades a result by the first of `scores` its block holds: `(marker, P(answers))`; other
    /// questions are answered `unrelated`. Keeps the grading states.
    struct ScoreGrader {
        scores: Vec<(&'static str, f64)>,
        states: Mutex<Vec<String>>,
    }

    impl alice_core::judge::jev::JevTransport for ScoreGrader {
        fn post(
            &self,
            _endpoint: &str,
            _key: &str,
            body: &[u8],
            _timeout: Duration,
        ) -> std::result::Result<
            alice_core::judge::jev::HttpReply,
            alice_core::judge::jev::TransportError,
        > {
            let req: serde_json::Value = serde_json::from_slice(body).unwrap();
            let state = req["state"].as_str().unwrap();
            let mut answers = serde_json::Map::new();
            for (qid, q) in req["questions"].as_object().unwrap() {
                let options: Vec<String> =
                    q["criteria"].as_object().unwrap().keys().cloned().collect();
                let p = qid.strip_prefix("r_").map(|k| {
                    let k: usize = k.parse().unwrap();
                    let block = state
                        .split(&format!("Result {k} ("))
                        .nth(1)
                        .unwrap_or("")
                        .split("\nResult ")
                        .next()
                        .unwrap_or("")
                        .to_string();
                    self.scores
                        .iter()
                        .find(|(m, _)| block.contains(m))
                        .map_or(0.02, |(_, p)| *p)
                });
                let (choice, probs): (&str, serde_json::Map<String, serde_json::Value>) = match p {
                    Some(p) => (
                        if p >= 0.5 { "answers" } else { "unrelated" },
                        options
                            .iter()
                            .map(|o| {
                                let v = match o.as_str() {
                                    "answers" => p,
                                    "unrelated" => 1.0 - p - 0.01,
                                    _ => 0.01 / (options.len().max(3) - 2) as f64,
                                };
                                (o.clone(), serde_json::json!(v))
                            })
                            .collect(),
                    ),
                    None => (
                        "unrelated",
                        options
                            .iter()
                            .map(|o| {
                                let v = if o == "unrelated" {
                                    0.9
                                } else {
                                    0.1 / options.len() as f64
                                };
                                (o.clone(), serde_json::json!(v))
                            })
                            .collect(),
                    ),
                };
                if p.is_some() {
                    self.states.lock().push(state.to_string());
                }
                answers.insert(
                    qid.clone(),
                    serde_json::json!({"type": "choice", "choice": choice, "confidence": 0.9, "probabilities": probs}),
                );
            }
            let resp = serde_json::json!({"model": "jev-1.13.0", "answers": answers,
                "usage": {"input_tokens": 800, "output_tokens": 0}});
            Ok(alice_core::judge::jev::HttpReply {
                status: 200,
                body: serde_json::to_vec(&resp).unwrap(),
                retry_after: None,
            })
        }
    }

    /// Final review C-I1: the judge grades an outdated fact as answering too; precise mode tells
    /// it the memory was superseded and keeps the status prior, so the replacement leads even
    /// when the old version is graded a little higher.
    #[tokio::test(flavor = "multi_thread")]
    async fn precise_mode_keeps_a_replacement_above_the_memory_it_superseded() {
        let t = Arc::new(ScoreGrader {
            scores: vec![("5432", 0.93), ("5433", 0.91)],
            states: Default::default(),
        });
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                jev_transport: Some(t.clone()),
                ..quiet()
            },
        )
        .unwrap();
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        let cwd = repo(&d, "proj");
        let old = remember_id(
            &s,
            "Postgres in staging listens on port 5432",
            Some(Kind::Fact),
            &cwd,
        )
        .await;
        s.remember(RememberRequest {
            supersedes: vec![old.clone()],
            ..remember_req(
                "Postgres in staging now listens on port 5433",
                Some(Kind::Fact),
                &cwd,
            )
        })
        .await
        .unwrap();
        wait_settled(&s).await;
        assert_eq!(state_of(&s, &old).await.1, "superseded");
        let r = s
            .recall(recall_req(
                "which port does Postgres in staging listen on",
                RecallMode::Precise,
                &cwd,
            ))
            .await
            .unwrap();
        assert!(r.reranked, "{:?}", r.notes);
        let titles: Vec<&str> = r.results.iter().map(|h| h.title.as_str()).collect();
        assert!(titles[0].contains("5433"), "{titles:?}");
        let old_hit = r
            .results
            .iter()
            .find(|h| h.id == old)
            .expect("the old one is shown");
        assert!(
            old_hit.relevance.unwrap() > r.results[0].relevance.unwrap(),
            "graded higher"
        );
        assert!(
            t.states
                .lock()
                .iter()
                .any(|st| st.contains("status superseded: a newer memory replaced it")),
            "the judge was told"
        );
        s.shutdown();
    }

    /// A fake embedder whose query embedding fails with `fail` while it is set (a text that
    /// starts with `MODEL_LOADING` fails as an unloaded model does, any other as a provider
    /// that is down).
    struct FlakyQueries {
        inner: FakeEmbedder,
        fail: Arc<Mutex<Option<String>>>,
    }

    impl Embedder for FlakyQueries {
        fn model_id(&self) -> &str {
            self.inner.model_id()
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.inner.embed_passages(texts)
        }
        fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
            match self.fail.lock().clone() {
                Some(m) if m.starts_with(alice_core::embed::MODEL_LOADING) => {
                    Err(AliceError::Other(m))
                }
                Some(m) => Err(AliceError::Unavailable(m)),
                None => self.inner.embed_query(text),
            }
        }
    }

    /// Final review C-I2: a recall whose query was not embedded (the model reloading after an
    /// idle unload, a provider that is down) says it used word matches only, and is never
    /// flagged `low_confidence`: meaning-search did not run.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recall_whose_query_was_not_embedded_says_so_and_is_not_low_confidence() {
        let grader = KeywordGrader::new("WOMBAT");
        let fail: Arc<Mutex<Option<String>>> = Default::default();
        let d = tempfile::tempdir().unwrap();
        let f = fail.clone();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &move || {
                Ok(Arc::new(FlakyQueries {
                    inner: FakeEmbedder::new(384),
                    fail: f.clone(),
                }) as Arc<dyn Embedder>)
            },
            ServiceOptions {
                jev_transport: Some(grader.clone()),
                ..quiet()
            },
        )
        .unwrap();
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        // With a query vector: graded, nothing answers, and it says so.
        let r = s
            .recall(recall_req(VAGUE, RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(r.reranked && r.low_confidence, "{:?}", r.notes);
        for (why, said) in [
            (
                alice_core::embed::MODEL_LOADING.to_string(),
                alice_core::embed::MODEL_LOADING,
            ),
            (
                "the provider did not answer".to_string(),
                "this search used word matches only: the embedding provider is unavailable",
            ),
        ] {
            *fail.lock() = Some(why);
            let r = s
                .recall(recall_req(VAGUE, RecallMode::Precise, &cwd))
                .await
                .unwrap();
            assert!(!r.results.is_empty(), "words still find them");
            assert!(r.notes.iter().any(|n| n == said), "{:?}", r.notes);
            assert!(!r.low_confidence, "{:?}", r.notes);
            assert!(
                r.notes.iter().all(|n| n != NOTHING_ANSWERS),
                "{:?}",
                r.notes
            );
            let text = s
                .recall(RecallRequest {
                    format: OutputFormat::Text,
                    ..recall_req(VAGUE, RecallMode::Fast, &cwd)
                })
                .await
                .unwrap();
            assert!(text.text.unwrap().contains("word matches only"));
        }
        s.shutdown();
    }

    /// Two checkouts of one repository (same remote), for the tests of a project's trees.
    fn checkouts(d: &tempfile::TempDir, names: [&str; 2]) -> [std::path::PathBuf; 2] {
        names.map(|n| {
            let dir = d.path().join(n);
            std::fs::create_dir(&dir).unwrap();
            for args in [
                &["init", "-q"][..],
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.com/me/client-x.git",
                ],
            ] {
                let ok = std::process::Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .status()
                    .is_ok_and(|s| s.success());
                assert!(ok, "git {args:?}");
            }
            dir
        })
    }

    /// Final re-review N-I1: a recall from a stored project is judged by the tree it comes
    /// from too. The project was last written from a worktree without `.alice.toml` and nothing
    /// is kept for it yet (every project right after the A-C1 build); a precise recall from the
    /// main checkout, whose file denies the cloud, sends nothing to the judge, and what it read
    /// is kept: a recall from the worktree is denied as well.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recall_from_a_tree_whose_file_denies_the_cloud_never_reaches_the_judge() {
        let grader = KeywordGrader::new("narwhal");
        let (d, s) = svc_with_grader(grader.clone()).await;
        let [main, feat] = checkouts(&d, ["client-x", "client-x-feat"]);
        std::fs::write(main.join(".alice.toml"), "[privacy]\ncloud_denied = true\n").unwrap();
        let (main, feat) = (
            Some(main.to_string_lossy().into_owned()),
            Some(feat.to_string_lossy().into_owned()),
        );
        remember_id(
            &s,
            "the narwhal tusk is in the vault",
            Some(Kind::Fact),
            &feat,
        )
        .await;
        remember_id(
            &s,
            "the narwhal migrates north in spring",
            Some(Kind::Fact),
            &feat,
        )
        .await;
        wait_settled(&s).await;
        let before = grader.graded();
        for (from, cwd) in [("the main checkout", &main), ("the worktree", &feat)] {
            let r = s
                .recall(recall_req(
                    "where is the narwhal tusk",
                    RecallMode::Precise,
                    cwd,
                ))
                .await
                .unwrap();
            assert!(!r.results.is_empty(), "{from}: words still find them");
            assert!(!r.reranked, "{from}: {:?}", r.notes);
            assert!(
                r.notes.iter().any(|n| n == NOTE_DENIED),
                "{from}: {:?}",
                r.notes
            );
            assert_eq!(grader.graded(), before, "{from}: nothing went to the judge");
        }
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn precise_mode_reorders_by_the_judges_relevance_and_fast_mode_does_not() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let fast = s
            .recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        assert!(!fast.reranked);
        assert_eq!(t.graded(), 0, "fast mode never asks the judge to grade");
        let order = |r: &RecallResponse| {
            r.results
                .iter()
                .map(|h| h.title.clone())
                .collect::<Vec<_>>()
        };
        assert_ne!(
            fast.results[0].title,
            DEPLOY_NOTES[1],
            "the test needs the judge to change the order: {:?}",
            order(&fast)
        );
        let precise = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(precise.reranked);
        assert_eq!(t.graded(), 1, "one batched call");
        assert!(
            precise.results[0].title.contains("ZEBRA"),
            "{:?}",
            order(&precise)
        );
        assert_eq!(precise.results.len(), fast.results.len());
        let mut a = order(&fast);
        let mut b = order(&precise);
        a.sort();
        b.sort();
        assert_eq!(a, b, "the same results, in another order");
        // Text format carries the flag too.
        let text = s
            .recall(RecallRequest {
                format: OutputFormat::Text,
                ..recall_req("deploy notes", RecallMode::Precise, &cwd)
            })
            .await
            .unwrap();
        assert!(text.reranked && text.text.unwrap().contains("ZEBRA"));
    }

    /// A vague two-word question: as a phrase it is too short to count as an exact match.
    const VAGUE: &str = "deploy notes";
    const NOTHING_ANSWERS: &str = "nothing found clearly answers this; the closest matches follow";

    #[tokio::test(flavor = "multi_thread")]
    async fn a_precise_recall_where_nothing_answers_says_so_and_one_that_answers_does_not() {
        // No note contains the keyword: the judge grades every result "unrelated".
        let none = KeywordGrader::new("WOMBAT");
        let (d, s) = svc_with_grader(none.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let r = s
            .recall(recall_req(VAGUE, RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(r.reranked && !r.results.is_empty(), "{r:?}");
        assert!(r.low_confidence, "{:?}", r.notes);
        assert!(
            r.notes.iter().any(|n| n == NOTHING_ANSWERS),
            "{:?}",
            r.notes
        );
        let text = s
            .recall(RecallRequest {
                format: OutputFormat::Text,
                ..recall_req(VAGUE, RecallMode::Precise, &cwd)
            })
            .await
            .unwrap();
        assert!(text.low_confidence, "{:?}", text.notes);
        assert!(text.text.unwrap().contains(NOTHING_ANSWERS));
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["low_confidence"], true, "{json}");
        // Fast mode never says it: the judge graded nothing.
        let fast = s
            .recall(recall_req(VAGUE, RecallMode::Fast, &cwd))
            .await
            .unwrap();
        assert!(
            !fast.low_confidence && fast.notes.is_empty(),
            "{:?}",
            fast.notes
        );

        // One result that answers clears the doubt.
        let some = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(some).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(r.reranked && !r.low_confidence, "{:?}", r.notes);
        assert!(r.notes.iter().all(|n| n != NOTHING_ANSWERS));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_verbatim_marker_or_phrase_in_the_top_hit_is_not_low_confidence() {
        // The judge grades every result "unrelated" (it knows no WOMBAT).
        let (d, s) = svc_with_grader(KeywordGrader::new("WOMBAT")).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        remember_id(
            &s,
            "release marker marmot-cobalt-4417 sits in the vault",
            Some(Kind::Note),
            &cwd,
        )
        .await;
        wait_settled(&s).await;
        let vague = s
            .recall(recall_req(VAGUE, RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(vague.reranked && vague.low_confidence, "{:?}", vague.notes);
        // A code of 6+ characters with `-`, found verbatim in the top hit.
        let marker = s
            .recall(recall_req(
                "deploy marmot-cobalt-4417 notes",
                RecallMode::Precise,
                &cwd,
            ))
            .await
            .unwrap();
        assert!(marker.reranked, "{marker:?}");
        assert!(
            marker.results[0].title.contains("marmot-cobalt-4417")
                || marker.results[0]
                    .body
                    .as_deref()
                    .is_some_and(|b| b.contains("marmot-cobalt-4417")),
            "{:?}",
            marker.results[0]
        );
        assert!(!marker.low_confidence, "{:?}", marker.notes);
        assert!(marker.notes.iter().all(|n| n != NOTHING_ANSWERS));
    }

    fn hit_with(title: &str, body: &str) -> alice_core::api::Hit {
        alice_core::api::Hit {
            id: "x".into(),
            tier: Tier::Knowledge,
            kind: None,
            project: "p".into(),
            title: title.into(),
            snippet: String::new(),
            body: Some(body.into()),
            created_at: String::new(),
            score: 0.0,
            relevance: None,
            matched: vec![],
            status: None,
            source: None,
            author: None,
            cloud_ok: None,
            origin: None,
        }
    }

    #[test]
    fn exact_match_needs_a_real_phrase_or_a_distinctive_token() {
        let h = hit_with(
            "capital follow-up",
            "the capital city, readme.md and auto-save; deploy notes here; deploy the staging cluster today; merge_suggestions ran; marmot-cobalt-4417",
        );
        // Short queries are no exact match, even when they occur in the text.
        assert!(!exact_match("api", &h));
        assert!(!exact_match("deploy notes", &h));
        assert!(!exact_match("capital", &h));
        // Ordinary hyphenated words and filenames are not distinctive tokens.
        assert!(!exact_match("follow-up", &h));
        assert!(!exact_match("auto-save", &h));
        assert!(!exact_match("readme.md", &h));
        // A code with a digit, or an identifier with `_`, is.
        assert!(exact_match("marmot-cobalt-4417", &h));
        assert!(exact_match("what is merge_suggestions", &h));
        // Three words found verbatim, on word boundaries.
        assert!(exact_match("deploy the staging cluster", &h));
        assert!(!exact_match("eploy the staging", &h));
        // No spaces (Thai): 12 characters or more as a substring.
        let th = hit_with("บันทึก", "ระบบความจำสำหรับเอเจนต์");
        assert!(exact_match("ความจำสำหรับเอเจนต์", &th));
        assert!(!exact_match("ความจำ", &th));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_text_format_prints_the_note_before_the_results() {
        let (d, s) = svc_with_grader(KeywordGrader::new("WOMBAT")).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let text = s
            .recall(RecallRequest {
                format: OutputFormat::Text,
                ..recall_req(VAGUE, RecallMode::Precise, &cwd)
            })
            .await
            .unwrap()
            .text
            .unwrap();
        assert!(
            text.starts_with(&format!("({NOTHING_ANSWERS})\n")),
            "{text}"
        );
        assert!(text.contains("results"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_query_without_letters_or_digits_is_refused_like_an_empty_one() {
        let (d, s) = svc_with_grader(KeywordGrader::new("ZEBRA")).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        for q in ["?", "...", "", "  "] {
            let e = s
                .recall(recall_req(q, RecallMode::Precise, &cwd))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("query is empty; pass keywords or a question"),
                "{q:?}: {e}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graded_hits_carry_the_judges_relevance_and_keep_the_search_score() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let fast = s
            .recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        assert!(fast.results.iter().all(|h| h.relevance.is_none()));
        let fast_json = serde_json::to_value(&fast).unwrap();
        assert!(
            fast_json["results"][0].get("relevance").is_none(),
            "absent, not null: {fast_json}"
        );
        let precise = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(precise.reranked);
        // The display order is the judge's: relevance never rises down the list.
        let rel: Vec<f32> = precise
            .results
            .iter()
            .map(|h| h.relevance.expect("every result was graded"))
            .collect();
        assert!(rel.windows(2).all(|w| w[0] >= w[1]), "{rel:?}");
        assert!(
            rel[0] >= 0.5 && precise.results[0].title.contains("ZEBRA"),
            "{rel:?}"
        );
        // `score` stays the search's fusion score, whatever the display order.
        for h in &precise.results {
            let before = fast.results.iter().find(|f| f.id == h.id).unwrap();
            assert_eq!(h.score, before.score, "{}", h.title);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn precise_mode_keeps_the_search_order_when_the_judge_cannot_grade() {
        // The offline judge does not grade: precise is fast.
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let fast = s
            .recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        let precise = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(!precise.reranked);
        let ids = |r: &RecallResponse| r.results.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&fast), ids(&precise));

        // An outage, a used-up budget and a denied project leave the order alone too.
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let fast = s
            .recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        t.fail_grading.store(true, Ordering::SeqCst);
        let broken = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(!broken.reranked, "a failing judge does not fail recall");
        assert_eq!(ids(&fast), ids(&broken));
        t.fail_grading.store(false, Ordering::SeqCst);
        let before = t.graded();
        s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(0.0)))
            .unwrap();
        let broke = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(!broke.reranked);
        assert_eq!(ids(&fast), ids(&broke));
        assert_eq!(
            t.graded(),
            before,
            "nothing is sent against a budget of zero"
        );
    }

    /// A PATCH that has returned is seen by the very next precise recall: the judge in force
    /// reads the budget from the settings at each call, not from the one it was built with (the
    /// rebuild after a `judge.*` change runs in the background). Without that, a recall right
    /// after a budget of zero was still graded and billed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_budget_patched_to_zero_stops_the_very_next_precise_recall() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        for round in 0..40 {
            s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(0.0)))
                .unwrap();
            let r = s
                .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
                .await
                .unwrap();
            assert!(
                !r.reranked,
                "round {round}: graded against a budget of zero"
            );
            assert_eq!(t.graded(), 0, "round {round}: a request was sent");
            // Back up, without a recall: the next round starts from a judge that has not
            // been rebuilt yet, which is the window the bug lived in.
            s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(1.0)))
                .unwrap();
        }
    }

    /// Precise mode sends the query and results of allowed projects only, scoped to the
    /// project the query was asked from.
    #[tokio::test(flavor = "multi_thread")]
    async fn precise_mode_sends_nothing_from_a_denied_project() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "filed");
        std::fs::write(
            std::path::Path::new(cwd.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        remember_deploy_notes(&s, &cwd).await;
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), 0, "the query of a denied project is not sent");
        // Asked from an open project, a denied project's memories are left in place, not sent.
        let open = repo(&d, "open");
        remember_deploy_notes(&s, &open).await;
        let r = s
            .recall(RecallRequest {
                project: None,
                ..recall_req("deploy notes", RecallMode::Precise, &open)
            })
            .await
            .unwrap();
        assert!(r.reranked);
        assert_eq!(t.graded(), 1);
        let state = t.graded_states.lock()[0].clone();
        assert!(state.contains("project open"), "{state}");
        assert!(
            !state.contains("project filed"),
            "a denied project's memories stay out of the request: {state}"
        );
        assert!(
            r.results.iter().any(|h| h.project == "filed"),
            "but they are still results, in their places"
        );
    }

    // ------------------------------------------------- Task 22: a recall from a never-stored tree

    /// A recall from a working tree whose project was never written to is judged by that tree's
    /// own `.alice.toml` and the global lists, like a remote embedding of its query.
    #[tokio::test(flavor = "multi_thread")]
    async fn precise_recall_from_a_never_stored_tree_is_graded_when_its_own_file_allows() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let stored = repo(&d, "stored");
        remember_deploy_notes(&s, &stored).await;
        let fresh = repo(&d, "fresh");
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &fresh))
            .await
            .unwrap();
        assert!(r.reranked, "notes: {:?}", r.notes);
        assert!(!r.notes.iter().any(|n| n == NOTE_DENIED), "{:?}", r.notes);
        assert_eq!(t.graded(), 1);
        assert!(r.results[0].title.contains("ZEBRA"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn precise_recall_from_a_never_stored_tree_that_denies_the_cloud_stays_fast() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let stored = repo(&d, "stored");
        remember_deploy_notes(&s, &stored).await;
        let fresh = repo(&d, "fresh");
        std::fs::write(
            std::path::Path::new(fresh.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &fresh))
            .await
            .unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), 0, "a denying tree's query is not sent");
        assert!(r.notes.iter().any(|n| n == NOTE_DENIED), "{:?}", r.notes);
        assert!(!r.results.is_empty(), "the fast answer is still given");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn precise_recall_from_a_never_stored_tree_sends_the_query_redacted() {
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let stored = repo(&d, "stored");
        remember_deploy_notes(&s, &stored).await;
        let fresh = repo(&d, "fresh");
        let r = s
            .recall(recall_req(
                &format!("deploy notes {token}"),
                RecallMode::Precise,
                &fresh,
            ))
            .await
            .unwrap();
        assert!(r.reranked, "notes: {:?}", r.notes);
        let states = t.graded_states.lock().clone();
        assert_eq!(states.len(), 1);
        assert!(!states[0].contains(token), "{}", states[0]);
        assert!(states[0].contains("deploy notes"), "{}", states[0]);
    }

    // ------------------------------------------------------- Task 21: all tiers, rescue, default

    const RAW_NOTE: &str =
        "session transcript about the deploy notes: the OKAPI rollout was paused";

    async fn ingest_raw(s: &Service, uri: &str, text: &str, cwd: &Option<String>) -> String {
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: uri.into(),
                ..Default::default()
            },
            chunks: vec![ChunkInput {
                ord: 0,
                text: text.into(),
                ..Default::default()
            }],
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_settled(s).await;
        // The chunk's id, as recall shows it.
        let r = s
            .recall(RecallRequest {
                include_raw: true,
                project: None,
                ..recall_req(text, RecallMode::Fast, cwd)
            })
            .await
            .unwrap();
        r.results
            .iter()
            .find(|h| h.tier == Tier::Raw)
            .map(|h| h.id.clone())
            .expect("the chunk is found by its own text")
    }

    fn raw_recall(mode: Option<RecallMode>, cwd: &Option<String>) -> RecallRequest {
        RecallRequest {
            mode,
            include_raw: true,
            ..recall_req("deploy notes", RecallMode::Fast, cwd)
        }
    }

    fn ids_of(r: &RecallResponse) -> Vec<String> {
        r.results.iter().map(|h| h.id.clone()).collect()
    }

    /// Task 21: precise mode grades raw chunks too, so a chunk the judge grades highest moves
    /// above the knowledge hits; fast mode leaves it where the search put it.
    #[tokio::test(flavor = "multi_thread")]
    async fn precise_mode_moves_a_raw_chunk_the_judge_grades_highest_above_knowledge_hits() {
        let t = KeywordGrader::new("OKAPI");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let chunk = ingest_raw(&s, "session-1", RAW_NOTE, &cwd).await;
        let fast = s
            .recall(raw_recall(Some(RecallMode::Fast), &cwd))
            .await
            .unwrap();
        let place = fast.results.iter().position(|h| h.id == chunk).unwrap();
        assert!(
            place > 0,
            "the test needs the chunk behind knowledge hits: {:?}",
            fast.results.iter().map(|h| &h.title).collect::<Vec<_>>()
        );
        assert_eq!(t.graded(), 0, "fast mode does not grade");
        let precise = s
            .recall(raw_recall(Some(RecallMode::Precise), &cwd))
            .await
            .unwrap();
        assert!(precise.reranked);
        assert_eq!(t.graded(), 1, "one batched call");
        assert_eq!(precise.results[0].id, chunk, "{:?}", ids_of(&precise));
        assert_eq!(precise.results[0].tier, Tier::Raw);
        let (mut a, mut b) = (ids_of(&fast), ids_of(&precise));
        a.sort();
        b.sort();
        assert_eq!(a, b, "the same results in another order");
        // The chunk's text went to the judge, redacted text bounded like a memory's.
        assert!(t.graded_states.lock()[0].contains("OKAPI rollout"));
        // And fast mode still keeps the place.
        let again = s
            .recall(raw_recall(Some(RecallMode::Fast), &cwd))
            .await
            .unwrap();
        assert_eq!(again.results[place].id, chunk);
    }

    /// Task 21: a raw chunk of a project the cloud may not see is neither sent nor moved in
    /// precise mode, and the others move around it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_raw_chunk_of_a_cloud_denied_project_keeps_its_place_in_precise_mode() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let filed = repo(&d, "filed");
        std::fs::write(
            std::path::Path::new(filed.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        let open = repo(&d, "open");
        remember_deploy_notes(&s, &open).await;
        let secret = "session transcript about the deploy notes: the ZEBRA vault is sealed";
        let chunk = ingest_raw(&s, "session-f", secret, &filed).await;
        let fast = s
            .recall(raw_recall(Some(RecallMode::Fast), &open))
            .await
            .unwrap();
        let place = fast.results.iter().position(|h| h.id == chunk).unwrap();
        let precise = s
            .recall(raw_recall(Some(RecallMode::Precise), &open))
            .await
            .unwrap();
        assert!(precise.reranked);
        assert_eq!(
            precise.results[place].id, chunk,
            "the chunk keeps its place"
        );
        let state = t.graded_states.lock()[0].clone();
        assert!(!state.contains("vault is sealed"), "not sent: {state}");
        assert!(!state.contains("project filed"), "{state}");
        assert!(
            precise.results[0].title.contains("ZEBRA"),
            "the allowed ones were ordered: {:?}",
            precise.results.iter().map(|h| &h.title).collect::<Vec<_>>()
        );
    }

    /// The cosine the fake embedder (and so the index) gives a query and a memory.
    async fn cosine_to(s: &Service, id: &str, query: &str) -> f64 {
        let m = s
            .get(GetRequest {
                ids: vec![id.into()],
                ..Default::default()
            })
            .await
            .unwrap()
            .memories
            .remove(0)
            .memory;
        let e = FakeEmbedder::new(384);
        let doc = e
            .embed_passages(&[alice_core::embed::memory_embed_text(&m.title, &m.body)])
            .unwrap()
            .remove(0);
        f64::from(alice_core::embed::cosine(
            &doc,
            &e.embed_query(query).unwrap(),
        ))
    }

    fn floors(x: f64) -> serde_json::Map<String, serde_json::Value> {
        let mut m = patch_one("search.relevance_floor", serde_json::json!(x));
        m.insert("search.relevance_sure".into(), serde_json::json!(x));
        m.insert("search.relevance_margin".into(), serde_json::json!(0.0));
        m
    }

    /// Shares no word, stem or substring with `RESCUE_NOTE`: only the vector list can find it.
    const RESCUE_QUERY: &str = "launch calendar";
    const RESCUE_NOTE: &str = "the OKAPI rollout plan for staging";

    /// Task 21: a knowledge candidate 0.01 below its floor is returned in precise mode when the
    /// judge says it answers, not when it says otherwise, never in fast mode, and a candidate
    /// further below (0.035; the band is 0.03) is not even sent.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_candidate_just_below_the_floor_is_rescued_only_when_the_judge_says_it_answers() {
        let recall = |mode, cwd: &Option<String>| RecallRequest {
            query: RESCUE_QUERY.into(),
            mode,
            ..recall_req(RESCUE_QUERY, RecallMode::Fast, cwd)
        };
        // Graded relevant.
        let t = KeywordGrader::new("OKAPI");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        let id = remember_id(&s, RESCUE_NOTE, Some(Kind::Fact), &cwd).await;
        wait_settled(&s).await;
        let c = cosine_to(&s, &id, RESCUE_QUERY).await;
        assert!((0.0..0.9).contains(&c), "{c}");
        s.patch_settings(floors(c + 0.01)).unwrap();
        let fast = s
            .recall(recall(Some(RecallMode::Fast), &cwd))
            .await
            .unwrap();
        assert!(
            fast.results.is_empty(),
            "below the floor: {:?}",
            fast.results
        );
        assert_eq!(t.graded(), 0, "fast mode sends nothing");
        let precise = s
            .recall(recall(Some(RecallMode::Precise), &cwd))
            .await
            .unwrap();
        assert_eq!(t.graded(), 1);
        assert!(precise.reranked);
        assert_eq!(ids_of(&precise), std::slice::from_ref(&id), "rescued");
        // Fast mode still returns nothing afterwards.
        let fast = s
            .recall(recall(Some(RecallMode::Fast), &cwd))
            .await
            .unwrap();
        assert!(fast.results.is_empty());

        // Graded irrelevant: dropped.
        let t2 = KeywordGrader::new("NO-SUCH-WORD");
        let (d2, s2) = svc_with_grader(t2.clone()).await;
        let cwd2 = repo(&d2, "proj");
        let id2 = remember_id(&s2, RESCUE_NOTE, Some(Kind::Fact), &cwd2).await;
        wait_settled(&s2).await;
        s2.patch_settings(floors(cosine_to(&s2, &id2, RESCUE_QUERY).await + 0.01))
            .unwrap();
        let r = s2
            .recall(recall(Some(RecallMode::Precise), &cwd2))
            .await
            .unwrap();
        assert_eq!(t2.graded(), 1, "it was shown to the judge");
        assert!(r.results.is_empty(), "graded unrelated: {:?}", r.results);

        // Beyond the band: not a candidate, nothing is sent.
        s.patch_settings(floors(c + 0.035)).unwrap();
        let before = t.graded();
        let r = s
            .recall(recall(Some(RecallMode::Precise), &cwd))
            .await
            .unwrap();
        assert!(r.results.is_empty());
        assert_eq!(t.graded(), before, "0.035 below is outside the band");
    }

    /// Task 21: when the judge cannot grade (a used-up budget, an outage, a deadline) the
    /// near-floor candidates are dropped and the answer is the fast one. (Each recall asks a
    /// query of its own: the judge answers an identical request from its cache.)
    #[tokio::test(flavor = "multi_thread")]
    async fn no_candidate_is_rescued_when_the_judge_cannot_grade() {
        let t = KeywordGrader::new("OKAPI");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        let id = remember_id(&s, RESCUE_NOTE, Some(Kind::Fact), &cwd).await;
        wait_settled(&s).await;
        // The floors sit 0.01 above the cosine of the query asked.
        async fn ask(s: &Service, id: &str, n: usize, cwd: &Option<String>) -> RecallResponse {
            let q = format!("{RESCUE_QUERY} q{n}");
            let c = cosine_to(s, id, &q).await;
            s.patch_settings(floors(c + 0.01)).unwrap();
            s.recall(RecallRequest {
                query: q.clone(),
                ..recall_req(&q, RecallMode::Precise, cwd)
            })
            .await
            .unwrap()
        }
        // Sanity: it is rescued while the judge works.
        assert_eq!(
            ids_of(&ask(&s, &id, 0, &cwd).await),
            std::slice::from_ref(&id)
        );
        // An outage.
        t.fail_grading.store(true, Ordering::SeqCst);
        let r = ask(&s, &id, 1, &cwd).await;
        assert!(!r.reranked && r.results.is_empty(), "{:?}", r.results);
        t.fail_grading.store(false, Ordering::SeqCst);
        // A deadline: `search.precise_timeout_ms`.
        s.patch_settings(patch_one(
            "search.precise_timeout_ms",
            serde_json::json!(200),
        ))
        .unwrap();
        t.grading_delay_ms.store(1_500, Ordering::SeqCst);
        let r = ask(&s, &id, 2, &cwd).await;
        assert!(!r.reranked && r.results.is_empty(), "{:?}", r.results);
        t.grading_delay_ms.store(0, Ordering::SeqCst);
        s.patch_settings(patch_one(
            "search.precise_timeout_ms",
            serde_json::json!(2_500),
        ))
        .unwrap();
        // A budget of zero: nothing is sent.
        s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(0.0)))
            .unwrap();
        let before = t.graded();
        let r = ask(&s, &id, 3, &cwd).await;
        assert!(!r.reranked && r.results.is_empty(), "{:?}", r.results);
        assert_eq!(t.graded(), before);
        // The heuristic judge cannot grade either.
        let (d, h) = svc().await;
        let cwd = repo(&d, "proj");
        let id = remember_id(&h, RESCUE_NOTE, Some(Kind::Fact), &cwd).await;
        wait_settled(&h).await;
        let r = ask(&h, &id, 4, &cwd).await;
        assert!(!r.reranked && r.results.is_empty());
    }

    /// An embedder with chosen cosines: every passage is the first axis, every query is at the
    /// cosine `cos` of it.
    struct AtCosine {
        cos: f32,
    }

    impl Embedder for AtCosine {
        fn model_id(&self) -> &str {
            "fake"
        }
        fn dim(&self) -> usize {
            384
        }
        fn embed_passages(&self, texts: &[String]) -> alice_core::error::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|_| {
                    let mut v = vec![0.0; 384];
                    v[0] = 1.0;
                    v
                })
                .collect())
        }
        fn embed_query(&self, _text: &str) -> alice_core::error::Result<Vec<f32>> {
            let mut v = vec![0.0; 384];
            v[0] = self.cos;
            v[1] = (1.0 - self.cos * self.cos).sqrt();
            Ok(v)
        }
    }

    /// Task 21: a raw chunk 0.01 below the chunk floor of 0.82 (a cosine of 0.81, on an
    /// embedder with chosen cosines) is rescued the same way, and one 0.031 below is not.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_raw_chunk_just_below_the_chunk_floor_is_rescued_in_precise_mode_only() {
        for (cos, rescued) in [(0.81f32, true), (0.789, false)] {
            let t = KeywordGrader::new("OKAPI");
            let d = tempfile::tempdir().unwrap();
            let s = Service::start(
                AliceHome::new(d.path().to_path_buf()),
                &move || Ok(Arc::new(AtCosine { cos }) as Arc<dyn Embedder>),
                ServiceOptions {
                    jev_transport: Some(t.clone()),
                    ..quiet()
                },
            )
            .unwrap();
            s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
                .unwrap();
            s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
                .unwrap();
            let cwd = repo(&d, "proj");
            s.ingest(IngestRequest {
                source: SourceInput {
                    uri: "s".into(),
                    ..Default::default()
                },
                chunks: vec![ChunkInput {
                    ord: 0,
                    text: "the OKAPI rollout was paused after the review".into(),
                    ..Default::default()
                }],
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
            wait_settled(&s).await;
            let q = "zzz yyy";
            let req = |mode| RecallRequest {
                query: q.into(),
                mode: Some(mode),
                include_raw: true,
                ..recall_req(q, RecallMode::Fast, &cwd)
            };
            let fast = s.recall(req(RecallMode::Fast)).await.unwrap();
            assert!(fast.results.is_empty(), "{cos}: {:?}", fast.results);
            assert_eq!(t.graded(), 0);
            let precise = s.recall(req(RecallMode::Precise)).await.unwrap();
            if rescued {
                assert_eq!(precise.results.len(), 1, "{cos}: rescued");
                assert_eq!(precise.results[0].tier, Tier::Raw);
                assert!(precise.reranked);
                assert_eq!(t.graded(), 1);
                assert!(
                    s.recall(req(RecallMode::Fast))
                        .await
                        .unwrap()
                        .results
                        .is_empty(),
                    "fast mode never returns it"
                );
            } else {
                assert!(precise.results.is_empty(), "{cos}: {:?}", precise.results);
                assert_eq!(t.graded(), 0, "outside the band: nothing is sent");
            }
        }
    }

    /// Task 21: `search.default_mode`. `auto` is precise with a model judge and fast with the
    /// heuristic; a request's own mode wins either way; the setting live-reloads.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_default_mode_is_precise_with_a_model_judge_and_a_request_overrides_it() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let ask = |mode: Option<RecallMode>| RecallRequest {
            mode,
            ..recall_req("deploy notes", RecallMode::Fast, &cwd)
        };
        assert_eq!(s.settings().search.default_mode, DefaultRecallMode::Auto);
        // auto, model judge, no mode named: precise.
        let r = s.recall(ask(None)).await.unwrap();
        assert!(r.reranked && t.graded() == 1);
        assert!(r.results[0].title.contains("ZEBRA"));
        // An explicit fast wins over auto.
        let r = s.recall(ask(Some(RecallMode::Fast))).await.unwrap();
        assert!(!r.reranked && t.graded() == 1);
        // Live: fast as the default; no request mode is fast, an explicit precise wins.
        s.patch_settings(patch_one("search.default_mode", serde_json::json!("fast")))
            .unwrap();
        let r = s.recall(ask(None)).await.unwrap();
        assert!(!r.reranked && t.graded() == 1);
        // (The same request again would come from the judge's cache, so no count here.)
        let r = s.recall(ask(Some(RecallMode::Precise))).await.unwrap();
        assert!(r.reranked);
        // Back to precise, then back to auto.
        s.patch_settings(patch_one(
            "search.default_mode",
            serde_json::json!("precise"),
        ))
        .unwrap();
        assert!(s.recall(ask(None)).await.unwrap().reranked);
        s.patch_settings(patch_one("search.default_mode", serde_json::json!("auto")))
            .unwrap();
        assert!(s.recall(ask(None)).await.unwrap().reranked);

        let jev_cwd = cwd.clone();
        // The heuristic cannot grade: auto is fast, and nothing is sent anywhere.
        let (d, h) = svc().await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&h, &cwd).await;
        let fast = h.recall(ask(Some(RecallMode::Fast))).await.unwrap();
        let auto = h.recall(ask(None)).await.unwrap();
        assert!(!auto.reranked);
        assert_eq!(ids_of(&auto), ids_of(&fast));
        // A client that sends `"mode":"fast"` still gets fast, with a model judge too.
        let old: RecallRequest =
            serde_json::from_str(r#"{"query":"deploy notes","mode":"fast"}"#).unwrap();
        let before = t.graded();
        let r = s
            .recall(RecallRequest {
                cwd: jev_cwd,
                ..old
            })
            .await
            .unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), before);
    }

    // ------------------------------------------------------- Task 21 fix round 1

    async fn svc_with_grader_clock(
        t: Arc<KeywordGrader>,
        clock: Option<crate::precise_gate::Clock>,
    ) -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                jev_transport: Some(t),
                precise_clock: clock,
                ..quiet()
            },
        )
        .unwrap();
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        (d, s)
    }

    fn precise_q(q: &str, cwd: &Option<String>) -> RecallRequest {
        RecallRequest {
            query: q.into(),
            ..recall_req(q, RecallMode::Precise, cwd)
        }
    }

    fn has_note(r: &RecallResponse, needle: &str) -> bool {
        r.notes.iter().any(|n| n.contains(needle))
    }

    #[test]
    fn the_precise_timeout_defaults_to_2500_ms_and_is_a_live_search_setting() {
        assert_eq!(Settings::default().search.precise_timeout_ms, 2_500);
    }

    /// A judge that never answers, with `judge.timeout_ms` at a minute: the recall is back
    /// within its own budget with the fast order and says why.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_judge_that_never_answers_cannot_stall_a_precise_recall() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        let fast = s
            .recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        s.patch_settings(patch_one("judge.timeout_ms", serde_json::json!(60_000)))
            .unwrap();
        s.patch_settings(patch_one(
            "search.precise_timeout_ms",
            serde_json::json!(300),
        ))
        .unwrap();
        t.grading_delay_ms.store(4_000, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(1_500));
        assert!(!r.reranked);
        assert_eq!(ids_of(&r), ids_of(&fast), "the fast order");
        assert!(has_note(&r, "precise_timeout_ms"), "{:?}", r.notes);
        assert_eq!(
            t.last_timeout_ms.load(Ordering::SeqCst),
            300,
            "the request's own timeout is the recall's budget"
        );
    }

    /// The recall's grading call is made once: a failing judge is not retried.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_precise_recall_asks_the_judge_once_and_does_not_retry() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        t.fail_grading.store(true, Ordering::SeqCst);
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), 1, "one attempt, no retry");
        assert!(has_note(&r, "unavailable"), "{:?}", r.notes);
    }

    /// Three failures in a row open the breaker; a minute later one recall probes.
    #[tokio::test(flavor = "multi_thread")]
    async fn three_failures_open_the_breaker_for_a_minute_and_then_one_recall_probes() {
        let base = std::time::Instant::now();
        let offset = Arc::new(Mutex::new(Duration::ZERO));
        let clock: crate::precise_gate::Clock = {
            let offset = offset.clone();
            Arc::new(move || base + *offset.lock())
        };
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader_clock(t.clone(), Some(clock)).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        t.fail_grading.store(true, Ordering::SeqCst);
        for i in 0..3 {
            let r = s
                .recall(precise_q(&format!("deploy notes {i}"), &cwd))
                .await
                .unwrap();
            assert!(!r.reranked);
        }
        assert_eq!(t.graded(), 3);
        let started = std::time::Instant::now();
        let r = s.recall(precise_q("deploy notes 3", &cwd)).await.unwrap();
        assert_eq!(t.graded(), 3, "the fourth recall made no judge call");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!r.reranked && has_note(&r, "unavailable"), "{:?}", r.notes);
        // 59 s later: still open.
        *offset.lock() = Duration::from_secs(59);
        s.recall(precise_q("deploy notes 4", &cwd)).await.unwrap();
        assert_eq!(t.graded(), 3);
        // A minute: one probe, which fails and opens it again.
        *offset.lock() = Duration::from_secs(61);
        s.recall(precise_q("deploy notes 5", &cwd)).await.unwrap();
        assert_eq!(t.graded(), 4, "one probe");
        s.recall(precise_q("deploy notes 6", &cwd)).await.unwrap();
        assert_eq!(t.graded(), 4, "open again");
        // Another minute and a judge that works again: the probe succeeds and closes it.
        t.fail_grading.store(false, Ordering::SeqCst);
        *offset.lock() = Duration::from_secs(125);
        let r = s.recall(precise_q("deploy notes 7", &cwd)).await.unwrap();
        assert!(r.reranked, "{:?}", r.notes);
        assert_eq!(t.graded(), 5);
        let r = s.recall(precise_q("deploy notes 8", &cwd)).await.unwrap();
        assert!(r.reranked && t.graded() == 6, "closed: every recall grades");
    }

    /// A fifth grading call beyond four in flight is answered fast without asking the judge.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recall_beyond_four_grading_calls_in_flight_answers_fast() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        t.grading_delay_ms.store(1_500, Ordering::SeqCst);
        let mut running = Vec::new();
        for i in 0..4 {
            let (s, cwd) = (s.clone(), cwd.clone());
            running.push(tokio::spawn(async move {
                s.recall(precise_q(&format!("deploy notes {i}"), &cwd))
                    .await
                    .unwrap()
            }));
        }
        for _ in 0..100 {
            if t.graded() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(t.graded(), 4);
        let started = std::time::Instant::now();
        let fifth = s.recall(precise_q("deploy notes 9", &cwd)).await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(1_000));
        assert_eq!(t.graded(), 4, "no judge call for the fifth");
        assert!(
            !fifth.reranked && has_note(&fifth, "busy"),
            "{:?}",
            fifth.notes
        );
        for r in running {
            assert!(r.await.unwrap().reranked);
        }
    }

    /// Every fast fallback of precise mode says why.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_precise_fallback_names_its_reason() {
        // No model judge.
        let (d, h) = svc().await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&h, &cwd).await;
        let r = h
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(has_note(&r, "needs a model judge"), "{:?}", r.notes);
        // Budget spent.
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        s.patch_settings(patch_one("judge.daily_budget_usd", serde_json::json!(0.0)))
            .unwrap();
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(has_note(&r, "budget"), "{:?}", r.notes);
        // A project the cloud policy denies.
        let (d, s) = svc_with_grader(KeywordGrader::new("ZEBRA")).await;
        let cwd = repo(&d, "filed");
        std::fs::write(
            std::path::Path::new(cwd.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        remember_deploy_notes(&s, &cwd).await;
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Precise, &cwd))
            .await
            .unwrap();
        assert!(has_note(&r, "cloud policy"), "{:?}", r.notes);
    }

    /// A near-floor candidate of a cloud-denied project is not sent and not returned.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_near_floor_candidate_of_a_denied_project_never_reaches_the_judge() {
        let t = KeywordGrader::new("OKAPI");
        let (d, s) = svc_with_grader(t.clone()).await;
        let filed = repo(&d, "filed");
        std::fs::write(
            std::path::Path::new(filed.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        let open = repo(&d, "open");
        let id = remember_id(&s, RESCUE_NOTE, Some(Kind::Fact), &filed).await;
        for text in ["launch calendar alpha", "launch calendar beta"] {
            remember_id(&s, text, Some(Kind::Fact), &open).await;
        }
        wait_settled(&s).await;
        s.patch_settings(floors(cosine_to(&s, &id, RESCUE_QUERY).await + 0.01))
            .unwrap();
        let r = s.recall(precise_q(RESCUE_QUERY, &open)).await.unwrap();
        assert!(r.reranked);
        assert_eq!(t.graded(), 1);
        let state = t.graded_states.lock()[0].clone();
        assert!(!state.contains("OKAPI"), "not sent: {state}");
        assert!(!r.results.iter().any(|h| h.id == id), "not returned");
    }

    /// A raw chunk is shown to the judge up to 2,000 characters, a memory up to 700.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_judge_sees_two_thousand_characters_of_a_raw_chunk_and_seven_hundred_of_a_memory() {
        let t = KeywordGrader::new("OKAPI");
        let (d, s) = svc_with_grader(t.clone()).await;
        let cwd = repo(&d, "proj");
        remember_id(
            &s,
            &format!("deploy notes ZEBRA {}", "z".repeat(5_000)),
            Some(Kind::Howto),
            &cwd,
        )
        .await;
        // A memory whose title merely starts like a raw chunk's is still a memory.
        remember_id(
            &s,
            &format!("raw · x deploy notes\n{}", "w".repeat(5_000)),
            Some(Kind::Howto),
            &cwd,
        )
        .await;
        ingest_raw(
            &s,
            "long",
            &format!("deploy notes OKAPI {}", "y".repeat(5_000)),
            &cwd,
        )
        .await;
        let r = s
            .recall(raw_recall(Some(RecallMode::Precise), &cwd))
            .await
            .unwrap();
        assert!(r.reranked);
        let state = t.graded_states.lock()[0].clone();
        assert!(state.contains(&"y".repeat(1_970)), "raw: 2,000 characters");
        assert!(!state.contains(&"y".repeat(2_000)));
        assert!(state.contains(&"z".repeat(670)), "memory: 700 characters");
        assert!(!state.contains(&"z".repeat(700)));
        assert!(state.contains(&"w".repeat(650)));
        assert!(
            !state.contains(&"w".repeat(700)),
            "a memory titled like a raw chunk keeps 700 characters"
        );
    }

    /// A rescued raw chunk of a recall that did not ask for raw results is labelled and packed
    /// like a raw fallback.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rescued_raw_chunk_is_labelled_like_any_raw_fallback_hit() {
        let t = KeywordGrader::new("OKAPI");
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(AtCosine { cos: 0.81 }) as Arc<dyn Embedder>),
            ServiceOptions {
                jev_transport: Some(t.clone()),
                ..quiet()
            },
        )
        .unwrap();
        s.patch_settings(patch_one("judge.provider", serde_json::json!("jev")))
            .unwrap();
        s.set_secret(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .unwrap();
        let cwd = repo(&d, "proj");
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            chunks: vec![ChunkInput {
                ord: 0,
                text: "the OKAPI rollout was paused after the review".into(),
                ..Default::default()
            }],
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_settled(&s).await;
        let req = |format| RecallRequest {
            query: "zzz yyy".into(),
            mode: Some(RecallMode::Precise),
            format,
            ..recall_req("zzz yyy", RecallMode::Precise, &cwd)
        };
        let json = s.recall(req(OutputFormat::Json)).await.unwrap();
        assert_eq!(json.results.len(), 1);
        assert_eq!(json.results[0].tier, Tier::Raw);
        assert!(json.raw_fallback, "the answer says a raw result was added");
        let text = s.recall(req(OutputFormat::Text)).await.unwrap();
        let text = text.text.unwrap();
        assert!(text.contains("raw"), "{text}");
        assert!(text.contains("OKAPI rollout"), "{text}");
    }

    // ------------------------------------------------------- Task 21 fix round 2

    /// R-1: a recall dropped while its probe is out (a client that disconnects) cannot leave
    /// the breaker stuck: the call still ends and settles, and a later recall closes it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_probing_recall_does_not_leave_the_breaker_stuck() {
        let base = std::time::Instant::now();
        let offset = Arc::new(Mutex::new(Duration::ZERO));
        let clock: crate::precise_gate::Clock = {
            let offset = offset.clone();
            Arc::new(move || base + *offset.lock())
        };
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader_clock(t.clone(), Some(clock)).await;
        let cwd = repo(&d, "proj");
        remember_deploy_notes(&s, &cwd).await;
        t.fail_grading.store(true, Ordering::SeqCst);
        for i in 0..3 {
            s.recall(precise_q(&format!("deploy notes {i}"), &cwd))
                .await
                .unwrap();
        }
        assert_eq!(t.graded(), 3, "the breaker is open now");
        // The judge works again, slowly; a minute later the probe starts and is dropped.
        t.fail_grading.store(false, Ordering::SeqCst);
        t.grading_delay_ms.store(600, Ordering::SeqCst);
        *offset.lock() = Duration::from_secs(61);
        let probing = {
            let (s, cwd) = (s.clone(), cwd.clone());
            tokio::spawn(async move { s.recall(precise_q("deploy notes 3", &cwd)).await })
        };
        for _ in 0..100 {
            if t.graded() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(t.graded(), 4, "the probe is out");
        probing.abort();
        let _ = probing.await;
        // While the dropped probe's call is still running, no other recall probes.
        let r = s.recall(precise_q("deploy notes 4", &cwd)).await.unwrap();
        assert_eq!(t.graded(), 4);
        assert!(!r.reranked);
        // The call ends and settles as a success: the breaker is closed.
        tokio::time::sleep(Duration::from_millis(900)).await;
        t.grading_delay_ms.store(0, Ordering::SeqCst);
        let r = s.recall(precise_q("deploy notes 5", &cwd)).await.unwrap();
        assert!(r.reranked, "{:?}", r.notes);
        assert_eq!(t.graded(), 5);
    }

    // ------------------------------------------------- server mode: client_project and tenant mode

    fn client_project(key: &str, slug: &str, rules: ClientRules) -> Option<ClientProject> {
        Some(ClientProject {
            key: key.into(),
            slug: slug.into(),
            name: slug.into(),
            remote: None,
            rules,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_refuses_manual_backups_and_server_palace_imports() {
        let (_d, s) = tenant_svc().await;
        let e = s.backup_now().await.unwrap_err().to_string();
        assert!(e.contains("backups are scheduled"), "{e}");
        let e = s
            .start_import(alice_core::import::ImportRequest {
                palace: Some("/srv/x".into()),
                ..Default::default()
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("server tenant"), "{e}");
        let e = s
            .start_import(alice_core::import::ImportRequest::default())
            .unwrap_err()
            .to_string();
        assert!(e.contains("server tenant"), "{e}");
    }

    /// A tenant whose disk always has room: an accept test must not depend on the host's.
    async fn tenant_svc_with_room() -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: true,
                free_space: Some(Arc::new(|_| Some(u64::MAX / 4))),
                ..quiet()
            },
        )
        .unwrap();
        (d, s)
    }

    async fn tenant_svc() -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: true,
                ..quiet()
            },
        )
        .unwrap();
        (d, s)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_project_hint_makes_or_finds_a_placeholder_in_tenant_mode_only() {
        let (_d, s) = tenant_svc().await;
        let hint = |content: &str, h: &str| RememberRequest {
            content: content.into(),
            project_hint: Some(h.into()),
            ..Default::default()
        };
        let r = s
            .remember(hint("first note via mcp", "My App"))
            .await
            .unwrap();
        assert_eq!(r.project, "my-app");
        let conn = alice_core::store::open_reader(&s.home().db()).unwrap();
        let (key, root): (String, Option<String>) = conn
            .query_row(
                "SELECT key, root_path FROM projects WHERE slug = 'my-app'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            key,
            format!(
                "{}my-app",
                alice_core::store::projects::PLACEHOLDER_KEY_PREFIX
            )
        );
        assert!(root.is_none(), "no path is stored for a hint");
        // The second time it finds the same project; an explicit slug still wins.
        assert_eq!(
            s.remember(hint("second note via mcp", "my-app"))
                .await
                .unwrap()
                .project,
            "my-app"
        );
        let r = s
            .remember(RememberRequest {
                content: "explicit".into(),
                project: Some("global".into()),
                project_hint: Some("my-app".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.project, "global");
        // A local daemon ignores the hint.
        let (_d2, local) = svc().await;
        assert_eq!(
            local
                .remember(hint("local note", "my-app"))
                .await
                .unwrap()
                .project,
            "global"
        );
    }

    /// A tenant's read-only spool keeps the project hint: after the repair the memory lands in
    /// the hinted project, not in `global`, and an old entry without the field still parses.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_only_spool_keeps_the_project_hint_through_the_drain() {
        let (d, s) = tenant_svc().await;
        s.force_read_only("disk full");
        let r = s
            .remember_or_spool(RememberRequest {
                content: "spooled with a hint".into(),
                project_hint: Some("Hinted App".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.status, RememberStatus::Spooled);
        let dir = s.home().spool_dir();
        let text: String = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect();
        assert!(text.contains("Hinted App"), "{text}");
        // The repair: a fresh service on the same home drains the spool.
        s.shutdown();
        let s2 = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: true,
                ..quiet()
            },
        )
        .unwrap();
        let report = spool::drain(&dir, &s2).await;
        assert_eq!(report.processed, 1, "{report:?}");
        let conn = alice_core::store::open_reader(&s2.home().db()).unwrap();
        let project: String = conn
            .query_row(
                "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(project, "hinted-app");
        // An entry written before the field existed still parses.
        let parsed: RememberRequest = serde_json::from_str(r#"{"content":"old entry"}"#).unwrap();
        assert!(parsed.project_hint.is_none());
    }

    /// Project names are capped: a hint or an explicit project over the limits is refused, one
    /// at the limit works, and the refusal never echoes the value.
    #[tokio::test(flavor = "multi_thread")]
    async fn project_names_over_the_limits_are_refused() {
        let (_d, s) = tenant_svc().await;
        let remember = |hint: Option<String>, project: Option<String>| RememberRequest {
            content: "a note".into(),
            project_hint: hint,
            project,
            ..Default::default()
        };
        let long = "a".repeat(101);
        for req in [
            remember(Some(long.clone()), None),
            remember(Some(format!("{} ", "b".repeat(201))), None),
            remember(None, Some(long.clone())),
        ] {
            let e = s.remember(req).await.unwrap_err();
            assert!(matches!(e, AliceError::Invalid(_)), "{e}");
            let msg = e.to_string();
            assert!(
                !msg.contains(&long) && !msg.contains("bbbbbbbb"),
                "echoed: {msg}"
            );
        }
        let ok = s
            .remember(remember(Some("c".repeat(100)), None))
            .await
            .unwrap();
        assert_eq!(ok.project, "c".repeat(100));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_redacts_even_when_redaction_is_off_in_the_file() {
        let (d, s) = tenant_svc().await;
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        assert!(
            s.patch_settings(patch_one("privacy.redact", serde_json::json!(false)))
                .is_err()
        );
        s.remember(RememberRequest {
            content: format!("deploy key {token}"),
            project_hint: Some("ops".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        // The database is in WAL mode: the text may sit in either file.
        let db = s.home().db();
        let mut wal = db.as_os_str().to_owned();
        wal.push("-wal");
        for f in [db, std::path::PathBuf::from(wal)] {
            let bytes = std::fs::read(&f).unwrap_or_default();
            assert!(
                !bytes.windows(token.len()).any(|w| w == token.as_bytes()),
                "the secret reached {}",
                f.display()
            );
        }
        drop(d);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_project_is_stored_with_its_mirror_as_the_root() {
        let (_d, s) = tenant_svc().await;
        assert!(s.is_tenant());
        let rules = || ClientRules::File {
            text: "[privacy]\nblock_secrets = true\n".into(),
        };
        let r = s
            .remember(RememberRequest {
                content: "Staging runs on Fly".into(),
                client_project: client_project("github.com/me/app", "app", rules()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.project, "app");
        let root = s
            .read(|c| projects::root_of(c, "app"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::path::PathBuf::from(&root),
            crate::client_projects::mirror_dir(s.home(), "github.com/me/app")
        );
        // The mirror's block_secrets applies on the server.
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let e = s
            .remember(RememberRequest {
                content: format!("deploy with {token}"),
                client_project: client_project("github.com/me/app", "app", rules()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(alice_core::redact::is_refusal(&e), "{e}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_ignores_cwd_and_an_explicit_slug_still_wins() {
        let (d, s) = tenant_svc().await;
        let cwd = repo(&d, "somewhere");
        let r = s
            .remember(RememberRequest {
                content: "a note".into(),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            r.project, "global",
            "no git on a client path in tenant mode"
        );
        s.remember(RememberRequest {
            content: "first app note".into(),
            client_project: client_project("k/app", "app", ClientRules::None),
            ..Default::default()
        })
        .await
        .unwrap();
        let r = s
            .remember(RememberRequest {
                content: "named note".into(),
                project: Some("global".into()),
                client_project: client_project("k/app", "app", ClientRules::None),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.project, "global");
        // The local meaning of cwd is untouched.
        let (d2, local) = svc().await;
        let cwd2 = repo(&d2, "proj");
        let r = local
            .remember(RememberRequest {
                content: "local note".into(),
                cwd: cwd2,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.project, "proj");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mirrored_cloud_denial_keeps_the_judge_and_the_query_away() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let denied = client_project(
            "k/secret",
            "secret",
            ClientRules::File {
                text: "[privacy]\ncloud_denied = true\n".into(),
            },
        );
        // Never stored yet: the Task 22 path (query_cloud on the mirror root) denies the grading.
        let r = s
            .recall(RecallRequest {
                client_project: denied.clone(),
                ..recall_req("deploy notes", RecallMode::Precise, &None)
            })
            .await
            .unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), 0);
        // Stored: the judge's policy reads the mirror for later calls.
        for text in DEPLOY_NOTES {
            s.remember(RememberRequest {
                content: text.into(),
                kind: Some(Kind::Howto),
                client_project: denied.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        wait_settled(&s).await;
        assert!(!s.cloud.cloud_allowed("secret"));
        let r = s
            .recall(RecallRequest {
                client_project: denied,
                ..recall_req("deploy notes", RecallMode::Precise, &None)
            })
            .await
            .unwrap();
        assert!(!r.reranked);
        assert!(r.notes.iter().any(|n| n == NOTE_DENIED), "{:?}", r.notes);
        assert_eq!(t.graded(), 0);
        drop(d);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_project_whose_mirror_is_gone_fails_closed() {
        let (_d, s) = tenant_svc().await;
        s.remember(RememberRequest {
            content: "app note".into(),
            client_project: client_project("k/app", "app", ClientRules::None),
            ..Default::default()
        })
        .await
        .unwrap();
        let dir = crate::client_projects::mirror_dir(s.home(), "k/app");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!s.cloud.cloud_allowed("app"));
        assert!(
            s.rules_fail_closed(Scope {
                project: Some("app"),
                ..Default::default()
            })
            .await
        );
    }

    // ---------------------------------------------------------------- tenant import inbox

    fn palace_request(p: &Path) -> alice_core::import::ImportRequest {
        alice_core::import::ImportRequest {
            palace: Some(p.display().to_string()),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_imports_only_from_its_import_inbox() {
        let (d, s) = tenant_svc_with_room().await;
        let inbox = d.path().join("import-inbox");
        let outside = tempfile::tempdir().unwrap();
        let out_palace = alice_core::import::fixture::synthetic_palace(outside.path(), 20).dir;
        std::fs::create_dir_all(inbox.join("j2")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&out_palace, inbox.join("j2/palace")).unwrap();
        for bad in [
            Some(out_palace.clone()),
            Some(inbox.clone()),
            Some(inbox.join("j2/palace")),
            None,
        ] {
            let req = match &bad {
                Some(p) => palace_request(p),
                None => Default::default(),
            };
            let e = s.start_import(req).unwrap_err().to_string();
            assert!(e.contains("import-inbox/"), "{e}");
        }
        let ok = inbox.join("j1/palace");
        alice_core::import::fixture::synthetic_palace(&ok, 20);
        let st = s.start_import(palace_request(&ok)).unwrap();
        assert_eq!(st.state, alice_core::import::ImportState::Running);
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_import_is_refused_when_the_disk_would_get_too_full() {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: true,
                free_space: Some(Arc::new(|_| Some(1))),
                ..quiet()
            },
        )
        .unwrap();
        let p = d.path().join("import-inbox/j1/palace");
        alice_core::import::fixture::synthetic_palace(&p, 20);
        let e = s.start_import(palace_request(&p)).unwrap_err().to_string();
        assert!(
            e.contains("1 bytes free") && e.contains("twice its size plus 2 GB"),
            "{e}"
        );
    }

    // ------------------------------------- reads from a checkout that only an import knows

    use alice_core::import::fixture::{DrawerSpec, build_sqlite_exact};
    use alice_core::import::palace::DrawerMeta;
    use alice_core::import::{ImportRequest, ImportState};

    /// Imports a palace of one wing holding `DEPLOY_NOTES` as knowledge (the wing's project
    /// is stored as an import placeholder, with no root).
    async fn import_wing(s: &Arc<Service>, d: &tempfile::TempDir, wing: &str) {
        let palace = d.path().join(format!("palace-{wing}")).join("palace");
        import_wing_at(s, &palace, wing).await;
    }

    /// `import_wing`, from a palace at `palace` (a tenant imports only from its inbox).
    async fn import_wing_at(s: &Arc<Service>, palace: &Path, wing: &str) {
        std::fs::create_dir_all(palace).unwrap();
        let specs: Vec<DrawerSpec> = DEPLOY_NOTES
            .iter()
            .enumerate()
            .map(|(i, text)| DrawerSpec {
                id: format!("k_{wing}_{i}"),
                text: text.to_string(),
                meta: DrawerMeta {
                    wing: wing.into(),
                    room: "decisions".into(),
                    added_by: Some("claude-code".into()),
                    filed_at: Some("2026-04-10T10:00:00".into()),
                    ..Default::default()
                },
            })
            .collect();
        build_sqlite_exact(palace, &specs);
        s.start_import(ImportRequest {
            palace: Some(palace.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        for _ in 0..3000 {
            let st = s.import_status();
            if st.state != ImportState::Running {
                assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
                wait_settled(s).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("import did not end");
    }

    /// A key memory in the imported project (it shows in that project's briefing).
    async fn key_note(s: &Service) {
        s.remember(RememberRequest {
            content: "KEYNOTE pg-ai ships on Fridays".into(),
            kind: Some(Kind::Note),
            importance: Some(5),
            project: Some("pg-ai".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_settled(s).await;
    }

    fn db_of(d: &tempfile::TempDir) -> Connection {
        alice_core::store::open_writer(&AliceHome::new(d.path().to_path_buf()).db()).unwrap()
    }

    fn project_rows(d: &tempfile::TempDir) -> Vec<(String, String)> {
        let c = db_of(d);
        let mut st = c
            .prepare("SELECT key, slug FROM projects ORDER BY key")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    async fn context_of(s: &Service, cwd: &Option<String>) -> ContextResponse {
        s.context(ContextRequest {
            project: None,
            max_tokens: None,
            cwd: cwd.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_from_an_unstored_checkout_uses_the_imported_placeholder() {
        let (d, s) = svc().await;
        import_wing(&s, &d, "pg_ai").await;
        key_note(&s).await;
        let rows = project_rows(&d);
        assert!(rows.iter().any(|(k, _)| k == "import:pg-ai"), "{rows:?}");
        let cwd = repo(&d, "pg-ai");
        // The briefing of the checkout is the imported project's.
        let by_name = s
            .context(ContextRequest {
                project: Some("pg-ai".into()),
                max_tokens: None,
                cwd: None,
                ..Default::default()
            })
            .await
            .unwrap();
        let ctx = context_of(&s, &cwd).await;
        assert!(ctx.text.contains("KEYNOTE"), "{}", ctx.text);
        assert_eq!(ctx.text, by_name.text);
        assert_eq!(ctx.project, "pg-ai");
        // The search boosts the imported project: the scope names its id.
        let detected = detect_project(Path::new(cwd.as_deref().unwrap()));
        let placeholder_id = s
            .read(|c| Ok(projects::resolve_slug(c, "pg-ai")?.unwrap().id))
            .await
            .unwrap();
        let scope = s
            .read(move |c| ProjectResolver::read_scope(c, ProjectChoice::Detected(detected), false))
            .await
            .unwrap();
        assert_eq!(scope.current_id(), Some(placeholder_id.clone()));
        assert_eq!(
            scope.filter_id, None,
            "a detected project boosts, never filters"
        );
        // An explicit slug of the placeholder filters to it (it did before, and still does).
        let scope = s
            .read(|c| ProjectResolver::read_scope(c, ProjectChoice::Slug("pg-ai".into()), true))
            .await
            .unwrap();
        assert_eq!(scope.filter_id, Some(placeholder_id));
        // Reads created nothing.
        assert_eq!(project_rows(&d), rows);
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        assert_eq!(r.project.as_deref(), Some("pg-ai"));
        assert!(r.results.iter().any(|h| h.project == "pg-ai"));
        assert_eq!(project_rows(&d), rows, "a recall created no project");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_precise_recall_from_an_unstored_checkout_is_judged_under_the_placeholder() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        import_wing(&s, &d, "pg_ai").await;
        let cwd = repo(&d, "pg-ai");
        // `global` is denied the cloud: an unstored checkout used to be judged as `global`.
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["global"]),
        ))
        .unwrap();
        let before = t.graded();
        let r = s.recall(precise_q("deploy notes", &cwd)).await.unwrap();
        assert!(r.reranked, "notes: {:?}", r.notes);
        assert_eq!(t.graded(), before + 1);
        assert!(r.results[0].title.contains("ZEBRA"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_denied_placeholder_keeps_a_precise_recall_from_its_checkout_local() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        import_wing(&s, &d, "pg_ai").await;
        let cwd = repo(&d, "pg-ai");
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["pg-ai"]),
        ))
        .unwrap();
        let before = t.graded();
        let r = s.recall(precise_q("deploy notes", &cwd)).await.unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), before, "the denied project's query is not sent");
        assert!(has_note(&r, NOTE_DENIED), "{:?}", r.notes);
        assert!(!r.results.is_empty(), "the fast answer is still given");
        // An alias of the placeholder in the list denies it too.
        s.patch_settings(patch_one(
            "privacy.cloud_denied_projects",
            serde_json::json!(["old-name"]),
        ))
        .unwrap();
        db_of(&d)
            .execute(
                "INSERT INTO project_aliases (alias, project_id) SELECT 'old-name', id FROM projects WHERE key = 'import:pg-ai'",
                [],
            )
            .unwrap();
        let r = s.recall(precise_q("deploy notes", &cwd)).await.unwrap();
        assert!(!r.reranked && has_note(&r, NOTE_DENIED), "{:?}", r.notes);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_checkout_that_denies_the_cloud_stays_denied_whatever_the_placeholder_allows() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        import_wing(&s, &d, "pg_ai").await;
        let cwd = repo(&d, "pg-ai");
        std::fs::write(
            Path::new(cwd.as_deref().unwrap()).join(".alice.toml"),
            "[privacy]\ncloud_denied = true\n",
        )
        .unwrap();
        let before = t.graded();
        let r = s.recall(precise_q("deploy notes", &cwd)).await.unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), before);
        assert!(has_note(&r, NOTE_DENIED), "{:?}", r.notes);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_matching_placeholders_are_not_guessed_between() {
        let (d, s) = svc().await;
        import_wing(&s, &d, "pg_ai").await;
        key_note(&s).await;
        import_wing(&s, &d, "other_wing").await;
        // A second placeholder that answers to the same name through an alias.
        db_of(&d)
            .execute(
                "INSERT INTO project_aliases (alias, project_id) SELECT 'pg-ai', id FROM projects WHERE key = 'import:other-wing'",
                [],
            )
            .unwrap();
        let rows = project_rows(&d);
        let cwd = repo(&d, "pg-ai");
        let ctx = context_of(&s, &cwd).await;
        assert!(!ctx.text.contains("KEYNOTE"), "{}", ctx.text);
        let detected = detect_project(Path::new(cwd.as_deref().unwrap()));
        let scope = s
            .read(move |c| ProjectResolver::read_scope(c, ProjectChoice::Detected(detected), false))
            .await
            .unwrap();
        assert_eq!(scope.current_id(), None, "no guess: the unstored stand-in");
        s.recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        assert_eq!(project_rows(&d), rows);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_explicit_unknown_slug_still_errors_and_a_placeholder_slug_resolves() {
        let (d, s) = svc().await;
        import_wing(&s, &d, "pg_ai").await;
        let cwd = repo(&d, "pg-ai");
        let err = s
            .context(ContextRequest {
                project: Some("no-such-project".into()),
                max_tokens: None,
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown project"), "{err}");
        let err = s
            .recall(RecallRequest {
                project: Some("no-such-project".into()),
                ..recall_req("deploy notes", RecallMode::Fast, &cwd)
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown project"), "{err}");
        // A placeholder-only slug (and its alias spellings) is a known project.
        for project in ["pg-ai", "pg_ai", "PG-AI"] {
            let r = s
                .recall(RecallRequest {
                    project: Some(project.into()),
                    ..recall_req("deploy notes", RecallMode::Fast, &None)
                })
                .await
                .unwrap_or_else(|e| panic!("{project}: {e}"));
            assert!(
                !r.results.is_empty() && r.results.iter().all(|h| h.project == "pg-ai"),
                "{project}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_a_real_write_the_real_project_adopts_the_placeholder_and_reads_still_work() {
        let (d, s) = svc().await;
        import_wing(&s, &d, "pg_ai").await;
        key_note(&s).await;
        let cwd = repo(&d, "pg-ai");
        let before = context_of(&s, &cwd).await;
        let imported_id = s
            .read(|c| Ok(projects::resolve_slug(c, "pg-ai")?.unwrap().id))
            .await
            .unwrap();
        remember_id(&s, "pg-ai first real note", Some(Kind::Note), &cwd).await;
        wait_settled(&s).await;
        let rows = project_rows(&d);
        assert!(
            !rows.iter().any(|(k, _)| k.starts_with("import:")),
            "{rows:?}"
        );
        let adopted = s
            .read(|c| Ok(projects::resolve_slug(c, "pg-ai")?.unwrap().id))
            .await
            .unwrap();
        assert_eq!(adopted, imported_id);
        let after = context_of(&s, &cwd).await;
        assert!(after.text.contains("KEYNOTE"), "{}", after.text);
        assert_eq!(after.project, before.project);
        let r = s
            .recall(recall_req("deploy notes", RecallMode::Fast, &cwd))
            .await
            .unwrap();
        assert!(r.results.iter().any(|h| h.project == "pg-ai"));
    }

    /// On a tenant, the project hint (`?project=` or the client's root basename) of a project an
    /// inbox import created reads and writes that `import:` placeholder: the read path that looks
    /// placeholders up for a detected checkout never runs there, because the hint resolves the
    /// slug first. After `projects merge`, the hint follows the placeholder to its new project.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_hint_reads_and_writes_the_imported_placeholder() {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: true,
                free_space: Some(Arc::new(|_| Some(u64::MAX / 4))),
                ..quiet()
            },
        )
        .unwrap();
        import_wing_at(&s, &d.path().join("import-inbox/j1/palace"), "pg_ai").await;
        key_note(&s).await;
        let rows = project_rows(&d);
        assert!(rows.iter().any(|(k, _)| k == "import:pg-ai"), "{rows:?}");
        // `pg_ai` is what a client whose root is `.../pg_ai` sends; `pg-ai` what `?project=` may.
        for hint in ["pg_ai", "pg-ai", "PG AI"] {
            let r = s
                .recall(RecallRequest {
                    query: "deploy notes".into(),
                    mode: Some(RecallMode::Fast),
                    format: OutputFormat::Json,
                    project_hint: Some(hint.into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(r.project.as_deref(), Some("pg-ai"), "{hint}");
            assert!(r.results.iter().any(|h| h.project == "pg-ai"), "{hint}");
            let ctx = s
                .context(ContextRequest {
                    project_hint: Some(hint.into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(ctx.project, "pg-ai", "{hint}");
            assert!(ctx.text.contains("KEYNOTE"), "{hint}: {}", ctx.text);
        }
        let w = s
            .remember(RememberRequest {
                content: "the pg-ai deploy window is 22:00".into(),
                project_hint: Some("pg_ai".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(w.project, "pg-ai");
        assert_eq!(
            project_rows(&d),
            rows,
            "a hint of a stored project adds no row"
        );

        // Merged into another project, the placeholder's slug still leads there.
        s.remember(RememberRequest {
            content: "the real app".into(),
            project_hint: Some("Real App".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_settled(&s).await;
        s.merge_projects(alice_core::api::ProjectMergeRequest {
            from: "pg-ai".into(),
            into: "real-app".into(),
            dry_run: false,
            author: Default::default(),
        })
        .await
        .unwrap();
        wait_settled(&s).await;
        let ctx = s
            .context(ContextRequest {
                project_hint: Some("pg_ai".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ctx.project, "real-app");
        assert!(ctx.text.contains("KEYNOTE"), "{}", ctx.text);
        let w = s
            .remember(RememberRequest {
                content: "after the merge".into(),
                project_hint: Some("pg-ai".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(w.project, "real-app");
    }

    /// Final review C1: on a server the Jev endpoint and the judge budget are the admin's, pinned
    /// in the tenant file (here: the settings environment). A person's PATCH does not move them,
    /// and the view shows the shared key as set without any hint of it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_cannot_move_the_jev_endpoint_or_budget_and_sees_no_key_hint() {
        let d = tempfile::tempdir().unwrap();
        let store = MemSecrets::new(Duration::ZERO);
        let official = "https://api.typesafe.ai/v1/systemone";
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: true,
                secrets: Some(store.clone()),
                settings_env: Some(vec![
                    ("ALICE_JUDGE_JEV_ENDPOINT".into(), official.into()),
                    ("ALICE_JUDGE_DAILY_BUDGET_USD".into(), "1.0".into()),
                ]),
                ..quiet()
            },
        )
        .unwrap();
        s.set_secret_async(SecretName::JevApiKey, JEV_TEST_KEY.into())
            .await
            .unwrap();
        let out = s
            .patch_settings(patch_one(
                "judge.jev.endpoint",
                serde_json::json!("https://collector.example/v1"),
            ))
            .unwrap();
        assert!(
            out.overridden_by_env
                .iter()
                .any(|k| k == "judge.jev.endpoint"),
            "{out:?}"
        );
        s.patch_settings(patch_one(
            "judge.daily_budget_usd",
            serde_json::json!(500.0),
        ))
        .unwrap();
        let v = s.settings_view_async().await;
        assert_eq!(v.settings["judge"]["jev"]["endpoint"], official);
        assert_eq!(v.settings["judge"]["daily_budget_usd"], 1.0);
        let k = &v.secrets["jev_api_key"];
        assert!(k.set && k.hint.is_none(), "{k:?}");
    }

    /// Final review M-3: the team store's own answers pass through; a timeout may have saved.
    #[test]
    fn a_shared_write_error_says_what_may_have_happened() {
        let invalid = shared_write_error(AliceError::invalid("content is empty"));
        assert!(matches!(&invalid, AliceError::Invalid(m) if m == "content is empty"));
        let missing = shared_write_error(AliceError::NotFound("memory x".into()));
        assert!(matches!(missing, AliceError::NotFound(_)));
        let late = shared_write_error(AliceError::Other(
            "the team store did not answer in time".into(),
        ))
        .to_string();
        assert!(late.contains("may have been saved"), "{late}");
        let garbled = shared_write_error(AliceError::Other(
            "the team store sent an answer this daemon cannot read: eof".into(),
        ))
        .to_string();
        assert!(garbled.contains("may have been saved"), "{garbled}");
        let down = shared_write_error(AliceError::Other(
            "the team store is unreachable: connection refused".into(),
        ))
        .to_string();
        assert!(down.contains("nothing was saved"), "{down}");
    }
}
