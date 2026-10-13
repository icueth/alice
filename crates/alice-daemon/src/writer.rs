//! The single writer. Jobs arrive on a channel, are grouped for up to 2 ms (max 256), and run in
//! one transaction with a savepoint per job, so one failing job never rolls back the others.
//! Only this thread writes to the database (spec §4.1), and every transaction is short: a bulk
//! ingest is written a slice at a time, so small writes queued behind it never wait for all of it.

use alice_core::error::{AliceError, Result};
use alice_core::ids::Hlc;
use alice_core::model::{Memory, Status};
use alice_core::project::ProjectRef;
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::kg::{self, AssertFact, EntityMatcher, FactOutcome};
use alice_core::store::memories::{self, InsertOutcome, MemoryUpdate, NewMemoryInput};
use alice_core::store::projects::{self, ResolvedProject};
use alice_core::store::sources::{self, ChunkInput, IngestOutcome, SourceInput};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use rusqlite::{Connection, ErrorCode, OptionalExtension};
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const BATCH_WINDOW: Duration = Duration::from_millis(2);
const BATCH_MAX: usize = 256;
/// Raw chunks written per batch, at most. A larger ingest continues in the following batches
/// (each its own short transaction) and is acknowledged once all of it is committed.
const INGEST_SLICE: usize = 200;
/// Chunk text written per batch, at most (FTS work grows with text, not with chunk count).
const INGEST_SLICE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub enum ProjectChoice {
    /// An existing slug or alias. Unknown slugs are rejected so agents cannot create stray projects.
    Slug(String),
    /// Detected from a working directory; created on first write.
    Detected(ProjectRef),
}

#[derive(Debug)]
pub enum WriteOp {
    Remember {
        input: NewMemoryInput,
        project: ProjectChoice,
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
    },
    AssertFact(AssertFact),
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
    TouchAccess(Vec<i64>),
    Shutdown,
}

#[derive(Debug)]
pub enum WriteOutcome {
    Remembered(InsertOutcome, ResolvedProject),
    Ingested(IngestOutcome),
    Updated(Memory),
    Fact(FactOutcome),
    /// The `StoreEmbeddings` items that were stored; the others changed while being embedded.
    Embedded(Vec<(VecKey, Vec<f32>)>),
    Done,
}

type Reply = Option<oneshot::Sender<Result<WriteOutcome>>>;

struct Job {
    op: WriteOp,
    reply: Reply,
    /// For an ingest continued from an earlier batch: what its committed slices did.
    progress: Option<IngestOutcome>,
}

/// What applying a job did.
enum Applied {
    Done(WriteOutcome),
    /// Part of an ingest is written (or none, when this batch had no room left); the rest
    /// continues in the next batch.
    More(WriteOp, Option<IngestOutcome>),
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
}

impl WriterHandle {
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
        let _ = self.tx.send(Job {
            op,
            reply: None,
            progress: None,
        });
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
    }
}

struct State {
    hlc: Hlc,
    /// Entity names for mention links; dropped whenever facts (and so entities) may have changed.
    matcher: Option<EntityMatcher>,
    /// A job in this batch added or changed text that needs embedding.
    inserted: bool,
    /// Raw chunks this batch may still write.
    chunk_budget: usize,
    /// Bytes of chunk text this batch may still write.
    byte_budget: usize,
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
    progress: Option<IngestOutcome>,
    st: &mut State,
) -> Result<Applied> {
    if progress.is_none() {
        // The whole request is checked before its first slice is written.
        sources::validate_chunks(&chunks)?;
    }
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
    let p = resolve(conn, &project)?;
    let out = sources::ingest(conn, &p, &source, &chunks)?;
    st.chunk_budget -= take;
    let bytes: usize = chunks.iter().map(|c| c.text.len()).sum();
    st.byte_budget = st.byte_budget.saturating_sub(bytes);
    if out.inserted > 0 {
        st.inserted = true;
    }
    let total = match progress {
        Some(before) => IngestOutcome {
            source_id: out.source_id,
            inserted: before.inserted + out.inserted,
            skipped: before.skipped + out.skipped,
        },
        None => out,
    };
    if rest.is_empty() {
        return Ok(Applied::Done(WriteOutcome::Ingested(total)));
    }
    let op = WriteOp::Ingest {
        project,
        source,
        chunks: rest,
    };
    Ok(Applied::More(op, Some(total)))
}

