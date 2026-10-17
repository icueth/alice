//! The single writer. Jobs arrive on a channel, are grouped for up to 2 ms (max 256), and run in
//! one transaction with a savepoint per job, so one failing job never rolls back the others.
//! Only this thread writes to the database (spec §4.1), and every transaction is short: a bulk
//! ingest is written a slice at a time, so small writes queued behind it never wait for all of it.

use crate::redaction::Redactors;
use crate::wal::{WalKeeper, WalPolicy};
use alice_core::api::StreamItem;
use alice_core::error::{AliceError, Result};
use alice_core::ids::{Hlc, new_id};
use alice_core::judge::pipeline::JudgeAction;
use alice_core::model::{Kind, Memory, Origin, SourceType, Status};
use alice_core::project::ProjectRef;
use alice_core::redact::Redactor;
use alice_core::store::capture::{self as capture_store, DistillState};
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::events::{self, Actor, EventInput};
use alice_core::store::history::{self, UndoOutcome};
use alice_core::store::import::{self, BatchOutcome, ImportBatch};
use alice_core::store::judge as judge_store;
use alice_core::store::kg::{self, AssertFact, EntityMatcher, FactOutcome};
use alice_core::store::maintenance;
use alice_core::store::memories::{
    self, InsertOutcome, MemoryUpdate, NewMemoryInput, UpdateAction,
};
use alice_core::store::project_merge;
use alice_core::store::projects::{self, ResolvedProject};
use alice_core::store::reviews::{self, NewReview, ReviewItem, ReviewType};
use alice_core::store::sources::{self, ChunkInput, IngestOutcome, SourceInput};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use rusqlite::{Connection, ErrorCode, OptionalExtension};
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};

const BATCH_WINDOW: Duration = Duration::from_millis(2);
const BATCH_MAX: usize = 256;
/// Committed changes a stream subscriber may fall behind by; a slower one misses the oldest
/// (`/api/v1/events` still lists every change).
const STREAM_CAPACITY: usize = 256;
/// Raw chunks written per batch, at most. A larger ingest continues in the following batches
/// (each its own short transaction) and is acknowledged once all of it is committed. A write
/// that arrives while a slice is being written waits for that slice, so its size is what a
/// `remember` can wait behind during an import: 200 chunks / 64 KiB gave a p95 of 117 ms with
/// 20 clients at a 100k-chunk import, 50 chunks / 32 KiB about 58 ms (`docs/benchmarks/m2.md`,
/// Task 20); the import took 16 % longer.
pub(crate) const INGEST_SLICE: usize = 50;
/// Chunk text written per batch, at most (FTS work grows with text, not with chunk count). It
/// must be at least one largest chunk ([`alice_core::model::limits::MAX_CHUNK_BYTES`]): a
/// chunk is deferred when it does not fit what is left of a slice, and only a slice with
/// all of this budget left takes one regardless (see `slice_len`).
pub(crate) const INGEST_SLICE_BYTES: usize = alice_core::model::limits::MAX_CHUNK_BYTES;

#[derive(Debug, Clone)]
pub enum ProjectChoice {
    /// An existing slug or alias. Unknown slugs are rejected so agents cannot create stray projects.
    Slug(String),
    /// Detected from a working directory; created on first write.
    Detected(ProjectRef),
    /// Nothing named a project and there is no working directory: `global`, and the judge may
    /// route the memory to a project later.
    Fallback,
}

/// Text a review resolution writes, as the privacy rules left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveText {
    /// A predicate mapping: `(predicate, canonical)`.
    Predicate(String, String),
    /// A move into another project, screened with that project's rules: the text to store with
    /// it (the memory's own when the rules change nothing). `content_hash` is the hash of the
    /// text that was screened: the move is refused if the memory changed since, so text edited
    /// in between is never stored unscreened.
    Moved {
        content_hash: String,
        /// `updated_at` of the screened memory (topics and importance are not in the hash).
        updated_at: String,
        title: String,
        body: String,
        topics: Vec<String>,
    },
}

/// What the judge decided about one memory (spec §5.1 step 5, §5.2). The writer applies it in
/// one transaction and one event group, then activates the memory.
#[derive(Debug, Clone, Default)]
pub struct Judgement {
    pub seq: i64,
    pub id: String,
    /// `content_hash` of the text that was judged: nothing is applied if the memory changed.
    pub stamp: String,
    /// `updated_at` when it was read: topics and importance are not in the hash.
    pub updated_at: String,
    /// The provider that compared the memory with its neighbours (the actor `judge:<provider>`
    /// of the actions); `heuristic` when the model could not answer.
    pub provider: String,
    pub actions: Vec<JudgeAction>,
    pub reviews: Vec<NewReview>,
    /// Set the kind (the writer gave none and the judge is sure).
    pub kind: Option<Kind>,
    /// The provider that chose `kind` (defaults to `provider`).
    pub kind_provider: Option<String>,
    /// Move to this project slug (routing, auto-applied).
    pub project_to: Option<String>,
    /// The provider that chose `project_to` (defaults to `provider`).
    pub project_provider: Option<String>,
    /// The judge failed or ran out of time: the heuristic decided; try again later.
    pub retry: bool,
    /// Every failure behind `retry` was the judge being unreachable (it does not count toward
    /// giving up).
    pub unavailable: bool,
    /// How long a memory that failed waits before it is judged again.
    pub retry_wait: Duration,
    /// The distillation settings, for a distilled memory's session best set: a newcomer judged
    /// live into a full set evicts its weakest member that may go (Task 12c review I1, option b).
    /// `None`: nothing is evicted.
    pub distill: Option<alice_core::settings::DistillSettings>,
    /// `(id, content_hash)` of every existing memory the judge saw, read together with its
    /// text. An action on one that has changed since becomes a review item instead (nothing is
    /// applied on stale information).
    pub seen: Vec<(String, String)>,
}

/// The provider that distils memories from captured sessions; their events carry the actor
/// `distill:<provider>` (SP2 spec §6.5).
const DISTILL_PROVIDER: &str = "jev";

/// One slice of a capture pass (SP2 spec §3.8), written in one transaction. At most
/// [`INGEST_SLICE`] chunks and [`INGEST_SLICE_BYTES`] of text: a larger slice is refused.
///
/// **A source's slices are sent one at a time, each awaited** before the next is built from the
/// state it left. Two slices of one source in flight together may commit in either order; the
/// writer keeps `next_ord` from moving back, but the second slice's ords would still be stale.
#[derive(Debug, Clone)]
pub struct CaptureSlice {
    /// The transcript's source: type `session`, uri `<agent>:session:<id>[:agent:<id>]`. Its
    /// `agent` and `session_id` are taken from `init` (whatever this says), and its time span
    /// from the chunks' `authored_at`: `ended_at` here is ignored, and `started_at` is used only
    /// when the source is created.
    pub source: SourceInput,
    /// The project of a new source. An existing source keeps its own (spec §3.1).
    pub project: ProjectChoice,
    /// The capture state of a new source.
    pub init: CaptureInit,
    /// At most [`INGEST_SLICE`] chunks and [`INGEST_SLICE_BYTES`] of text in all.
    pub chunks: Vec<ChunkInput>,
    /// `(turn key, first ord)` of the turns that end in this slice. A turn that rendered no
    /// chunk names the ord the next chunk would get.
    pub turn_keys: Vec<(String, i64)>,
    /// Keys of turns that are not stored (an excluded project, spec §3.6). Recorded with the ord
    /// the next chunk would get.
    pub skipped_keys: Vec<String>,
    /// `(byte_offset, next_ord)` after the slice's last complete turn.
    pub cursor: Option<(u64, i64)>,
    /// The hook event behind the pass.
    pub last_event: Option<String>,
    /// The session ended (an ended source stays ended).
    pub ended: bool,
    /// Record the pass's `source.capture` event with this slice, if it stored a chunk.
    pub record_event: bool,
}

/// What a new capture source's state starts with. `agent` and `session_id` are also the
/// source's own (they override `CaptureSlice::source`).
#[derive(Debug, Clone, Default)]
pub struct CaptureInit {
    pub agent: String,
    pub session_id: String,
    /// A subagent's main session; its source id is stored when that source exists.
    pub parent_uri: Option<String>,
    /// Local only.
    pub transcript_path: Option<String>,
    /// Local only: `dev:inode:sha256(first line)`.
    pub file_key: Option<String>,
    /// The tenant client's flag. It only ever turns on.
    pub no_distill: bool,
    /// A backfill registers the source (`alice capture backfill`, spec §4.4): a new source's
    /// distillation starts `skipped_backfill`, a subagent's too. An existing source keeps its
    /// state.
    pub backfill: bool,
}

/// A change to a capture source's state (`WriteOp::CaptureStateUpdate`).
#[derive(Debug, Clone)]
pub enum CaptureChange {
    /// Where its distillation stands, when it is tried next, and how often it failed.
    Distill {
        state: DistillState,
        next_try_at: Option<String>,
        attempts: i64,
    },
    /// Its chunks below `distill_ord` are distilled (it never moves back).
    Distilled {
        distill_ord: i64,
    },
    /// A source a backfill registered goes on live (`store::capture::revive_backfilled`): its
    /// chunks below `from_ord` stay undistilled for good, and it is `idle` again. Only a
    /// `skipped_backfill` source changes.
    LiveAfterBackfill {
        from_ord: i64,
    },
    Ended,
    /// The stale end (spec §6.2, §10; Task 11b): the source is ended and, with `due_since`
    /// (distillation is on), a main session that captured at or after it and holds chunks not
    /// distilled yet becomes due (`store::capture::due_after_stale_end`), in one transaction.
    StaleEnd {
        due_since: Option<String>,
    },
    /// A pass starts beyond the cursor (a tenant client sent from there, or the local worker
    /// skipped a line longer than a pass): the cursor moves there and the gap is counted.
    Gap {
        byte_offset: u64,
    },
    /// The local file is new or was replaced: its identity and where reading goes on.
    FileKey {
        file_key: String,
        byte_offset: u64,
    },
}

#[derive(Debug)]
pub enum WriteOp {
    Remember {
        input: NewMemoryInput,
        project: ProjectChoice,
    },
    /// A distilled memory (SP2 spec §6.4, §6.5; Task 12c): `remember`, with its rank inputs (`p`,
    /// and whether it is a subagent's report) stored. It evicts nothing: when it is stored into a
    /// full best set, a member leaves only once the judge finds it live (`ApplyJudgement`; review
    /// I1, option b).
    Distilled {
        input: NewMemoryInput,
        project: ProjectChoice,
        p: f64,
        subagent: bool,
    },
    /// Validated as a whole before anything is written, then written `INGEST_SLICE` chunks per
    /// transaction. If a later slice fails, the earlier ones stay; ingest is incremental by `ord`,
    /// so retrying the same request completes it.
    Ingest {
        project: ProjectChoice,
        source: SourceInput,
        chunks: Vec<ChunkInput>,
    },
    Update {
        id: String,
        update: MemoryUpdate,
        actor: Actor,
    },
    AssertFact {
        fact: AssertFact,
        actor: Actor,
        /// `(new, canonical)`: the judge matched the fact's new predicate to an existing one, so
        /// `new` is mapped to `canonical` (one undoable group with the fact). Skipped if `new`
        /// has become known since.
        alias: Option<(String, String)>,
        /// Created with the fact: the judge saw a similar predicate but was not sure enough to
        /// map it.
        review: Option<NewReview>,
    },
    /// Reverse an event (and its group); a changed target yields a review item instead.
    Undo {
        event_id: String,
        actor: Actor,
    },
    /// Apply a review decision and record it. `text` is what the item holds after the privacy
    /// rules (the service screens it before the writer, which does not scan content); `None`
    /// uses the item's own.
    ResolveReview {
        id: String,
        choice: String,
        actor: Actor,
        text: Option<ResolveText>,
    },
    /// Close every open review item of a type (and `provider`) with the answer that changes
    /// nothing; one history row records it. Yields `Count`.
    DismissReviews {
        review_type: reviews::ReviewType,
        provider: Option<String>,
        actor: Actor,
    },
    /// `others` become duplicates of `survivor` (admin tool, review `merge`).
    Merge {
        survivor: String,
        others: Vec<String>,
        actor: Actor,
    },
    /// `(key, stamp, vector)`: the vector of the text whose `content_hash` was `stamp`. It is
    /// stored (and the row marked done) only if the row still holds that text.
    StoreEmbeddings {
        model: String,
        items: Vec<(VecKey, String, Vec<f32>)>,
    },
    /// `(key, stamp)` pairs whose embedding failed; `retry: false` gives up on them.
    EmbedFailed {
        keys: Vec<(VecKey, String)>,
        retry: bool,
    },
    /// `(key, stamp)` pairs that were not sent to a remote embedder because the cloud policy
    /// keeps their project at home: `embed_state` becomes `skipped_cloud` (searchable by words,
    /// not retried until the policy or the provider changes). Memories that no judge waits for
    /// are activated, as for a given-up embedding.
    EmbedSkipped {
        keys: Vec<(VecKey, String)>,
    },
    /// Queue up to `limit` rows in state `skipped_cloud` for embedding again, those of
    /// `projects` only (`None`: all); answers `Count` (call until 0). The embedding worker is
    /// woken.
    RequeueSkippedCloud {
        projects: Option<Vec<String>>,
        limit: usize,
    },
    /// The model a re-embed is filling, or `None` when none is. While one is, an edited row's
    /// vector for that model is dropped when the active model re-embeds the row (the re-embed's
    /// final pass embeds the new text).
    SetReembedTarget(Option<String>),
    /// `(key, stamp, vector)` of the re-embed's target `model`: stored only if the row still holds
    /// the text stamped `stamp`; `embed_state` (the active model's) is not touched. Answers
    /// `Embedded` with the items that were stored.
    StoreModelVectors {
        model: String,
        items: Vec<(VecKey, String, Vec<f32>)>,
    },
    /// Delete up to `limit` stored vectors of `model` (after a model switch); answers `Count`.
    DeleteModelVectors {
        model: String,
        limit: usize,
    },
    /// A judgement for one memory: its actions, review items, kind and project, applied in one
    /// group, after which the memory is active. Ignored if the memory's text changed since it
    /// was judged, or if it is not waiting for the judge.
    ApplyJudgement(Judgement),
    /// `retry` memories go back to `pending` (up to `max_attempts` in all); the rest give up. A
    /// distilled memory given up on settles live, so it evicts from its session's best set as a
    /// judged one does (Task 12c review 2, N1), with the distillation settings `distill`.
    RequeueJudgeRetries {
        max_attempts: u32,
        now: String,
        distill: Option<alice_core::settings::DistillSettings>,
    },
    TouchAccess(Vec<i64>),
    /// A change that is not to a memory or a fact (settings, secrets): one history row, and for
    /// `settings.change` a `settings` stream item with the keys in `after.changed`. Never carries
    /// a value of a secret.
    RecordEvent {
        actor: Actor,
        action: String,
        target_type: String,
        target_id: String,
        before: Option<serde_json::Value>,
        after: Option<serde_json::Value>,
    },
    /// A paid judge call that was answered: its response goes to the judge cache and its cost
    /// to the day's ledger, in one transaction (a failing job rolls back alone).
    JudgeCall(alice_core::judge::jev::JevCall),
    /// Remember a value under a `meta` key (maintenance runs, import markers). Nothing else is
    /// recorded.
    SetMeta {
        key: String,
        value: String,
    },
    /// Delete trashed memories for good, with a `memory.purge` event for each. Only memories
    /// that are still in the trash and were last changed before `cutoff` (re-checked here: one
    /// restored or edited since the job looked is kept). The writer works for about
    /// [`SLICE_BUDGET`](crate::maintenance::SLICE_BUDGET) and answers `Purged` with the seqs it
    /// did (the caller drops their vectors); the rest are still in the trash for the next look.
    /// The history of a purged memory is scrubbed.
    PurgeMemories {
        seqs: Vec<i64>,
        cutoff: String,
    },
    /// Delete raw chunks (retention); the caller records one `raw.retention` event per run and drops their
    /// vectors. Works for about the same time box and answers `Purged` with the chunk seqs it
    /// dealt with (the others are untouched).
    DeleteChunks {
        seqs: Vec<i64>,
    },
    /// One bounded FTS merge step; `WriteOutcome::Flag(true)` while there was work.
    FtsMerge {
        table: String,
        pages: i64,
    },
    /// Delete up to `limit` events older than `before`; answers `Count`.
    PruneEvents {
        before: String,
        limit: usize,
    },
    /// Delete up to `limit` judge-cache entries older than `before` or beyond the `keep` newest;
    /// answers `Count`.
    PruneJudgeCache {
        before: String,
        keep: usize,
        limit: usize,
    },
    /// Add a review item (the weekly merge suggestions); announced on the stream. Answers
    /// `Count` of 1, or 0 when an open item asks the same question already.
    CreateReview(NewReview),
    /// One bounded, atomic, idempotent batch of imported records (the importer and nothing
    /// else). It takes a group commit of its own: it runs only when no ingest slice or import
    /// batch has written in the commit yet, and nothing of that kind joins it. Records one
    /// `import.batch` event; answers `Imported`.
    ImportBatch(Box<ImportBatch>),
    /// `alice projects merge`: move everything of `from` into `into` and record the event that
    /// undo reverses (see `store::project_merge`). One transaction, and a commit of its own.
    /// `rules`: what `from`'s own `.alice.toml` puts in force (the merge carries them, and what
    /// earlier merges carried into `from`, into `into`).
    MergeProjects {
        from: String,
        into: String,
        actor: Actor,
        rules: Box<alice_core::settings::ProjectSettings>,
    },
    /// One slice of a capture pass (SP2 spec §3.8): the source and its capture state (created
    /// if new), the chunks, the turn keys and the cursor, in one transaction. It is written
    /// whole and counts against the ingest slice budget: it joins a group commit only if it
    /// fits what is left, and a fresh commit always takes it. A slice over the budget itself is
    /// refused. Send a source's slices one at a time (see [`CaptureSlice`]). Answers `Captured`.
    CaptureSlice(Box<CaptureSlice>),
    /// A change to a capture source's state. Answers `Count`: 0 when the source has no capture
    /// state (retention deleted it).
    CaptureStateUpdate {
        source_id: String,
        change: CaptureChange,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum WriteOutcome {
    Remembered(InsertOutcome, ResolvedProject),
    Ingested(IngestOutcome),
    /// The memory after the change, and the event that recorded it (spec §8.1).
    Updated(Memory, Option<String>),
    Fact(FactOutcome),
    Undo(UndoOutcome),
    /// The review item after its resolution.
    Resolved(Box<ReviewItem>),
    /// The ids that became duplicates.
    Merged(Vec<String>),
    /// The `StoreEmbeddings` items that were stored; the others changed while being embedded.
    Embedded(Vec<(VecKey, Vec<f32>)>),
    /// The seqs of the memories (or chunks) a time-boxed maintenance step dealt with.
    Purged(Vec<i64>),
    /// How many rows a maintenance step deleted (or review items it added).
    Count(usize),
    /// Whether a maintenance step still had work to do.
    Flag(bool),
    Imported(BatchOutcome),
    /// A project merge: what moved, and the event that recorded it.
    ProjectMerged(Box<(project_merge::MergeDone, String)>),
    /// A capture slice: its source, the chunks it stored and the turn keys that were new to
    /// `capture_turns` (repeats count in neither). A key the slice sent that was not new was
    /// stored by another source first.
    Captured {
        source_id: String,
        inserted: usize,
        keys_new: usize,
    },
    Done,
}

impl WriteOutcome {
    /// The variant's name. Logs and error messages use this instead of `Debug`, which would
    /// print the memory text an outcome carries.
    pub fn name(&self) -> &'static str {
        match self {
            WriteOutcome::Remembered(..) => "Remembered",
            WriteOutcome::Ingested(_) => "Ingested",
            WriteOutcome::Updated(..) => "Updated",
            WriteOutcome::Fact(_) => "Fact",
            WriteOutcome::Undo(_) => "Undo",
            WriteOutcome::Resolved(_) => "Resolved",
            WriteOutcome::Merged(_) => "Merged",
            WriteOutcome::Embedded(_) => "Embedded",
            WriteOutcome::Purged(_) => "Purged",
            WriteOutcome::Count(_) => "Count",
            WriteOutcome::Flag(_) => "Flag",
            WriteOutcome::Imported(_) => "Imported",
            WriteOutcome::ProjectMerged(_) => "ProjectMerged",
            WriteOutcome::Captured { .. } => "Captured",
            WriteOutcome::Done => "Done",
        }
    }
}

type Reply = Option<oneshot::Sender<Result<WriteOutcome>>>;

struct Job {
    op: WriteOp,
    reply: Reply,
    /// For an ingest continued from an earlier batch: what its committed slices did.
    progress: Option<IngestProgress>,
}

/// An ingest request part-way through: what its committed slices did, and the text bytes of
/// the whole request (for its summary event).
#[derive(Debug)]
struct IngestProgress {
    done: IngestOutcome,
    request_bytes: usize,
}

/// What applying a job did.
enum Applied {
    Done(WriteOutcome),
    /// Part of an ingest is written (or none, when this batch had no room left); the rest
    /// continues in the next batch.
    More(WriteOp, Option<IngestProgress>),
}

/// Set when the database refuses writes (disk full, corruption); cleared only by restart after repair.
#[derive(Debug, Default)]
pub struct ReadOnlyState(Mutex<Option<String>>);

impl ReadOnlyState {
    pub fn reason(&self) -> Option<String> {
        self.0.lock().clone()
    }
    pub(crate) fn set(&self, reason: String) {
        self.0.lock().get_or_insert(reason);
    }
}

#[derive(Clone)]
pub struct WriterHandle {
    tx: Sender<Job>,
    pub read_only: Arc<ReadOnlyState>,
    /// Items the writer broadcasts after each commit (events, review items).
    stream: broadcast::Sender<StreamItem>,
}

impl WriterHandle {
    /// Changes committed from now on (events, reviews); slow receivers miss old items.
    pub fn subscribe(&self) -> broadcast::Receiver<StreamItem> {
        self.stream.subscribe()
    }

    fn send(&self, op: WriteOp) -> Result<oneshot::Receiver<Result<WriteOutcome>>> {
        if let Some(r) = self.read_only.reason() {
            return Err(AliceError::ReadOnly(r));
        }
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Job {
                op,
                reply: Some(tx),
                progress: None,
            })
            .map_err(|_| AliceError::Other("writer stopped; restart the daemon".into()))?;
        Ok(rx)
    }

    pub async fn submit(&self, op: WriteOp) -> Result<WriteOutcome> {
        self.send(op)?
            .await
            .map_err(|_| AliceError::Other("writer dropped the request; retry".into()))?
    }

    /// For non-async threads (the embedding worker). Must not be called on a runtime thread.
    pub fn submit_blocking(&self, op: WriteOp) -> Result<WriteOutcome> {
        self.send(op)?
            .blocking_recv()
            .map_err(|_| AliceError::Other("writer dropped the request; retry".into()))?
    }

    /// Fire and forget (access counters, shutdown).
    pub fn send_nowait(&self, op: WriteOp) {
        let _ = self.try_send(op);
    }

    /// Fire and forget, saying whether the writer took it (`false`: it has stopped).
    pub fn try_send(&self, op: WriteOp) -> bool {
        self.tx
            .send(Job {
                op,
                reply: None,
                progress: None,
            })
            .is_ok()
    }
}

fn resolve(conn: &Connection, choice: &ProjectChoice) -> Result<ResolvedProject> {
    match choice {
        ProjectChoice::Slug(s) => projects::resolve_slug(conn, s)?.ok_or_else(|| {
            AliceError::invalid(format!(
                "unknown project \"{s}\"; omit project to use the current one, or pass an existing project slug"
            ))
        }),
        ProjectChoice::Detected(p) => projects::upsert_project(conn, p),
        ProjectChoice::Fallback => projects::resolve_slug(conn, "global")?
            .ok_or_else(|| AliceError::Other("the global project is missing".into())),
    }
}

pub(crate) struct State {
    hlc: Hlc,
    /// Entity names for mention links; dropped whenever facts (and so entities) may have changed.
    matcher: Option<EntityMatcher>,
    /// A job in this batch added or changed text that needs embedding.
    inserted: bool,
    /// The model a re-embed is filling (see `WriteOp::SetReembedTarget`).
    reembed_target: Option<String>,
    /// Raw chunks this batch may still write.
    chunk_budget: usize,
    /// Bytes of chunk text this batch may still write.
    byte_budget: usize,
    /// Items this batch broadcasts once it commits (events; later reviews and settings). A job
    /// that fails takes its own items back; a batch that fails to commit sends none.
    stream: Vec<StreamItem>,
    /// The privacy rules of every project, for what the writer itself derives from stored rows.
    redactors: Arc<Redactors>,
    /// Tests: bulk work written in the current commit, and where each commit's list goes.
    #[cfg(test)]
    applied: Vec<&'static str>,
    #[cfg(test)]
    commit_log: Option<CommitLog>,
}

#[cfg(test)]
pub(crate) type CommitLog = Arc<Mutex<Vec<Vec<&'static str>>>>;

/// The redactor for the project with id `project_id` (`None`: no project), looked up with the
/// writer's own connection. A project that was never rooted (no working tree) has only the
/// global rules. A project whose rules the store cannot read gets the closed ones.
fn rules_for(conn: &Connection, redactors: &Redactors, project_id: Option<&str>) -> Arc<Redactor> {
    let tree = project_id.and_then(|p| {
        projects::tree_of(conn, p).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "a project's rules could not be read; its history is scrubbed with the closed rules");
            Some(alice_core::settings::ProjectTree::unreadable(p, None))
        })
    });
    redactors.for_root(tree.as_ref())
}

/// Record an event in the current transaction; it is broadcast after the batch commits.
/// Returns the event id. The before and after snapshots are redacted first, so neither the
/// `events` table nor the stream ever carries a secret.
pub(crate) fn record(conn: &Connection, st: &mut State, mut e: EventInput<'_>) -> Result<String> {
    {
        let redactors = st.redactors.clone();
        let rules = |c: &Connection, project: Option<&str>| rules_for(c, &redactors, project);
        history::scrub_snapshots(conn, &rules, &mut e.before, &mut e.after);
    }
    let hlc = st.hlc.tick();
    let id = events::record(conn, &e, &hlc)?;
    if let Some(event) = events::get(conn, &id)? {
        st.stream.push(StreamItem::Event { event });
    }
    Ok(id)
}

/// The action of the event a settings change records.
pub(crate) const SETTINGS_CHANGE: &str = "settings.change";

/// The keys a `settings.change` event names (`after.changed`).
fn settings_changed(after: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let keys = after?.get("changed")?.as_array()?;
    Some(
        keys.iter()
            .filter_map(|k| k.as_str().map(String::from))
            .collect(),
    )
}

/// A memory as events record it (`None`: no such row).
fn snap(conn: &Connection, id: &str) -> Result<Option<serde_json::Value>> {
    history::snapshot_memory(conn, id)?
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| AliceError::Other(format!("memory snapshot: {e}")))
}

/// A relation as events record it (`None`: no such row).
fn snap_relation(conn: &Connection, id: &str) -> Result<Option<serde_json::Value>> {
    history::snapshot_relation(conn, id)?
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| AliceError::Other(format!("relation snapshot: {e}")))
}

fn update_action(a: UpdateAction) -> &'static str {
    match a {
        UpdateAction::Edit => "memory.edit",
        UpdateAction::Supersede => "memory.supersede",
        UpdateAction::Archive => "memory.archive",
        UpdateAction::Forget => "memory.forget",
        UpdateAction::Restore => "memory.restore",
        UpdateAction::Move => "memory.move",
    }
}

/// `remember`: the insert, a `memory.create` event, and for each memory it supersedes that
/// changed, a `memory.supersede` event, all in one group.
fn remember(
    conn: &Connection,
    input: NewMemoryInput,
    project: &ProjectChoice,
    st: &mut State,
) -> Result<WriteOutcome> {
    let p = resolve(conn, project)?;
    let mut input = input;
    input.project_defaulted = matches!(project, ProjectChoice::Fallback);
    let prepared = memories::prepare(input, &p, st.hlc.tick())?;
    // Snapshots of the memories this one supersedes, before the insert changes them.
    let mut superseded: Vec<(String, Option<serde_json::Value>)> = Vec::new();
    for old in &prepared.supersedes {
        if let Some(id) = memories::id_of(conn, old)?
            && !superseded.iter().any(|(s, _)| *s == id)
        {
            let before = snap(conn, &id)?;
            superseded.push((id, before));
        }
    }
    let out = memories::insert(conn, &prepared)?;
    if let InsertOutcome::New { id, .. } = &out {
        st.inserted = true;
        let actor = match prepared.origin {
            Origin::Distill => Actor::Distill(DISTILL_PROVIDER.into()),
            Origin::Agent | Origin::Import => Actor::from_author(&prepared.author),
        };
        let group = (!prepared.supersedes.is_empty()).then(new_id);
        let after = snap(conn, id)?;
        record(
            conn,
            st,
            EventInput {
                actor: &actor,
                action: "memory.create",
                target_type: "memory",
                target_id: id,
                before: None,
                after,
                group: group.as_deref(),
            },
        )?;
        for (old, before) in superseded {
            let after = snap(conn, &old)?;
            if after != before {
                record(
                    conn,
                    st,
                    EventInput {
                        actor: &actor,
                        action: "memory.supersede",
                        target_type: "memory",
                        target_id: &old,
                        before,
                        after,
                        group: group.as_deref(),
                    },
                )?;
            }
        }
    }
    Ok(WriteOutcome::Remembered(out, p))
}

/// The actor of a trash that makes room in a session's best set (Task 12c): `distill:cap`.
const DISTILL_CAP: &str = "cap";

/// A distilled memory (`WriteOp::Distilled`): [`remember`]; then, when it is new, its rank inputs.
/// It evicts nothing: a member of a full set leaves only once the judge finds the newcomer live
/// ([`evict_over_cap`]; the controller's ruling on Task 12c review I1, option b).
fn distilled(
    conn: &Connection,
    input: NewMemoryInput,
    project: &ProjectChoice,
    p: f64,
    subagent: bool,
    st: &mut State,
) -> Result<WriteOutcome> {
    let out = remember(conn, input, project, st)?;
    if let WriteOutcome::Remembered(InsertOutcome::New { id, .. }, _) = &out {
        let tier = if subagent { "subagent" } else { "main" };
        conn.execute(
            "UPDATE memories SET distill_p = ?1, distill_tier = ?2 WHERE id = ?3",
            rusqlite::params![p, tier, id],
        )?;
    }
    Ok(out)
}

/// The author of the distilled memory `id`, as its session's best set is read by
/// (`store::capture::session_held`): `None` for a memory that is not distilled or has none.
fn distilled_author(conn: &Connection, id: &str) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT author_agent, author_session FROM memories WHERE id = ?1 AND origin = 'distill'",
            [id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .and_then(|(a, s)| a.zip(s)))
}

/// The distilled newcomer `id` settled live, judged or given up on: its session's best set keeps
/// its size ([`evict_over_cap`]). `settings` come with the op. Without them eviction would be
/// skipped in silence (Task 12c review 2, N2): a debug build stops, a release build warns and
/// evicts nothing.
fn settle_distilled(
    conn: &Connection,
    st: &mut State,
    id: &str,
    settings: Option<&alice_core::settings::DistillSettings>,
) -> Result<()> {
    debug_assert!(
        settings.is_some(),
        "a judgement of a distilled newcomer carries the distillation settings ({id})"
    );
    match settings {
        Some(s) => evict_over_cap(conn, st, id, s),
        None => {
            tracing::warn!(
                id,
                "a distilled memory settled without the distillation settings: its session's best set may stay past its size"
            );
            Ok(())
        }
    }
}

/// After the judgement of the distilled newcomer `id` left it live: when its session's best set
/// is now over its size, or over its subagents' share, the weakest member that may go is trashed
/// by `distill:cap`, in the judgement's op (spec §6.4; Task 12c review I1, option b). It may be
/// the newcomer itself; never a handoff or a protected member ([`alice_core::distill::evictee`]).
fn evict_over_cap(
    conn: &Connection,
    st: &mut State,
    id: &str,
    settings: &alice_core::settings::DistillSettings,
) -> Result<()> {
    let Some((agent, session)) = distilled_author(conn, id)? else {
        return Ok(());
    };
    let held = alice_core::store::capture::session_held(conn, &agent, &session)?;
    if !held.iter().any(|h| h.id == id) {
        return Ok(()); // no longer live
    }
    if let Some(out) = alice_core::distill::evictee(&held, settings) {
        let forget = MemoryUpdate {
            action: UpdateAction::Forget,
            ..Default::default()
        };
        update(
            conn,
            &out.id,
            &forget,
            &Actor::Distill(DISTILL_CAP.into()),
            st,
        )?;
    }
    Ok(())
}