fn apply(
    conn: &Connection,
    op: WriteOp,
    progress: Option<IngestOutcome>,
    st: &mut State,
) -> Result<Applied> {
    let outcome = match op {
        WriteOp::Remember { input, project } => {
            let p = resolve(conn, &project)?;
            let prepared = memories::prepare(input, &p, st.hlc.tick())?;
            let out = memories::insert(conn, &prepared)?;
            if matches!(out, InsertOutcome::New { .. }) {
                st.inserted = true;
            }
            WriteOutcome::Remembered(out, p)
        }
        WriteOp::Ingest {
            project,
            source,
            chunks,
        } => return ingest_slice(conn, project, source, chunks, progress, st),
        WriteOp::Update { id, update } => {
            let m = memories::apply_update(conn, &id, &update, st.hlc.tick())?;
            st.inserted = true; // edits may need re-embedding
            WriteOutcome::Updated(m)
        }
        WriteOp::AssertFact(f) => {
            st.matcher = None; // the entity list may change
            WriteOutcome::Fact(kg::assert_fact(conn, &f, st.hlc.tick())?)
        }
        WriteOp::StoreEmbeddings { model, items } => {
            let mut applied = Vec::with_capacity(items.len());
            for (key, stamp, v) in items {
                if !embeddings::put_if_current(conn, key, &stamp, &model, &v)? {
                    continue;
                }
                if let VecKey::Memory(seq) = key {
                    activate(conn, seq, st.matcher(conn)?)?;
                }
                applied.push((key, v));
            }
            WriteOutcome::Embedded(applied)
        }
        WriteOp::EmbedFailed { keys, retry } => {
            for (key, stamp) in keys {
                let marked = embeddings::mark_failed_if_current(conn, key, &stamp, retry)?;
                if let (VecKey::Memory(seq), true, false) = (key, marked, retry) {
                    // Searchable by words even without a vector.
                    activate(conn, seq, st.matcher(conn)?)?;
                }
            }
            WriteOutcome::Done
        }
        WriteOp::TouchAccess(seqs) => {
            memories::touch_access(conn, &seqs)?;
            WriteOutcome::Done
        }
        WriteOp::Shutdown => WriteOutcome::Done,
    };
    Ok(Applied::Done(outcome))
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
        memories::set_status_by_seq(conn, seq, Status::Active)?;
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
fn newest_hlc(conn: &Connection) -> Result<Option<String>> {
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
                    if let Some(reason) = fatal_reason(&e) {
                        tracing::error!(%reason, error = %e, "writer entering read-only mode");
                        ro.set(reason);
                    }
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

pub fn spawn_writer(
    mut conn: Connection,
    device: String,
    wake_worker: Sender<()>,
) -> (WriterHandle, JoinHandle<()>) {
    let (tx, rx): (Sender<Job>, Receiver<Job>) = unbounded();
    let read_only = Arc::new(ReadOnlyState::default());
    let ro = read_only.clone();
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
                chunk_budget: INGEST_SLICE,
                byte_budget: INGEST_SLICE_BYTES,
            };
            let mut carry: VecDeque<Job> = VecDeque::new();
            let mut stopping = false;
            loop {
                let mut batch: Vec<Job> = carry.drain(..).collect();
                if !stopping {
                    if batch.is_empty() {
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
                        // Continuing an ingest: add what is already queued, without waiting.
                        while !ends_batch(&batch) && batch.len() < BATCH_MAX {
                            match rx.try_recv() {
                                Ok(j) => batch.push(j),
                                Err(_) => break,
                            }
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
                match out.commit {
                    Ok(()) => {
                        for (reply, r) in out.replies {
                            if let Some(reply) = reply {
                                let _ = reply.send(r);
                            }
                        }
                        carry.extend(out.carry);
                        if st.inserted {
                            let _ = wake_worker.try_send(());
                        }
                    }
                    Err(e) => {
                        // A matcher loaded inside the rolled-back transaction may name entities
                        // that were never committed.
                        st.matcher = None;
                        if let Some(reason) = is_fatal(&e) {
                            tracing::error!(%reason, error = %e, "writer entering read-only mode");
                            ro.set(reason);
                        }
                        let replies = out.replies.into_iter().map(|(reply, _)| reply);
                        for reply in replies
                            .chain(out.carry.into_iter().map(|j| j.reply))
                            .flatten()
                        {
                            let _ = reply.send(Err(AliceError::Other(format!(
                                "write failed: {e}; retry shortly"
                            ))));
                        }
                    }
                }
            }
        })
        .expect("spawn writer thread");
    (WriterHandle { tx, read_only }, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::model::Kind;
    use alice_core::store::embeddings::{self, VecKey};
    use alice_core::store::kg::AssertFact;
    use alice_core::store::memories::{MemoryUpdate, NewMemoryInput};
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
        let status: String = f.read("SELECT status FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(status, "active");
    }

    #[test]
    fn giving_up_on_an_embedding_activates_the_memory_but_not_after_an_edit() {
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
        let status: String = f.read("SELECT status FROM memories WHERE seq = ?1", [seq]);
        assert_eq!(
            status, "active",
            "searchable by words even without a vector"
        );
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
            seq
        };
        store("nothing to link yet");
        f.h.submit_blocking(WriteOp::AssertFact(AssertFact {
            subject: "Hostlink".into(),
            predicate: "written in".into(),
            object: "Swift".into(),
            ..Default::default()
        }))
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
        assert_eq!(slice_len(&chunks(1_000, 2_048), fresh), 32);
        assert_eq!(slice_len(&chunks(10, 32_768), fresh), 2);
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

    /// A database that refuses writes switches the writer to read-only mode with a reason.
    #[test]
    fn a_write_refused_by_the_database_makes_the_writer_read_only() {
        let d = tempfile::tempdir().unwrap();
        let conn = open(d.path());
        conn.execute_batch("PRAGMA query_only = ON").unwrap();
        let f = start(d, conn);
        let first = f.h.submit_blocking(remember("refused")).unwrap_err();
        assert!(!matches!(first, AliceError::Invalid(_)), "{first}");
        let reason = f.h.read_only.reason().expect("read-only reason");
        assert!(reason.contains("cannot be written"), "{reason}");
        let second = f.h.submit_blocking(remember("also refused")).unwrap_err();
        assert!(matches!(second, AliceError::ReadOnly(_)), "{second}");
        let _ = Kind::Note;
    }
}