/// `update`: the change and its `memory.<action>` event, whose id it returns. A supersede also
/// links the newer memory to this one: a `memory.link` event in the same group.
fn update(
    conn: &Connection,
    id: &str,
    update: &MemoryUpdate,
    actor: &Actor,
    st: &mut State,
) -> Result<WriteOutcome> {
    // Snapshots by the memory's own id (the caller may have named it by a legacy id).
    let before = match memories::id_of(conn, id)? {
        Some(own) => snap(conn, &own)?,
        None => None,
    };
    let by = match (update.action, update.superseded_by.as_deref()) {
        (UpdateAction::Supersede, Some(b)) => memories::id_of(conn, b)?,
        _ => None,
    };
    let before_by = match &by {
        Some(b) => snap(conn, b)?,
        None => None,
    };
    // A move into a slug no project has creates its placeholder first, in this transaction;
    // the event that records it shares the group with the move, so undo reverses both.
    let mut group = None;
    if update.action == UpdateAction::Move {
        let own = memories::id_of(conn, id)?.ok_or_else(|| {
            AliceError::NotFound(format!(
                "memory {id}; check the id returned by recall or remember"
            ))
        })?;
        let slug = projects::move_slug(update.project.as_deref().ok_or_else(|| {
            AliceError::invalid("move needs project: the slug of the project to move it to")
        })?)?;
        // The text screened with the destination's rules must be the text that moves: an edit
        // that landed since is refused, never stored unscreened.
        if let Some(s) = &update.screened {
            let now: (String, String) = conn.query_row(
                "SELECT content_hash, updated_at FROM memories WHERE id = ?1",
                [&own],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if now != (s.content_hash.clone(), s.updated_at.clone()) {
                return Err(AliceError::invalid(format!(
                    "memory {id} was edited while its move was being checked against the privacy rules of project {slug}; nothing was changed, try again"
                )));
            }
        }
        match projects::resolve_move_target(conn, &slug)? {
            Some(target) => {
                let current: String = conn.query_row(
                    "SELECT project_id FROM memories WHERE id = ?1",
                    [&own],
                    |r| r.get(0),
                )?;
                if current == target.id {
                    // Already there: nothing to change, nothing to undo.
                    let m = memories::get_many(conn, std::slice::from_ref(&own))?
                        .pop()
                        .ok_or_else(|| AliceError::NotFound(format!("memory {id}")))?;
                    return Ok(WriteOutcome::Updated(m, None));
                }
            }
            None => {
                let g = new_id();
                let spec = projects::create_placeholder(conn, &slug)?;
                record(
                    conn,
                    st,
                    EventInput {
                        actor,
                        action: projects::PLACEHOLDER_ACTION,
                        target_type: projects::PLACEHOLDER_TARGET,
                        target_id: &spec.id,
                        before: None,
                        after: Some(serde_json::to_value(&spec).map_err(|e| {
                            AliceError::Other(format!("placeholder snapshot: {e}"))
                        })?),
                        group: Some(&g),
                    },
                )?;
                group = Some(g);
            }
        }
    }
    let mut m = memories::apply_update(conn, id, update, st.hlc.tick())?;
    st.inserted = true; // edits may need re-embedding
    // A moved memory whose text the destination's rules clean is stored cleaned, as an edit in
    // the same group (undo of the move reverses both), as the review move does.
    let cleaned = match &update.screened {
        Some(s)
            if update.action == UpdateAction::Move
                && (m.title != s.title || m.body != s.body || m.topics != s.topics) =>
        {
            Some(s)
        }
        _ => None,
    };
    let group = group
        .or_else(|| by.is_some().then(new_id))
        .or_else(|| cleaned.is_some().then(new_id));
    let after = snap(conn, &m.id)?;
    let event = record(
        conn,
        st,
        EventInput {
            actor,
            action: update_action(update.action),
            target_type: "memory",
            target_id: &m.id,
            before,
            after,
            group: group.as_deref(),
        },
    )?;
    if let Some(s) = cleaned {
        let before = snap(conn, &m.id)?;
        m = memories::apply_update(
            conn,
            &m.id,
            &MemoryUpdate {
                action: UpdateAction::Edit,
                title: Some(s.title.clone()),
                body: Some(s.body.clone()),
                topics: Some(s.topics.clone()),
                ..Default::default()
            },
            st.hlc.tick(),
        )?;
        record(
            conn,
            st,
            EventInput {
                actor,
                action: "memory.edit",
                target_type: "memory",
                target_id: &m.id,
                before,
                after: snap(conn, &m.id)?,
                group: group.as_deref(),
            },
        )?;
    }
    if let Some(by) = &by {
        let after_by = snap(conn, by)?;
        if after_by != before_by {
            record(
                conn,
                st,
                EventInput {
                    actor,
                    action: "memory.link",
                    target_type: "memory",
                    target_id: by,
                    before: before_by,
                    after: after_by,
                    group: group.as_deref(),
                },
            )?;
        }
    }
    Ok(WriteOutcome::Updated(m, Some(event)))
}

/// `assert_fact`: the change, a `relation.close` event for each open relation of the subject
/// and predicate that it closed, and `relation.add` for a new relation, all in one group.
/// Asserting what is already current changes nothing and records nothing.
fn assert_fact(
    conn: &Connection,
    fact: &AssertFact,
    actor: &Actor,
    alias: Option<(String, String)>,
    review: Option<NewReview>,
    st: &mut State,
) -> Result<WriteOutcome> {
    st.matcher = None; // the entity list may change
    let group = new_id();
    // The mapping comes first, so the fact below resolves to the canonical predicate. A name
    // that became known since the judge looked is left alone, and a mapping that no longer
    // applies never fails the fact.
    if let Some((new, canonical)) = alias
        && !fact.predicate.trim().is_empty()
        && !kg::predicate_exists(conn, &new)?
    {
        let h = st.hlc.tick();
        match kg::map_predicate_recorded(conn, &new, &canonical, &h) {
            Ok((before, after)) => {
                let json = |v: &kg::PredicateSnapshot| {
                    serde_json::to_value(v)
                        .map_err(|e| AliceError::Other(format!("predicate snapshot: {e}")))
                };
                record(
                    conn,
                    st,
                    EventInput {
                        actor,
                        action: "predicate.map",
                        target_type: "predicate",
                        target_id: &new,
                        before: Some(json(&before)?),
                        after: Some(json(&after)?),
                        group: Some(&group),
                    },
                )?;
            }
            Err(e) => tracing::warn!(error = %e, "the judge's predicate mapping no longer applies"),
        }
    }
    let open = kg::open_relation_ids(conn, &fact.subject, &fact.predicate)?;
    let mut befores = Vec::with_capacity(open.len());
    for id in &open {
        befores.push((id.as_str(), snap_relation(conn, id)?));
    }
    let out = kg::assert_fact(conn, fact, st.hlc.tick())?;
    for (id, before) in befores {
        let after = snap_relation(conn, id)?;
        if after != before {
            record(
                conn,
                st,
                EventInput {
                    actor,
                    action: "relation.close",
                    target_type: "relation",
                    target_id: id,
                    before,
                    after,
                    group: Some(&group),
                },
            )?;
        }
    }
    if let Some(new) = out
        .relation_id
        .as_deref()
        .filter(|r| !open.iter().any(|o| o == r))
    {
        let after = snap_relation(conn, new)?;
        record(
            conn,
            st,
            EventInput {
                actor,
                action: "relation.add",
                target_type: "relation",
                target_id: new,
                before: None,
                after,
                group: Some(&group),
            },
        )?;
    }
    if let Some(r) = review
        && let Some(id) = reviews::create(conn, &r)?
    {
        st.stream.push(StreamItem::Review {
            id,
            review_type: r.kind.as_str().into(),
        });
    }
    Ok(WriteOutcome::Fact(out))
}

/// `undo`: reverse an event (and its group), or, when a target changed since, a review item.
fn undo(conn: &Connection, event_id: &str, actor: &Actor, st: &mut State) -> Result<WriteOutcome> {
    undo_with(conn, event_id, actor, st, false)
}

/// `undo`, or with `force` an undo that does not look at whether the target changed since (the
/// answer "undo anyway" to a review item). Privacy rules apply either way.
fn undo_with(
    conn: &Connection,
    event_id: &str,
    actor: &Actor,
    st: &mut State,
    force: bool,
) -> Result<WriteOutcome> {
    let out = {
        let redactors = st.redactors.clone();
        let rules = |c: &Connection, project: Option<&str>| rules_for(c, &redactors, project);
        let hlc = &mut st.hlc;
        let mut tick = || hlc.tick();
        if force {
            history::force_undo(conn, event_id, actor, &mut tick, &rules)?
        } else {
            history::undo(conn, event_id, actor, &mut tick, &rules)?
        }
    };
    match &out {
        UndoOutcome::Undone { group, .. } => {
            for event in events::group(conn, group)? {
                st.stream.push(StreamItem::Event { event });
            }
            st.inserted = true; // restored text is embedded again
        }
        // Announced once, when it is added; asking again reuses the open item.
        UndoOutcome::Conflict {
            review_id,
            created: true,
            ..
        } => st.stream.push(StreamItem::Review {
            id: review_id.clone(),
            review_type: "undo".into(),
        }),
        UndoOutcome::Conflict { created: false, .. } => {}
    }
    Ok(WriteOutcome::Undo(out))
}

/// `old` becomes superseded by `new` (status and a `supersedes` link), with events in one
/// group: `group`, or a new one. A memory that is gone or in the trash cannot take part.
fn supersede(
    conn: &Connection,
    st: &mut State,
    old: &str,
    new: &str,
    actor: &Actor,
    group: Option<&str>,
) -> Result<()> {
    for (what, id) in [("older", old), ("newer", new)] {
        match snap(conn, id)? {
            None => {
                return Err(AliceError::invalid(format!(
                    "the {what} memory {id} no longer exists; reject this item instead"
                )));
            }
            Some(s) if s["status"] == "trashed" => {
                return Err(AliceError::invalid(format!(
                    "the {what} memory {id} is in the trash; reject this item instead"
                )));
            }
            Some(s) if s["status"] == "superseded" || s["status"] == "archived" => {
                return Err(AliceError::invalid(format!(
                    "the {what} memory {id} is already {} since this item was raised; reject it instead",
                    s["status"].as_str().unwrap_or("changed")
                )));
            }
            Some(_) => {}
        }
    }
    let group = group.map(str::to_owned).unwrap_or_else(new_id);
    let (b_old, b_new) = (snap(conn, old)?, snap(conn, new)?);
    let h = st.hlc.tick();
    memories::apply_update(
        conn,
        old,
        &MemoryUpdate {
            action: UpdateAction::Supersede,
            superseded_by: Some(new.into()),
            ..Default::default()
        },
        h,
    )?;
    let (old_id, new_own) = (
        memories::id_of(conn, old)?.unwrap_or_else(|| old.to_string()),
        memories::id_of(conn, new)?.unwrap_or_else(|| new.to_string()),
    );
    record(
        conn,
        st,
        EventInput {
            actor,
            action: "memory.supersede",
            target_type: "memory",
            target_id: &old_id,
            before: b_old,
            after: snap(conn, &old_id)?,
            group: Some(&group),
        },
    )?;
    record(
        conn,
        st,
        EventInput {
            actor,
            action: "memory.link",
            target_type: "memory",
            target_id: &new_own,
            before: b_new,
            after: snap(conn, &new_own)?,
            group: Some(&group),
        },
    )?;
    Ok(())
}

/// `others` become trashed duplicates of `survivor`, with events in one group: `group`, or a
/// new one. Returns the ids merged. (Provenance moved by a merge is not part of a snapshot:
/// undoing a merge restores status and links, and the provenance copied to the survivor stays,
/// which is harmless.)
fn merge(
    conn: &Connection,
    st: &mut State,
    survivor: &str,
    others: &[String],
    actor: &Actor,
    group: Option<&str>,
) -> Result<Vec<String>> {
    let group = group.map(str::to_owned).unwrap_or_else(new_id);
    // By their own ids (a caller may name a memory by a legacy id); unknown ones are skipped.
    let survivor = memories::id_of(conn, survivor)?.ok_or_else(|| {
        AliceError::NotFound(format!("memory {survivor} (survivor); check the id"))
    })?;
    let survivor = survivor.as_str();
    let mut own: Vec<String> = Vec::new();
    for o in others {
        if let Some(id) = memories::id_of(conn, o)?
            && !own.contains(&id)
        {
            own.push(id);
        }
    }
    let others = own.as_slice();
    let befores: Vec<(String, Option<serde_json::Value>)> = others
        .iter()
        .map(|o| Ok((o.clone(), snap(conn, o)?)))
        .collect::<Result<_>>()?;
    let b_survivor = snap(conn, survivor)?;
    let h = st.hlc.tick();
    let merged = memories::merge_into(conn, survivor, others, &h)?;
    for (id, before) in befores.into_iter().filter(|(id, _)| merged.contains(id)) {
        record(
            conn,
            st,
            EventInput {
                actor,
                action: "memory.merge",
                target_type: "memory",
                target_id: &id,
                before,
                after: snap(conn, &id)?,
                group: Some(&group),
            },
        )?;
    }
    if !merged.is_empty() {
        record(
            conn,
            st,
            EventInput {
                actor,
                action: "memory.merge_into",
                target_type: "memory",
                target_id: survivor,
                before: b_survivor,
                after: snap(conn, survivor)?,
                group: Some(&group),
            },
        )?;
    }
    Ok(merged)
}

/// Apply the decision `choice` on review item `id` and record it. Everything happens in the
/// job's savepoint: a decision that fails changes nothing and leaves the item open.
fn resolve_review(
    conn: &Connection,
    st: &mut State,
    id: &str,
    choice: &str,
    actor: &Actor,
    text: Option<ResolveText>,
) -> Result<WriteOutcome> {
    let item = reviews::get(conn, id)?.ok_or_else(|| {
        AliceError::NotFound(format!("review item {id}; list them with `alice review`"))
    })?;
    // Not open, or not a choice of its type: the same errors as recording the resolution, before
    // anything is applied.
    if item.status != reviews::ReviewStatus::Open || !item.choices.iter().any(|c| c.id == choice) {
        reviews::mark_resolved(conn, id, choice, actor)?;
    }
    let p = &item.payload;
    let s = |k: &str| {
        p.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    // One decision is one undoable group, whatever it touches.
    let group = new_id();
    match (item.review_type, choice) {
        (ReviewType::Supersede, "accept") | (ReviewType::Contradiction, "keep_new") => {
            supersede(conn, st, &s("old_id"), &s("new_id"), actor, Some(&group))?;
        }
        (ReviewType::Contradiction, "keep_old") => {
            supersede(conn, st, &s("new_id"), &s("old_id"), actor, Some(&group))?;
        }
        (ReviewType::Merge, "merge") => {
            let others: Vec<String> = p["others"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            let merged = merge(conn, st, &s("survivor"), &others, actor, Some(&group))?;
            if merged.is_empty() {
                return Err(AliceError::invalid(
                    "nothing is left to merge (the duplicates are gone or already in the trash); reject this item instead",
                ));
            }
        }
        (ReviewType::Predicate, "map") => {
            let (name, canonical) = match text {
                Some(ResolveText::Predicate(n, c)) => (n, c),
                _ => (s("predicate"), s("canonical")),
            };
            let h = st.hlc.tick();
            let (before, after) = kg::map_predicate_recorded(conn, &name, &canonical, &h)?;
            st.matcher = None;
            let json = |v: &kg::PredicateSnapshot| {
                serde_json::to_value(v)
                    .map_err(|e| AliceError::Other(format!("predicate snapshot: {e}")))
            };
            record(
                conn,
                st,
                EventInput {
                    actor,
                    action: "predicate.map",
                    target_type: "predicate",
                    target_id: &name,
                    before: Some(json(&before)?),
                    after: Some(json(&after)?),
                    group: Some(&group),
                },
            )?;
        }
        (ReviewType::Project, "move") => {
            let memory = s("memory_id");
            let target = projects::resolve_slug(conn, &s("to"))?.ok_or_else(|| {
                AliceError::invalid(format!(
                    "project {} no longer exists; keep the memory where it is",
                    s("to")
                ))
            })?;
            let Some(before) = snap(conn, &memory)? else {
                return Err(AliceError::NotFound(format!("memory {memory}")));
            };
            if before["status"] == "trashed" {
                return Err(AliceError::invalid(format!(
                    "memory {memory} is in the trash; keep it where it is"
                )));
            }
            // The memory must still be where the item saw it.
            let current = before["project_id"].as_str().unwrap_or_default();
            if current == target.id {
                return Err(AliceError::invalid(format!(
                    "memory {memory} is already in project {}; keep it where it is",
                    target.slug
                )));
            }
            if let Some(from) = projects::resolve_slug(conn, &s("from"))?
                && from.id != current
            {
                return Err(AliceError::invalid(format!(
                    "memory {memory} is no longer in project {}; keep it where it is",
                    from.slug
                )));
            }
            // The text that was screened with the destination's rules must be the text that
            // moves: an edit that landed since is refused, never stored unscreened.
            if let Some(ResolveText::Moved {
                content_hash,
                updated_at,
                ..
            }) = &text
            {
                let now: Option<(String, String)> = conn
                    .query_row(
                        "SELECT content_hash, updated_at FROM memories WHERE id = ?1",
                        [before["id"].as_str().unwrap_or(&memory)],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if now.as_ref().map(|(h, u)| (h.as_str(), u.as_str()))
                    != Some((content_hash.as_str(), updated_at.as_str()))
                {
                    return Err(AliceError::invalid(format!(
                        "memory {memory} was edited while its move was being checked against the privacy rules of project {}; nothing was changed, try again",
                        target.slug
                    )));
                }
            }
            let before = Some(before);
            let h = st.hlc.tick();
            match memories::move_to_project(conn, &memory, &target.id, &h)? {
                memories::MoveOutcome::Moved => {
                    let own = memories::id_of(conn, &memory)?.unwrap_or(memory);
                    record(
                        conn,
                        st,
                        EventInput {
                            actor,
                            action: "memory.move",
                            target_type: "memory",
                            target_id: &own,
                            before,
                            after: snap(conn, &own)?,
                            group: Some(&group),
                        },
                    )?;
                    // Under redact mode the destination's rules may clean the text: stored as
                    // an edit in the same group.
                    if let Some(ResolveText::Moved {
                        title,
                        body,
                        topics,
                        ..
                    }) = text
                        && memories::get_many(conn, std::slice::from_ref(&own))?
                            .first()
                            .is_some_and(|m| {
                                m.title != title || m.body != body || m.topics != topics
                            })
                    {
                        let before = snap(conn, &own)?;
                        let h = st.hlc.tick();
                        memories::apply_update(
                            conn,
                            &own,
                            &MemoryUpdate {
                                action: UpdateAction::Edit,
                                title: Some(title),
                                body: Some(body),
                                topics: Some(topics),
                                ..Default::default()
                            },
                            h,
                        )?;
                        st.inserted = true;
                        record(
                            conn,
                            st,
                            EventInput {
                                actor,
                                action: "memory.edit",
                                target_type: "memory",
                                target_id: &own,
                                before,
                                after: snap(conn, &own)?,
                                group: Some(&group),
                            },
                        )?;
                    }
                }
                memories::MoveOutcome::DuplicateOf(other) => {
                    merge(conn, st, &other, &[memory], actor, Some(&group))?;
                }
            }
        }
        (ReviewType::Undo, "apply") => match undo_with(conn, &s("event_id"), actor, st, true)? {
            WriteOutcome::Undo(UndoOutcome::Undone { .. }) => {}
            _ => return Err(AliceError::Other("forced undo did not apply".into())),
        },
        (ReviewType::SyncConflict, _) => {
            return Err(AliceError::invalid(
                "sync conflicts are resolved by `alice sync` (coming in a later release); nothing was changed",
            ));
        }
        // reject / keep / keep_both / dismiss: nothing changes.
        _ => {}
    }
    reviews::mark_resolved(conn, id, choice, actor)?;
    record(
        conn,
        st,
        EventInput {
            actor,
            action: "review.resolve",
            target_type: "review",
            target_id: id,
            before: None,
            after: Some(serde_json::json!({"choice": choice})),
            group: None,
        },
    )?;
    Ok(WriteOutcome::Resolved(Box::new(
        reviews::get(conn, id)?.ok_or_else(|| AliceError::Other("review item vanished".into()))?,
    )))
}

/// Dismiss the open items of `review_type` in one step; no memory or fact changes.
fn dismiss_reviews(
    conn: &Connection,
    st: &mut State,
    review_type: reviews::ReviewType,
    provider: Option<&str>,
    actor: &Actor,
) -> Result<WriteOutcome> {
    let n = reviews::dismiss_open(conn, review_type, provider, actor)?;
    if n > 0 {
        record(
            conn,
            st,
            EventInput {
                actor,
                action: "review.dismiss_all",
                target_type: "review",
                target_id: review_type.as_str(),
                before: None,
                after: Some(serde_json::json!({"dismissed": n, "provider": provider})),
                group: None,
            },
        )?;
    }
    Ok(WriteOutcome::Count(n as usize))
}

impl State {
    fn matcher(&mut self, conn: &Connection) -> Result<&EntityMatcher> {
        if self.matcher.is_none() {
            self.matcher = Some(EntityMatcher::load(conn)?);
        }
        Ok(self.matcher.as_ref().expect("loaded above"))
    }
}

/// How many leading `chunks` fit in a slice with room for `max_chunks` chunks and `max_bytes` of
/// text. A slice with all of its byte room takes at least one chunk, so a large chunk is never
/// deferred forever (chunks are at most 32 KB, so this is a safety net).
fn slice_len(chunks: &[ChunkInput], (max_chunks, max_bytes): (usize, usize)) -> usize {
    let mut bytes = 0;
    let mut n = 0;
    for c in chunks.iter().take(max_chunks) {
        bytes += c.text.len();
        if bytes > max_bytes {
            break;
        }
        n += 1;
    }
    if n == 0 && max_chunks > 0 && max_bytes >= INGEST_SLICE_BYTES {
        n = chunks.len().min(1);
    }
    n
}

fn ingest_slice(
    conn: &Connection,
    project: ProjectChoice,
    source: SourceInput,
    mut chunks: Vec<ChunkInput>,
    progress: Option<IngestProgress>,
    st: &mut State,
) -> Result<Applied> {
    let request_bytes = match &progress {
        Some(p) => p.request_bytes,
        None => {
            // The whole request is checked before its first slice is written.
            sources::validate_chunks(&chunks)?;
            chunks.iter().map(|c| c.text.len()).sum()
        }
    };
    let take = slice_len(&chunks, (st.chunk_budget, st.byte_budget));
    if take == 0 && !chunks.is_empty() {
        let op = WriteOp::Ingest {
            project,
            source,
            chunks,
        };
        return Ok(Applied::More(op, progress));
    }
    let rest = chunks.split_off(take);
    #[cfg(test)]
    st.applied.push("Ingest");
    let p = resolve(conn, &project)?;
    let out = sources::ingest(conn, &p, &source, &chunks)?;
    st.chunk_budget -= take;
    let bytes: usize = chunks.iter().map(|c| c.text.len()).sum();
    st.byte_budget = st.byte_budget.saturating_sub(bytes);
    if out.inserted > 0 {
        st.inserted = true;
    }
    let total = match progress {
        Some(p) => IngestOutcome {
            source_id: out.source_id,
            inserted: p.done.inserted + out.inserted,
            skipped: p.done.skipped + out.skipped,
        },
        None => out,
    };
    if rest.is_empty() {
        record_ingest(conn, st, &source, &total, request_bytes)?;
        return Ok(Applied::Done(WriteOutcome::Ingested(total)));
    }
    let op = WriteOp::Ingest {
        project,
        source,
        chunks: rest,
    };
    Ok(Applied::More(
        op,
        Some(IngestProgress {
            done: total,
            request_bytes,
        }),
    ))
}

/// One summary event per ingest request (spec §5.5), never one per chunk: recorded with its
/// last slice, by the source's agent, when the request wrote any chunk. Raw chunks are
/// evidence, not memories: the event is history only and cannot be undone. A request whose
/// later slice fails keeps the slices already written, without an event; retrying it writes
/// the rest and records it.
fn record_ingest(
    conn: &Connection,
    st: &mut State,
    source: &SourceInput,
    total: &IngestOutcome,
    request_bytes: usize,
) -> Result<()> {
    if total.inserted == 0 {
        return Ok(());
    }
    let actor = Actor::from_author(&alice_core::model::Author {
        agent: source.agent.clone(),
        ..Default::default()
    });
    record(
        conn,
        st,
        EventInput {
            actor: &actor,
            action: "source.ingest",
            target_type: "source",
            target_id: &total.source_id,
            before: None,
            after: Some(serde_json::json!({
                "source_id": total.source_id,
                "source_type": source.source_type,
                "uri": source.uri,
                "inserted": total.inserted,
                "skipped": total.skipped,
                "bytes": request_bytes,
            })),
            group: None,
        },
    )?;
    Ok(())
}

/// `WriteOp::ImportBatch`. The batch takes the commit to itself: with any bulk work already
/// written in this commit (budget touched) it waits for the next one, and it spends the whole
/// budget so no ingest slice joins it. Alone in a fresh commit it always runs, however heavy.
fn import_batch(conn: &Connection, batch: Box<ImportBatch>, st: &mut State) -> Result<Applied> {
    let fresh = st.chunk_budget == INGEST_SLICE && st.byte_budget == INGEST_SLICE_BYTES;
    if !fresh {
        return Ok(Applied::More(WriteOp::ImportBatch(batch), None));
    }
    let (records, bytes) = batch.weight();
    if records > INGEST_SLICE || bytes > INGEST_SLICE_BYTES {
        tracing::warn!(
            records,
            bytes,
            "an import batch is heavier than one commit's budget; the importer should send smaller ones"
        );
    }
    // Charged in full, whatever the batch weighs (`ImportBatch::weight` is at most what a
    // fresh commit holds for the importer's batch sizes): nothing else of its kind joins it.
    st.chunk_budget = 0;
    st.byte_budget = 0;
    #[cfg(test)]
    st.applied.push("ImportBatch");
    let hlc = &mut st.hlc;
    let out = import::apply_batch(conn, &batch, &mut || hlc.tick())?;
    // Entities and mentions may have changed.
    st.matcher = None;
    if out.memories_new > 0 || out.chunks_inserted > 0 {
        st.inserted = true;
    }
    record(
        conn,
        st,
        EventInput {
            actor: &Actor::Import,
            action: "import.batch",
            target_type: "import",
            target_id: &batch.job_id,
            before: None,
            after: Some(serde_json::json!({
                "memories_new": out.memories_new,
                "memories_duplicate": out.memories_duplicate,
                "chunks_inserted": out.chunks_inserted,
                "chunks_skipped": out.chunks_skipped,
                "sources": out.sources,
                "facts_added": out.facts_added,
                "facts_ended": out.facts_ended,
                "facts_skipped": out.facts_skipped,
                "links_made": out.links_made,
                "links_dropped": out.links_dropped,
                "reviews_created": out.reviews_created,
                "reviews_skipped": out.reviews_skipped,
                "collapsed_recorded": out.collapsed_recorded,
                "joined_recorded": out.joined_recorded,
                "projects_created": out.projects_created,
                "mentions_linked": out.mentions_linked,
                "errors": out.errors.len(),
            })),
            group: None,
        },
    )?;
    Ok(Applied::Done(WriteOutcome::Imported(out)))
}

/// `WriteOp::MergeProjects`: takes a commit to itself, like an import batch (it can update
/// every row a project holds), and records one `project.merge` event.
fn merge_projects(
    conn: &Connection,
    from: String,
    into: String,
    actor: Actor,
    rules: Box<alice_core::settings::ProjectSettings>,
    st: &mut State,
) -> Result<Applied> {
    let fresh = st.chunk_budget == INGEST_SLICE && st.byte_budget == INGEST_SLICE_BYTES;
    if !fresh {
        return Ok(Applied::More(
            WriteOp::MergeProjects {
                from,
                into,
                actor,
                rules,
            },
            None,
        ));
    }
    st.chunk_budget = 0;
    st.byte_budget = 0;
    #[cfg(test)]
    st.applied.push("MergeProjects");
    let hlc = st.hlc.tick();
    let done = project_merge::merge(conn, &from, &into, &hlc, &rules)?;
    let event = record(
        conn,
        st,
        EventInput {
            actor: &actor,
            action: project_merge::MERGE_ACTION,
            target_type: project_merge::MERGE_TARGET,
            target_id: &done.from.id,
            before: Some(done.before.clone()),
            after: Some(done.after.clone()),
            group: None,
        },
    )?;
    // Moved text is searched and embedded as before; only the project of the rows changed.
    Ok(Applied::Done(WriteOutcome::ProjectMerged(Box::new((
        done, event,
    )))))
}

/// `WriteOp::CaptureSlice`. Everything is checked before anything is written; a failure rolls
/// back the whole slice (its savepoint), so chunks, keys and cursor land together or not at all.
fn capture_slice(conn: &Connection, slice: Box<CaptureSlice>, st: &mut State) -> Result<Applied> {
    let bytes: usize = slice.chunks.iter().map(|c| c.text.len()).sum();
    if slice.chunks.len() > INGEST_SLICE || bytes > INGEST_SLICE_BYTES {
        return Err(AliceError::invalid(format!(
            "a capture slice holds {} chunks and {bytes} bytes of text; send at most {INGEST_SLICE} chunks and {INGEST_SLICE_BYTES} bytes per slice",
            slice.chunks.len()
        )));
    }
    let fresh = st.chunk_budget == INGEST_SLICE && st.byte_budget == INGEST_SLICE_BYTES;
    if !fresh && (slice.chunks.len() > st.chunk_budget || bytes > st.byte_budget) {
        return Ok(Applied::More(WriteOp::CaptureSlice(slice), None));
    }
    let s = *slice;
    if s.source.source_type != SourceType::Session {
        return Err(AliceError::invalid(
            "a captured transcript must be a source of type session",
        ));
    }
    if s.turn_keys
        .iter()
        .map(|(k, _)| k)
        .chain(&s.skipped_keys)
        .any(|k| k.is_empty())
    {
        return Err(AliceError::invalid(
            "a capture turn key is empty; every captured turn needs its key",
        ));
    }
    sources::validate_chunks(&s.chunks)?;
    st.chunk_budget = st.chunk_budget.saturating_sub(s.chunks.len());
    st.byte_budget = st.byte_budget.saturating_sub(bytes);
    #[cfg(test)]
    st.applied.push("CaptureSlice");
    // The source owns its project (spec §3.1): only a new source takes the one the slice
    // names, so a later slice neither moves it nor creates the project it named.
    let owner: Option<String> = conn
        .query_row(
            "SELECT project_id FROM sources WHERE type = ?1 AND uri = ?2",
            rusqlite::params![s.source.source_type.as_str(), s.source.uri],
            |r| r.get(0),
        )
        .optional()?;
    let project = match owner {
        Some(id) => projects::get_by_id(conn, &id)?,
        None => resolve(conn, &s.project)?,
    };
    // The source's identity comes from `init`; its span from the records (below), never from
    // the input's `ended_at`, which `ingest` would otherwise write over a later end.
    let source = SourceInput {
        agent: Some(s.init.agent.clone()),
        session_id: Some(s.init.session_id.clone()),
        ended_at: None,
        ..s.source.clone()
    };
    let out = sources::ingest(conn, &project, &source, &s.chunks)?;
    // The source spans its first and its newest captured record.
    let times: Vec<String> = s
        .chunks
        .iter()
        .filter_map(|c| sources::normalise_authored(c.authored_at.as_deref()))
        .collect();
    if let (Some(first), Some(newest)) = (times.iter().min(), times.iter().max()) {
        conn.execute(
            "UPDATE sources SET started_at = COALESCE(started_at, ?2),
                ended_at = CASE WHEN ended_at IS NULL OR ended_at < ?3 THEN ?3 ELSE ended_at END
             WHERE id = ?1",
            rusqlite::params![out.source_id, first, newest],
        )?;
    }
    let parent = match &s.init.parent_uri {
        Some(uri) => capture_store::source_id_by_uri(conn, uri)?,
        None => None,
    };
    capture_store::ensure_state(
        conn,
        &out.source_id,
        &capture_store::NewState {
            agent: &s.init.agent,
            session_id: &s.init.session_id,
            parent_source_id: parent.as_deref(),
            transcript_path: s.init.transcript_path.as_deref(),
            file_key: s.init.file_key.as_deref(),
            no_distill: s.init.no_distill,
            distill_state: if s.init.backfill {
                DistillState::SkippedBackfill
            } else {
                DistillState::Idle
            },
        },
    )?;
    let (agent, stored_next): (String, i64) = conn.query_row(
        "SELECT agent, next_ord FROM capture_state WHERE source_id = ?1",
        [&out.source_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let next_ord = match s.cursor {
        Some((_, ord)) => ord,
        None => s
            .chunks
            .iter()
            .map(|c| c.ord + 1)
            .fold(stored_next, i64::max),
    };
    let keys = s
        .turn_keys
        .iter()
        .map(|(k, ord)| (k.as_str(), *ord))
        .chain(s.skipped_keys.iter().map(|k| (k.as_str(), next_ord)));
    let new_keys = capture_store::record_turns(conn, &agent, &out.source_id, keys)?;
    capture_store::advance(
        conn,
        &out.source_id,
        &capture_store::Progress {
            cursor: s.cursor,
            last_event: s.last_event.as_deref(),
            ended: s.ended,
            captured: out.inserted > 0 || new_keys > 0,
        },
    )?;
    if out.inserted > 0 {
        st.inserted = true;
    }
    // One history row per pass, like `source.ingest`: never per chunk, and not undoable.
    if s.record_event && out.inserted > 0 {
        let actor = Actor::from_author(&alice_core::model::Author {
            agent: Some(agent.clone()),
            ..Default::default()
        });
        record(
            conn,
            st,
            EventInput {
                actor: &actor,
                action: "source.capture",
                target_type: "source",
                target_id: &out.source_id,
                before: None,
                after: Some(serde_json::json!({
                    "source_id": out.source_id,
                    "uri": source.uri,
                    "agent": agent,
                    "session_id": s.init.session_id,
                    "inserted": out.inserted,
                    "skipped": out.skipped,
                    "turns": s.turn_keys.len(),
                    "turns_skipped": s.skipped_keys.len(),
                    "bytes": bytes,
                })),
                group: None,
            },
        )?;
    }
    Ok(Applied::Done(WriteOutcome::Captured {
        source_id: out.source_id,
        inserted: out.inserted,
        keys_new: new_keys,
    }))
}

/// `WriteOp::CaptureStateUpdate`: the rows changed (0: no such capture source).
fn capture_state_update(
    conn: &Connection,
    source_id: &str,
    change: &CaptureChange,
) -> Result<usize> {
    match change {
        CaptureChange::Distill {
            state,
            next_try_at,
            attempts,
        } => capture_store::set_distill(conn, source_id, *state, next_try_at.as_deref(), *attempts),
        CaptureChange::Distilled { distill_ord } => {
            capture_store::set_distilled(conn, source_id, *distill_ord)
        }
        CaptureChange::LiveAfterBackfill { from_ord } => {
            capture_store::revive_backfilled(conn, source_id, *from_ord)
        }
        CaptureChange::Ended => capture_store::set_ended(conn, source_id),
        CaptureChange::StaleEnd { due_since } => {
            let ended = capture_store::set_ended(conn, source_id)?;
            if let Some(since) = due_since {
                capture_store::due_after_stale_end(conn, source_id, since)?;
            }
            Ok(ended)
        }
        CaptureChange::Gap { byte_offset } => {
            capture_store::record_gap(conn, source_id, *byte_offset)
        }
        CaptureChange::FileKey {
            file_key,
            byte_offset,
        } => capture_store::set_file_key(conn, source_id, file_key, *byte_offset),
    }
}

fn apply(
    conn: &Connection,
    op: WriteOp,
    progress: Option<IngestProgress>,
    st: &mut State,
) -> Result<Applied> {
    let outcome = match op {
        WriteOp::Remember { input, project } => remember(conn, input, &project, st)?,
        WriteOp::Distilled {
            input,
            project,
            p,
            subagent,
        } => distilled(conn, input, &project, p, subagent, st)?,
        WriteOp::Ingest {
            project,
            source,
            chunks,
        } => return ingest_slice(conn, project, source, chunks, progress, st),
        WriteOp::Update {
            id,
            update: u,
            actor,
        } => update(conn, &id, &u, &actor, st)?,
        WriteOp::AssertFact {
            fact,
            actor,
            alias,
            review,
        } => assert_fact(conn, &fact, &actor, alias, review, st)?,
        WriteOp::Undo { event_id, actor } => undo(conn, &event_id, &actor, st)?,
        WriteOp::ResolveReview {
            id,
            choice,
            actor,
            text,
        } => resolve_review(conn, st, &id, &choice, &actor, text)?,
        WriteOp::DismissReviews {
            review_type,
            provider,
            actor,
        } => dismiss_reviews(conn, st, review_type, provider.as_deref(), &actor)?,
        WriteOp::Merge {
            survivor,
            others,
            actor,
        } => WriteOutcome::Merged(merge(conn, st, &survivor, &others, &actor, None)?),
        WriteOp::StoreEmbeddings { model, items } => {
            let mut applied = Vec::with_capacity(items.len());
            for (key, stamp, v) in items {
                if !embeddings::put_if_current(conn, key, &stamp, &model, &v)? {
                    continue;
                }
                if let VecKey::Memory(seq) = key {
                    activate_if_unjudged(conn, seq, st)?;
                }
                // The text changed since the re-embed's target model embedded it (this row was
                // queued again): that vector is stale, and the re-embed's final pass makes a
                // new one.
                if let Some(t) = st.reembed_target.as_deref().filter(|t| *t != model) {
                    embeddings::delete_vector(conn, key, t)?;
                }
                applied.push((key, v));
            }
            WriteOutcome::Embedded(applied)
        }
        WriteOp::EmbedSkipped { keys } => {
            for (key, stamp) in keys {
                let marked = embeddings::mark_skipped_cloud_if_current(conn, key, &stamp)?;
                if let (VecKey::Memory(seq), true) = (key, marked) {
                    activate_if_unjudged(conn, seq, st)?;
                }
            }
            WriteOutcome::Done
        }
        WriteOp::RequeueSkippedCloud { projects, limit } => {
            let n = embeddings::requeue_skipped_cloud(conn, projects.as_deref(), limit)?;
            st.inserted |= n > 0;
            WriteOutcome::Count(n)
        }
        WriteOp::SetReembedTarget(t) => {
            st.reembed_target = t;
            WriteOutcome::Done
        }
        WriteOp::StoreModelVectors { model, items } => {
            let mut applied = Vec::with_capacity(items.len());
            for (key, stamp, v) in items {
                if embeddings::put_model_if_current(conn, key, &stamp, &model, &v)? {
                    applied.push((key, v));
                }
            }
            WriteOutcome::Embedded(applied)
        }
        WriteOp::DeleteModelVectors { model, limit } => {
            WriteOutcome::Count(embeddings::delete_model(conn, &model, limit)?)
        }
        WriteOp::EmbedFailed { keys, retry } => {
            for (key, stamp) in keys {
                let marked = embeddings::mark_failed_if_current(conn, key, &stamp, retry)?;
                if let (VecKey::Memory(seq), true, false) = (key, marked, retry) {
                    activate_if_unjudged(conn, seq, st)?;
                }
            }
            WriteOutcome::Done
        }
        WriteOp::ApplyJudgement(j) => apply_judgement(conn, st, j)?,
        WriteOp::RequeueJudgeRetries {
            max_attempts,
            now,
            distill,
        } => {
            let settling = judge_store::distilled_giving_up(conn, max_attempts)?;
            let (requeued, given_up) = judge_store::requeue_retries(conn, max_attempts, &now)?;
            for id in &settling {
                settle_distilled(conn, st, id, distill.as_ref())?;
            }
            if requeued + given_up > 0 {
                tracing::info!(requeued, given_up, "memories with a failed judgement");
            }
            WriteOutcome::Done
        }
        WriteOp::TouchAccess(seqs) => {
            memories::touch_access(conn, &seqs)?;
            WriteOutcome::Done
        }
        WriteOp::RecordEvent {
            actor,
            action,
            target_type,
            target_id,
            before,
            after,
        } => {
            let changed = (action == SETTINGS_CHANGE)
                .then(|| settings_changed(after.as_ref()))
                .flatten();
            record(
                conn,
                st,
                EventInput {
                    actor: &actor,
                    action: &action,
                    target_type: &target_type,
                    target_id: &target_id,
                    before,
                    after,
                    group: None,
                },
            )?;
            if let Some(changed) = changed {
                st.stream.push(StreamItem::Settings { changed });
            }
            WriteOutcome::Done
        }
        WriteOp::JudgeCall(c) => {
            use alice_core::judge::jev::{LEDGER_DISTILL, LEDGER_JUDGE};
            // One cache for every consumer: an answer is an answer, whoever asked.
            if c.cacheable {
                judge_store::cache_put(conn, &c.hash, LEDGER_JUDGE, &c.model, &c.response_json)?;
            }
            // The consumer's ledger row. Anything but distillation's is billed to the judge's
            // own row, so no spend lands on a row the day's total does not count.
            let provider = if c.provider == LEDGER_DISTILL {
                LEDGER_DISTILL
            } else {
                LEDGER_JUDGE
            };
            judge_store::add_cost(
                conn,
                &judge_store::CostDelta {
                    day: c.day,
                    provider: provider.into(),
                    calls: 1,
                    input_tokens: c.input_tokens,
                    output_tokens: c.output_tokens,
                    usd: c.usd,
                },
            )?;
            WriteOutcome::Done
        }
        WriteOp::SetMeta { key, value } => {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
            WriteOutcome::Done
        }
        WriteOp::PurgeMemories { seqs, cutoff } => purge_memories(conn, st, &seqs, &cutoff)?,
        WriteOp::DeleteChunks { seqs } => delete_chunks(conn, &seqs)?,
        WriteOp::FtsMerge { table, pages } => {
            WriteOutcome::Flag(maintenance::fts_merge(conn, &table, pages)?)
        }
        WriteOp::PruneEvents { before, limit } => {
            WriteOutcome::Count(maintenance::prune_events(conn, &before, limit)?)
        }
        WriteOp::PruneJudgeCache {
            before,
            keep,
            limit,
        } => WriteOutcome::Count(maintenance::prune_judge_cache(conn, &before, keep, limit)?),
        WriteOp::CreateReview(r) => match reviews::create(conn, &r)? {
            Some(id) => {
                st.stream.push(StreamItem::Review {
                    id,
                    review_type: r.kind.as_str().into(),
                });
                WriteOutcome::Count(1)
            }
            None => WriteOutcome::Count(0),
        },
        WriteOp::ImportBatch(batch) => return import_batch(conn, batch, st),
        WriteOp::MergeProjects {
            from,
            into,
            actor,
            rules,
        } => {
            return merge_projects(conn, from, into, actor, rules, st);
        }
        WriteOp::CaptureSlice(slice) => return capture_slice(conn, slice, st),
        WriteOp::CaptureStateUpdate { source_id, change } => {
            WriteOutcome::Count(capture_state_update(conn, &source_id, &change)?)
        }
        WriteOp::Shutdown => WriteOutcome::Done,
    };
    Ok(Applied::Done(outcome))
}

/// `PurgeMemories`: delete for good, then record a `memory.purge` event per memory. The event
/// comes after the deletion, which scrubs the history of the memory: it names the id and the
/// stub title `[purged]`, never the text. Stops after [`SLICE_BUDGET`] so a request queued
/// behind a purge waits for a few rows, not for a whole page of them.
fn purge_memories(
    conn: &Connection,
    st: &mut State,
    seqs: &[i64],
    cutoff: &str,
) -> Result<WriteOutcome> {
    let started = Instant::now();
    let mut done = Vec::with_capacity(seqs.len());
    for &seq in seqs {
        if started.elapsed() >= crate::maintenance::SLICE_BUDGET {
            break;
        }
        // Only a memory that is still trashed and still old enough: the job read the list a
        // moment ago, and a restore or an edit in between wins.
        let trashed: Option<i64> = conn
            .query_row(
                "SELECT seq FROM memories WHERE seq = ?1 AND status = 'trashed' AND updated_at < ?2",
                rusqlite::params![seq, cutoff],
                |r| r.get(0),
            )
            .optional()?;
        if trashed.is_none() {
            continue;
        }
        for (id, _title) in maintenance::purge_memories(conn, &[seq])? {
            let stub = serde_json::json!({ "id": id, "title": maintenance::PURGED_TITLE });
            record(
                conn,
                st,
                EventInput {
                    actor: &Actor::System,
                    action: "memory.purge",
                    target_type: "memory",
                    target_id: &id,
                    before: Some(stub),
                    after: None,
                    group: None,
                },
            )?;
        }
        done.push(seq);
    }
    // Seqs the box left over are not "done"; seqs skipped as no longer purgeable are not either
    // (they are not in the trash any more, so the next look does not list them).
    Ok(WriteOutcome::Purged(done))
}

/// `DeleteChunks`: retention deletes raw chunks, in groups of 50, until [`SLICE_BUDGET`] has
/// passed. The history row (`raw.retention`) is one per run, recorded by the job.
fn delete_chunks(conn: &Connection, seqs: &[i64]) -> Result<WriteOutcome> {
    const GROUP: usize = 50;
    let started = Instant::now();
    let mut done = Vec::new();
    for group in seqs.chunks(GROUP) {
        if started.elapsed() >= crate::maintenance::SLICE_BUDGET {
            break;
        }
        maintenance::delete_chunks(conn, group)?;
        done.extend_from_slice(group);
    }
    Ok(WriteOutcome::Purged(done))
}

/// A memory the judge never sees (`judge_state = 'skipped'`, an import) becomes active when its
/// embedding step is over; every other memory is activated by its judgement.
fn activate_if_unjudged(conn: &Connection, seq: i64, st: &mut State) -> Result<()> {
    let skipped: bool = conn
        .query_row(
            "SELECT judge_state = 'skipped' FROM memories WHERE seq = ?1",
            [seq],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if skipped {
        activate(conn, seq, st.matcher(conn)?)?;
    }
    Ok(())
}

/// What became of an existing memory a judgement wanted to act on.
enum Target {
    /// Still live and the same text the judge read.
    Unchanged,
    /// Live, but its text changed since the judge read it.
    Edited,
    /// Gone, trashed, superseded or archived: there is nothing left to ask about.
    Gone,
}

fn target_state(conn: &Connection, seen: &[(String, String)], id: &str) -> Result<Target> {
    let now: Option<(String, String)> = conn
        .query_row(
            "SELECT content_hash, status FROM memories WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let live =
        |status: &str| status == Status::Active.as_str() || status == Status::Pending.as_str();
    Ok(match now {
        None => Target::Gone,
        Some((_, status)) if !live(&status) => Target::Gone,
        Some((hash, _)) if seen.iter().any(|(i, h)| i == id && *h == hash) => Target::Unchanged,
        Some(_) => Target::Edited,
    })
}

/// Apply a judgement (spec §5.1 step 5): the project move, the actions, the kind and the
/// review items of one memory in one event group, and activate it. Nothing is applied on stale
/// information: a memory whose text (or `updated_at`) changed since it was judged is left for
/// the next judgement, and an action on an existing memory that changed since becomes a review
/// item. A memory the judge moved is judged again in its new project (its neighbours were the
/// old project's), so it stays `pending`.
fn apply_judgement(conn: &Connection, st: &mut State, j: Judgement) -> Result<WriteOutcome> {
    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT content_hash, status, judge_state, updated_at FROM memories WHERE seq = ?1",
            [j.seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((hash, status, judge_state, updated_at)) = row else {
        return Ok(WriteOutcome::Done);
    };
    // Edited while being judged: it stays pending and the new text is judged. Imports
    // (`skipped`) are never judged, and a memory already judged is not judged twice.
    if hash != j.stamp
        || updated_at != j.updated_at
        || !matches!(judge_state.as_str(), "pending" | "retry")
    {
        return Ok(WriteOutcome::Done);
    }
    let live = status == Status::Pending.as_str() || status == Status::Active.as_str();
    let provider = |own: &Option<String>| own.clone().unwrap_or_else(|| j.provider.clone());
    let actor = Actor::Judge(j.provider.clone());
    let group = new_id();
    // Active before anything else changes: the events below record this as the memory's state
    // before, so undoing one of them never leaves a memory `pending` for good.
    if live {
        activate(conn, j.seq, st.matcher(conn)?)?;
    }
    let mut trashed = !live;
    let mut moved = false;
    if live
        && let Some(slug) = &j.project_to
        && let Some(target) = projects::resolve_slug(conn, slug)?
    {
        let actor = Actor::Judge(provider(&j.project_provider));
        let before = snap(conn, &j.id)?;
        let h = st.hlc.tick();
        match memories::move_to_project(conn, &j.id, &target.id, &h)? {
            memories::MoveOutcome::Moved => {
                moved = true;
                record(
                    conn,
                    st,
                    EventInput {
                        actor: &actor,
                        action: "memory.move",
                        target_type: "memory",
                        target_id: &j.id,
                        before,
                        after: snap(conn, &j.id)?,
                        group: Some(&group),
                    },
                )?;
            }
            memories::MoveOutcome::DuplicateOf(other) => {
                merge(
                    conn,
                    st,
                    &other,
                    std::slice::from_ref(&j.id),
                    &actor,
                    Some(&group),
                )?;
                trashed = true;
            }
        }
    }
    let mut reviews = j.reviews.clone();
    // A distilled memory never asks a person (SP2 spec §6.6).
    let distilled_newcomer = is_distilled(conn, j.seq)?;
    // The comparisons were made in the project the memory has left.
    let actions: &[JudgeAction] = if moved { &[] } else { &j.actions };
    // A distilled newcomer that superseded a member of its own session's best set took that
    // member's slot: it evicts nothing (review I1, option b).
    let mut took_a_slot = false;
    if moved {
        reviews.retain(|r| r.kind == ReviewType::Project);
    }
    for a in actions {
        if trashed {
            break;
        }
        let target = match a {
            JudgeAction::DuplicateOf { survivor, .. } => survivor,
            JudgeAction::Supersedes { old, .. } => old,
            JudgeAction::Contradicts { other, .. } => other,
            JudgeAction::RelatesTo { other, .. } => other,
            JudgeAction::ReplaceDistilled { distilled, .. } => distilled,
        };
        match target_state(conn, &j.seen, target)? {
            Target::Unchanged => {}
            Target::Edited => {
                // Edited since it was judged: ask instead of acting (a distilled memory drops it).
                if !distilled_newcomer
                    && let Some(r) = alice_core::judge::pipeline::as_review(&j.id, a, &j.provider)
                {
                    reviews.push(r);
                }
                continue;
            }
            Target::Gone => continue,
        }
        match a {
            JudgeAction::DuplicateOf { survivor, .. } => {
                match merge(
                    conn,
                    st,
                    survivor,
                    std::slice::from_ref(&j.id),
                    &actor,
                    Some(&group),
                ) {
                    Ok(merged) => trashed = !merged.is_empty(),
                    Err(AliceError::Invalid(_) | AliceError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            JudgeAction::Supersedes { old, .. } => {
                let member = distilled_newcomer && {
                    let theirs = distilled_author(conn, old)?;
                    theirs.is_some() && theirs == distilled_author(conn, &j.id)?
                };
                match supersede(conn, st, old, &j.id, &actor, Some(&group)) {
                    Ok(()) => took_a_slot |= member,
                    Err(AliceError::Invalid(_) | AliceError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            JudgeAction::Contradicts { other, .. } => {
                let before = snap(conn, &j.id)?;
                memories::link(conn, &j.id, other, "contradicts", &actor.as_string())?;
                record(
                    conn,
                    st,
                    EventInput {
                        actor: &actor,
                        action: "memory.link",
                        target_type: "memory",
                        target_id: &j.id,
                        before,
                        after: snap(conn, &j.id)?,
                        group: Some(&group),
                    },
                )?;
            }
            JudgeAction::RelatesTo { other, .. } => {
                let before = snap(conn, &j.id)?;
                memories::link(conn, &j.id, other, "relates_to", &actor.as_string())?;
                record(
                    conn,
                    st,
                    EventInput {
                        actor: &actor,
                        action: "memory.link",
                        target_type: "memory",
                        target_id: &j.id,
                        before,
                        after: snap(conn, &j.id)?,
                        group: Some(&group),
                    },
                )?;
            }
            // The new memory survives its older distilled duplicate.
            JudgeAction::ReplaceDistilled { distilled, .. } => {
                match merge(
                    conn,
                    st,
                    &j.id,
                    std::slice::from_ref(distilled),
                    &actor,
                    Some(&group),
                ) {
                    Ok(_) | Err(AliceError::Invalid(_) | AliceError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    // After the actions: a memory that went to the trash is not reclassified.
    if !trashed && let Some(kind) = j.kind {
        let actor = Actor::Judge(provider(&j.kind_provider));
        let before = snap(conn, &j.id)?;
        let h = st.hlc.tick();
        let edit = MemoryUpdate {
            action: UpdateAction::Edit,
            kind: Some(kind),
            ..Default::default()
        };
        match memories::apply_update(conn, &j.id, &edit, h) {
            Ok(_) => {
                record(
                    conn,
                    st,
                    EventInput {
                        actor: &actor,
                        action: "memory.edit",
                        target_type: "memory",
                        target_id: &j.id,
                        before,
                        after: snap(conn, &j.id)?,
                        group: Some(&group),
                    },
                )?;
            }
            // The project already holds this text under that kind: nothing was written, and
            // the memory keeps the kind it has.
            Err(AliceError::Invalid(_)) => {}
            Err(e) => tracing::warn!(error = %e, id = %j.id, "the judge's kind was not stored"),
        }
    }
    for r in reviews.iter().filter(|_| live) {
        if let Some(id) = reviews::create(conn, r)? {
            st.stream.push(StreamItem::Review {
                id,
                review_type: r.kind.as_str().into(),
            });
        }
    }
    if moved && !trashed {
        // Judged again in the new project.
        return Ok(WriteOutcome::Done);
    }
    if j.retry {
        // A failure counts toward giving up unless the judge was merely unreachable; both
        // lengthen the wait.
        let (attempts, unavailable): (i64, i64) = conn.query_row(
            "SELECT judge_attempts + ?1, judge_unavailable + ?2 FROM memories WHERE seq = ?3",
            rusqlite::params![i64::from(!j.unavailable), i64::from(j.unavailable), j.seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let next = (chrono::Utc::now()
            + chrono::Duration::from_std(j.retry_wait).unwrap_or(chrono::Duration::hours(6)))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        conn.execute(
            "UPDATE memories SET judge_state = 'retry', judge_attempts = ?1, judge_unavailable = ?2,
                judge_next_try_at = ?3 WHERE seq = ?4",
            rusqlite::params![attempts, unavailable, next, j.seq],
        )?;
    } else {
        conn.execute(
            "UPDATE memories SET judge_state = 'done', judge_attempts = judge_attempts + 1,
                judge_next_try_at = NULL WHERE seq = ?1",
            [j.seq],
        )?;
        // Judged live into its session's best set: the set keeps its size (Task 12c review I1,
        // option b). A newcomer the judge trashed, or that took a member's slot, evicts nothing.
        if distilled_newcomer {
            debug_assert!(
                j.distill.is_some(),
                "a judgement of a distilled newcomer carries the distillation settings ({})",
                j.id
            );
            if !trashed && !took_a_slot {
                settle_distilled(conn, st, &j.id, j.distill.as_ref())?;
            }
        }
    }
    Ok(WriteOutcome::Done)
}

/// Whether the memory was distilled from a captured session (SP2 spec §6.5).
fn is_distilled(conn: &Connection, seq: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT origin = ?1 FROM memories WHERE seq = ?2",
            rusqlite::params![Origin::Distill.as_str(), seq],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// Pending -> active, and record entity mentions.
fn activate(conn: &Connection, seq: i64, matcher: &EntityMatcher) -> Result<()> {
    let Some((id, title, body, status)) = conn
        .query_row(
            "SELECT id, title, body, status FROM memories WHERE seq = ?1",
            [seq],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(());
    };
    if status == Status::Pending.as_str() {
        memories::activate_pending(conn, seq)?;
    }
    let ids = matcher.find_in(&format!("{title}\n{body}"));
    if !ids.is_empty() {
        kg::link_mentions(conn, &id, &ids)?;
    }
    Ok(())
}

/// Errors after which no write can succeed until someone repairs the disk or the file.
fn is_fatal(e: &rusqlite::Error) -> Option<String> {
    match e.sqlite_error_code() {
        Some(ErrorCode::DiskFull) => Some("the disk is full".into()),
        Some(ErrorCode::DatabaseCorrupt) | Some(ErrorCode::NotADatabase) => {
            Some("the database file is damaged".into())
        }
        Some(ErrorCode::ReadOnly) | Some(ErrorCode::CannotOpen) => {
            Some("the database cannot be written".into())
        }
        _ => None,
    }
}

fn fatal_reason(e: &AliceError) -> Option<String> {
    match e {
        AliceError::Db(e) => is_fatal(e),
        _ => None,
    }
}

/// The newest HLC stored (memories and relations), so the clock never starts behind it.
/// HLC strings sort as text; values not in the `{millis:013}-…` form are skipped, since a
/// malformed one (say from an import) would otherwise sort above every real HLC.
/// Public for `alice bench`, which times it as one part of start-up.
pub fn newest_hlc(conn: &Connection) -> Result<Option<String>> {
    const WELL_FORMED: &str =
        "'[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-*'";
    let sql = format!(
        "SELECT max(h) FROM (SELECT max(hlc) AS h FROM memories WHERE hlc GLOB {WELL_FORMED}
                         UNION ALL SELECT max(hlc) FROM relations WHERE hlc GLOB {WELL_FORMED})"
    );
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

struct BatchResult {
    /// Finished jobs and their results, in batch order.
    replies: Vec<(Reply, Result<WriteOutcome>)>,
    /// Ingests to continue in the next batch.
    carry: Vec<Job>,
    /// The transaction's commit; on error nothing in the batch was written.
    commit: rusqlite::Result<()>,
}

/// Run one batch: a transaction with a savepoint per job.
fn run_batch(
    conn: &mut Connection,
    batch: Vec<Job>,
    st: &mut State,
    ro: &ReadOnlyState,
) -> BatchResult {
    let mut replies = Vec::with_capacity(batch.len());
    let mut carry = Vec::new();
    let commit = (|| -> rusqlite::Result<()> {
        let mut tx = conn.transaction()?;
        for job in batch {
            let sp = tx.savepoint()?;
            let streamed = st.stream.len();
            match apply(&sp, job.op, job.progress, st) {
                Ok(Applied::Done(outcome)) => {
                    sp.commit()?;
                    replies.push((job.reply, Ok(outcome)));
                }
                Ok(Applied::More(op, progress)) => {
                    sp.commit()?;
                    carry.push(Job {
                        op,
                        reply: job.reply,
                        progress,
                    });
                }
                Err(e) => {
                    drop(sp); // rolls back this job only
                    st.stream.truncate(streamed); // and its items
                    let e = match fatal_reason(&e) {
                        Some(reason) => {
                            tracing::error!(%reason, error = %e, "writer entering read-only mode");
                            ro.set(reason.clone());
                            // The job that found out is refused like every later one, so a
                            // client spools it instead of seeing a raw database error.
                            AliceError::ReadOnly(reason)
                        }
                        None => e,
                    };
                    replies.push((job.reply, Err(e)));
                }
            }
        }
        tx.commit()
    })();
    BatchResult {
        replies,
        carry,
        commit,
    }
}

fn ends_batch(batch: &[Job]) -> bool {
    batch
        .last()
        .is_some_and(|j| matches!(j.op, WriteOp::Shutdown))
}

#[cfg(test)]
pub(crate) fn spawn_writer(
    conn: Connection,
    device: String,
    wake_worker: Sender<()>,
) -> (WriterHandle, JoinHandle<()>) {
    let redactors = Redactors::fixed(alice_core::settings::PrivacySettings::default());
    spawn_writer_inner(
        conn,
        device,
        wake_worker,
        redactors,
        WalPolicy::default(),
        None,
    )
}

/// [`spawn_writer`] that also lists, per commit, the bulk work (ingest slices, import batches)
/// it wrote.
#[cfg(test)]
pub(crate) fn spawn_writer_logged(
    conn: Connection,
    device: String,
    wake_worker: Sender<()>,
    log: CommitLog,
) -> (WriterHandle, JoinHandle<()>) {
    let redactors = Redactors::fixed(alice_core::settings::PrivacySettings::default());
    spawn_writer_inner(
        conn,
        device,
        wake_worker,
        redactors,
        WalPolicy::default(),
        Some(log),
    )
}

/// [`spawn_writer`] with the privacy rules the writer applies to the history it records and to
/// the text an undo restores.
pub fn spawn_writer_with(
    conn: Connection,
    device: String,
    wake_worker: Sender<()>,
    redactors: Arc<Redactors>,
) -> (WriterHandle, JoinHandle<()>) {
    spawn_writer_tuned(conn, device, wake_worker, redactors, WalPolicy::default())
}

/// [`spawn_writer_with`] with an explicit [`WalPolicy`] (tests use small limits).
pub fn spawn_writer_tuned(
    conn: Connection,
    device: String,
    wake_worker: Sender<()>,
    redactors: Arc<Redactors>,
    wal_policy: WalPolicy,
) -> (WriterHandle, JoinHandle<()>) {
    spawn_writer_inner(conn, device, wake_worker, redactors, wal_policy, None)
}

#[cfg(test)]
type LogArg = Option<CommitLog>;
#[cfg(not(test))]
type LogArg = Option<()>;

fn spawn_writer_inner(
    mut conn: Connection,
    device: String,
    wake_worker: Sender<()>,
    redactors: Arc<Redactors>,
    wal_policy: WalPolicy,
    #[cfg_attr(not(test), allow(unused_variables))] log: LogArg,
) -> (WriterHandle, JoinHandle<()>) {
    let (tx, rx): (Sender<Job>, Receiver<Job>) = unbounded();
    let read_only = Arc::new(ReadOnlyState::default());
    let ro = read_only.clone();
    let (stream, _) = broadcast::channel::<StreamItem>(STREAM_CAPACITY);
    let broadcaster = stream.clone();
    let handle = std::thread::Builder::new()
        .name("alice-writer".into())
        .spawn(move || {
            let mut hlc = Hlc::new(device);
            match newest_hlc(&conn) {
                Ok(Some(newest)) => hlc.observe(&newest),
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "reading the newest hlc failed"),
            }
            let mut st = State {
                hlc,
                matcher: None,
                inserted: false,
                reembed_target: None,
                chunk_budget: INGEST_SLICE,
                byte_budget: INGEST_SLICE_BYTES,
                stream: Vec::new(),
                redactors,
                #[cfg(test)]
                applied: Vec::new(),
                #[cfg(test)]
                commit_log: log,
            };
            let mut carry: VecDeque<Job> = VecDeque::new();
            let mut stopping = false;
            // The last batch held only jobs that came in during an ingest, ahead of its next slice.
            let mut queued_ran_last = false;
            let mut wal = WalKeeper::new(&conn, wal_policy);
            loop {
                let mut batch: Vec<Job> = carry.drain(..).collect();
                if !stopping {
                    if batch.is_empty() {
                        queued_ran_last = false;
                        let Ok(first) = rx.recv() else { break };
                        batch.push(first);
                        let deadline = Instant::now() + BATCH_WINDOW;
                        while !ends_batch(&batch) && batch.len() < BATCH_MAX {
                            match rx.recv_deadline(deadline) {
                                Ok(j) => batch.push(j),
                                Err(_) => break,
                            }
                        }
                    } else {
                        // Continuing an ingest. What was queued meanwhile is written first and
                        // alone, in a transaction of its own: a `remember` then waits for the
                        // slice in progress and its own short transaction, not for the next
                        // slice as well. The slice that follows is the ingest's alone again
                        // (so a busy queue slows an import by at most half, never stops it).
                        let mut queued = Vec::new();
                        while !ends_batch(&queued) && queued.len() < BATCH_MAX {
                            match rx.try_recv() {
                                Ok(j) => queued.push(j),
                                Err(_) => break,
                            }
                        }
                        if queued.is_empty() || queued_ran_last {
                            batch.extend(queued);
                            queued_ran_last = false;
                        } else {
                            carry.extend(batch.drain(..));
                            batch = queued;
                            queued_ran_last = true;
                        }
                    }
                    // Jobs sent after a shutdown are dropped (their callers get an error);
                    // ingests already started are finished first.
                    stopping = ends_batch(&batch);
                }
                if batch.is_empty() {
                    break;
                }
                st.inserted = false;
                st.chunk_budget = INGEST_SLICE;
                st.byte_budget = INGEST_SLICE_BYTES;
                let out = run_batch(&mut conn, batch, &mut st, &ro);
                #[cfg(test)]
                {
                    let applied = std::mem::take(&mut st.applied);
                    if out.commit.is_ok()
                        && !applied.is_empty()
                        && let Some(log) = &st.commit_log
                    {
                        log.lock().push(applied);
                    }
                }
                match out.commit {
                    Ok(()) => {
                        // Wake before replying: once a caller has its reply, the wake for its
                        // text has been sent (tests on Windows saw it arrive after the reply).
                        if st.inserted {
                            let _ = wake_worker.try_send(());
                        }
                        // Broadcast before replying too, so a caller that has its reply finds
                        // the change already sent to every subscriber. No subscriber is fine.
                        for item in st.stream.drain(..) {
                            let _ = broadcaster.send(item);
                        }
                        for (reply, r) in out.replies {
                            if let Some(reply) = reply {
                                let _ = reply.send(r);
                            }
                        }
                        carry.extend(out.carry);
                    }
                    Err(e) => {
                        // A matcher loaded inside the rolled-back transaction may name entities
                        // that were never committed; nothing it recorded happened.
                        st.matcher = None;
                        st.stream.clear();
                        let fatal = is_fatal(&e);
                        if let Some(reason) = &fatal {
                            tracing::error!(%reason, error = %e, "writer entering read-only mode");
                            ro.set(reason.clone());
                        }
                        let replies = out.replies.into_iter().map(|(reply, _)| reply);
                        for reply in replies
                            .chain(out.carry.into_iter().map(|j| j.reply))
                            .flatten()
                        {
                            let _ = reply.send(Err(match &fatal {
                                Some(reason) => AliceError::ReadOnly(reason.clone()),
                                None => {
                                    AliceError::Other(format!("write failed: {e}; retry shortly"))
                                }
                            }));
                        }
                    }
                }
                // Between transactions, after the callers have their replies.
                if !stopping {
                    wal.check(&conn, Instant::now());
                }
            }
        })
        .expect("spawn writer thread");
    (
        WriterHandle {
            tx,
            read_only,
            stream,
        },
        handle,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::model::Kind;
    use alice_core::store::embeddings::{self, VecKey};
    use alice_core::store::kg::{AssertFact, FactAction};
    use alice_core::store::memories::{MemoryUpdate, NewMemoryInput, UpdateAction};
    use alice_core::store::sources::{ChunkInput, IngestOutcome, SourceInput};
    use std::path::Path;

    struct Fixture {
        dir: tempfile::TempDir,
        h: WriterHandle,
        wake: crossbeam_channel::Receiver<()>,
        thread: std::thread::JoinHandle<()>,
    }

    impl Fixture {
        fn db(&self) -> std::path::PathBuf {
            self.dir.path().join("a.db")
        }
        fn read<T>(&self, sql: &str, p: impl rusqlite::Params) -> T
        where
            T: rusqlite::types::FromSql,
        {
            let c = alice_core::store::open_reader(&self.db()).unwrap();
            c.query_row(sql, p, |r| r.get(0)).unwrap()
        }
    }

    fn start(dir: tempfile::TempDir, conn: rusqlite::Connection) -> Fixture {
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let (h, thread) = spawn_writer(conn, "dev".into(), wake_tx);
        Fixture {
            dir,
            h,
            wake: wake_rx,
            thread,
        }
    }

    fn open(dir: &Path) -> rusqlite::Connection {
        alice_core::store::open_writer(&dir.join("a.db")).unwrap()
    }

    fn writer() -> (
        tempfile::TempDir,
        WriterHandle,
        crossbeam_channel::Receiver<()>,
    ) {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let (h, _j) = spawn_writer(conn, "dev".into(), wake_tx);
        (d, h, wake_rx)
    }

    fn fixture() -> Fixture {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        start(d, conn)
    }

    fn remember(text: &str) -> WriteOp {
        WriteOp::Remember {
            input: NewMemoryInput {
                content: text.into(),
                ..Default::default()
            },
            project: ProjectChoice::Slug("global".into()),
        }
    }

    fn new_id(o: WriteOutcome) -> (String, i64) {
        match o {
            WriteOutcome::Remembered(InsertOutcome::New { id, seq }, _) => (id, seq),
            other => panic!("expected a new memory, got {other:?}"),
        }
    }

    /// Final re-review 2, I-1: the rules the writer scrubs a project's event snapshots with,
    /// when the store cannot say which rules merges carried into it (its `project_rules` table
    /// is missing), are the closed ones, never the global ones alone.
    #[test]
    fn a_project_whose_carried_rules_cannot_be_read_is_scrubbed_with_closed_rules() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path());
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
        let home = crate::paths::AliceHome::new(d.path().to_path_buf());
        let redactors = Redactors::new(crate::config::SettingsHandle::load_with_env(
            &home,
            Vec::new(),
        ));
        assert!(!rules_for(&c, &redactors, Some(&p.id)).blocks());
        c.execute_batch("DROP TABLE project_rules").unwrap();
        assert!(rules_for(&c, &redactors, Some(&p.id)).blocks());
    }

    /// A heuristic judgement that decides nothing: the memory is activated as it stands.
    fn judged(f: &Fixture, id: &str, seq: i64) {
        let stamp: String = f.read("SELECT content_hash FROM memories WHERE seq = ?1", [seq]);
        let updated_at: String = f.read("SELECT updated_at FROM memories WHERE seq = ?1", [seq]);
        f.h.submit_blocking(WriteOp::ApplyJudgement(Judgement {
            seq,
            id: id.into(),
            stamp,
            updated_at,
            provider: "heuristic".into(),
            // As the judge worker sends it (Task 12c review 2, N2).
            distill: Some(Default::default()),
            ..Default::default()
        }))
        .unwrap();
    }

    #[tokio::test]
    async fn one_bad_op_does_not_poison_its_batch() {
        let (_d, h, wake) = writer();
        let ok = h.submit(remember("good one"));
        let bad = h.submit(remember("   "));
        let ok2 = h.submit(remember("good two"));
        let (a, b, c) = tokio::join!(ok, bad, ok2);
        assert!(matches!(
            a.unwrap(),
            WriteOutcome::Remembered(InsertOutcome::New { .. }, _)
        ));
        assert!(b.is_err());
        assert!(matches!(
            c.unwrap(),
            WriteOutcome::Remembered(InsertOutcome::New { .. }, _)
        ));
        assert!(wake.try_recv().is_ok(), "worker is woken after inserts");
    }

    #[tokio::test]
    async fn unknown_project_slug_is_an_error_not_a_new_project() {
        let (_d, h, _w) = writer();
        let err = h
            .submit(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "x".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Slug("nope".into()),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown project"), "{err}");
    }

    #[test]
    fn blocking_submit_works_outside_the_runtime() {
        let (_d, h, _w) = writer();
        assert!(h.submit_blocking(remember("from a plain thread")).is_ok());
    }

    fn import_memory(i: usize) -> alice_core::store::import::ImportMemory {
        alice_core::store::import::ImportMemory {
            project_slug: "pg-ai".into(),
            input: NewMemoryInput {
                content: format!("imported body number {i}"),
                legacy_id: Some(format!("d{i}")),
                created_at: Some("2025-01-02T03:04:05.000Z".into()),
                ..Default::default()
            },
        }
    }

    fn sample_import(memories: usize) -> ImportBatch {
        ImportBatch {
            projects: vec![alice_core::store::import::ImportProject {
                slug: "pg-ai".into(),
                aliases: vec![],
            }],
            memories: (0..memories).map(import_memory).collect(),
            job_id: "job-1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_import_op_goes_through_the_writer_records_one_event_and_wakes_embedding() {
        let f = fixture();
        let out =
            f.h.submit_blocking(WriteOp::ImportBatch(Box::new(sample_import(2))));
        let Ok(WriteOutcome::Imported(out)) = out else {
            panic!("expected an import outcome, got {out:?}")
        };
        assert_eq!(out.memories_new, 2);
        let n: i64 = f.read("SELECT count(*) FROM events", []);
        assert_eq!(n, 1, "one event for the whole batch");
        let (action, actor, target, after): (String, String, String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT action, actor, target_id, after_json FROM events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        };
        assert_eq!(
            (action.as_str(), actor.as_str(), target.as_str()),
            ("import.batch", "import", "job-1")
        );
        assert!(!after.contains("imported body"), "{after}");
        assert!(f.wake.try_recv().is_ok(), "embedding is woken");
        let live: i64 = f.read(
            "SELECT count(*) FROM memories WHERE status = 'active' AND judge_state = 'skipped'",
            [],
        );
        assert_eq!(live, 2);
    }

    #[tokio::test]
    async fn an_import_batch_and_an_ingest_slice_never_share_one_group_commit() {
        for import_first in [false, true] {
            never_share(import_first).await;
        }
    }

    async fn never_share(import_first: bool) {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        let (wake_tx, _wake_rx) = crossbeam_channel::unbounded();
        let log: CommitLog = Arc::new(Mutex::new(Vec::new()));
        let (h, _t) = spawn_writer_logged(conn, "dev".into(), wake_tx, log.clone());
        let ing = h.submit(ingest("many", 200, |i| format!("short {i}")));
        let imp = h.submit(WriteOp::ImportBatch(Box::new(sample_import(200))));
        let (a, b) = if import_first {
            let (b, a) = tokio::join!(imp, ing);
            (a, b)
        } else {
            tokio::join!(ing, imp)
        };
        assert_eq!(ingested(a).inserted, 200);
        assert!(matches!(b, Ok(WriteOutcome::Imported(ref o)) if o.memories_new == 200));
        let log = log.lock();
        assert!(log.iter().any(|c| c.contains(&"ImportBatch")), "{log:?}");
        assert!(log.iter().any(|c| c.contains(&"Ingest")), "{log:?}");
        assert!(
            log.iter()
                .all(|c| !(c.contains(&"Ingest") && c.contains(&"ImportBatch"))),
            "shared a commit: {log:?}"
        );
    }

    /// A project merge can update every row of a project: it takes a commit to itself, one
    /// event records it, and one that is refused writes nothing.
    #[tokio::test]
    async fn a_project_merge_never_shares_a_group_commit_and_a_refused_one_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        for (key, slug) in [("k/a", "a"), ("k/b", "b")] {
            projects::upsert_project(
                &conn,
                &alice_core::project::ProjectRef {
                    key: key.into(),
                    slug: slug.into(),
                    name: slug.into(),
                    root: None,
                    remote: None,
                },
            )
            .unwrap();
        }
        let (wake_tx, _wake_rx) = crossbeam_channel::unbounded();
        let log: CommitLog = Arc::new(Mutex::new(Vec::new()));
        let (h, _t) = spawn_writer_logged(conn, "dev".into(), wake_tx, log.clone());
        let ing = h.submit(WriteOp::Ingest {
            project: ProjectChoice::Slug("a".into()),
            source: SourceInput {
                uri: "many".into(),
                ..Default::default()
            },
            chunks: (0..200)
                .map(|i| ChunkInput {
                    ord: i,
                    text: format!("short {i}"),
                    ..Default::default()
                })
                .collect(),
        });
        let merge = |from: &str, into: &str| {
            h.submit(WriteOp::MergeProjects {
                from: from.into(),
                into: into.into(),
                actor: Actor::User,
                rules: Default::default(),
            })
        };
        let refused = merge("global", "b");
        let ok = merge("a", "b");
        let (ing, refused, ok) = tokio::join!(ing, refused, ok);
        assert_eq!(ingested(ing).inserted, 200);
        let msg = refused.unwrap_err().to_string();
        assert!(msg.contains("global"), "{msg}");
        let Ok(WriteOutcome::ProjectMerged(done)) = ok else {
            panic!("merge failed")
        };
        let (done, event) = *done;
        assert_eq!(done.counts.chunks, 200, "the ingest ahead of it was moved");
        {
            let log = log.lock();
            assert!(log.iter().any(|c| c.contains(&"MergeProjects")), "{log:?}");
            assert!(
                log.iter()
                    .all(|c| !(c.contains(&"Ingest") && c.contains(&"MergeProjects"))),
                "shared a commit: {log:?}"
            );
        }
        let c = open(d.path());
        let merges: i64 = c
            .query_row(
                "SELECT count(*) FROM events WHERE action = 'project.merge' AND id = ?1",
                [&event],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(merges, 1);
        let all: i64 = c
            .query_row(
                "SELECT count(*) FROM events WHERE action = 'project.merge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(all, 1, "the refused merge recorded nothing");
    }

    #[test]
    fn an_import_batch_is_refused_while_the_database_is_read_only() {
        let f = fixture();
        f.h.read_only.set("disk full".into());
        let err =
            f.h.submit_blocking(WriteOp::ImportBatch(Box::new(sample_import(1))));
        assert!(matches!(err, Err(AliceError::ReadOnly(_))), "{err:?}");
    }

    /// A failing job rolls back everything it wrote, including a project it created on the way.
    #[test]
    fn a_failed_job_leaves_no_partial_writes() {
        let f = fixture();
        let detected = alice_core::project::ProjectRef {
            key: "path:/work/new".into(),
            slug: "new".into(),
            name: "new".into(),
            root: None,
            remote: None,
        };
        let err = f.h.submit_blocking(WriteOp::Remember {
            input: NewMemoryInput {
                content: "  ".into(),
                ..Default::default()
            },
            project: ProjectChoice::Detected(detected.clone()),
        });
        assert!(err.is_err());
        let n: i64 = f.read(
            "SELECT count(*) FROM projects WHERE key = ?1",
            [&detected.key],
        );
        assert_eq!(
            n, 0,
            "the project created for the failed job is rolled back"
        );
        let out =
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "now it works".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Detected(detected.clone()),
            })
            .unwrap();
        let WriteOutcome::Remembered(InsertOutcome::New { .. }, p) = out else {
            panic!("{out:?}")
        };
        assert_eq!(p.slug, "new");
    }

    fn future_hlc(offset_ms: i64, device: &str) -> String {
        let ms = chrono::Utc::now().timestamp_millis() + 10 * 365 * 24 * 3_600_000 + offset_ms;
        format!("{ms:013}-0005-{device}")
    }

    fn hlc_of_new_memory_after(seed: impl FnOnce(&rusqlite::Connection)) -> String {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        seed(&conn);
        let f = start(d, conn);
        let (id, _) = new_id(
            f.h.submit_blocking(remember("written after a restart"))
                .unwrap(),
        );
        f.read("SELECT hlc FROM memories WHERE id = ?1", [&id])
    }

    /// The wall clock can step back across a restart; new HLCs must still sort after stored ones.
    #[test]
    fn the_clock_starts_after_the_newest_stored_hlc() {
        let global = alice_core::store::projects::ResolvedProject {
            id: alice_core::store::projects::GLOBAL_ID.into(),
            slug: "global".into(),
            name: "global".into(),
        };
        let seeded_memory = future_hlc(0, "zz");
        let hlc = hlc_of_new_memory_after(|c| {
            let m = memories::prepare(
                NewMemoryInput {
                    content: "from the future".into(),
                    ..Default::default()
                },
                &global,
                seeded_memory.clone(),
            )
            .unwrap();
            memories::insert(c, &m).unwrap();
        });
        assert!(hlc > seeded_memory, "{hlc} <= {seeded_memory}");

        let seeded_relation = future_hlc(1_000, "zz");
        let hlc = hlc_of_new_memory_after(|c| {
            let m = memories::prepare(
                NewMemoryInput {
                    content: "from the future".into(),
                    ..Default::default()
                },
                &global,
                future_hlc(0, "zz"),
            )
            .unwrap();
            memories::insert(c, &m).unwrap();
            kg::assert_fact(
                c,
                &AssertFact {
                    subject: "alice".into(),
                    predicate: "uses".into(),
                    object: "rust".into(),
                    ..Default::default()
                },
                seeded_relation.clone(),
            )
            .unwrap();
        });
        assert!(hlc > seeded_relation, "{hlc} <= {seeded_relation}");

        // A malformed stored value sorts above every real HLC as text; it must not hide them.
        let seeded = future_hlc(0, "zz");
        let hlc = hlc_of_new_memory_after(|c| {
            for (content, hlc) in [("well formed", seeded.clone()), ("malformed", "h".into())] {
                let m = memories::prepare(
                    NewMemoryInput {
                        content: content.into(),
                        ..Default::default()
                    },
                    &global,
                    hlc,
                )
                .unwrap();
                memories::insert(c, &m).unwrap();
            }
        });
        assert!(hlc > seeded, "{hlc} <= {seeded}");
    }

    fn pending_items(f: &Fixture) -> Vec<embeddings::PendingItem> {
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        embeddings::pending(&c, 10).unwrap()
    }

    fn model_count(f: &Fixture, model: &str) -> i64 {
        f.read("SELECT count(*) FROM embeddings WHERE model = ?1", [model])
    }

    /// A re-embed's vectors are stored for its own model: only for the text they were made from,
    /// without touching the active model's `embed_state`; an edit re-embedded by the active
    /// model drops the target's now-stale vector while a re-embed is on.
    #[test]
    fn a_re_embeds_vectors_go_to_its_model_and_an_edit_drops_the_stale_one() {
        let f = fixture();
        let (id, seq) = new_id(f.h.submit_blocking(remember("port is 8080")).unwrap());
        let old = pending_items(&f).remove(0);
        // Embedded by the active model first.
        f.h.submit_blocking(WriteOp::StoreEmbeddings {
            model: "old".into(),
            items: vec![(old.key, old.stamp.clone(), vec![1.0, 0.0])],
        })
        .unwrap();
        f.h.submit_blocking(WriteOp::SetReembedTarget(Some("new".into())))
            .unwrap();
        let WriteOutcome::Embedded(applied) =
            f.h.submit_blocking(WriteOp::StoreModelVectors {
                model: "new".into(),
                items: vec![
                    (old.key, "stale-stamp".into(), vec![0.0, 1.0]),
                    (old.key, old.stamp.clone(), vec![0.0, 1.0]),
                ],
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(applied.len(), 1, "the stale stamp is refused");
        assert_eq!((model_count(&f, "new"), model_count(&f, "old")), (1, 1));
        let state: String = f.read("SELECT embed_state FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(
            state, "done",
            "embed_state is the active model's and is not touched"
        );
        // The memory is edited and the active model embeds the new text: the target's vector of
        // the old text goes, so the re-embed's final pass makes a new one.
        f.h.submit_blocking(WriteOp::Update {
            id,
            update: MemoryUpdate {
                body: Some("port is 9090".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        let edited = pending_items(&f).remove(0);
        f.h.submit_blocking(WriteOp::StoreEmbeddings {
            model: "old".into(),
            items: vec![(edited.key, edited.stamp, vec![1.0, 0.0])],
        })
        .unwrap();
        assert_eq!((model_count(&f, "new"), model_count(&f, "old")), (0, 1));
        // With no re-embed going, nothing is dropped.
        f.h.submit_blocking(WriteOp::SetReembedTarget(None))
            .unwrap();
        f.h.submit_blocking(WriteOp::DeleteModelVectors {
            model: "old".into(),
            limit: 10,
        })
        .unwrap();
        assert_eq!(model_count(&f, "old"), 0);
    }

    /// Rows held back by the cloud policy leave the queue, leave the judge's wait (a memory
    /// that is not judged is activated) and come back when asked, per project.
    #[test]
    fn rows_held_back_by_the_cloud_policy_are_marked_and_queued_again() {
        let f = fixture();
        let (_, a) = new_id(f.h.submit_blocking(remember("first held back")).unwrap());
        let (_, b) = new_id(
            f.h.submit_blocking(remember("second stays pending"))
                .unwrap(),
        );
        let items = pending_items(&f);
        let first = items.iter().find(|i| i.key == VecKey::Memory(a)).unwrap();
        f.h.submit_blocking(WriteOp::EmbedSkipped {
            keys: vec![
                (first.key, first.stamp.clone()),
                (VecKey::Memory(b), "stale".into()),
            ],
        })
        .unwrap();
        let state = |seq: i64| -> String {
            f.read("SELECT embed_state FROM memories WHERE seq = ?1", [seq])
        };
        assert_eq!(
            (state(a).as_str(), state(b).as_str()),
            ("skipped_cloud", "pending")
        );
        let project: String = f.read("SELECT project_id FROM memories WHERE seq = ?1", [a]);
        let n = |projects: Option<Vec<String>>| match f
            .h
            .submit_blocking(WriteOp::RequeueSkippedCloud {
                projects,
                limit: 10,
            })
            .unwrap()
        {
            WriteOutcome::Count(n) => n,
            other => panic!("{other:?}"),
        };
        assert_eq!(n(Some(vec!["another-project".into()])), 0);
        assert_eq!(n(Some(vec![])), 0);
        assert_eq!(state(a), "skipped_cloud");
        assert_eq!(n(Some(vec![project])), 1);
        assert_eq!(state(a), "pending");
        assert_eq!(n(None), 0);
    }

    /// Ruling: an edit that lands while the old text is embedded must not be marked done.
    #[test]
    fn stored_embeddings_apply_only_to_the_text_they_were_made_from() {
        let f = fixture();
        let (id, seq) = new_id(f.h.submit_blocking(remember("port is 8080")).unwrap());
        let old = pending_items(&f).remove(0);
        f.h.submit_blocking(WriteOp::Update {
            id: id.clone(),
            update: MemoryUpdate {
                body: Some("port is 9090".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        let out =
            f.h.submit_blocking(WriteOp::StoreEmbeddings {
                model: "m".into(),
                items: vec![(old.key, old.stamp.clone(), vec![1.0, 0.0])],
            })
            .unwrap();
        assert!(
            matches!(&out, WriteOutcome::Embedded(applied) if applied.is_empty()),
            "{out:?}"
        );
        let state: String = f.read("SELECT embed_state FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(state, "pending");
        let status: String = f.read("SELECT status FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(status, "pending", "not activated by a stale vector");
        let vectors: i64 = f.read("SELECT count(*) FROM embeddings", []);
        assert_eq!(vectors, 0);

        let new = pending_items(&f).remove(0);
        let out =
            f.h.submit_blocking(WriteOp::StoreEmbeddings {
                model: "m".into(),
                items: vec![(new.key, new.stamp, vec![0.0, 1.0])],
            })
            .unwrap();
        let WriteOutcome::Embedded(applied) = out else {
            panic!("{out:?}")
        };
        assert_eq!(applied, vec![(VecKey::Memory(seq), vec![0.0, 1.0])]);
        // Stored, but not active: the judge activates it.
        let row: (String, String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT status, embed_state, judge_state FROM memories WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(
            row,
            (
                "pending".to_string(),
                "done".to_string(),
                "pending".to_string()
            )
        );
        let id: String = f.read("SELECT id FROM memories WHERE seq = ?1", [seq]);
        judged(&f, &id, seq);
        let row: (String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT status, judge_state FROM memories WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(row, ("active".to_string(), "done".to_string()));
    }

    #[test]
    fn giving_up_on_an_embedding_leaves_the_memory_for_the_judge_but_not_after_an_edit() {
        let f = fixture();
        let (id, seq) = new_id(f.h.submit_blocking(remember("cannot be embedded")).unwrap());
        let item = pending_items(&f).remove(0);
        f.h.submit_blocking(WriteOp::EmbedFailed {
            keys: vec![(item.key, item.stamp.clone())],
            retry: true,
        })
        .unwrap();
        let state: String = f.read("SELECT embed_state FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(state, "retry");
        f.h.submit_blocking(WriteOp::Update {
            id,
            update: MemoryUpdate {
                body: Some("edited meanwhile".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        f.h.submit_blocking(WriteOp::EmbedFailed {
            keys: vec![(item.key, item.stamp)],
            retry: false,
        })
        .unwrap();
        let (state, status): (String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT embed_state, status FROM memories WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!((state.as_str(), status.as_str()), ("pending", "pending"));

        let again = pending_items(&f).remove(0);
        f.h.submit_blocking(WriteOp::EmbedFailed {
            keys: vec![(again.key, again.stamp)],
            retry: false,
        })
        .unwrap();
        let (state, status): (String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT embed_state, status FROM memories WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            (state.as_str(), status.as_str()),
            ("failed", "pending"),
            "given up on, and still live (searched by words) until the judge has it"
        );
        let id: String = f.read("SELECT id FROM memories WHERE seq = ?1", [seq]);
        judged(&f, &id, seq);
        assert_eq!(status_of(&f, &id), "active");
    }

    /// The entity matcher is cached between batches and must be reloaded after a new fact.
    #[test]
    fn mentions_use_entities_added_after_the_matcher_was_loaded() {
        let f = fixture();
        let store = |text: &str| {
            let (_, seq) = new_id(f.h.submit_blocking(remember(text)).unwrap());
            let item = pending_items(&f)
                .into_iter()
                .find(|i| i.key == VecKey::Memory(seq))
                .unwrap();
            f.h.submit_blocking(WriteOp::StoreEmbeddings {
                model: "m".into(),
                items: vec![(item.key, item.stamp, vec![1.0])],
            })
            .unwrap();
            // Mentions are recorded when the judgement activates the memory.
            let id: String = f.read("SELECT id FROM memories WHERE seq = ?1", [seq]);
            judged(&f, &id, seq);
            seq
        };
        store("nothing to link yet");
        f.h.submit_blocking(WriteOp::AssertFact {
            fact: AssertFact {
                subject: "Hostlink".into(),
                predicate: "written in".into(),
                object: "Swift".into(),
                ..Default::default()
            },
            actor: Actor::User,
            alias: None,
            review: None,
        })
        .unwrap();
        let seq = store("Hostlink reconnects after sleep");
        let id: String = f.read("SELECT id FROM memories WHERE seq = ?1", [seq]);
        let mentions: i64 = f.read("SELECT count(*) FROM mentions WHERE memory_id = ?1", [&id]);
        assert_eq!(mentions, 1);
    }

    #[test]
    fn fire_and_forget_ops_are_applied_in_order() {
        let f = fixture();
        let (_, seq) = new_id(f.h.submit_blocking(remember("count my reads")).unwrap());
        f.h.send_nowait(WriteOp::TouchAccess(vec![seq, seq]));
        // The channel is FIFO, so once a later job is acknowledged the touch is committed.
        f.h.submit_blocking(remember("barrier")).unwrap();
        let n: i64 = f.read("SELECT access_count FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(n, 2);
        while f.wake.try_recv().is_ok() {}
        f.h.submit_blocking(WriteOp::TouchAccess(vec![seq]))
            .unwrap();
        assert!(f.wake.try_recv().is_err(), "no wake without new text");
    }

    #[test]
    fn shutdown_stops_the_thread_and_later_writes_fail_cleanly() {
        let f = fixture();
        f.h.submit_blocking(remember("before shutdown")).unwrap();
        f.h.send_nowait(WriteOp::Shutdown);
        f.thread.join().unwrap();
        let err =
            f.h.submit_blocking(remember("after shutdown"))
                .unwrap_err()
                .to_string();
        assert!(err.contains("writer stopped"), "{err}");
    }

    fn ingest(uri: &str, n: usize, text: impl Fn(usize) -> String) -> WriteOp {
        WriteOp::Ingest {
            project: ProjectChoice::Slug("global".into()),
            source: SourceInput {
                uri: uri.into(),
                ..Default::default()
            },
            chunks: (0..n)
                .map(|i| ChunkInput {
                    ord: i as i64,
                    text: text(i),
                    ..Default::default()
                })
                .collect(),
        }
    }

    /// Roughly 2 KB of varied words, so each chunk costs real FTS work.
    fn long_text(i: usize) -> String {
        (0..300)
            .map(|w| format!("w{}x{}", (i * 7 + w) % 997, w % 13))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn ingested(o: Result<WriteOutcome>) -> IngestOutcome {
        match o {
            Ok(WriteOutcome::Ingested(out)) => out,
            other => panic!("expected an ingest outcome, got {other:?}"),
        }
    }

    /// A bulk ingest is written in slices (short transactions), so a small write queued behind
    /// it is committed and acknowledged before the rest of the ingest is written.
    #[tokio::test]
    async fn small_writes_do_not_wait_for_a_whole_bulk_ingest() {
        let f = fixture();
        let reader = alice_core::store::open_reader(&f.db()).unwrap();
        let (bulk, chunks_at_ack) =
            tokio::join!(f.h.submit(ingest("bulk", 1_000, long_text)), async {
                f.h.submit(remember("small and urgent")).await.unwrap();
                reader
                    .query_row("SELECT count(*) FROM chunks", [], |r| r.get::<_, i64>(0))
                    .unwrap()
            });
        assert!(
            chunks_at_ack < 1_000,
            "the small write waited for the whole ingest ({chunks_at_ack} chunks were already in)"
        );
        let out = ingested(bulk);
        assert_eq!((out.inserted, out.skipped), (1_000, 0));
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 1_000);
        let again = ingested(f.h.submit(ingest("bulk", 1_000, long_text)).await);
        assert_eq!((again.inserted, again.skipped), (0, 1_000));
        assert_eq!(again.source_id, out.source_id);
    }

    /// Slices are bounded by text size too: 100 chunks of 16 KB are far more work than 200
    /// short lines, and a small write must still not wait for all of them.
    #[tokio::test]
    async fn small_writes_do_not_wait_for_a_bulk_ingest_of_large_chunks() {
        let f = fixture();
        let reader = alice_core::store::open_reader(&f.db()).unwrap();
        let big = |i: usize| {
            let mut t = long_text(i);
            while t.len() < 16 * 1024 {
                t.push(' ');
                t.push_str(&long_text(i + t.len()));
            }
            t
        };
        let (bulk, chunks_at_ack) = tokio::join!(f.h.submit(ingest("big", 100, big)), async {
            f.h.submit(remember("small and urgent")).await.unwrap();
            reader
                .query_row("SELECT count(*) FROM chunks", [], |r| r.get::<_, i64>(0))
                .unwrap()
        });
        assert!(
            chunks_at_ack < 100,
            "the small write waited for the whole ingest ({chunks_at_ack} chunks were already in)"
        );
        assert_eq!(ingested(bulk).inserted, 100);
    }

    #[test]
    fn a_slice_is_bounded_by_chunks_and_bytes_and_holds_at_least_one_chunk() {
        let chunks = |n: usize, len: usize| -> Vec<ChunkInput> {
            (0..n)
                .map(|i| ChunkInput {
                    ord: i as i64,
                    text: "x".repeat(len),
                    ..Default::default()
                })
                .collect()
        };
        let fresh = (INGEST_SLICE, INGEST_SLICE_BYTES);
        assert_eq!(slice_len(&chunks(1_000, 60), fresh), INGEST_SLICE);
        assert_eq!(slice_len(&chunks(1_000, 2_048), fresh), 16);
        assert_eq!(
            slice_len(&chunks(10, 32_768), fresh),
            1,
            "a largest chunk fills a slice"
        );
        assert_eq!(
            slice_len(&chunks(10, 100_000), fresh),
            1,
            "a fresh slice takes one chunk"
        );
        assert_eq!(slice_len(&chunks(3, 10), fresh), 3);
        assert_eq!(slice_len(&chunks(10, 2_048), (4, INGEST_SLICE_BYTES)), 4);
        assert_eq!(slice_len(&chunks(10, 2_048), (INGEST_SLICE, 5_000)), 2);
        assert_eq!(
            slice_len(&chunks(10, 32_768), (INGEST_SLICE, 1_000)),
            0,
            "a batch that already wrote chunks defers what does not fit"
        );
        assert_eq!(slice_len(&chunks(10, 10), (0, INGEST_SLICE_BYTES)), 0);
    }

    #[tokio::test]
    async fn concurrent_bulk_ingests_both_complete() {
        let f = fixture();
        let (a, b) = tokio::join!(
            f.h.submit(ingest("a", 1_000, |i| format!("alpha {i}"))),
            f.h.submit(ingest("b", 700, |i| format!("beta {i}"))),
        );
        let (a, b) = (ingested(a), ingested(b));
        assert_eq!((a.inserted, b.inserted), (1_000, 700));
        assert_ne!(a.source_id, b.source_id);
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 1_700);
    }

    /// The whole request is checked before its first slice is written.
    #[test]
    fn an_invalid_chunk_late_in_a_bulk_ingest_writes_nothing() {
        let f = fixture();
        let err =
            f.h.submit_blocking(ingest("bad", 1_000, |i| {
                if i == 999 {
                    "y".repeat(40 * 1024)
                } else {
                    format!("fine {i}")
                }
            }))
            .unwrap_err();
        assert!(
            matches!(&err, AliceError::Invalid(m) if m.contains("32768")),
            "{err}"
        );
        let (sources, chunks): (i64, i64) = (
            f.read("SELECT count(*) FROM sources", []),
            f.read("SELECT count(*) FROM chunks", []),
        );
        assert_eq!((sources, chunks), (0, 0));
    }

    #[tokio::test]
    async fn shutdown_finishes_an_ingest_already_started() {
        let f = fixture();
        let (out, ()) = tokio::join!(
            f.h.submit(ingest("bulk", 1_000, |i| format!("line {i}"))),
            async { f.h.send_nowait(WriteOp::Shutdown) }
        );
        assert_eq!(ingested(out).inserted, 1_000);
        f.thread.join().unwrap();
    }

    #[test]
    fn writes_record_events_with_their_actor_and_are_broadcast_after_commit() {
        let f = fixture();
        let mut rx = f.h.subscribe();
        let id = match f
            .h
            .submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "event me".into(),
                    author: alice_core::model::Author {
                        agent: Some("claude-code".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .unwrap()
        {
            WriteOutcome::Remembered(InsertOutcome::New { id, .. }, _) => id,
            o => panic!("{o:?}"),
        };
        let actor: String = f.read(
            "SELECT actor FROM events WHERE target_id = ?1 AND action = 'memory.create'",
            [&id],
        );
        assert_eq!(actor, "agent:claude-code");
        match rx.try_recv().unwrap() {
            StreamItem::Event { event } => assert_eq!(event.target_id, id),
            other => panic!("{other:?}"),
        }
        f.h.submit_blocking(WriteOp::Update {
            id: id.clone(),
            update: MemoryUpdate {
                action: UpdateAction::Archive,
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        let n: i64 = f.read(
            "SELECT count(*) FROM events WHERE target_id = ?1 AND action = 'memory.archive' AND actor = 'user'",
            [&id],
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn a_fact_supersede_records_the_closed_and_the_new_relation_in_one_group() {
        let f = fixture();
        for (object, action) in [
            ("SQLite", FactAction::Add),
            ("Postgres", FactAction::Supersede),
        ] {
            f.h.submit_blocking(WriteOp::AssertFact {
                fact: AssertFact {
                    subject: "alice".into(),
                    predicate: "uses".into(),
                    object: object.into(),
                    action,
                    ..Default::default()
                },
                actor: Actor::User,
                alias: None,
                review: None,
            })
            .unwrap();
        }
        let groups: i64 = f.read("SELECT count(DISTINCT group_id) FROM events WHERE action IN ('relation.close','relation.add') AND group_id IS NOT NULL", []);
        let closes: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'relation.close'",
            [],
        );
        assert_eq!((groups, closes), (2, 1));
        // Asserting what is already current changes nothing, so it records nothing.
        f.h.submit_blocking(WriteOp::AssertFact {
            fact: AssertFact {
                subject: "Alice".into(),
                predicate: "uses".into(),
                object: "Postgres".into(),
                ..Default::default()
            },
            actor: Actor::Agent("codex".into()),
            alias: None,
            review: None,
        })
        .unwrap();
        let n: i64 = f.read("SELECT count(*) FROM events", []);
        assert_eq!(n, 3);
        let ended = f.h.submit_blocking(WriteOp::AssertFact {
            fact: AssertFact {
                subject: "alice".into(),
                predicate: "uses".into(),
                object: "Postgres".into(),
                action: FactAction::End,
                ..Default::default()
            },
            actor: Actor::Agent("codex".into()),
            alias: None,
            review: None,
        });
        assert!(
            matches!(ended, Ok(WriteOutcome::Fact(ref o)) if o.ended == 1),
            "{ended:?}"
        );
        let actor: String = f.read(
            "SELECT actor FROM events WHERE action = 'relation.close' ORDER BY rowid DESC LIMIT 1",
            [],
        );
        assert_eq!(actor, "agent:codex");
    }

    fn update(id: &str, action: UpdateAction) -> WriteOp {
        WriteOp::Update {
            id: id.into(),
            update: MemoryUpdate {
                action,
                ..Default::default()
            },
            actor: Actor::User,
        }
    }

    /// Spec §8.1: `update_memory` answers with the event id, recorded under the memory's own
    /// id even when the caller named it by its legacy id.
    #[test]
    fn an_update_returns_its_event_recorded_under_the_memory_id() {
        let f = fixture();
        let (id, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "imported from the palace".into(),
                    legacy_id: Some("drawer_7".into()),
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .unwrap(),
        );
        let event = match f
            .h
            .submit_blocking(update("drawer_7", UpdateAction::Archive))
            .unwrap()
        {
            WriteOutcome::Updated(m, Some(event)) => {
                assert_eq!(m.id, id);
                event
            }
            o => panic!("{o:?}"),
        };
        let (target, action, before, after): (String, String, String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT target_id, action, before_json, after_json FROM events WHERE id = ?1",
                [&event],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        };
        assert_eq!(
            (target.as_str(), action.as_str()),
            (id.as_str(), "memory.archive")
        );
        let status =
            |j: &str| serde_json::from_str::<serde_json::Value>(j).unwrap()["status"].clone();
        assert_eq!(
            (status(&before), status(&after)),
            ("pending".into(), "archived".into())
        );
    }

    /// A supersede through `update` changes two memories: one group, undone together.
    #[test]
    fn supersede_by_update_and_by_remember_are_one_group_each_and_undo_whole() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("port is 8080")).unwrap());
        let (newer, _) = new_id(f.h.submit_blocking(remember("port is 9090")).unwrap());
        let event = match f
            .h
            .submit_blocking(WriteOp::Update {
                id: old.clone(),
                update: MemoryUpdate {
                    action: UpdateAction::Supersede,
                    superseded_by: Some(newer.clone()),
                    ..Default::default()
                },
                actor: Actor::User,
            })
            .unwrap()
        {
            WriteOutcome::Updated(_, Some(e)) => e,
            o => panic!("{o:?}"),
        };
        let actions: String = f.read(
            "SELECT group_concat(action, ',') FROM (SELECT action FROM events WHERE group_id = (SELECT group_id FROM events WHERE id = ?1) ORDER BY rowid)",
            [&event],
        );
        assert_eq!(actions, "memory.supersede,memory.link");
        match f
            .h
            .submit_blocking(WriteOp::Undo {
                event_id: event,
                actor: Actor::User,
            })
            .unwrap()
        {
            WriteOutcome::Undo(UndoOutcome::Undone { reverted, .. }) => {
                assert_eq!(reverted.len(), 2)
            }
            o => panic!("{o:?}"),
        }
        let links: i64 = f.read("SELECT count(*) FROM links", []);
        let status: String = f.read("SELECT status FROM memories WHERE id = ?1", [&old]);
        assert_eq!((links, status.as_str()), (0, "active"));

        // At write time: the create and the superseded memory share a group.
        let id = match f
            .h
            .submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "port is 7070".into(),
                    supersedes: vec![old.clone(), old.clone(), "NOSUCHID".into()],
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .unwrap()
        {
            WriteOutcome::Remembered(InsertOutcome::New { id, .. }, _) => id,
            o => panic!("{o:?}"),
        };
        let actions: String = f.read(
            "SELECT group_concat(action || ':' || (target_id = ?1), ',') FROM (SELECT action, target_id FROM events WHERE group_id = (SELECT group_id FROM events WHERE target_id = ?1 AND action = 'memory.create') ORDER BY rowid)",
            [&id],
        );
        assert_eq!(actions, "memory.create:1,memory.supersede:0");
    }

    #[test]
    fn undo_through_the_writer_is_broadcast_and_wakes_the_worker_for_restored_text() {
        let f = fixture();
        let (id, seq) = new_id(f.h.submit_blocking(remember("Use WAL mode")).unwrap());
        let event = match f
            .h
            .submit_blocking(WriteOp::Update {
                id: id.clone(),
                update: MemoryUpdate {
                    body: Some("Use rollback journal".into()),
                    ..Default::default()
                },
                actor: Actor::Agent("codex".into()),
            })
            .unwrap()
        {
            WriteOutcome::Updated(_, Some(e)) => e,
            o => panic!("{o:?}"),
        };
        while f.wake.try_recv().is_ok() {}
        let mut rx = f.h.subscribe();
        let out =
            f.h.submit_blocking(WriteOp::Undo {
                event_id: event.clone(),
                actor: Actor::User,
            })
            .unwrap();
        assert!(
            matches!(&out, WriteOutcome::Undo(UndoOutcome::Undone { reverted, .. }) if reverted == &vec![event.clone()]),
            "{out:?}"
        );
        let body: String = f.read("SELECT body FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(body, "Use WAL mode");
        assert!(
            f.wake.try_recv().is_ok(),
            "the restored text is embedded again"
        );
        let mut actions = Vec::new();
        while let Ok(StreamItem::Event { event }) = rx.try_recv() {
            actions.push(event.action);
        }
        assert_eq!(actions, ["undo:memory.edit", "undo"]);

        // Twice: refused, nothing broadcast. A later change: a review item, broadcast.
        let again = f.h.submit_blocking(WriteOp::Undo {
            event_id: event.clone(),
            actor: Actor::User,
        });
        assert!(
            matches!(&again, Err(AliceError::Invalid(m)) if m.contains("already undone")),
            "{again:?}"
        );
        assert!(rx.try_recv().is_err());
        let archive = match f
            .h
            .submit_blocking(update(&id, UpdateAction::Archive))
            .unwrap()
        {
            WriteOutcome::Updated(_, Some(e)) => e,
            o => panic!("{o:?}"),
        };
        f.h.submit_blocking(update(&id, UpdateAction::Restore))
            .unwrap();
        while rx.try_recv().is_ok() {}
        let out =
            f.h.submit_blocking(WriteOp::Undo {
                event_id: archive.clone(),
                actor: Actor::User,
            })
            .unwrap();
        let WriteOutcome::Undo(UndoOutcome::Conflict { review_id, .. }) = out else {
            panic!("{out:?}")
        };
        match rx.try_recv().unwrap() {
            StreamItem::Review { id, review_type } => {
                assert_eq!(
                    (id.as_str(), review_type.as_str()),
                    (review_id.as_str(), "undo")
                )
            }
            other => panic!("{other:?}"),
        }
        // Asking again reuses the open item: nothing new to announce.
        let out =
            f.h.submit_blocking(WriteOp::Undo {
                event_id: archive,
                actor: Actor::User,
            })
            .unwrap();
        assert!(
            matches!(&out, WriteOutcome::Undo(UndoOutcome::Conflict { review_id: again, created: false, .. }) if *again == review_id),
            "{out:?}"
        );
        assert!(rx.try_recv().is_err(), "no review item was added");
    }

    /// Spec §5.5: every change is recorded. An ingest request is one summary event (never one
    /// per chunk), recorded with its last slice, by the source's agent.
    #[test]
    fn an_ingest_request_records_one_summary_event() {
        let f = fixture();
        let text = |i: usize| format!("transcript line {i}");
        let mut op = ingest("session-a", 450, text);
        if let WriteOp::Ingest { source, .. } = &mut op {
            source.agent = Some("claude-code".into());
        }
        let out = ingested(f.h.submit_blocking(op));
        assert_eq!(out.inserted, 450);
        let (n, action, target_type, target, actor, after): (
            i64,
            String,
            String,
            String,
            String,
            String,
        ) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT (SELECT count(*) FROM events), action, target_type, target_id, actor, after_json FROM events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap()
        };
        assert_eq!(n, 1, "one event for a request written in three slices");
        assert_eq!(
            (
                action.as_str(),
                target_type.as_str(),
                target.as_str(),
                actor.as_str()
            ),
            (
                "source.ingest",
                "source",
                out.source_id.as_str(),
                "agent:claude-code"
            )
        );
        let after: serde_json::Value = serde_json::from_str(&after).unwrap();
        let bytes: usize = (0..450).map(|i| text(i).len()).sum();
        assert_eq!(after["inserted"], 450);
        assert_eq!(after["skipped"], 0);
        assert_eq!(after["bytes"], bytes);
        assert_eq!(after["uri"], "session-a");
        // A repeat writes no chunk, so it records nothing.
        ingested(f.h.submit_blocking(ingest("session-a", 450, text)));
        let n: i64 = f.read("SELECT count(*) FROM events", []);
        assert_eq!(n, 1);
    }

    /// Only committed work is broadcast: a job that fails sends nothing, its neighbours do.
    #[tokio::test]
    async fn a_failed_job_broadcasts_nothing() {
        let f = fixture();
        let mut rx = f.h.subscribe();
        let (ok, bad) = tokio::join!(
            f.h.submit(remember("committed")),
            f.h.submit(update("01K7NOSUCHMEMORY0000000000", UpdateAction::Archive)),
        );
        let (id, _) = new_id(ok.unwrap());
        assert!(matches!(bad, Err(AliceError::NotFound(_))), "{bad:?}");
        match rx.try_recv().unwrap() {
            StreamItem::Event { event } => assert_eq!(event.target_id, id),
            other => panic!("{other:?}"),
        }
        assert!(rx.try_recv().is_err(), "nothing from the failed job");
    }

    /// A database that refuses writes switches the writer to read-only mode with a reason.
    #[test]
    fn a_write_refused_by_the_database_makes_the_writer_read_only() {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        conn.execute_batch("PRAGMA query_only = ON").unwrap();
        let f = start(d, conn);
        // Review finding m-4: the write that finds out gets the read-only error too (so a
        // client spools it), not the raw database error.
        let first = f.h.submit_blocking(remember("refused")).unwrap_err();
        assert!(
            matches!(&first, AliceError::ReadOnly(r) if r.contains("cannot be written")),
            "{first:?}"
        );
        let reason = f.h.read_only.reason().expect("read-only reason");
        assert!(reason.contains("cannot be written"), "{reason}");
        let second = f.h.submit_blocking(remember("also refused")).unwrap_err();
        assert!(matches!(second, AliceError::ReadOnly(_)), "{second}");
        let _ = Kind::Note;
    }

    /// Whatever a caller hands `RecordEvent`, the history and the stream get it redacted.
    #[tokio::test(flavor = "multi_thread")]
    async fn recorded_events_and_their_stream_items_are_redacted() {
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let (_d, h, _wake) = writer();
        let mut rx = h.subscribe();
        h.submit(WriteOp::RecordEvent {
            actor: Actor::User,
            action: "test.event".into(),
            target_type: "thing".into(),
            target_id: "t1".into(),
            before: Some(serde_json::json!({ "note": format!("old {token}") })),
            after: Some(serde_json::json!({ "nested": { "list": [format!("new {token}")] } })),
        })
        .await
        .unwrap();
        let item = serde_json::to_string(&rx.recv().await.unwrap()).unwrap();
        assert!(
            !item.contains(token) && item.contains("[REDACTED:github_token]"),
            "{item}"
        );
    }

    // ---- review resolutions ----

    fn add_review(f: &Fixture, kind: ReviewType, payload: serde_json::Value, key: &str) -> String {
        let c = open(f.dir.path());
        reviews::create(
            &c,
            &reviews::NewReview {
                kind,
                payload,
                confidence: Some(0.7),
                dedupe_key: Some(key.into()),
            },
        )
        .unwrap()
        .unwrap()
    }

    fn resolve(f: &Fixture, id: &str, choice: &str) -> Result<WriteOutcome> {
        f.h.submit_blocking(WriteOp::ResolveReview {
            id: id.into(),
            choice: choice.into(),
            actor: Actor::User,
            text: None,
        })
    }

    fn status_of(f: &Fixture, id: &str) -> String {
        f.read("SELECT status FROM memories WHERE id = ?1", [id])
    }

    #[test]
    fn accepting_a_supersede_item_supersedes_in_one_group_and_rejecting_changes_nothing() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("port is 8080")).unwrap());
        let (new, _) = new_id(f.h.submit_blocking(remember("port is 9090")).unwrap());
        let item = |key: &str| {
            add_review(
                &f,
                ReviewType::Supersede,
                serde_json::json!({"new_id": new, "old_id": old, "p": 0.7, "provider": "heuristic"}),
                key,
            )
        };
        let (a, b) = (item("a"), item("b"));
        let out = resolve(&f, &b, "reject").unwrap();
        assert!(
            matches!(&out, WriteOutcome::Resolved(r) if r.resolution.as_deref() == Some("reject"))
        );
        assert_ne!(status_of(&f, &old), "superseded");
        let mut rx = f.h.subscribe();
        let WriteOutcome::Resolved(done) = resolve(&f, &a, "accept").unwrap() else {
            panic!()
        };
        assert_eq!(
            (done.status, done.resolved_by.as_deref()),
            (reviews::ReviewStatus::Resolved, Some("user"))
        );
        assert_eq!(status_of(&f, &old), "superseded");
        let links: i64 = f.read(
            "SELECT count(*) FROM links WHERE from_id = ?1 AND to_id = ?2 AND type = 'supersedes'",
            [&new, &old],
        );
        assert_eq!(links, 1);
        let mut actions = Vec::new();
        while let Ok(StreamItem::Event { event }) = rx.try_recv() {
            actions.push((event.action, event.group_id.is_some()));
        }
        assert_eq!(
            actions,
            [
                ("memory.supersede".to_string(), true),
                ("memory.link".to_string(), true),
                ("review.resolve".to_string(), false)
            ]
        );
        // Once resolved, the item is closed.
        let err = resolve(&f, &a, "accept").unwrap_err().to_string();
        assert!(err.contains("already resolved"), "{err}");
        let err = resolve(&f, "NOSUCH", "accept").unwrap_err();
        assert!(matches!(err, AliceError::NotFound(_)), "{err}");
    }

    #[test]
    fn a_decision_that_cannot_be_applied_leaves_the_item_open() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("port is 8080")).unwrap());
        let (new, _) = new_id(f.h.submit_blocking(remember("port is 9090")).unwrap());
        f.h.submit_blocking(update(&old, UpdateAction::Forget))
            .unwrap();
        let item = add_review(
            &f,
            ReviewType::Contradiction,
            serde_json::json!({"new_id": new, "old_id": old, "p": 0.7, "provider": "heuristic"}),
            "c",
        );
        let err = resolve(&f, &item, "keep_new").unwrap_err().to_string();
        assert!(err.contains("trash"), "{err}");
        let err = resolve(&f, &item, "maybe").unwrap_err().to_string();
        assert!(err.contains("keep_new, keep_old, keep_both"), "{err}");
        let st: String = f.read("SELECT status FROM review_items WHERE id = ?1", [&item]);
        assert_eq!(st, "open");
        // keep_both is nothing but a record.
        resolve(&f, &item, "keep_both").unwrap();
        assert_eq!(status_of(&f, &new), "pending");
    }

    #[test]
    fn contradiction_keep_old_supersedes_the_new_memory() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("uses tabs")).unwrap());
        let (new, _) = new_id(f.h.submit_blocking(remember("uses spaces")).unwrap());
        let item = add_review(
            &f,
            ReviewType::Contradiction,
            serde_json::json!({"new_id": new, "old_id": old, "p": 0.8, "provider": "heuristic"}),
            "c",
        );
        resolve(&f, &item, "keep_old").unwrap();
        assert_eq!(status_of(&f, &new), "superseded");
        assert_ne!(status_of(&f, &old), "superseded");
    }

    #[test]
    fn merge_by_review_or_directly_trashes_the_others_with_events() {
        let f = fixture();
        let ids: Vec<String> = ["survivor", "dup one", "dup two", "dup three"]
            .iter()
            .map(|t| new_id(f.h.submit_blocking(remember(t)).unwrap()).0)
            .collect();
        let item = add_review(
            &f,
            ReviewType::Merge,
            serde_json::json!({"survivor": ids[0], "others": [ids[1], ids[2]], "p": 0.9, "provider": "heuristic"}),
            "m",
        );
        resolve(&f, &item, "merge").unwrap();
        assert_eq!(
            (
                status_of(&f, &ids[1]).as_str(),
                status_of(&f, &ids[2]).as_str()
            ),
            ("trashed", "trashed")
        );
        let WriteOutcome::Merged(merged) =
            f.h.submit_blocking(WriteOp::Merge {
                survivor: ids[0].clone(),
                others: vec![
                    ids[3].clone(),
                    ids[0].clone(),
                    "NOSUCH".into(),
                    ids[1].clone(),
                ],
                actor: Actor::Agent("codex".into()),
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            merged,
            [ids[3].clone()],
            "the survivor, unknown and already merged ids are skipped"
        );
        let n: i64 = f.read(
            "SELECT count(*) FROM events WHERE action IN ('memory.merge','memory.merge_into') AND actor = 'agent:codex'",
            [],
        );
        assert_eq!(n, 2, "one event for the duplicate, one for the survivor");
        // A merge can be undone: the duplicate is back.
        let ev: String = f.read(
            "SELECT id FROM events WHERE action = 'memory.merge' AND target_id = ?1",
            [&ids[3]],
        );
        f.h.submit_blocking(WriteOp::Undo {
            event_id: ev,
            actor: Actor::User,
        })
        .unwrap();
        assert_ne!(status_of(&f, &ids[3]), "trashed");
    }

    #[test]
    fn mapping_a_predicate_renames_relations_and_records_an_event() {
        let f = fixture();
        for (p, o) in [("uses", "SQLite"), ("use", "Rust")] {
            f.h.submit_blocking(WriteOp::AssertFact {
                fact: AssertFact {
                    subject: "alice".into(),
                    predicate: p.into(),
                    object: o.into(),
                    ..Default::default()
                },
                actor: Actor::User,
                alias: None,
                review: None,
            })
            .unwrap();
        }
        let item = add_review(
            &f,
            ReviewType::Predicate,
            serde_json::json!({"predicate": "uses", "canonical": "use", "p": 0.9, "provider": "heuristic"}),
            "p",
        );
        resolve(&f, &item, "map").unwrap();
        let n: i64 = f.read("SELECT count(*) FROM relations WHERE predicate = 'use'", []);
        assert_eq!(n, 2);
        let after: String = f.read(
            "SELECT after_json FROM events WHERE action = 'predicate.map'",
            [],
        );
        assert!(after.contains("\"relations\":[["), "{after}");
        // `text` (the service's redacted names) wins over the item's own.
        let item2 = add_review(
            &f,
            ReviewType::Predicate,
            serde_json::json!({"predicate": "x", "canonical": "y", "p": 0.9, "provider": "heuristic"}),
            "p2",
        );
        f.h.submit_blocking(WriteOp::ResolveReview {
            id: item2,
            choice: "map".into(),
            actor: Actor::User,
            text: Some(ResolveText::Predicate("employs".into(), "use".into())),
        })
        .unwrap();
        let canonical: String = f.read(
            "SELECT canonical FROM predicates WHERE name = 'employs'",
            [],
        );
        assert_eq!(canonical, "use");
    }

    #[test]
    fn moving_a_memory_changes_its_project_and_a_duplicate_there_becomes_a_merge() {
        let f = fixture();
        {
            let c = open(f.dir.path());
            alice_core::store::projects::upsert_project(
                &c,
                &ProjectRef {
                    key: "k2".into(),
                    slug: "other".into(),
                    name: "other".into(),
                    root: None,
                    remote: None,
                },
            )
            .unwrap();
        }
        let (m, _) = new_id(f.h.submit_blocking(remember("move me")).unwrap());
        let (dup, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "twin".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Slug("other".into()),
            })
            .unwrap(),
        );
        let item = add_review(
            &f,
            ReviewType::Project,
            serde_json::json!({"memory_id": m, "from": "global", "to": "other", "p": 0.8, "provider": "heuristic"}),
            "mv",
        );
        resolve(&f, &item, "move").unwrap();
        let slug: String = f.read(
            "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1",
            [&m],
        );
        assert_eq!(slug, "other");
        let n: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'memory.move'",
            [],
        );
        assert_eq!(n, 1);
        // The same text already lives in the target: the moved memory is merged into it.
        let (twin, _) = new_id(f.h.submit_blocking(remember("twin")).unwrap());
        let item2 = add_review(
            &f,
            ReviewType::Project,
            serde_json::json!({"memory_id": twin, "from": "global", "to": "other", "p": 0.8, "provider": "heuristic"}),
            "mv2",
        );
        resolve(&f, &item2, "move").unwrap();
        assert_eq!(status_of(&f, &twin), "trashed");
        assert_ne!(status_of(&f, &dup), "trashed");
        // A project that is gone: refused, the item stays open.
        let item3 = add_review(
            &f,
            ReviewType::Project,
            serde_json::json!({"memory_id": dup, "from": "other", "to": "vanished"}),
            "mv3",
        );
        let err = resolve(&f, &item3, "move").unwrap_err().to_string();
        assert!(err.contains("no longer exists"), "{err}");
    }

    #[test]
    fn applying_an_undo_item_restores_over_the_later_change_and_dismissing_does_not() {
        let f = fixture();
        let (id, _) = new_id(f.h.submit_blocking(remember("first text")).unwrap());
        let edit = |body: &str| match f
            .h
            .submit_blocking(WriteOp::Update {
                id: id.clone(),
                update: MemoryUpdate {
                    body: Some(body.into()),
                    ..Default::default()
                },
                actor: Actor::User,
            })
            .unwrap()
        {
            WriteOutcome::Updated(_, Some(e)) => e,
            o => panic!("{o:?}"),
        };
        let ev = edit("second text");
        edit("third text");
        let WriteOutcome::Undo(UndoOutcome::Conflict { review_id, .. }) =
            f.h.submit_blocking(WriteOp::Undo {
                event_id: ev.clone(),
                actor: Actor::User,
            })
            .unwrap()
        else {
            panic!()
        };
        let body =
            |f: &Fixture| -> String { f.read("SELECT body FROM memories WHERE id = ?1", [&id]) };
        let mut rx = f.h.subscribe();
        while f.wake.try_recv().is_ok() {}
        resolve(&f, &review_id, "apply").unwrap();
        assert_eq!(body(&f), "first text");
        assert!(f.wake.try_recv().is_ok(), "restored text is embedded again");
        let mut actions = Vec::new();
        while let Ok(StreamItem::Event { event }) = rx.try_recv() {
            actions.push(event.action);
        }
        assert_eq!(actions, ["undo:memory.edit", "undo", "review.resolve"]);
        // Dismissing another conflict leaves the text alone.
        let ev2 = edit("fourth text");
        edit("fifth text");
        let WriteOutcome::Undo(UndoOutcome::Conflict { review_id: r2, .. }) =
            f.h.submit_blocking(WriteOp::Undo {
                event_id: ev2,
                actor: Actor::User,
            })
            .unwrap()
        else {
            panic!()
        };
        resolve(&f, &r2, "dismiss").unwrap();
        assert_eq!(body(&f), "fifth text");
    }

    #[test]
    fn sync_conflicts_cannot_be_resolved_yet() {
        let f = fixture();
        let item = add_review(&f, ReviewType::SyncConflict, serde_json::json!({}), "s");
        let err = resolve(&f, &item, "keep_local").unwrap_err().to_string();
        assert!(err.contains("coming"), "{err}");
        let st: String = f.read("SELECT status FROM review_items WHERE id = ?1", [&item]);
        assert_eq!(st, "open");
    }

    // ---- stale items and undo of resolutions ----

    fn event_of(f: &Fixture, action: &str, target: &str) -> String {
        f.read(
            "SELECT id FROM events WHERE action = ?1 AND target_id = ?2 ORDER BY rowid DESC LIMIT 1",
            [action, target],
        )
    }

    fn undo(f: &Fixture, event: &str) -> Result<WriteOutcome> {
        f.h.submit_blocking(WriteOp::Undo {
            event_id: event.into(),
            actor: Actor::User,
        })
    }

    fn open_state(f: &Fixture, item: &str) -> String {
        f.read("SELECT status FROM review_items WHERE id = ?1", [item])
    }

    #[test]
    fn stale_items_are_refused_and_stay_open() {
        let f = fixture();
        let ids: Vec<String> = ["survivor", "dup one", "dup two", "mover"]
            .iter()
            .map(|t| new_id(f.h.submit_blocking(remember(t)).unwrap()).0)
            .collect();
        // A merge whose others are all in the trash.
        for d in &ids[1..3] {
            f.h.submit_blocking(update(d, UpdateAction::Forget))
                .unwrap();
        }
        let merge_item = add_review(
            &f,
            ReviewType::Merge,
            serde_json::json!({"survivor": ids[0], "others": [ids[1], ids[2]]}),
            "m",
        );
        let err = resolve(&f, &merge_item, "merge").unwrap_err().to_string();
        assert!(err.contains("nothing is left to merge"), "{err}");
        assert_eq!(open_state(&f, &merge_item), "open");
        let n: i64 = f.read(
            "SELECT count(*) FROM events WHERE action LIKE 'memory.merge%'",
            [],
        );
        assert_eq!(n, 0);

        // A move of a memory in the trash, of one that left `from`, and of one already there.
        {
            let c = open(f.dir.path());
            alice_core::store::projects::upsert_project(
                &c,
                &ProjectRef {
                    key: "k2".into(),
                    slug: "other".into(),
                    name: "other".into(),
                    root: None,
                    remote: None,
                },
            )
            .unwrap();
        }
        let mv = |memory: &str, from: &str, key: &str| {
            add_review(
                &f,
                ReviewType::Project,
                serde_json::json!({"memory_id": memory, "from": from, "to": "other"}),
                key,
            )
        };
        let trashed = mv(&ids[1], "global", "t");
        let err = resolve(&f, &trashed, "move").unwrap_err().to_string();
        assert!(err.contains("in the trash"), "{err}");
        let wrong_from = mv(&ids[3], "other", "w");
        let err = resolve(&f, &wrong_from, "move").unwrap_err().to_string();
        assert!(err.contains("no longer in project other"), "{err}");
        let ok = mv(&ids[3], "global", "ok");
        resolve(&f, &ok, "move").unwrap();
        let there = mv(&ids[3], "global", "there");
        let err = resolve(&f, &there, "move").unwrap_err().to_string();
        assert!(err.contains("already in project other"), "{err}");
        for i in [&trashed, &wrong_from, &there] {
            assert_eq!(open_state(&f, i), "open");
        }
        let moves: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'memory.move'",
            [],
        );
        assert_eq!(moves, 1, "no no-op move event");
    }

    #[test]
    fn an_already_superseded_or_archived_memory_is_not_superseded_again() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("v1")).unwrap());
        let (v2, _) = new_id(f.h.submit_blocking(remember("v2")).unwrap());
        let (v3, _) = new_id(f.h.submit_blocking(remember("v3")).unwrap());
        let item = |new: &str, key: &str| {
            add_review(
                &f,
                ReviewType::Supersede,
                serde_json::json!({"new_id": new, "old_id": old}),
                key,
            )
        };
        let (a, b) = (item(&v2, "a"), item(&v3, "b"));
        resolve(&f, &a, "accept").unwrap();
        let err = resolve(&f, &b, "accept").unwrap_err().to_string();
        assert!(err.contains("already superseded"), "{err}");
        assert_eq!(open_state(&f, &b), "open");
        f.h.submit_blocking(update(&v3, UpdateAction::Archive))
            .unwrap();
        let c = add_review(
            &f,
            ReviewType::Contradiction,
            serde_json::json!({"new_id": v3, "old_id": v2}),
            "c",
        );
        let err = resolve(&f, &c, "keep_new").unwrap_err().to_string();
        assert!(err.contains("already archived"), "{err}");
    }

    #[test]
    fn undoing_an_accepted_supersede_restores_status_and_links_and_the_item_stays_resolved() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("port is 8080")).unwrap());
        let (new, _) = new_id(f.h.submit_blocking(remember("port is 9090")).unwrap());
        let before_links: i64 = f.read("SELECT count(*) FROM links", []);
        let item = add_review(
            &f,
            ReviewType::Supersede,
            serde_json::json!({"new_id": new, "old_id": old}),
            "s",
        );
        resolve(&f, &item, "accept").unwrap();
        assert_eq!(status_of(&f, &old), "superseded");
        let ev = event_of(&f, "memory.supersede", &old);
        undo(&f, &ev).unwrap();
        assert_eq!(status_of(&f, &old), "active");
        let links: i64 = f.read("SELECT count(*) FROM links", []);
        assert_eq!(links, before_links, "the supersedes link went with it");
        assert_eq!(open_state(&f, &item), "resolved");
    }

    #[test]
    fn undoing_a_move_restores_the_project_and_the_content_hash() {
        let f = fixture();
        {
            let c = open(f.dir.path());
            alice_core::store::projects::upsert_project(
                &c,
                &ProjectRef {
                    key: "k2".into(),
                    slug: "other".into(),
                    name: "other".into(),
                    root: None,
                    remote: None,
                },
            )
            .unwrap();
        }
        let (m, _) = new_id(f.h.submit_blocking(remember("move and come back")).unwrap());
        let snapshot = |f: &Fixture| -> (String, String) {
            (
                f.read("SELECT project_id FROM memories WHERE id = ?1", [&m]),
                f.read("SELECT content_hash FROM memories WHERE id = ?1", [&m]),
            )
        };
        let before = snapshot(&f);
        let item = add_review(
            &f,
            ReviewType::Project,
            serde_json::json!({"memory_id": m, "from": "global", "to": "other"}),
            "mv",
        );
        resolve(&f, &item, "move").unwrap();
        let moved = snapshot(&f);
        assert_ne!(moved, before);
        undo(&f, &event_of(&f, "memory.move", &m)).unwrap();
        assert_eq!(snapshot(&f), before);
        let slug: String = f.read(
            "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1",
            [&m],
        );
        assert_eq!(slug, "global");
    }

    #[test]
    fn undoing_a_merge_by_review_reverts_the_duplicates_and_the_survivor_event() {
        let f = fixture();
        let ids: Vec<String> = ["survivor", "dup one", "dup two"]
            .iter()
            .map(|t| new_id(f.h.submit_blocking(remember(t)).unwrap()).0)
            .collect();
        let links_before: i64 = f.read("SELECT count(*) FROM links", []);
        let item = add_review(
            &f,
            ReviewType::Merge,
            serde_json::json!({"survivor": ids[0], "others": [ids[1], ids[2]]}),
            "m",
        );
        resolve(&f, &item, "merge").unwrap();
        let ev = event_of(&f, "memory.merge", &ids[1]);
        let WriteOutcome::Undo(UndoOutcome::Undone { reverted, .. }) = undo(&f, &ev).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            reverted.len(),
            3,
            "two duplicates and the survivor: {reverted:?}"
        );
        for d in &ids[1..] {
            assert_ne!(status_of(&f, d), "trashed");
        }
        let links: i64 = f.read("SELECT count(*) FROM links", []);
        assert_eq!(links, links_before, "no duplicate_of link is left");
    }

    /// Final review B-I1: the merge never changed the survivor, so undoing it neither waits for
    /// the survivor to be as it was nor puts its old state back: after an edit of the survivor
    /// (and after it was superseded) the merge is undone, the duplicate is back, and the
    /// survivor keeps what happened to it since.
    #[test]
    fn undoing_a_merge_after_the_survivor_changed_keeps_the_survivor_as_it_is() {
        let f = fixture();
        let ids: Vec<String> = ["survivor note", "duplicate note"]
            .iter()
            .map(|t| new_id(f.h.submit_blocking(remember(t)).unwrap()).0)
            .collect();
        let item = add_review(
            &f,
            ReviewType::Merge,
            serde_json::json!({"survivor": ids[0], "others": [ids[1]]}),
            "m",
        );
        resolve(&f, &item, "merge").unwrap();
        assert_eq!(status_of(&f, &ids[1]), "trashed");
        f.h.submit_blocking(WriteOp::Update {
            id: ids[0].clone(),
            update: MemoryUpdate {
                action: UpdateAction::Edit,
                body: Some("survivor note, edited a week later".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        let ev = event_of(&f, "memory.merge", &ids[1]);
        let out = undo(&f, &ev).unwrap();
        let WriteOutcome::Undo(UndoOutcome::Undone { reverted, .. }) = out else {
            panic!("{out:?}")
        };
        assert_eq!(reverted.len(), 2, "{reverted:?}");
        assert_eq!(status_of(&f, &ids[1]), "active", "the duplicate is back");
        let body: String = f.read("SELECT body FROM memories WHERE id = ?1", [&ids[0]]);
        assert_eq!(body, "survivor note, edited a week later", "the edit stays");
        let links: i64 = f.read(
            "SELECT count(*) FROM links WHERE type = 'duplicate_of' AND from_id = ?1",
            [&ids[1]],
        );
        assert_eq!(links, 0);

        // A survivor superseded since: undoing the merge leaves it superseded.
        let ids: Vec<String> = ["old survivor", "old duplicate"]
            .iter()
            .map(|t| new_id(f.h.submit_blocking(remember(t)).unwrap()).0)
            .collect();
        let item = add_review(
            &f,
            ReviewType::Merge,
            serde_json::json!({"survivor": ids[0], "others": [ids[1]]}),
            "m2",
        );
        resolve(&f, &item, "merge").unwrap();
        f.h.submit_blocking(WriteOp::Remember {
            input: NewMemoryInput {
                content: "the newer version".into(),
                supersedes: vec![ids[0].clone()],
                ..Default::default()
            },
            project: ProjectChoice::Fallback,
        })
        .unwrap();
        assert_eq!(status_of(&f, &ids[0]), "superseded");
        let ev = event_of(&f, "memory.merge", &ids[1]);
        assert!(matches!(
            undo(&f, &ev).unwrap(),
            WriteOutcome::Undo(UndoOutcome::Undone { .. })
        ));
        assert_eq!(
            status_of(&f, &ids[0]),
            "superseded",
            "never two current versions"
        );
        assert_eq!(status_of(&f, &ids[1]), "active");
    }

    #[test]
    fn a_predicate_mapping_by_review_is_undone_to_the_letter() {
        let f = fixture();
        for (p, o) in [("uses", "SQLite"), ("use", "Rust")] {
            f.h.submit_blocking(WriteOp::AssertFact {
                fact: AssertFact {
                    subject: "alice".into(),
                    predicate: p.into(),
                    object: o.into(),
                    ..Default::default()
                },
                actor: Actor::User,
                alias: None,
                review: None,
            })
            .unwrap();
        }
        let rels = |f: &Fixture| -> String {
            f.read("SELECT group_concat(id || predicate, ',') FROM (SELECT * FROM relations ORDER BY id)", [])
        };
        let preds = |f: &Fixture| -> String {
            f.read("SELECT group_concat(name || ':' || COALESCE(canonical, '-'), ',') FROM (SELECT * FROM predicates ORDER BY name)", [])
        };
        let (r0, p0) = (rels(&f), preds(&f));
        let item = add_review(
            &f,
            ReviewType::Predicate,
            serde_json::json!({"predicate": "uses", "canonical": "use"}),
            "p",
        );
        resolve(&f, &item, "map").unwrap();
        assert_ne!(rels(&f), r0);
        undo(&f, &event_of(&f, "predicate.map", "uses")).unwrap();
        assert_eq!((rels(&f), preds(&f)), (r0, p0));
    }

    // ------------------------------------------------------------------ the judge's writes

    fn hash_of(f: &Fixture, id: &str) -> String {
        f.read("SELECT content_hash FROM memories WHERE id = ?1", [id])
    }

    fn seq_of(f: &Fixture, id: &str) -> i64 {
        f.read("SELECT seq FROM memories WHERE id = ?1", [id])
    }

    /// `(actor, group)` of every event of `action`, oldest first.
    fn events_of(f: &Fixture, action: &str) -> Vec<(String, Option<String>)> {
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        let mut stmt = c
            .prepare("SELECT actor, group_id FROM events WHERE action = ?1 ORDER BY ts, id")
            .unwrap();
        stmt.query_map([action], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn judgement(f: &Fixture, id: &str, provider: &str) -> Judgement {
        Judgement {
            seq: seq_of(f, id),
            id: id.into(),
            stamp: hash_of(f, id),
            updated_at: f.read("SELECT updated_at FROM memories WHERE id = ?1", [id]),
            provider: provider.into(),
            // As the judge worker sends it (Task 12c review 2, N2).
            distill: Some(Default::default()),
            ..Default::default()
        }
    }

    fn apply_j(f: &Fixture, j: Judgement) {
        f.h.submit_blocking(WriteOp::ApplyJudgement(j)).unwrap();
    }

    fn judge_state_of(f: &Fixture, id: &str) -> (String, i64) {
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        c.query_row(
            "SELECT judge_state, judge_attempts FROM memories WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn add_project(f: &Fixture, slug: &str) {
        let c = open(f.dir.path());
        alice_core::store::projects::upsert_project(
            &c,
            &ProjectRef {
                key: format!("k-{slug}"),
                slug: slug.into(),
                name: slug.into(),
                root: None,
                remote: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn a_judgement_on_an_old_memory_edited_meanwhile_becomes_a_review_item() {
        let f = fixture();
        let (old, _) = new_id(
            f.h.submit_blocking(remember("Staging Postgres listens on port 5432"))
                .unwrap(),
        );
        let (new, _) = new_id(
            f.h.submit_blocking(remember("Staging Postgres now listens on 5433"))
                .unwrap(),
        );
        let old_hash = hash_of(&f, &old);
        // The old memory is edited after the judge read it.
        f.h.submit_blocking(WriteOp::Update {
            id: old.clone(),
            update: MemoryUpdate {
                body: Some("Staging Postgres listens on 6543".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        apply_j(
            &f,
            Judgement {
                actions: vec![JudgeAction::Supersedes {
                    old: old.clone(),
                    p: 0.95,
                }],
                seen: vec![(old.clone(), old_hash)],
                ..judgement(&f, &new, "jev")
            },
        );
        assert_ne!(
            status_of(&f, &old),
            "superseded",
            "not applied on stale information"
        );
        let n: i64 = f.read(
            "SELECT count(*) FROM review_items WHERE type = 'supersede' AND status = 'open'",
            [],
        );
        assert_eq!(n, 1);
        assert_eq!(
            status_of(&f, &new),
            "active",
            "the new memory is judged all the same"
        );
    }

    /// SP2 spec §6.6: a distilled newcomer never asks a person. An action whose target changed
    /// after the judge read it is dropped, not turned into a review item.
    #[test]
    fn a_stale_target_of_a_distilled_newcomer_is_dropped() {
        let f = fixture();
        let (old, old_seq) = new_id(
            f.h.submit_blocking(remember("Staging Postgres listens on port 5432"))
                .unwrap(),
        );
        judged(&f, &old, old_seq);
        let (auto, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "Staging Postgres now listens on 5433".into(),
                    origin: Origin::Distill,
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .unwrap(),
        );
        let old_hash = hash_of(&f, &old);
        // The old memory is edited after the judge read it.
        f.h.submit_blocking(WriteOp::Update {
            id: old.clone(),
            update: MemoryUpdate {
                body: Some("Staging Postgres listens on 6543".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        let links = |id: &str| -> i64 {
            f.read(
                "SELECT count(*) FROM links WHERE from_id = ?1 OR to_id = ?1",
                [id],
            )
        };
        let before = links(&old);
        apply_j(
            &f,
            Judgement {
                actions: vec![
                    JudgeAction::Supersedes {
                        old: old.clone(),
                        p: 0.95,
                    },
                    JudgeAction::RelatesTo {
                        other: old.clone(),
                        p: 0.9,
                    },
                    JudgeAction::Contradicts {
                        other: old.clone(),
                        p: 0.9,
                    },
                    JudgeAction::DuplicateOf {
                        survivor: old.clone(),
                        p: 0.95,
                    },
                ],
                seen: vec![(old.clone(), old_hash)],
                ..judgement(&f, &auto, "jev")
            },
        );
        let reviews: i64 = f.read("SELECT count(*) FROM review_items", []);
        assert_eq!(reviews, 0, "a distilled newcomer never asks");
        assert_eq!(links(&old), before, "nothing applied on stale information");
        assert_eq!(status_of(&f, &old), "active");
        assert_eq!(status_of(&f, &auto), "active");
        assert_eq!(judge_state_of(&f, &auto).0, "done", "judged all the same");
    }

    /// SP2 spec §6.6, R4: an explicit newcomer's replacement of a distilled duplicate that
    /// changed after the judge read it is not applied; it becomes the merge question, with the
    /// explicit newcomer as the survivor.
    #[test]
    fn a_stale_distilled_duplicate_of_an_explicit_newcomer_is_asked_about() {
        let f = fixture();
        let (auto, auto_seq) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "Staging Postgres listens on port 5432".into(),
                    origin: Origin::Distill,
                    ..Default::default()
                },
                project: ProjectChoice::Slug("global".into()),
            })
            .unwrap(),
        );
        judged(&f, &auto, auto_seq);
        let (mine, _) = new_id(
            f.h.submit_blocking(remember("The staging Postgres server listens on 5432"))
                .unwrap(),
        );
        let auto_hash = hash_of(&f, &auto);
        // The distilled memory is edited after the judge read it.
        f.h.submit_blocking(WriteOp::Update {
            id: auto.clone(),
            update: MemoryUpdate {
                body: Some("Staging Postgres listens on port 6543".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        apply_j(
            &f,
            Judgement {
                actions: vec![JudgeAction::ReplaceDistilled {
                    distilled: auto.clone(),
                    p: 0.95,
                }],
                seen: vec![(auto.clone(), auto_hash)],
                ..judgement(&f, &mine, "jev")
            },
        );
        assert_eq!(
            status_of(&f, &auto),
            "active",
            "not merged on stale information"
        );
        let merged: i64 = f.read(
            "SELECT count(*) FROM links WHERE type = 'duplicate_of' AND from_id = ?1",
            [&auto],
        );
        assert_eq!(merged, 0);
        assert_eq!(status_of(&f, &mine), "active");
        let n: i64 = f.read("SELECT count(*) FROM review_items", []);
        assert_eq!(n, 1);
        let (kind, payload): (String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT type, payload_json FROM review_items WHERE status = 'open'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(kind, "merge");
        assert_eq!(payload["survivor"], mine.as_str(), "{payload}");
        assert_eq!(payload["others"], serde_json::json!([auto]), "{payload}");
    }

    #[test]
    fn a_judgement_is_one_group_of_events_by_the_judge_and_activates_the_memory() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("port is 8080")).unwrap());
        let (new, _) = new_id(f.h.submit_blocking(remember("port is now 9090")).unwrap());
        apply_j(
            &f,
            Judgement {
                actions: vec![JudgeAction::Supersedes {
                    old: old.clone(),
                    p: 0.95,
                }],
                kind: Some(Kind::Fact),
                kind_provider: Some("jev".into()),
                seen: vec![(old.clone(), hash_of(&f, &old))],
                ..judgement(&f, &new, "jev")
            },
        );
        assert_eq!(status_of(&f, &old), "superseded");
        assert_eq!(status_of(&f, &new), "active");
        let kind: String = f.read("SELECT kind FROM memories WHERE id = ?1", [&new]);
        assert_eq!(kind, "fact");
        assert_eq!(judge_state_of(&f, &new), ("done".to_string(), 1));
        let all: Vec<(String, Option<String>)> = ["memory.supersede", "memory.link", "memory.edit"]
            .iter()
            .flat_map(|a| events_of(&f, a))
            .filter(|(actor, _)| actor == "judge:jev")
            .collect();
        assert_eq!(all.len(), 3, "{all:?}");
        let groups: std::collections::HashSet<_> = all.iter().map(|(_, g)| g.clone()).collect();
        assert_eq!(groups.len(), 1, "one judgement is one undoable group");
        assert!(groups.iter().all(|g| g.is_some()));
    }

    #[test]
    fn a_confident_duplicate_trashes_the_new_memory_and_skips_the_rest() {
        let f = fixture();
        let (survivor, survivor_seq) =
            new_id(f.h.submit_blocking(remember("use WAL mode")).unwrap());
        judged(&f, &survivor, survivor_seq);
        let (dup, dup_seq) = new_id(
            f.h.submit_blocking(remember("use  WAL  mode please"))
                .unwrap(),
        );
        apply_j(
            &f,
            Judgement {
                actions: vec![JudgeAction::DuplicateOf {
                    survivor: survivor.clone(),
                    p: 0.99,
                }],
                kind: Some(Kind::Decision),
                seen: vec![(survivor.clone(), hash_of(&f, &survivor))],
                ..judgement(&f, &dup, "heuristic")
            },
        );
        assert_eq!(status_of(&f, &dup), "trashed");
        assert_eq!(status_of(&f, &survivor), "active");
        let link: i64 = f.read(
            "SELECT count(*) FROM links WHERE from_id = ?1 AND to_id = ?2 AND type = 'duplicate_of'",
            [&dup, &survivor],
        );
        assert_eq!(link, 1);
        let kind: String = f.read("SELECT kind FROM memories WHERE seq = ?1", [dup_seq]);
        assert_eq!(
            kind, "note",
            "a memory that goes to the trash is not reclassified"
        );
        assert_eq!(events_of(&f, "memory.merge")[0].0, "judge:heuristic");
        assert_eq!(judge_state_of(&f, &dup).0, "done");
    }

    /// Undoing a judge's change restores a live memory, never one stuck in `pending`.
    #[test]
    fn undoing_a_judgement_restores_an_active_memory() {
        let f = fixture();
        let (survivor, survivor_seq) =
            new_id(f.h.submit_blocking(remember("cache layer")).unwrap());
        judged(&f, &survivor, survivor_seq);
        let (dup, _) = new_id(f.h.submit_blocking(remember("the cache layer")).unwrap());
        apply_j(
            &f,
            Judgement {
                actions: vec![JudgeAction::DuplicateOf {
                    survivor: survivor.clone(),
                    p: 0.99,
                }],
                seen: vec![(survivor.clone(), hash_of(&f, &survivor))],
                ..judgement(&f, &dup, "heuristic")
            },
        );
        assert_eq!(status_of(&f, &dup), "trashed");
        undo(&f, &event_of(&f, "memory.merge", &dup)).unwrap();
        assert_eq!(status_of(&f, &dup), "active", "restored as a live memory");
        let link: i64 = f.read(
            "SELECT count(*) FROM links WHERE from_id = ?1 AND type = 'duplicate_of'",
            [&dup],
        );
        assert_eq!(link, 0);
    }

    #[test]
    fn a_contradiction_is_linked_and_asked_about_and_both_stay_live() {
        let f = fixture();
        let (a, a_seq) = new_id(
            f.h.submit_blocking(remember("the cache is write-through"))
                .unwrap(),
        );
        judged(&f, &a, a_seq);
        let (b, _) = new_id(
            f.h.submit_blocking(remember("the cache is write-back"))
                .unwrap(),
        );
        let ask = alice_core::judge::pipeline::decide(
            &alice_core::judge::JudgeMemory {
                id: b.clone(),
                project: "global".into(),
                kind: Kind::Note,
                title: "t".into(),
                body: "b".into(),
                importance: 3,
                created_at: "2026-10-13T00:00:00.000Z".into(),
                similarity: None,
                raw: false,
                status: None,
                origin: Origin::Agent,
            },
            &[alice_core::judge::Relation {
                existing_id: a.clone(),
                probs: alice_core::judge::Distribution::from_pairs([
                    ("contradicts".to_string(), 0.9),
                    ("unrelated".to_string(), 0.1),
                ]),
                agreement: None,
                existing_origin: Origin::Agent,
                existing_body: String::new(),
            }],
            &alice_core::judge::pipeline::Thresholds {
                auto: 0.85,
                review: 0.5,
            },
            "jev",
            false,
        );
        apply_j(
            &f,
            Judgement {
                actions: ask.actions,
                reviews: ask.reviews,
                seen: vec![(a.clone(), hash_of(&f, &a))],
                ..judgement(&f, &b, "jev")
            },
        );
        let link: i64 = f.read(
            "SELECT count(*) FROM links WHERE from_id = ?1 AND to_id = ?2 AND type = 'contradicts' AND created_by = 'judge:jev'",
            [&b, &a],
        );
        assert_eq!(link, 1);
        let open: i64 = f.read(
            "SELECT count(*) FROM review_items WHERE type = 'contradiction' AND status = 'open'",
            [],
        );
        assert_eq!(open, 1);
        assert_eq!(
            (status_of(&f, &a).as_str(), status_of(&f, &b).as_str()),
            ("active", "active")
        );
        assert_eq!(events_of(&f, "memory.link")[0].0, "judge:jev");
    }

    #[test]
    fn an_action_on_a_memory_that_is_gone_is_dropped_without_a_review_item() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("old fact")).unwrap());
        let (new, _) = new_id(f.h.submit_blocking(remember("new fact")).unwrap());
        let seen = vec![(old.clone(), hash_of(&f, &old))];
        f.h.submit_blocking(WriteOp::Update {
            id: old.clone(),
            update: MemoryUpdate {
                action: UpdateAction::Forget,
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        apply_j(
            &f,
            Judgement {
                actions: vec![JudgeAction::Supersedes {
                    old: old.clone(),
                    p: 0.95,
                }],
                seen,
                ..judgement(&f, &new, "jev")
            },
        );
        assert_eq!(
            status_of(&f, &old),
            "trashed",
            "a trashed memory is not touched"
        );
        let n: i64 = f.read("SELECT count(*) FROM review_items", []);
        assert_eq!(n, 0, "nothing is left to ask about");
        assert_eq!(status_of(&f, &new), "active");
    }

    /// P7: imported memories (`skipped`) are never judged; a changed text or a judged memory is
    /// left alone too.
    #[test]
    fn a_judgement_is_ignored_for_imports_changed_text_and_memories_already_judged() {
        let f = fixture();
        let (imported, seq) = new_id(f.h.submit_blocking(remember("imported memory")).unwrap());
        open(f.dir.path())
            .execute(
                "UPDATE memories SET judge_state = 'skipped' WHERE seq = ?1",
                [seq],
            )
            .unwrap();
        apply_j(&f, judgement(&f, &imported, "jev"));
        assert_eq!(judge_state_of(&f, &imported), ("skipped".to_string(), 0));
        assert_eq!(
            status_of(&f, &imported),
            "pending",
            "not activated by a judgement"
        );
        // Its embedding is its last step: that activates it (nobody else will).
        let item = embeddings::pending(&open(f.dir.path()), 10)
            .unwrap()
            .into_iter()
            .find(|i| i.key == VecKey::Memory(seq))
            .unwrap();
        f.h.submit_blocking(WriteOp::StoreEmbeddings {
            model: "m".into(),
            items: vec![(item.key, item.stamp, vec![1.0])],
        })
        .unwrap();
        assert_eq!(status_of(&f, &imported), "active");
        assert_eq!(judge_state_of(&f, &imported).0, "skipped");

        // Text changed since it was judged: nothing happens, the new text is judged next.
        let (edited, _) = new_id(f.h.submit_blocking(remember("before the edit")).unwrap());
        let stale = judgement(&f, &edited, "jev");
        f.h.submit_blocking(WriteOp::Update {
            id: edited.clone(),
            update: MemoryUpdate {
                body: Some("after the edit".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        apply_j(&f, stale);
        assert_eq!(
            (
                status_of(&f, &edited).as_str(),
                judge_state_of(&f, &edited).0.as_str()
            ),
            ("pending", "pending")
        );

        // Judged once; a second judgement of the same text does nothing.
        let (once, _) = new_id(f.h.submit_blocking(remember("judged once")).unwrap());
        apply_j(&f, judgement(&f, &once, "jev"));
        apply_j(&f, judgement(&f, &once, "jev"));
        assert_eq!(judge_state_of(&f, &once), ("done".to_string(), 1));
    }

    #[test]
    fn a_failed_judgement_is_tried_again_until_the_attempts_run_out() {
        let f = fixture();
        let (id, _) = new_id(f.h.submit_blocking(remember("flaky judge")).unwrap());
        apply_j(
            &f,
            Judgement {
                retry: true,
                ..judgement(&f, &id, "heuristic")
            },
        );
        assert_eq!(judge_state_of(&f, &id), ("retry".to_string(), 1));
        assert_eq!(
            status_of(&f, &id),
            "active",
            "the heuristic's answer is live meanwhile"
        );
        f.h.submit_blocking(WriteOp::RequeueJudgeRetries {
            max_attempts: 2,
            now: "2099-01-01T00:00:00.000Z".into(),
            distill: None,
        })
        .unwrap();
        assert_eq!(judge_state_of(&f, &id), ("pending".to_string(), 1));
        apply_j(
            &f,
            Judgement {
                retry: true,
                ..judgement(&f, &id, "heuristic")
            },
        );
        assert_eq!(judge_state_of(&f, &id), ("retry".to_string(), 2));
        f.h.submit_blocking(WriteOp::RequeueJudgeRetries {
            max_attempts: 2,
            now: "2099-01-01T00:00:00.000Z".into(),
            distill: None,
        })
        .unwrap();
        assert_eq!(
            judge_state_of(&f, &id),
            ("done".to_string(), 2),
            "given up after two attempts"
        );
    }

    #[test]
    fn a_fallback_project_is_global_and_remembered_as_defaulted_and_the_judge_can_route_it() {
        let f = fixture();
        add_project(&f, "proj");
        let remember_in = |text: &str, project: ProjectChoice| {
            new_id(
                f.h.submit_blocking(WriteOp::Remember {
                    input: NewMemoryInput {
                        content: text.into(),
                        ..Default::default()
                    },
                    project,
                })
                .unwrap(),
            )
        };
        let (id, seq) = remember_in("the proj deploy runs on Fridays", ProjectChoice::Fallback);
        let (named, _) = remember_in("named global", ProjectChoice::Slug("global".into()));
        let flags = |id: &str| -> (i64, i64, String) {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT m.kind_explicit, m.project_defaulted, p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(flags(&id), (0, 1, "global".to_string()));
        assert_eq!(
            flags(&named),
            (0, 0, "global".to_string()),
            "a named project is not defaulted"
        );
        apply_j(
            &f,
            Judgement {
                project_to: Some("proj".into()),
                project_provider: Some("jev".into()),
                ..judgement(&f, &id, "heuristic")
            },
        );
        assert_eq!(flags(&id), (0, 0, "proj".to_string()));
        assert_eq!(status_of(&f, &id), "active");
        let moves = events_of(&f, "memory.move");
        assert_eq!(moves.len(), 1);
        assert_eq!(
            moves[0].0, "judge:jev",
            "the router, not the comparison, is named"
        );
        let _ = seq;
    }

    /// A routed memory whose text already lives in the target project is merged into it.
    #[test]
    fn a_routed_duplicate_is_merged_into_the_twin_in_the_target_project() {
        let f = fixture();
        add_project(&f, "proj");
        let (twin, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "same words".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Slug("proj".into()),
            })
            .unwrap(),
        );
        let (id, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "same words".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Fallback,
            })
            .unwrap(),
        );
        apply_j(
            &f,
            Judgement {
                project_to: Some("proj".into()),
                ..judgement(&f, &id, "jev")
            },
        );
        assert_eq!(status_of(&f, &id), "trashed");
        assert_ne!(status_of(&f, &twin), "trashed");
    }

    /// P8: the text a move was screened with must be the text that moves.
    #[test]
    fn a_move_screened_for_one_text_is_refused_once_the_memory_changed() {
        let f = fixture();
        add_project(&f, "other");
        let (m, _) = new_id(f.h.submit_blocking(remember("screened text")).unwrap());
        let screened = hash_of(&f, &m);
        let screened_at: String = f.read("SELECT updated_at FROM memories WHERE id = ?1", [&m]);
        let item = add_review(
            &f,
            ReviewType::Project,
            serde_json::json!({"memory_id": m, "from": "global", "to": "other", "p": 0.8, "provider": "heuristic"}),
            "mv",
        );
        // Edited after it was screened (say, with a secret the destination's rules would catch).
        f.h.submit_blocking(WriteOp::Update {
            id: m.clone(),
            update: MemoryUpdate {
                body: Some("edited after screening".into()),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        let moved = |hash: &str, at: &str, body: &str| ResolveText::Moved {
            content_hash: hash.into(),
            updated_at: at.into(),
            title: "screened text".into(),
            body: body.into(),
            topics: Vec::new(),
        };
        let err =
            f.h.submit_blocking(WriteOp::ResolveReview {
                id: item.clone(),
                choice: "move".into(),
                actor: Actor::User,
                text: Some(moved(&screened, &screened_at, "screened text")),
            })
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("edited while its move was being checked"),
            "{err}"
        );
        let slug: String = f.read(
            "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1",
            [&m],
        );
        assert_eq!(slug, "global", "nothing moved");
        let open: i64 = f.read(
            "SELECT count(*) FROM review_items WHERE status = 'open'",
            [],
        );
        assert_eq!(open, 1, "the item stays open");
        // Screened again with the current text, the move goes through; text the rules left as
        // it is records no edit, text they changed is stored in the same group.
        let now = hash_of(&f, &m);
        let now_at: String = f.read("SELECT updated_at FROM memories WHERE id = ?1", [&m]);
        let cur: (String, String) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT title, body FROM memories WHERE id = ?1",
                [&m],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        f.h.submit_blocking(WriteOp::ResolveReview {
            id: item,
            choice: "move".into(),
            actor: Actor::User,
            text: Some(ResolveText::Moved {
                content_hash: now,
                updated_at: now_at,
                title: cur.0,
                body: cur.1,
                topics: Vec::new(),
            }),
        })
        .unwrap();
        let slug: String = f.read(
            "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1",
            [&m],
        );
        assert_eq!(slug, "other");
        let n: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'memory.edit'",
            [],
        );
        assert_eq!(
            n, 1,
            "no edit event for text the rules left alone (the one is the user's)"
        );

        let (m2, _) = new_id(f.h.submit_blocking(remember("second one")).unwrap());
        let item2 = add_review(
            &f,
            ReviewType::Project,
            serde_json::json!({"memory_id": m2, "from": "global", "to": "other", "p": 0.8, "provider": "heuristic"}),
            "mv2",
        );
        f.h.submit_blocking(WriteOp::ResolveReview {
            id: item2,
            choice: "move".into(),
            actor: Actor::User,
            text: Some(moved(
                &hash_of(&f, &m2),
                &f.read::<String>("SELECT updated_at FROM memories WHERE id = ?1", [&m2]),
                "second [REDACTED:x]",
            )),
        })
        .unwrap();
        let body: String = f.read("SELECT body FROM memories WHERE id = ?1", [&m2]);
        assert_eq!(
            body, "second [REDACTED:x]",
            "the destination's cleaned text is stored with the move"
        );
    }

    /// SP2 Task 9: a Jev call is billed to its consumer's ledger row (an unknown row to the
    /// judge's own, so the day's total still counts it) and cached under one provider, `jev`.
    #[test]
    fn a_judge_call_is_billed_to_its_consumers_row_and_cached_as_jev() {
        use alice_core::judge::jev::{JevCall, LEDGER_DISTILL, LEDGER_JUDGE};
        let f = fixture();
        let call = |hash: &str, provider: &str, usd: f64| JevCall {
            day: "2026-10-16".into(),
            hash: hash.into(),
            model: "jev-1.13.0".into(),
            response_json: "{}".into(),
            input_tokens: 10,
            output_tokens: 1,
            usd,
            latency_ms: 5,
            cacheable: true,
            provider: provider.into(),
        };
        for c in [
            call("h1", LEDGER_JUDGE, 0.25),
            call("h2", LEDGER_DISTILL, 0.5),
            call("h3", "bogus", 1.0),
        ] {
            f.h.submit_blocking(WriteOp::JudgeCall(c)).unwrap();
        }
        let row = |p: &str| -> (i64, f64) {
            let col = |c: &str| {
                format!("SELECT {c} FROM cost_ledger WHERE day = '2026-10-16' AND provider = ?1")
            };
            (f.read(&col("calls"), [p]), f.read(&col("usd"), [p]))
        };
        assert_eq!(row(LEDGER_DISTILL), (1, 0.5));
        assert_eq!(
            row(LEDGER_JUDGE),
            (2, 1.25),
            "the unknown row went to the judge's"
        );
        assert_eq!(f.read::<i64>("SELECT count(*) FROM cost_ledger", []), 2);
        assert_eq!(
            f.read::<i64>(
                "SELECT count(*) FROM judge_cache WHERE provider = 'jev'",
                []
            ),
            3
        );
    }

    /// Task 14: `SetMeta` upserts, `PurgeMemories` takes only trashed memories (the others are
    /// left alone), records a `memory.purge` event by `system` that names no text, and scrubs
    /// the history of the purged memory.
    #[test]
    fn set_meta_upserts_and_a_purge_takes_only_trashed_memories() {
        let f = fixture();
        for v in ["one", "two"] {
            f.h.submit_blocking(WriteOp::SetMeta {
                key: "maint.x.last_run".into(),
                value: v.into(),
            })
            .unwrap();
        }
        let v: String = f.read("SELECT value FROM meta WHERE key = 'maint.x.last_run'", []);
        assert_eq!(v, "two");

        let (keep, keep_seq) = new_id(f.h.submit_blocking(remember("alive and well")).unwrap());
        let (gone, gone_seq) = new_id(
            f.h.submit_blocking(remember("the safe code is 1234 swordfish"))
                .unwrap(),
        );
        f.h.submit_blocking(update(&gone, UpdateAction::Forget))
            .unwrap();
        let mut rx = f.h.subscribe();
        let out = f.h.submit_blocking(WriteOp::PurgeMemories {
            seqs: vec![keep_seq, gone_seq, 99_999],
            cutoff: "2999-01-01T00:00:00.000Z".into(),
        });
        let WriteOutcome::Purged(done) = out.unwrap() else {
            panic!("expected Purged")
        };
        assert_eq!(
            done,
            vec![gone_seq],
            "a live memory and a missing seq are skipped"
        );
        let n: i64 = f.read("SELECT count(*) FROM memories WHERE id = ?1", [&keep]);
        assert_eq!(n, 1);
        let n: i64 = f.read("SELECT count(*) FROM memories WHERE id = ?1", [&gone]);
        assert_eq!(n, 0);
        match rx.try_recv().unwrap() {
            StreamItem::Event { event } => {
                assert_eq!(event.action, "memory.purge");
                assert_eq!(event.actor, "system");
                assert_eq!(event.target_id, gone);
                assert_eq!(
                    event.before,
                    Some(serde_json::json!({"id": gone, "title": "[purged]"}))
                );
                assert_eq!(event.after, None);
            }
            other => panic!("{other:?}"),
        }
        let leaked: i64 = f.read(
            "SELECT count(*) FROM events WHERE before_json LIKE '%swordfish%' OR after_json LIKE '%swordfish%'",
            [],
        );
        assert_eq!(leaked, 0, "the history no longer holds the purged text");
        // The memory next to it keeps its history.
        let kept: i64 = f.read("SELECT count(*) FROM events WHERE target_id = ?1 AND after_json LIKE '%alive and well%'", [&keep]);
        assert_eq!(kept, 1);
    }

    /// Task 14: retention deletes chunks with one event; pruning and merging report their work;
    /// a review item is announced once and an open duplicate adds nothing.
    #[test]
    fn retention_pruning_merging_and_review_creation() {
        let f = fixture();
        let chunks: Vec<ChunkInput> = (0..4)
            .map(|i| ChunkInput {
                ord: i,
                text: format!("transcript line {i}"),
                ..Default::default()
            })
            .collect();
        f.h.submit_blocking(WriteOp::Ingest {
            project: ProjectChoice::Slug("global".into()),
            source: SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            chunks,
        })
        .unwrap();
        let seqs: Vec<i64> = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.prepare("SELECT seq FROM chunks ORDER BY seq")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        let WriteOutcome::Purged(done) =
            f.h.submit_blocking(WriteOp::DeleteChunks {
                seqs: seqs[..3].to_vec(),
            })
            .unwrap()
        else {
            panic!("expected Purged")
        };
        assert_eq!(done, seqs[..3].to_vec());
        // The job records one `raw.retention` event per run; the writer op itself none.
        let events: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'raw.retention'",
            [],
        );
        assert_eq!(events, 0);
        let left: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(left, 1);
        // Nothing to delete: still fine.
        f.h.submit_blocking(WriteOp::DeleteChunks {
            seqs: vec![seqs[0]],
        })
        .unwrap();

        let WriteOutcome::Count(pruned) =
            f.h.submit_blocking(WriteOp::PruneEvents {
                before: "2999-01-01T00:00:00.000Z".into(),
                limit: 2,
            })
            .unwrap()
        else {
            panic!("expected Count")
        };
        assert!(
            (1..=2).contains(&pruned),
            "about `limit` events go per step: {pruned}"
        );
        assert!(matches!(
            f.h.submit_blocking(WriteOp::PruneJudgeCache {
                before: "2999-01-01T00:00:00.000Z".into(),
                keep: 10,
                limit: 10,
            })
            .unwrap(),
            WriteOutcome::Count(0)
        ));
        assert!(matches!(
            f.h.submit_blocking(WriteOp::FtsMerge {
                table: "chunks_fts".into(),
                pages: 100
            })
            .unwrap(),
            WriteOutcome::Flag(_)
        ));
        assert!(
            f.h.submit_blocking(WriteOp::FtsMerge {
                table: "events".into(),
                pages: 1
            })
            .is_err()
        );

        let mut rx = f.h.subscribe();
        let review = || NewReview {
            kind: ReviewType::Merge,
            payload: serde_json::json!({"survivor": "A", "others": ["B"], "p": 0.93, "provider": "cluster"}),
            confidence: Some(0.93),
            dedupe_key: Some("merge:A:B".into()),
        };
        assert!(matches!(
            f.h.submit_blocking(WriteOp::CreateReview(review()))
                .unwrap(),
            WriteOutcome::Count(1)
        ));
        assert!(matches!(
            f.h.submit_blocking(WriteOp::CreateReview(review()))
                .unwrap(),
            WriteOutcome::Count(0)
        ));
        match rx.try_recv().unwrap() {
            StreamItem::Review { review_type, .. } => assert_eq!(review_type, "merge"),
            other => panic!("{other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "an open duplicate is not announced again"
        );
    }

    // ------------------------------------------------------------------ fix round 1

    /// M5: topics and importance are not in the hash, but a judgement made before they changed
    /// is stale all the same.
    #[test]
    fn a_judgement_made_before_an_importance_edit_is_stale() {
        let f = fixture();
        let (id, _) = new_id(f.h.submit_blocking(remember("importance moves")).unwrap());
        let stale = judgement(&f, &id, "jev");
        f.h.submit_blocking(WriteOp::Update {
            id: id.clone(),
            update: MemoryUpdate {
                importance: Some(5),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        apply_j(&f, stale);
        assert_eq!(
            (
                status_of(&f, &id).as_str(),
                judge_state_of(&f, &id).0.as_str()
            ),
            ("pending", "pending")
        );
        apply_j(&f, judgement(&f, &id, "jev"));
        assert_eq!(judge_state_of(&f, &id).0, "done");
    }

    #[test]
    fn a_move_screened_before_an_importance_edit_is_refused() {
        let f = fixture();
        add_project(&f, "other");
        let (m, _) = new_id(f.h.submit_blocking(remember("importance edit")).unwrap());
        let hash = hash_of(&f, &m);
        let at: String = f.read("SELECT updated_at FROM memories WHERE id = ?1", [&m]);
        let item = add_review(
            &f,
            ReviewType::Project,
            serde_json::json!({"memory_id": m, "from": "global", "to": "other", "p": 0.8, "provider": "heuristic"}),
            "mv",
        );
        f.h.submit_blocking(WriteOp::Update {
            id: m.clone(),
            update: MemoryUpdate {
                importance: Some(4),
                ..Default::default()
            },
            actor: Actor::User,
        })
        .unwrap();
        let err =
            f.h.submit_blocking(WriteOp::ResolveReview {
                id: item,
                choice: "move".into(),
                actor: Actor::User,
                text: Some(ResolveText::Moved {
                    content_hash: hash,
                    updated_at: at,
                    title: "importance edit".into(),
                    body: "importance edit".into(),
                    topics: Vec::new(),
                }),
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("edited while its move"), "{err}");
    }

    /// M6: a memory the judge moved is compared again in its new project, not acted on with
    /// the old project's neighbours.
    #[test]
    fn a_moved_memory_skips_its_old_actions_and_is_judged_again_there() {
        let f = fixture();
        add_project(&f, "proj");
        let (old, old_seq) = new_id(f.h.submit_blocking(remember("global fact")).unwrap());
        judged(&f, &old, old_seq);
        let (id, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "global fact again".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Fallback,
            })
            .unwrap(),
        );
        apply_j(
            &f,
            Judgement {
                project_to: Some("proj".into()),
                actions: vec![JudgeAction::Supersedes {
                    old: old.clone(),
                    p: 0.95,
                }],
                reviews: vec![
                    alice_core::judge::pipeline::as_review(
                        &id,
                        &JudgeAction::Supersedes {
                            old: old.clone(),
                            p: 0.95,
                        },
                        "jev",
                    )
                    .unwrap(),
                ],
                seen: vec![(old.clone(), hash_of(&f, &old))],
                ..judgement(&f, &id, "jev")
            },
        );
        assert_eq!(
            status_of(&f, &old),
            "active",
            "the old project's neighbour is left alone"
        );
        let n: i64 = f.read("SELECT count(*) FROM review_items", []);
        assert_eq!(n, 0);
        assert_eq!(status_of(&f, &id), "active");
        assert_eq!(judge_state_of(&f, &id).0, "pending", "judged again in proj");
    }

    /// M2: a failed judgement waits, and a judge that was only unreachable does not count
    /// toward giving up.
    #[test]
    fn a_failed_judgement_waits_and_unreachability_does_not_count() {
        let f = fixture();
        let (id, _) = new_id(f.h.submit_blocking(remember("backoff")).unwrap());
        apply_j(
            &f,
            Judgement {
                retry: true,
                unavailable: true,
                retry_wait: Duration::from_secs(600),
                ..judgement(&f, &id, "heuristic")
            },
        );
        let counts: (i64, i64) = {
            let c = alice_core::store::open_reader(&f.db()).unwrap();
            c.query_row(
                "SELECT judge_attempts, judge_unavailable FROM memories WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(counts, (0, 1), "unreachable: not counted");
        // Not yet due.
        f.h.submit_blocking(WriteOp::RequeueJudgeRetries {
            max_attempts: 1,
            now: alice_core::ids::now_rfc3339(),
            distill: None,
        })
        .unwrap();
        assert_eq!(judge_state_of(&f, &id).0, "retry");
        // Due, and never given up however often it was unreachable (max 1, attempts 0).
        f.h.submit_blocking(WriteOp::RequeueJudgeRetries {
            max_attempts: 1,
            now: "2099-01-01T00:00:00.000Z".into(),
            distill: None,
        })
        .unwrap();
        assert_eq!(judge_state_of(&f, &id).0, "pending");
    }

    /// C1/M7 at the writer: nothing here changes a judgement's own actions; the decision is
    /// made upstream, but a stand-in's judgement carries none.
    #[test]
    fn a_plain_judgement_activates_without_touching_anything() {
        let f = fixture();
        let (id, _) = new_id(f.h.submit_blocking(remember("just activate")).unwrap());
        apply_j(
            &f,
            Judgement {
                retry: true,
                ..judgement(&f, &id, "heuristic")
            },
        );
        assert_eq!(status_of(&f, &id), "active");
        let n: i64 = f.read("SELECT count(*) FROM events WHERE actor LIKE 'judge:%'", []);
        assert_eq!(n, 0);
    }

    /// Small writes that arrive during an import are written ahead of its next slice, but never
    /// instead of it: with writers that never pause, the import still finishes.
    #[test]
    fn an_import_still_finishes_while_small_writes_keep_arriving() {
        let f = fixture();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..4)
            .map(|t| {
                let (h, stop) = (f.h.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut n = 0;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        h.submit_blocking(remember(&format!("busy writer {t} note {n}")))
                            .unwrap();
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        let started = Instant::now();
        let out = ingested(f.h.submit_blocking(ingest("busy", 1_000, long_text)));
        let took = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let written: i32 = writers.into_iter().map(|w| w.join().unwrap()).sum();
        assert_eq!(out.inserted, 1_000);
        assert!(written > 0, "the small writes ran during the import");
        assert!(
            took < Duration::from_secs(120),
            "the import took {took:?} beside {written} small writes"
        );
    }

    fn wal_bytes(db: &Path) -> u64 {
        let mut p = db.as_os_str().to_owned();
        p.push("-wal");
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    }

    fn tuned(limit: u64, gap: Duration) -> Fixture {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let redactors = Redactors::fixed(alice_core::settings::PrivacySettings::default());
        let policy = WalPolicy {
            limit,
            min_gap: gap,
            busy_wait: Duration::from_millis(100),
        };
        let (h, thread) = spawn_writer_tuned(conn, "dev".into(), wake_tx, redactors, policy);
        Fixture {
            dir: d,
            h,
            wake: wake_rx,
            thread,
        }
    }

    /// Queries as a client does: short read transactions, back to back, never a gap in which
    /// no reader is on the log. Returns the stop flag and the thread.
    fn busy_reader(db: std::path::PathBuf) -> (Arc<std::sync::atomic::AtomicBool>, JoinHandle<()>) {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let t = std::thread::spawn(move || {
            let a = alice_core::store::open_reader(&db).unwrap();
            let b = alice_core::store::open_reader(&db).unwrap();
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                for c in [&a, &b] {
                    let _: i64 = c
                        .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
                        .unwrap();
                    let _: i64 = c
                        .query_row(
                            "SELECT count(*) FROM (SELECT rowid FROM chunks LIMIT 50)",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                }
            }
        });
        (stop, t)
    }

    /// The incident behind Task 20: with readers that never leave the log, SQLite's own
    /// checkpoint never restarts it and the file only grows. With the writer's policy it
    /// stays near the limit. (With `WalPolicy { limit: u64::MAX, .. }` this test fails: the
    /// log ends several times over the bound.)
    #[test]
    fn a_long_ingest_beside_a_reader_that_keeps_querying_leaves_a_bounded_wal() {
        const LIMIT: u64 = 2 * 1024 * 1024;
        let f = tuned(LIMIT, Duration::ZERO);
        let (stop, reader) = busy_reader(f.db());
        let mut peak = 0;
        for round in 0..6 {
            ingested(f.h.submit_blocking(ingest(&format!("bulk{round}"), 1_000, long_text)));
            peak = peak.max(wal_bytes(&f.db()));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap();
        let n: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(n, 6_000, "nothing was lost to the restarts");
        assert!(
            peak <= 2 * LIMIT,
            "the -wal reached {peak} bytes with a limit of {LIMIT}"
        );
    }

    /// A reader that began before the restart keeps its snapshot, so the restart is skipped,
    /// and not one write in flight fails or waits more than the bounded wait for it.
    #[test]
    fn a_restart_skipped_for_a_reader_never_fails_the_write_in_flight() {
        let f = tuned(1, Duration::ZERO);
        f.h.submit_blocking(remember("first")).unwrap();
        let held = alice_core::store::open_reader(&f.db()).unwrap();
        held.execute_batch("BEGIN").unwrap();
        let _: i64 = held
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        let started = Instant::now();
        for i in 0..5 {
            f.h.submit_blocking(remember(&format!("while a reader holds the log {i}")))
                .expect("a skipped restart is not a failed write");
        }
        // Five writes, each followed by at most one 100 ms attempt.
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "took {:?}",
            started.elapsed()
        );
        // The held reader still sees its own snapshot, undisturbed.
        let seen: i64 = held
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, 1);
        held.execute_batch("COMMIT").unwrap();
        // Once it lets go, the next write is followed by a restart that works.
        f.h.submit_blocking(remember("after the reader left"))
            .unwrap();
        f.h.submit_blocking(remember("and one more")).unwrap();
        let n: i64 = f.read("SELECT count(*) FROM memories", []);
        assert_eq!(n, 8);
    }

    // ---- `update` action move ----

    fn pref(key: &str, slug: &str) -> ProjectRef {
        ProjectRef {
            key: key.into(),
            slug: slug.into(),
            name: slug.into(),
            root: None,
            remote: None,
        }
    }

    fn remember_in(text: &str, slug: &str) -> WriteOp {
        WriteOp::Remember {
            input: NewMemoryInput {
                content: text.into(),
                ..Default::default()
            },
            project: ProjectChoice::Slug(slug.into()),
        }
    }

    fn mv(id: &str, project: Option<&str>) -> WriteOp {
        WriteOp::Update {
            id: id.into(),
            update: MemoryUpdate {
                action: UpdateAction::Move,
                project: project.map(String::from),
                ..Default::default()
            },
            actor: Actor::User,
        }
    }

    fn project_of(f: &Fixture, id: &str) -> String {
        f.read(
            "SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1",
            [id],
        )
    }

    fn placeholders(f: &Fixture, slug: &str) -> i64 {
        f.read(
            "SELECT count(*) FROM projects WHERE key = ?1 AND root_path IS NULL",
            [format!("import:{slug}")],
        )
    }

    fn with_project(f: &Fixture, key: &str, slug: &str) -> ResolvedProject {
        projects::upsert_project(&open(f.dir.path()), &pref(key, slug)).unwrap()
    }

    #[test]
    fn a_move_into_an_existing_project_follows_its_slug_and_its_aliases() {
        let f = fixture();
        let other = with_project(&f, "k-other", "other");
        projects::add_alias(&open(f.dir.path()), "second-name", &other.id).unwrap();
        let (a, _) = new_id(f.h.submit_blocking(remember("move me to other")).unwrap());
        let (b, _) = new_id(f.h.submit_blocking(remember("move me by alias")).unwrap());
        let WriteOutcome::Updated(m, Some(ev)) =
            f.h.submit_blocking(mv(&a, Some("Other"))).unwrap()
        else {
            panic!()
        };
        assert_eq!(m.project, "other");
        assert_eq!(project_of(&f, &a), "other");
        f.h.submit_blocking(mv(&b, Some("second-name"))).unwrap();
        assert_eq!(project_of(&f, &b), "other");
        let action: String = f.read("SELECT action FROM events WHERE id = ?1", [&ev]);
        assert_eq!(action, "memory.move");
        let created: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'project.create'",
            [],
        );
        assert_eq!(created, 0, "an existing project is not created");
        let projects_now: i64 = f.read("SELECT count(*) FROM projects", []);
        assert_eq!(projects_now, 2, "global and other");
    }

    #[test]
    fn a_move_into_an_unknown_slug_creates_one_placeholder_and_reuses_it() {
        let f = fixture();
        let (a, _) = new_id(f.h.submit_blocking(remember("first to move")).unwrap());
        let (b, _) = new_id(f.h.submit_blocking(remember("second to move")).unwrap());
        f.h.submit_blocking(mv(&a, Some("Fresh Start"))).unwrap();
        assert_eq!(project_of(&f, &a), "fresh-start");
        assert_eq!(placeholders(&f, "fresh-start"), 1);
        let name: String = f.read(
            "SELECT name FROM projects WHERE key = 'import:fresh-start'",
            [],
        );
        assert_eq!(name, "fresh-start");
        let remote: Option<String> = f.read(
            "SELECT git_remote FROM projects WHERE key = 'import:fresh-start'",
            [],
        );
        assert_eq!(remote, None);
        f.h.submit_blocking(mv(&b, Some("fresh-start"))).unwrap();
        assert_eq!(project_of(&f, &b), "fresh-start");
        assert_eq!(
            placeholders(&f, "fresh-start"),
            1,
            "the second move reuses it"
        );
        let created: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'project.create'",
            [],
        );
        assert_eq!(created, 1);
    }

    /// A move the daemon screened carries the text it screened: an edit that landed since
    /// refuses the move (nothing is created); a text the destination's rules cleaned is stored
    /// with the move, as an edit in its group, and undo of the move brings back both.
    #[test]
    fn a_screened_move_refuses_a_changed_memory_and_stores_the_cleaned_text_with_the_move() {
        let f = fixture();
        let (a, _) = new_id(f.h.submit_blocking(remember("see TCK-12345 here")).unwrap());
        let home = project_of(&f, &a);
        let hash: String = f.read("SELECT content_hash FROM memories WHERE id = ?1", [&a]);
        let at: String = f.read("SELECT updated_at FROM memories WHERE id = ?1", [&a]);
        let clean = "see [REDACTED:custom:ticket] here";
        let op = |hash: &str| WriteOp::Update {
            id: a.clone(),
            update: MemoryUpdate {
                action: UpdateAction::Move,
                project: Some("strict".into()),
                screened: Some(alice_core::store::memories::ScreenedMove {
                    content_hash: hash.into(),
                    updated_at: at.clone(),
                    title: clean.into(),
                    body: clean.into(),
                    topics: Vec::new(),
                }),
                ..Default::default()
            },
            actor: Actor::User,
        };
        let err = f.h.submit_blocking(op("stale")).unwrap_err().to_string();
        assert!(
            err.contains("was edited while its move was being checked"),
            "{err}"
        );
        assert_eq!(project_of(&f, &a), home);
        assert_eq!(
            placeholders(&f, "strict"),
            0,
            "a refused move makes nothing"
        );

        f.h.submit_blocking(op(&hash)).unwrap();
        assert_eq!(project_of(&f, &a), "strict");
        let (title, body): (String, String) = (
            f.read("SELECT title FROM memories WHERE id = ?1", [&a]),
            f.read("SELECT body FROM memories WHERE id = ?1", [&a]),
        );
        assert_eq!((title.as_str(), body.as_str()), (clean, clean));
        let actions: String = f.read(
            "SELECT group_concat(action, ',') FROM (SELECT action FROM events WHERE group_id = (SELECT group_id FROM events WHERE action = 'memory.move') ORDER BY rowid)",
            [],
        );
        assert_eq!(actions, "project.create,memory.move,memory.edit");
        undo(&f, &event_of(&f, "memory.move", &a)).unwrap();
        assert_eq!(project_of(&f, &a), home);
        let body: String = f.read("SELECT body FROM memories WHERE id = ?1", [&a]);
        assert_eq!(body, "see TCK-12345 here");
        assert_eq!(placeholders(&f, "strict"), 0);
    }

    /// Activation ends a memory's write path; it is not an edit. A move screened while the
    /// memory was still `pending` goes through when the judge's verdict (or, for an import,
    /// the embedder's) activated it in between: nothing it screened changed.
    #[test]
    fn a_move_screened_while_pending_goes_through_after_the_memory_is_activated() {
        let f = fixture();
        let (judged, judged_seq) =
            new_id(f.h.submit_blocking(remember("judged then moved")).unwrap());
        let (imported, imported_seq) = new_id(
            f.h.submit_blocking(remember("imported then moved"))
                .unwrap(),
        );
        let w = open(f.dir.path());
        w.execute(
            "UPDATE memories SET judge_state = 'skipped' WHERE seq = ?1",
            [imported_seq],
        )
        .unwrap();
        // Written a while ago (so the clock has moved on by the time it is activated).
        w.execute(
            "UPDATE memories SET updated_at = '2020-01-01T00:00:00.000Z' WHERE seq IN (?1, ?2)",
            [judged_seq, imported_seq],
        )
        .unwrap();
        drop(w);
        let screened = |id: &str| alice_core::store::memories::ScreenedMove {
            content_hash: hash_of(&f, id),
            updated_at: f.read("SELECT updated_at FROM memories WHERE id = ?1", [id]),
            title: f.read("SELECT title FROM memories WHERE id = ?1", [id]),
            body: f.read("SELECT body FROM memories WHERE id = ?1", [id]),
            topics: Vec::new(),
        };
        let (judged_screen, imported_screen) = (screened(&judged), screened(&imported));
        assert_eq!(status_of(&f, &judged), "pending");
        apply_j(&f, judgement(&f, &judged, "heuristic"));
        let stamp = hash_of(&f, &imported);
        f.h.submit_blocking(WriteOp::EmbedSkipped {
            keys: vec![(VecKey::Memory(imported_seq), stamp)],
        })
        .unwrap();
        for (id, s) in [(&judged, judged_screen), (&imported, imported_screen)] {
            assert_eq!(status_of(&f, id), "active", "activated in between");
            f.h.submit_blocking(WriteOp::Update {
                id: id.clone(),
                update: MemoryUpdate {
                    action: UpdateAction::Move,
                    project: Some("elsewhere".into()),
                    screened: Some(s),
                    ..Default::default()
                },
                actor: Actor::User,
            })
            .unwrap_or_else(|e| panic!("the move of {id} was refused: {e}"));
            assert_eq!(project_of(&f, id), "elsewhere");
        }
    }

    #[test]
    fn a_read_from_an_unstored_checkout_finds_the_placeholder_and_a_write_adopts_it() {
        let f = fixture();
        let (a, _) = new_id(
            f.h.submit_blocking(remember("lives in a placeholder"))
                .unwrap(),
        );
        f.h.submit_blocking(mv(&a, Some("newproj"))).unwrap();
        let placeholder: String = f.read("SELECT id FROM projects WHERE slug = 'newproj'", []);
        let checkout = pref("git:newproj-checkout", "newproj");
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        assert_eq!(
            projects::detected_for_read(&c, &checkout).unwrap(),
            projects::DetectedForRead::Placeholder(ResolvedProject {
                id: placeholder.clone(),
                slug: "newproj".into(),
                name: "newproj".into(),
            })
        );
        let (b, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "first real write".into(),
                    ..Default::default()
                },
                project: ProjectChoice::Detected(checkout),
            })
            .unwrap(),
        );
        let (id_b, key): (String, String) = (
            f.read("SELECT project_id FROM memories WHERE id = ?1", [&b]),
            f.read("SELECT key FROM projects WHERE id = ?1", [&placeholder]),
        );
        assert_eq!(id_b, placeholder, "the write adopted the placeholder");
        assert_eq!(key, "git:newproj-checkout");
        assert_eq!(project_of(&f, &a), "newproj");
    }

    #[test]
    fn undoing_a_move_restores_the_project_and_removes_the_empty_placeholder_it_made() {
        let f = fixture();
        let (a, _) = new_id(f.h.submit_blocking(remember("will come back")).unwrap());
        f.h.submit_blocking(mv(&a, Some("gone-again"))).unwrap();
        assert_eq!(placeholders(&f, "gone-again"), 1);
        let placeholder: String = f.read("SELECT id FROM projects WHERE slug = 'gone-again'", []);
        let WriteOutcome::Undo(UndoOutcome::Undone { reverted, .. }) =
            undo(&f, &event_of(&f, "memory.move", &a)).unwrap()
        else {
            panic!()
        };
        assert_eq!(reverted.len(), 2, "the move and the placeholder's creation");
        assert_eq!(project_of(&f, &a), "global");
        assert_eq!(placeholders(&f, "gone-again"), 0);
        // Undoing the undo puts both back.
        undo(&f, &event_of(&f, "undo:memory.move", &a)).unwrap();
        assert_eq!(project_of(&f, &a), "gone-again");
        let again: String = f.read("SELECT id FROM projects WHERE slug = 'gone-again'", []);
        assert_eq!(again, placeholder);
    }

    #[test]
    fn undoing_a_move_leaves_a_placeholder_that_has_since_received_other_memories() {
        let f = fixture();
        let (a, _) = new_id(
            f.h.submit_blocking(remember("made the placeholder"))
                .unwrap(),
        );
        let (b, _) = new_id(f.h.submit_blocking(remember("arrived later")).unwrap());
        f.h.submit_blocking(mv(&a, Some("busy"))).unwrap();
        f.h.submit_blocking(mv(&b, Some("busy"))).unwrap();
        undo(&f, &event_of(&f, "memory.move", &a)).unwrap();
        assert_eq!(project_of(&f, &a), "global");
        assert_eq!(project_of(&f, &b), "busy");
        assert_eq!(placeholders(&f, "busy"), 1, "it still holds a memory");
    }

    #[test]
    fn undoing_a_move_leaves_a_placeholder_that_a_checkout_adopted() {
        let f = fixture();
        let (a, _) = new_id(f.h.submit_blocking(remember("adopted soon")).unwrap());
        f.h.submit_blocking(mv(&a, Some("adoptee"))).unwrap();
        f.h.submit_blocking(WriteOp::Remember {
            input: NewMemoryInput {
                content: "a real write".into(),
                ..Default::default()
            },
            project: ProjectChoice::Detected(pref("git:adoptee", "adoptee")),
        })
        .unwrap();
        undo(&f, &event_of(&f, "memory.move", &a)).unwrap();
        assert_eq!(project_of(&f, &a), "global");
        let n: i64 = f.read("SELECT count(*) FROM projects WHERE slug = 'adoptee'", []);
        assert_eq!(n, 1);
    }

    #[test]
    fn a_move_that_would_duplicate_text_in_the_target_is_refused_and_creates_nothing() {
        let f = fixture();
        with_project(&f, "k-other", "other");
        let (a, _) = new_id(
            f.h.submit_blocking(remember("identical text here"))
                .unwrap(),
        );
        f.h.submit_blocking(remember_in("identical text here", "other"))
            .unwrap();
        let err = f.h.submit_blocking(mv(&a, Some("other"))).unwrap_err();
        assert!(err.to_string().contains("same text"), "{err}");
        assert_eq!(project_of(&f, &a), "global");
        let moves: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'memory.move'",
            [],
        );
        assert_eq!(moves, 0);
    }

    #[test]
    fn moves_into_and_out_of_global_follow_the_store_rules_only() {
        let f = fixture();
        with_project(&f, "k-other", "other");
        let (a, _) = new_id(
            f.h.submit_blocking(remember_in("project notes", "other"))
                .unwrap(),
        );
        f.h.submit_blocking(mv(&a, Some("global"))).unwrap();
        assert_eq!(project_of(&f, &a), "global");
        f.h.submit_blocking(mv(&a, Some("other"))).unwrap();
        assert_eq!(project_of(&f, &a), "other");
        assert_eq!(placeholders(&f, "global"), 0);
        // Into global where the same text already is: the store's same-text check.
        f.h.submit_blocking(remember("project notes")).unwrap();
        let err = f.h.submit_blocking(mv(&a, Some("global"))).unwrap_err();
        assert!(err.to_string().contains("same text"), "{err}");
    }

    #[test]
    fn a_move_to_where_the_memory_already_is_changes_and_records_nothing() {
        let f = fixture();
        let (a, _) = new_id(f.h.submit_blocking(remember("stay put")).unwrap());
        let WriteOutcome::Updated(_, event) = f.h.submit_blocking(mv(&a, Some("global"))).unwrap()
        else {
            panic!()
        };
        assert!(event.is_none());
        let n: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'memory.move'",
            [],
        );
        assert_eq!(n, 0);
    }

    #[test]
    fn a_move_names_an_unknown_memory_a_missing_project_and_a_bad_slug_in_one_sentence() {
        let f = fixture();
        let (a, _) = new_id(
            f.h.submit_blocking(remember("target of bad moves"))
                .unwrap(),
        );
        let cases = [
            (mv("01K7NOSUCHMEMORY0000000000", Some("x")), "memory"),
            (mv(&a, None), "needs project"),
            (mv(&a, Some("   ")), "empty"),
            (mv(&a, Some("a/b")), "not a project slug"),
            (mv(&a, Some("..\\x")), "not a project slug"),
        ];
        for (op, needle) in cases {
            let e = f.h.submit_blocking(op).unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
            assert!(!e.trim_end().contains('\n'), "one sentence: {e}");
        }
        let n: i64 = f.read("SELECT count(*) FROM projects", []);
        assert_eq!(n, 1, "no placeholder came of a refused move");
        assert_eq!(project_of(&f, &a), "global");
    }

    // ---- SP2 capture (Task 5) ----

    const CAPTURE_AGENT: &str = "claude-code";

    fn capture_uri(session: &str) -> String {
        format!("{CAPTURE_AGENT}:session:{session}")
    }

    /// A capture slice of `session` holding the chunks `ords` (authored one second apart), with
    /// no turn keys and no cursor.
    fn capture_slice(
        session: &str,
        project: ProjectChoice,
        ords: std::ops::Range<i64>,
    ) -> CaptureSlice {
        CaptureSlice {
            source: SourceInput {
                source_type: alice_core::model::SourceType::Session,
                uri: capture_uri(session),
                agent: Some(CAPTURE_AGENT.into()),
                session_id: Some(session.into()),
                ..Default::default()
            },
            project,
            init: CaptureInit {
                agent: CAPTURE_AGENT.into(),
                session_id: session.into(),
                parent_uri: None,
                transcript_path: Some(format!("/tmp/t/{session}.jsonl")),
                file_key: None,
                no_distill: false,
                backfill: false,
            },
            chunks: ords
                .map(|i| ChunkInput {
                    ord: i,
                    text: format!("captured turn text {session} {i}"),
                    authored_at: Some(format!("2026-10-16T10:00:{i:02}.000Z")),
                    ..Default::default()
                })
                .collect(),
            turn_keys: Vec::new(),
            skipped_keys: Vec::new(),
            cursor: None,
            last_event: Some("stop".into()),
            ended: false,
            record_event: true,
        }
    }

    fn global() -> ProjectChoice {
        ProjectChoice::Slug("global".into())
    }

    fn capture(s: &CaptureSlice) -> WriteOp {
        WriteOp::CaptureSlice(Box::new(s.clone()))
    }

    /// `(source_id, inserted, keys_new)` of a `Captured` outcome.
    fn captured_full(o: Result<WriteOutcome>) -> (String, usize, usize) {
        match o {
            Ok(WriteOutcome::Captured {
                source_id,
                inserted,
                keys_new,
            }) => (source_id, inserted, keys_new),
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    fn captured(o: Result<WriteOutcome>) -> (String, usize) {
        let (source_id, inserted, _) = captured_full(o);
        (source_id, inserted)
    }

    fn capture_cursor(c: &Connection, session: &str) -> (u64, i64) {
        let st = capture_store::by_uri(c, &capture_uri(session))
            .unwrap()
            .expect("the source has a capture state");
        (st.byte_offset, st.next_ord)
    }

    fn first_ord(f: &Fixture, key: &str) -> i64 {
        f.read(
            "SELECT first_ord FROM capture_turns WHERE turn_key = ?1",
            [key],
        )
    }

    fn turn_keys(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    fn known(c: &Connection, k: &[&str]) -> std::collections::HashSet<String> {
        capture_store::known_turns(c, CAPTURE_AGENT, &turn_keys(k)).unwrap()
    }

    /// SP2 spec §3.8: one slice writes its chunks, its turn keys and the cursor in one
    /// transaction. Repeating it stores nothing and leaves the cursor where it was; a slice that
    /// is refused writes nothing at all.
    #[test]
    fn capture_slice_writes_chunks_keys_and_cursor_atomically() {
        let f = fixture();
        let mut s = capture_slice("S1", global(), 0..3);
        s.turn_keys = vec![("u-1".into(), 0), ("u-2".into(), 2)];
        s.cursor = Some((4096, 3));
        let (source_id, inserted, keys_new) = captured_full(f.h.submit_blocking(capture(&s)));
        assert_eq!((inserted, keys_new), (3, 2));
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        assert_eq!(
            known(&c, &["u-1", "u-2", "u-3"]),
            known_set(&["u-1", "u-2"])
        );
        assert!(
            capture_store::known_turns(&c, "codex", &turn_keys(&["u-1"]))
                .unwrap()
                .is_empty(),
            "turn keys are per agent"
        );
        let st = capture_store::by_uri(&c, &capture_uri("S1"))
            .unwrap()
            .expect("the source has a capture state");
        assert_eq!((st.byte_offset, st.next_ord), (4096, 3));
        assert_eq!(st.source_id, source_id);
        assert_eq!(
            (st.agent.as_str(), st.session_id.as_str()),
            (CAPTURE_AGENT, "S1")
        );
        assert_eq!(st.transcript_path.as_deref(), Some("/tmp/t/S1.jsonl"));
        assert_eq!(st.last_event.as_deref(), Some("stop"));
        assert!(st.last_capture_at.is_some());
        assert_eq!(st.distill_state, DistillState::Idle);
        assert!(!st.ended && !st.no_distill);
        assert_eq!(st.project_id, projects::GLOBAL_ID);
        let first_ord: i64 = f.read(
            "SELECT first_ord FROM capture_turns WHERE turn_key = 'u-2'",
            [],
        );
        assert_eq!(first_ord, 2);
        let started: String = f.read("SELECT started_at FROM sources WHERE id = ?1", [&source_id]);
        let ended: String = f.read("SELECT ended_at FROM sources WHERE id = ?1", [&source_id]);
        assert_eq!(
            (started.as_str(), ended.as_str()),
            ("2026-10-16T10:00:00.000Z", "2026-10-16T10:00:02.000Z"),
            "the source spans its first and newest captured record"
        );
        let events = || -> i64 {
            f.read(
                "SELECT count(*) FROM events WHERE action = 'source.capture' AND target_id = ?1",
                [&source_id],
            )
        };
        assert_eq!(events(), 1);
        let actor: String = f.read(
            "SELECT actor FROM events WHERE action = 'source.capture'",
            [],
        );
        assert_eq!(actor, "agent:claude-code");

        // The same slice again: a hook that fired twice, or a pass repeated after a crash.
        let (again_id, again, keys_again) = captured_full(f.h.submit_blocking(capture(&s)));
        assert_eq!(
            (again_id.as_str(), again, keys_again),
            (source_id.as_str(), 0, 0)
        );
        let st = capture_store::by_uri(&c, &capture_uri("S1"))
            .unwrap()
            .unwrap();
        assert_eq!((st.byte_offset, st.next_ord), (4096, 3));
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 3);
        assert_eq!(events(), 1, "a slice that stored nothing records no event");

        // A refused slice (an empty turn key after its chunks) writes nothing.
        let mut bad = capture_slice("S2", global(), 0..2);
        bad.turn_keys = vec![("u-9".into(), 0), (String::new(), 1)];
        bad.cursor = Some((100, 2));
        let err = f.h.submit_blocking(capture(&bad)).unwrap_err().to_string();
        assert!(err.contains("turn key"), "{err}");
        assert!(
            capture_store::by_uri(&c, &capture_uri("S2"))
                .unwrap()
                .is_none()
        );
        let sources: i64 = f.read(
            "SELECT count(*) FROM sources WHERE uri = ?1",
            [capture_uri("S2")],
        );
        assert_eq!(sources, 0);
        assert!(known(&c, &["u-9"]).is_empty());
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 3);
    }

    fn known_set(k: &[&str]) -> std::collections::HashSet<String> {
        turn_keys(k).into_iter().collect()
    }

    /// SP2 spec §3.1: the source owns its project. A later slice that names another project
    /// stores its chunks under the first one, and the project it named is not created.
    #[test]
    fn a_capture_source_keeps_its_first_project() {
        let f = fixture();
        let detected = |slug: &str| {
            ProjectChoice::Detected(alice_core::project::ProjectRef {
                key: format!("path:/work/{slug}"),
                slug: slug.into(),
                name: slug.into(),
                root: None,
                remote: None,
            })
        };
        let (source_id, first) =
            captured(f.h.submit_blocking(capture(&capture_slice("S1", detected("alpha"), 0..2))));
        let (again, second) =
            captured(f.h.submit_blocking(capture(&capture_slice("S1", detected("beta"), 2..4))));
        assert_eq!((first, second), (2, 2));
        assert_eq!(again, source_id);
        let alpha: String = f.read("SELECT id FROM projects WHERE slug = 'alpha'", []);
        let owner: String = f.read("SELECT project_id FROM sources WHERE id = ?1", [&source_id]);
        assert_eq!(owner, alpha);
        let theirs: i64 = f.read(
            "SELECT count(*) FROM chunks WHERE source_id = ?1 AND project_id = ?2",
            [&source_id, &alpha],
        );
        assert_eq!(theirs, 4, "the second slice's chunks follow the source");
        let beta: i64 = f.read("SELECT count(*) FROM projects WHERE slug = 'beta'", []);
        assert_eq!(beta, 0, "the project the later slice named is not created");
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        let st = capture_store::by_source(&c, &source_id).unwrap().unwrap();
        assert_eq!(st.project_id, alpha);
    }

    /// SP2 spec §3.6: an excluded project's turns store nothing, but their keys are recorded and
    /// the cursor moves past them. A skipped key is recorded at the cursor's next ord, or without
    /// a cursor at the stored one.
    #[test]
    fn skipped_keys_are_recorded_without_chunks() {
        let f = fixture();
        let mut s = capture_slice("S1", global(), 0..0);
        s.skipped_keys = turn_keys(&["x-1", "x-2"]);
        s.cursor = Some((2048, 7));
        let (source_id, inserted, keys_new) = captured_full(f.h.submit_blocking(capture(&s)));
        assert_eq!((inserted, keys_new), (0, 2));
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        assert_eq!(known(&c, &["x-1", "x-2"]), known_set(&["x-1", "x-2"]));
        let rows: i64 = f.read(
            "SELECT count(*) FROM capture_turns WHERE source_id = ?1 AND first_ord = 7",
            [&source_id],
        );
        assert_eq!(rows, 2, "recorded at the cursor's next ord");
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 0);
        assert_eq!(capture_cursor(&c, "S1"), (2048, 7));
        // No cursor: the stored next ord.
        let mut s = capture_slice("S1", global(), 0..0);
        s.skipped_keys = turn_keys(&["x-3"]);
        captured(f.h.submit_blocking(capture(&s)));
        assert_eq!(first_ord(&f, "x-3"), 7);
        assert_eq!(capture_cursor(&c, "S1"), (2048, 7));
        let events: i64 = f.read(
            "SELECT count(*) FROM events WHERE action = 'source.capture'",
            [],
        );
        assert_eq!(events, 0);
    }

    /// A turn that rendered no chunk still records its key on the `turn_keys` path, at the ord
    /// the next chunk would get, beside a turn with chunks and alone.
    #[test]
    fn a_turn_key_without_chunks_is_recorded() {
        let f = fixture();
        let mut s = capture_slice("S1", global(), 0..2);
        s.turn_keys = vec![("t-1".into(), 0), ("t-empty".into(), 2)];
        s.cursor = Some((300, 2));
        let (_, inserted, keys_new) = captured_full(f.h.submit_blocking(capture(&s)));
        assert_eq!((inserted, keys_new), (2, 2));
        let mut solo = capture_slice("S1", global(), 0..0);
        solo.turn_keys = vec![("t-solo".into(), 2)];
        solo.cursor = Some((420, 2));
        let (_, inserted, keys_new) = captured_full(f.h.submit_blocking(capture(&solo)));
        assert_eq!((inserted, keys_new), (0, 1));
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        assert_eq!(
            known(&c, &["t-1", "t-empty", "t-solo"]),
            known_set(&["t-1", "t-empty", "t-solo"])
        );
        assert_eq!(
            (
                first_ord(&f, "t-1"),
                first_ord(&f, "t-empty"),
                first_ord(&f, "t-solo")
            ),
            (0, 2, 2)
        );
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 2);
        assert_eq!(capture_cursor(&c, "S1"), (420, 2));
    }

    /// Review fix: a stale slice (an older pass's, committed late) never moves `next_ord` back,
    /// so a later pass cannot reuse stored ords and lose its turns to `INSERT OR IGNORE`.
    #[test]
    fn a_stale_slice_never_moves_next_ord_back() {
        let f = fixture();
        let mut s = capture_slice("S1", global(), 0..3);
        s.cursor = Some((4096, 3));
        captured(f.h.submit_blocking(capture(&s)));
        let mut stale = capture_slice("S1", global(), 0..1);
        stale.cursor = Some((1024, 1));
        captured(f.h.submit_blocking(capture(&stale)));
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        assert_eq!(capture_cursor(&c, "S1").1, 3);
    }

    /// A slice over the writer's slice limits is a caller bug: refused whole, naming the limits,
    /// even when a fresh commit would have room for it.
    #[test]
    fn an_oversized_capture_slice_is_refused() {
        let f = fixture();
        let many = capture_slice("S1", global(), 0..51);
        let mut wide = capture_slice("S2", global(), 0..2);
        for c in &mut wide.chunks {
            c.text = "w".repeat(20_000);
        }
        for (s, session) in [(many, "S1"), (wide, "S2")] {
            let err = f.h.submit_blocking(capture(&s)).unwrap_err();
            assert!(matches!(err, AliceError::Invalid(_)), "{err:?}");
            let msg = err.to_string();
            assert!(
                msg.contains("50 chunks") && msg.contains("32768 bytes"),
                "{msg}"
            );
            let sources: i64 = f.read(
                "SELECT count(*) FROM sources WHERE uri = ?1",
                [capture_uri(session)],
            );
            assert_eq!(sources, 0);
        }
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 0);
    }

    /// Atomicity, proved: a slice that fails after its chunks and keys are written (a trigger
    /// refuses the cursor update) leaves no chunk, no key and no cursor change behind, and the
    /// same slice goes through once the trigger is gone.
    #[test]
    fn a_capture_slice_that_fails_mid_apply_leaves_nothing() {
        let f = fixture();
        let mut first = capture_slice("S1", global(), 0..2);
        first.turn_keys = vec![("a-1".into(), 0)];
        first.cursor = Some((100, 2));
        captured(f.h.submit_blocking(capture(&first)));
        let side = rusqlite::Connection::open(f.db()).unwrap();
        side.execute_batch(
            "CREATE TRIGGER refuse_cursor BEFORE UPDATE ON capture_state
             BEGIN SELECT RAISE(ABORT, 'cursor refused by the test'); END;",
        )
        .unwrap();
        let mut next = capture_slice("S1", global(), 2..5);
        next.turn_keys = vec![("a-2".into(), 2), ("a-3".into(), 4)];
        next.cursor = Some((900, 5));
        let err = f.h.submit_blocking(capture(&next)).unwrap_err().to_string();
        assert!(err.contains("cursor refused"), "{err}");
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        let chunks: i64 = f.read("SELECT count(*) FROM chunks", []);
        assert_eq!(chunks, 2, "the failed slice's chunks are rolled back");
        assert!(known(&c, &["a-2", "a-3"]).is_empty(), "and its keys");
        assert_eq!(capture_cursor(&c, "S1"), (100, 2), "and the cursor stays");
        side.execute_batch("DROP TRIGGER refuse_cursor").unwrap();
        let (_, inserted, keys_new) = captured_full(f.h.submit_blocking(capture(&next)));
        assert_eq!((inserted, keys_new), (3, 2));
        assert_eq!(capture_cursor(&c, "S1"), (900, 5));
    }

    /// The source's agent and session come from `init`, and its time span from the captured
    /// records: `SourceInput`'s own `ended_at` is ignored and the end never moves back.
    #[test]
    fn a_capture_source_takes_its_identity_from_init_and_its_span_from_records() {
        let f = fixture();
        let mut s = capture_slice("S1", global(), 3..5);
        s.source.agent = Some("someone-else".into());
        s.source.session_id = Some("not-the-session".into());
        s.source.ended_at = Some("2030-01-01T00:00:00.000Z".into());
        let (source_id, _) = captured(f.h.submit_blocking(capture(&s)));
        let col = |name: &str| -> Option<String> {
            f.read(
                &format!("SELECT {name} FROM sources WHERE id = ?1"),
                [&source_id],
            )
        };
        assert_eq!(col("agent").as_deref(), Some(CAPTURE_AGENT));
        assert_eq!(col("session_id").as_deref(), Some("S1"));
        assert_eq!(col("ended_at").as_deref(), Some("2026-10-16T10:00:04.000Z"));
        assert_eq!(
            col("started_at").as_deref(),
            Some("2026-10-16T10:00:03.000Z")
        );
        // A later slice of older records, whose input claims an earlier span, changes neither:
        // the end only moves forward and the start is set once.
        let mut older = capture_slice("S1", global(), 0..2);
        older.source.ended_at = Some("2020-01-01T00:00:00.000Z".into());
        older.source.started_at = Some("2020-01-01T00:00:00.000Z".into());
        captured(f.h.submit_blocking(capture(&older)));
        assert_eq!(col("ended_at").as_deref(), Some("2026-10-16T10:00:04.000Z"));
        assert_eq!(
            col("started_at").as_deref(),
            Some("2026-10-16T10:00:03.000Z")
        );
        // A new source takes `started_at` from its input when it has one.
        let mut given = capture_slice("S2", global(), 0..1);
        given.source.started_at = Some("2026-10-16T09:00:00.000Z".into());
        let (s2, _) = captured(f.h.submit_blocking(capture(&given)));
        let started: String = f.read("SELECT started_at FROM sources WHERE id = ?1", [&s2]);
        assert_eq!(started, "2026-10-16T09:00:00.000Z");
    }

    /// SP2 spec §6.5: a distilled memory carries its provenance (inserted with it) and the actor
    /// `distill:jev`. A re-run returns the first memory by its idempotency key and adds no
    /// provenance, even when it names one more chunk.
    #[test]
    fn a_distilled_remember_records_provenance_and_its_actor() {
        let f = fixture();
        let (source_id, _) =
            captured(f.h.submit_blocking(capture(&capture_slice("S1", global(), 0..3))));
        let chunk = |ord: i64| -> String {
            f.read(
                "SELECT id FROM chunks WHERE source_id = ?1 AND ord = ?2",
                rusqlite::params![source_id, ord],
            )
        };
        let (c1, c2, c3) = (chunk(0), chunk(1), chunk(2));
        let distilled = |provenance: Vec<String>| WriteOp::Remember {
            input: NewMemoryInput {
                content: "We pin the toolchain to Rust 1.99 so CI and laptops agree".into(),
                kind: Some(Kind::Decision),
                idempotency_key: Some("distill:0123456789abcdef0123456789abcdef".into()),
                author: alice_core::model::Author {
                    agent: Some(CAPTURE_AGENT.into()),
                    session: Some("S1".into()),
                    model: None,
                },
                origin: Origin::Distill,
                provenance,
                ..Default::default()
            },
            project: global(),
        };
        let (id, _) = new_id(
            f.h.submit_blocking(distilled(vec![c1.clone(), c2.clone()]))
                .unwrap(),
        );
        let rows = || -> i64 { f.read("SELECT count(*) FROM provenance", []) };
        assert_eq!(rows(), 2);
        let mine: i64 = f.read(
            "SELECT count(*) FROM provenance WHERE memory_id = ?1 AND chunk_id IN (?2, ?3)",
            [&id, &c1, &c2],
        );
        assert_eq!(mine, 2);
        let actor: String = f.read(
            "SELECT actor FROM events WHERE action = 'memory.create' AND target_id = ?1",
            [&id],
        );
        assert_eq!(actor, "distill:jev");
        let origin: String = f.read("SELECT origin FROM memories WHERE id = ?1", [&id]);
        assert_eq!(origin, "distill");
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        let m = memories::get_many(&c, std::slice::from_ref(&id)).unwrap();
        assert_eq!(m[0].origin, Origin::Distill);

        let again = f.h.submit_blocking(distilled(vec![c1, c2, c3])).unwrap();
        assert!(
            matches!(&again, WriteOutcome::Remembered(InsertOutcome::Duplicate { id: d }, _) if *d == id),
            "{again:?}"
        );
        assert_eq!(rows(), 2, "a duplicate inserts no provenance");

        // An agent's memory keeps the agent's actor and origin.
        let (plain, _) = new_id(f.h.submit_blocking(remember("an ordinary memory")).unwrap());
        let origin: String = f.read("SELECT origin FROM memories WHERE id = ?1", [&plain]);
        assert_eq!(origin, "agent");
        let actor: String = f.read(
            "SELECT actor FROM events WHERE action = 'memory.create' AND target_id = ?1",
            [&plain],
        );
        assert_eq!(actor, "user");
    }

    /// A distilled memory of session `session`, written as the distill worker writes it.
    fn distilled_op(text: &str, key: &str, kind: Kind, p: f64, session: &str) -> WriteOp {
        WriteOp::Distilled {
            input: NewMemoryInput {
                content: text.into(),
                kind: Some(kind),
                idempotency_key: Some(key.into()),
                author: alice_core::model::Author {
                    agent: Some(CAPTURE_AGENT.into()),
                    session: Some(session.into()),
                    model: None,
                },
                origin: Origin::Distill,
                ..Default::default()
            },
            project: global(),
            p,
            subagent: false,
        }
    }

    /// The judge's verdict on memory `id`, applied as the judge worker applies it, with the
    /// distillation settings at `max_per_session = n`.
    fn verdict(f: &Fixture, id: &str, actions: Vec<JudgeAction>, retry: bool, n: usize) {
        let seq: i64 = f.read("SELECT seq FROM memories WHERE id = ?1", [id]);
        let stamp: String = f.read("SELECT content_hash FROM memories WHERE id = ?1", [id]);
        let updated_at: String = f.read("SELECT updated_at FROM memories WHERE id = ?1", [id]);
        // The judge read each memory it acts on as it is now.
        let seen = actions
            .iter()
            .map(|a| {
                let other = match a {
                    JudgeAction::DuplicateOf { survivor, .. } => survivor,
                    JudgeAction::Supersedes { old, .. } => old,
                    JudgeAction::Contradicts { other, .. }
                    | JudgeAction::RelatesTo { other, .. } => other,
                    JudgeAction::ReplaceDistilled { distilled, .. } => distilled,
                };
                let hash: String =
                    f.read("SELECT content_hash FROM memories WHERE id = ?1", [other]);
                (other.clone(), hash)
            })
            .collect();
        f.h.submit_blocking(WriteOp::ApplyJudgement(Judgement {
            seq,
            id: id.into(),
            stamp,
            updated_at,
            provider: "jev".into(),
            actions,
            retry,
            seen,
            distill: Some(cap_of(n)),
            ..Default::default()
        }))
        .unwrap();
    }

    /// The distillation settings with a best set of `n`.
    fn cap_of(n: usize) -> alice_core::settings::DistillSettings {
        alice_core::settings::DistillSettings {
            max_per_session: n,
            ..Default::default()
        }
    }

    /// A distilled memory of session S1, judged live under a set large enough that nothing goes:
    /// a member of its best set.
    fn member(f: &Fixture, text: &str, key: &str, kind: Kind, p: f64) -> String {
        let (id, _) = new_id(
            f.h.submit_blocking(distilled_op(text, key, kind, p, "S1"))
                .unwrap(),
        );
        verdict(f, &id, Vec::new(), false, 200);
        id
    }

    fn cap_forgets(f: &Fixture) -> i64 {
        f.read(
            "SELECT count(*) FROM events WHERE action = 'memory.forget' AND actor = 'distill:cap'",
            [],
        )
    }

    /// Task 12c (spec §6.4) and its review's I1, option b: a distilled memory is written with its
    /// rank inputs and evicts nothing. Once the judge finds it live in a full best set (three
    /// here), the weakest member that may go is trashed in the judgement's op: a `memory.forget`
    /// by `distill:cap`, undoable like any trash. Never a handoff, nor a member an agent edited.
    #[test]
    fn a_newcomer_judged_live_into_a_full_set_evicts_the_weakest_that_may_go() {
        let f = fixture();
        let handoff = member(
            &f,
            "Where things stand: the cache is set up, next the restart",
            "distill:h",
            Kind::Handoff,
            0.71,
        );
        let edited = member(
            &f,
            "The cache keeps its data in memory only",
            "distill:g",
            Kind::Fact,
            0.70,
        );
        let weak = member(
            &f,
            "The queue drains every five minutes",
            "distill:x",
            Kind::Fact,
            0.72,
        );
        f.h.submit_blocking(WriteOp::Update {
            id: edited.clone(),
            update: MemoryUpdate {
                action: UpdateAction::Edit,
                title: Some("The cache is in memory".into()),
                ..Default::default()
            },
            actor: Actor::Agent(CAPTURE_AGENT.into()),
        })
        .unwrap();
        let (new, _) = new_id(
            f.h.submit_blocking(distilled_op(
                "The cache service listens on its own port",
                "distill:new",
                Kind::Fact,
                0.95,
                "S1",
            ))
            .unwrap(),
        );
        let (p, tier): (f64, String) = (
            f.read("SELECT distill_p FROM memories WHERE id = ?1", [&new]),
            f.read("SELECT distill_tier FROM memories WHERE id = ?1", [&new]),
        );
        assert_eq!((p, tier.as_str()), (0.95, "main"));
        let status =
            |id: &str| -> String { f.read("SELECT status FROM memories WHERE id = ?1", [id]) };
        assert_eq!(cap_forgets(&f), 0, "written, nothing evicted");
        verdict(&f, &new, Vec::new(), false, 3);
        assert_eq!(status(&weak), "trashed");
        for id in [&handoff, &edited, &new] {
            assert_ne!(status(id), "trashed");
        }
        let event: String = f.read(
            "SELECT id FROM events WHERE action = 'memory.forget' AND actor = 'distill:cap' AND target_id = ?1",
            [&weak],
        );
        assert_eq!(cap_forgets(&f), 1);
        // The trash is undone like any other.
        f.h.submit_blocking(WriteOp::Undo {
            event_id: event,
            actor: Actor::User,
        })
        .unwrap();
        assert_ne!(status(&weak), "trashed");
    }

    /// Review I1, option b: a newcomer the judge trashes as a duplicate evicts nothing, so the
    /// full set's unique weakest member stays.
    #[test]
    fn a_newcomer_the_judge_trashes_as_a_duplicate_evicts_nothing() {
        let f = fixture();
        let weak = member(
            &f,
            "The queue drains every five minutes",
            "distill:x",
            Kind::Fact,
            0.72,
        );
        member(
            &f,
            "The cache keeps its data in memory only",
            "distill:y",
            Kind::Fact,
            0.9,
        );
        member(
            &f,
            "Invoices go out on Fridays",
            "distill:z",
            Kind::Fact,
            0.85,
        );
        let (saved, _) = new_id(
            f.h.submit_blocking(WriteOp::Remember {
                input: NewMemoryInput {
                    content: "The berth tracker cache is warmed by the night job".into(),
                    kind: Some(Kind::Fact),
                    origin: Origin::Agent,
                    ..Default::default()
                },
                project: global(),
            })
            .unwrap(),
        );
        let (dup, _) = new_id(
            f.h.submit_blocking(distilled_op(
                "The berth tracker cache is warmed by the night job, as said",
                "distill:dup",
                Kind::Fact,
                0.95,
                "S1",
            ))
            .unwrap(),
        );
        verdict(
            &f,
            &dup,
            vec![JudgeAction::DuplicateOf {
                survivor: saved,
                p: 0.95,
            }],
            false,
            3,
        );
        let status =
            |id: &str| -> String { f.read("SELECT status FROM memories WHERE id = ?1", [id]) };
        assert_eq!(status(&dup), "trashed");
        assert_ne!(status(&weak), "trashed", "the unique member stays");
        assert_eq!(cap_forgets(&f), 0);
    }

    /// Review I1, option b: a newcomer that supersedes a member of its own session's set took that
    /// member's slot and evicts nothing, even over a set already past its size (three of four
    /// here, as after a lowered `max_per_session`). One that supersedes another session's memory
    /// takes a new slot: the weakest member that may go leaves.
    #[test]
    fn a_newcomer_that_supersedes_a_member_evicts_nothing() {
        let f = fixture();
        let weak = member(
            &f,
            "The queue drains every five minutes",
            "distill:x",
            Kind::Fact,
            0.72,
        );
        let old = member(
            &f,
            "The berth planner reads the tide table",
            "distill:y",
            Kind::Fact,
            0.9,
        );
        member(
            &f,
            "Invoices go out on Fridays",
            "distill:z",
            Kind::Fact,
            0.85,
        );
        member(
            &f,
            "The north crane is the fastest",
            "distill:w",
            Kind::Fact,
            0.8,
        );
        let (sup, _) = new_id(
            f.h.submit_blocking(distilled_op(
                "The berth planner reads the tide table before each shift",
                "distill:sup",
                Kind::Fact,
                0.95,
                "S1",
            ))
            .unwrap(),
        );
        verdict(
            &f,
            &sup,
            vec![JudgeAction::Supersedes {
                old: old.clone(),
                p: 0.95,
            }],
            false,
            3,
        );
        let status =
            |id: &str| -> String { f.read("SELECT status FROM memories WHERE id = ?1", [id]) };
        assert_eq!(status(&old), "superseded");
        assert_ne!(status(&sup), "trashed");
        assert_ne!(status(&weak), "trashed");
        assert_eq!(cap_forgets(&f), 0);
        // Another session's memory superseded: the newcomer takes a slot of its own.
        let (theirs, _) = new_id(
            f.h.submit_blocking(distilled_op(
                "The gate opens at dawn",
                "distill:o",
                Kind::Fact,
                0.9,
                "S2",
            ))
            .unwrap(),
        );
        verdict(&f, &theirs, Vec::new(), false, 200);
        let (next, _) = new_id(
            f.h.submit_blocking(distilled_op(
                "The gate opens at dawn and closes at dusk",
                "distill:next",
                Kind::Fact,
                0.96,
                "S1",
            ))
            .unwrap(),
        );
        verdict(
            &f,
            &next,
            vec![JudgeAction::Supersedes {
                old: theirs,
                p: 0.95,
            }],
            false,
            3,
        );
        assert_eq!(status(&weak), "trashed");
        assert_eq!(cap_forgets(&f), 1);
    }

    /// Task 12c review 2, N1: a distilled newcomer the judge gives up on in bulk
    /// (`RequeueJudgeRetries`, after `max_attempts` counted failures) settles live, so it evicts
    /// as a judged one does, in the same op: its session's set keeps its size.
    #[test]
    fn a_distilled_newcomer_the_judge_gives_up_on_evicts_too() {
        let f = fixture();
        let weak = member(
            &f,
            "The queue drains every five minutes",
            "distill:x",
            Kind::Fact,
            0.72,
        );
        member(
            &f,
            "The cache keeps its data in memory only",
            "distill:y",
            Kind::Fact,
            0.9,
        );
        member(
            &f,
            "Invoices go out on Fridays",
            "distill:z",
            Kind::Fact,
            0.85,
        );
        let (new, _) = new_id(
            f.h.submit_blocking(distilled_op(
                "The cache service listens on its own port",
                "distill:new",
                Kind::Fact,
                0.95,
                "S1",
            ))
            .unwrap(),
        );
        verdict(&f, &new, Vec::new(), true, 3);
        assert_eq!(cap_forgets(&f), 0, "retry");
        f.h.submit_blocking(WriteOp::RequeueJudgeRetries {
            max_attempts: 1,
            now: "2099-01-01T00:00:00.000Z".into(),
            distill: Some(cap_of(3)),
        })
        .unwrap();
        let (state, status): (String, String) = (
            f.read("SELECT judge_state FROM memories WHERE id = ?1", [&new]),
            f.read("SELECT status FROM memories WHERE id = ?1", [&new]),
        );
        assert_eq!((state.as_str(), status.as_str()), ("done", "active"));
        let weak_status: String = f.read("SELECT status FROM memories WHERE id = ?1", [&weak]);
        assert_eq!(weak_status, "trashed");
        assert_eq!(cap_forgets(&f), 1);
    }

    /// Task 12c review 2, N2: a judgement of a distilled newcomer that does not carry the
    /// distillation settings would skip eviction in silence; a debug build stops on it (a
    /// release build warns). Applied on this thread: the writer's own would die with it.
    #[test]
    #[should_panic(expected = "carries the distillation settings")]
    fn a_judgement_of_a_distilled_newcomer_needs_the_distillation_settings() {
        let d = tempfile::tempdir().unwrap();
        let conn = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
        let mut st = State {
            hlc: Hlc::new("test"),
            matcher: None,
            inserted: false,
            reembed_target: None,
            chunk_budget: INGEST_SLICE,
            byte_budget: INGEST_SLICE_BYTES,
            stream: Vec::new(),
            redactors: Redactors::fixed(alice_core::settings::PrivacySettings::default()),
            applied: Vec::new(),
            commit_log: None,
        };
        let WriteOp::Distilled {
            input,
            project,
            p,
            subagent,
        } = distilled_op(
            "The queue drains every five minutes",
            "distill:x",
            Kind::Fact,
            0.9,
            "S1",
        )
        else {
            unreachable!()
        };
        let (id, seq) = new_id(distilled(&conn, input, &project, p, subagent, &mut st).unwrap());
        let (stamp, updated_at): (String, String) = conn
            .query_row(
                "SELECT content_hash, updated_at FROM memories WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        apply_judgement(
            &conn,
            &mut st,
            Judgement {
                seq,
                id,
                stamp,
                updated_at,
                provider: "jev".into(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    /// Review I1, option b: a newcomer waiting for the judge (`pending`, or `retry` after an
    /// outage) evicts nothing yet, and does not count toward the set.
    #[test]
    fn a_newcomer_waiting_for_the_judge_evicts_nothing_yet() {
        let f = fixture();
        let weak = member(
            &f,
            "The queue drains every five minutes",
            "distill:x",
            Kind::Fact,
            0.72,
        );
        member(
            &f,
            "The cache keeps its data in memory only",
            "distill:y",
            Kind::Fact,
            0.9,
        );
        member(
            &f,
            "Invoices go out on Fridays",
            "distill:z",
            Kind::Fact,
            0.85,
        );
        let (new, _) = new_id(
            f.h.submit_blocking(distilled_op(
                "The cache service listens on its own port",
                "distill:new",
                Kind::Fact,
                0.95,
                "S1",
            ))
            .unwrap(),
        );
        assert_eq!(cap_forgets(&f), 0, "pending");
        verdict(&f, &new, Vec::new(), true, 3);
        let state: String = f.read("SELECT judge_state FROM memories WHERE id = ?1", [&new]);
        assert_eq!(state, "retry");
        assert_eq!(cap_forgets(&f), 0, "retry");
        let status: String = f.read("SELECT status FROM memories WHERE id = ?1", [&weak]);
        assert_ne!(status, "trashed");
    }

    /// SP2 spec §3.9 and R11: retention that deletes a capture source's last chunk keeps the
    /// source with its capture state (the cursor) and its turn keys, so the file is not captured
    /// again and the importer still skips the session.
    #[test]
    fn deleting_a_sources_chunks_keeps_its_state_and_turn_keys() {
        let f = fixture();
        let mut s = capture_slice("S1", global(), 0..2);
        s.turn_keys = vec![("u-1".into(), 0), ("u-2".into(), 1)];
        s.cursor = Some((900, 2));
        let (source_id, _) = captured(f.h.submit_blocking(capture(&s)));
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        let seqs: Vec<i64> = c
            .prepare("SELECT seq FROM chunks WHERE source_id = ?1 ORDER BY seq")
            .unwrap()
            .query_map([&source_id], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(seqs.len(), 2);
        let WriteOutcome::Purged(done) =
            f.h.submit_blocking(WriteOp::DeleteChunks { seqs: seqs.clone() })
                .unwrap()
        else {
            panic!("expected Purged")
        };
        assert_eq!(done, seqs);
        assert!(
            capture_store::by_uri(&c, &capture_uri("S1"))
                .unwrap()
                .is_some()
        );
        let states: i64 = f.read(
            "SELECT count(*) FROM capture_state WHERE source_id = ?1",
            [&source_id],
        );
        assert_eq!(states, 1, "the state stays with its emptied source");
        let sources: i64 = f.read("SELECT count(*) FROM sources WHERE id = ?1", [&source_id]);
        assert_eq!(sources, 1);
        assert_eq!(known(&c, &["u-1", "u-2"]), known_set(&["u-1", "u-2"]));
    }

    /// `CaptureStateUpdate`: each change sets its own fields; `due` lists a due source once its
    /// next try has come; an unknown source changes nothing.
    #[test]
    fn capture_state_updates_change_their_fields_and_due_follows_them() {
        let f = fixture();
        let mut s = capture_slice("S1", global(), 0..2);
        s.cursor = Some((500, 2));
        let (source_id, _) = captured(f.h.submit_blocking(capture(&s)));
        let update = |id: &str, change: CaptureChange| -> usize {
            match f.h.submit_blocking(WriteOp::CaptureStateUpdate {
                source_id: id.into(),
                change,
            }) {
                Ok(WriteOutcome::Count(n)) => n,
                other => panic!("expected Count, got {other:?}"),
            }
        };
        let c = alice_core::store::open_reader(&f.db()).unwrap();
        let state = || capture_store::by_source(&c, &source_id).unwrap().unwrap();

        let at = "2026-10-16T12:00:00.000Z";
        let changed = update(
            &source_id,
            CaptureChange::Distill {
                state: DistillState::Due,
                next_try_at: Some(at.into()),
                attempts: 2,
            },
        );
        assert_eq!(changed, 1);
        let st = state();
        assert_eq!(
            (
                st.distill_state,
                st.distill_next_try_at.as_deref(),
                st.distill_attempts
            ),
            (DistillState::Due, Some(at), 2)
        );
        assert!(
            capture_store::due(&c, "2026-10-16T11:59:59.999Z", 10)
                .unwrap()
                .is_empty(),
            "not before its next try"
        );
        let due = capture_store::due(&c, at, 10).unwrap();
        assert_eq!(
            due.iter().map(|d| d.source_id.as_str()).collect::<Vec<_>>(),
            [source_id.as_str()]
        );

        update(&source_id, CaptureChange::Distilled { distill_ord: 2 });
        assert_eq!(state().distill_ord, 2);
        update(&source_id, CaptureChange::Gap { byte_offset: 9000 });
        let st = state();
        assert_eq!((st.byte_offset, st.gaps), (9000, 1));
        update(
            &source_id,
            CaptureChange::FileKey {
                file_key: "1:2:abc".into(),
                byte_offset: 0,
            },
        );
        let st = state();
        assert_eq!(
            (st.file_key.as_deref(), st.byte_offset, st.next_ord),
            (Some("1:2:abc"), 0, 2),
            "a replaced file resets the offset, not the ords"
        );
        let open = |c: &Connection| -> bool {
            capture_store::open_sources(c)
                .unwrap()
                .iter()
                .any(|o| o.source_id == source_id)
        };
        assert!(open(&c));
        update(&source_id, CaptureChange::Ended);
        assert!(state().ended);
        assert!(!open(&c));
        assert_eq!(
            update("01K7NOSUCHSOURCE0000000000", CaptureChange::Ended),
            0
        );
    }

    /// A capture slice is written whole, within the ingest slice budget: it joins a commit only
    /// when it fits what is left of it, so two 30-chunk slices never share one.
    #[tokio::test]
    async fn capture_slices_share_the_ingest_budget_and_are_never_split() {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        let (wake_tx, _wake_rx) = crossbeam_channel::unbounded();
        let log: CommitLog = Arc::new(Mutex::new(Vec::new()));
        let (h, _t) = spawn_writer_logged(conn, "dev".into(), wake_tx, log.clone());
        let ing = h.submit(ingest("many", 120, |i| format!("short {i}")));
        let a = h.submit(capture(&capture_slice("A", global(), 0..30)));
        let b = h.submit(capture(&capture_slice("B", global(), 0..30)));
        let (ing, a, b) = tokio::join!(ing, a, b);
        assert_eq!(ingested(ing).inserted, 120);
        assert_eq!((captured(a).1, captured(b).1), (30, 30));
        let log = log.lock();
        let slices: usize = log
            .iter()
            .map(|c| c.iter().filter(|n| **n == "CaptureSlice").count())
            .sum();
        assert_eq!(slices, 2, "each slice is written once, whole: {log:?}");
        assert!(
            log.iter()
                .all(|c| c.iter().filter(|n| **n == "CaptureSlice").count() <= 1),
            "two slices shared a commit: {log:?}"
        );
    }
}
