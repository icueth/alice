//! The distill worker (SP2 spec §6.2, §6.4–§6.7): one daemon thread, `alice-distill`, that turns
//! captured sessions into memories.
//!
//! A main capture source becomes **due** when its session ends or compacts (the capture
//! pipeline marks it), when it has been quiet for `distill.idle_minutes` with chunks not yet
//! distilled (its own or a subagent's) and captured within the last week, or when someone asks
//! (`alice capture distill`). On a server tenant, a source with no capture for 24 h is also
//! marked ended, and due when it captured within the last 7 days plus those 24 h (tenants have
//! no sweep; locally the capture sweep owns the stale end, Task 11b). The worker
//! wakes when the capture pipeline marks a source, and every [`TICK`]; due sources are run the
//! most recently captured first.
//!
//! One **run** of one due source:
//! 1. The gate, in this order:
//!    - `distill.provider` is off, or a global capture switch is (`capture.enabled`, the agent's
//!      own): `idle`, so it is distilled once they are on again;
//!    - the judge in force is not Jev: `waiting_judge` (the judge is what removes doubles, R14;
//!      a missing key is said in health as `no_key`);
//!    - the project is excluded (`capture.excluded_projects`, its `no_capture`), or the tenant
//!      client sent `no_distill`: `skipped_excluded`;
//!    - the cloud policy refuses the project: `skipped_cloud`, asked again every 5 minutes;
//!    - the breaker is open: `due` again at its time.
//! 2. Each subagent by its own project's rules (the same three, without the switches): one
//!    that is not allowed keeps its chunks and says why in its own state.
//! 3. The session's chunks from `distill_ord` on, and the allowed subagents' from theirs, about
//!    two runs' worth and below each source's `next_ord`, with their turn starts (Task 12
//!    supplement, items 1 and 11).
//! 4. [`distill::prepare`] within the run's caps, one Jev request for every project the run
//!    covers ([`Judge::ask_distill`], so at most one in flight), then [`distill::decide`]. A
//!    session whose best set is full is asked as any other (Task 12c), within
//!    `distill.budget_share`.
//! 5. The kept passages fitted into the session's best set ([`distill::place`]: at most
//!    `distill.max_per_session` live distilled memories, the best by rank; a passage better than
//!    the weakest that may go is stored, never in place of a handoff nor of a memory a person or
//!    an agent touched; with none to outrank it is not stored, and the run counts it in
//!    `capped`), then each written with [`write_memory`] in the project of the source it came
//!    from: redacted with that project's rules, `origin = distill`, its rank inputs, its chunks
//!    as provenance and an idempotency key, so a repeated run (a crash before the cursor moved)
//!    stores nothing twice. Nothing is evicted here: once the judge finds a newcomer live in a
//!    full set, the writer trashes the set's weakest member that may go, by `distill:cap` (Task
//!    12c review I1, option b).
//! 6. The cursors move to the end of the run; the source is `done`, or `due` again when more of
//!    it is left.
//!
//! `Budget` waits for the next UTC day (`waiting_budget`) and `Denied` is `skipped_cloud`: both
//! are answers, not failures. `Unavailable` and `Failed` count toward the [`Breaker`] (a cached
//! answer does not close it) and back the source off (5 minutes, doubling to 6 h; after
//! [`MAX_ATTEMPTS`] it is `failed`); so does a read the run needs that fails. Nothing runs while
//! the database refuses writes. Raw capture never waits on any of this. The Jev call runs on
//! this thread, never on the writer or an async runtime thread.

use crate::config::{CaptureVerdict, EXCLUDED_AGENT_OFF, EXCLUDED_CAPTURE_OFF, SettingsHandle};
use crate::judge_http::JudgeHub;
use crate::redaction::Redactors;
use crate::writer::{CaptureChange, ProjectChoice, WriteOp, WriteOutcome, WriterHandle};
use alice_core::capture::AgentKind;
use alice_core::distill::{
    self, DistillConfig, DistilledMemory, RUN_MAX_CHARS, RunChunk, RunInput,
};
use alice_core::ids::now_rfc3339;
use alice_core::judge::{Judge, JudgeError};
use alice_core::model::{Author, Origin};
use alice_core::settings::{DistillProvider, JudgeProvider, ProjectTree, Settings};
use alice_core::store::ReaderPool;
use alice_core::store::capture::{self as store, CaptureState, DistillState};
use alice_core::store::memories::{InsertOutcome, NewMemoryInput};
use alice_core::store::projects;
use alice_core::{AliceError, Result};
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Failed distillation requests in a row that open the breaker (spec §6.7).
pub const FAILURES_TO_OPEN: u32 = 3;
/// How long the breaker stays open the first time; it doubles at each reopening.
pub const OPEN_FIRST: Duration = Duration::from_secs(15 * 60);
/// The longest the breaker stays open.
pub const OPEN_MAX: Duration = Duration::from_secs(6 * 3600);
/// How often the worker looks for work when nothing wakes it (spec §6.2).
pub const TICK: Duration = Duration::from_secs(60);
/// The first wait of a source whose run failed; it doubles with each attempt.
pub const RETRY_FIRST: Duration = Duration::from_secs(5 * 60);
/// The longest wait of a source whose run failed.
pub const RETRY_MAX: Duration = Duration::from_secs(6 * 3600);
/// Failed runs after which a source is `failed` (`alice capture distill` tries it again).
pub const MAX_ATTEMPTS: i64 = 5;
/// How often the cloud policy of `skipped_cloud` sources is asked again (spec §6.7).
pub const CLOUD_RECHECK: Duration = Duration::from_secs(5 * 60);
/// A source with no capture for this long is marked ended (the stale end, spec §6.2), on a
/// server tenant only: locally the capture sweep owns the stale end.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);
/// The quiet trigger marks only sessions that captured something within this long; an older
/// backlog is distilled when asked (`alice capture distill`). Task 12 fix round 1. A stale end
/// marks a session due only when it captured within this long plus the stale age (8 days by
/// default), counted back from the end (Task 11b).
pub const QUIET_RECENT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Characters of a session's chunks read for one run: two runs' worth, so the turn the load
/// cuts is dropped cleanly by `prepare` (Task 10 report, "Run bounding").
const LOAD_CHARS: usize = 2 * RUN_MAX_CHARS;
/// Characters of subagent chunks read for one run, all subagents together.
const SUBAGENT_CHARS: usize = 2 * RUN_MAX_CHARS;
/// Chunks read per query.
const PAGE: usize = 200;
/// Due sources read per query.
const BATCH: usize = 16;
/// Runs per wake-up, at most (a long backlog goes on at the next one).
const RUNS_PER_ROUND: usize = 64;
/// Sources one housekeeping step changes, at most.
const HOUSEKEEPING_BATCH: usize = 500;
/// Run reports kept in memory (`Service::distill_runs`).
const RUNS_KEPT: usize = 64;
/// How long `stop` waits for a run in flight (it may be waiting for Jev).
const STOP_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(3)
};

/// The breaker around distillation's Jev requests (spec §6.7). Three failed runs in a row open
/// it for [`OPEN_FIRST`]; once it has opened, the first failure after its time reopens it at
/// once, for twice as long each time, up to [`OPEN_MAX`]. A request that is answered closes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
    reopenings: u32,
}

impl Breaker {
    /// Whether no request may be sent at `now`.
    pub fn is_open(&self, now: Instant) -> bool {
        self.open_until.is_some_and(|t| now < t)
    }

    /// When it closes (the last opening's end), if it ever opened since the last success.
    pub fn open_until(&self) -> Option<Instant> {
        self.open_until
    }

    /// A request was answered.
    pub fn success(&mut self) {
        *self = Breaker::default();
    }

    /// A request failed at `now`. Returns whether the breaker opened.
    pub fn failure(&mut self, now: Instant) -> bool {
        self.failures = self.failures.saturating_add(1);
        if self.reopenings == 0 && self.failures < FAILURES_TO_OPEN {
            return false;
        }
        let factor = 2u32.saturating_pow(self.reopenings.min(16));
        self.open_until = Some(now + OPEN_FIRST.saturating_mul(factor).min(OPEN_MAX));
        self.reopenings = self.reopenings.saturating_add(1);
        self.failures = 0;
        true
    }
}

/// The breaker as health and the doctor see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    /// No distillation request is sent before `until`.
    Open {
        until: DateTime<Utc>,
    },
}

/// What a run and [`write_memory`] use.
pub struct DistillCtx {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub settings: Arc<SettingsHandle>,
    pub redactors: Arc<Redactors>,
    pub judge: Arc<JudgeHub>,
}

/// How a [`DistillCtl`] is made.
pub struct DistillCfg {
    pub ctx: DistillCtx,
    /// How often it looks for work unwoken ([`TICK`]; tests shorten it).
    pub tick: Duration,
    /// Tests: the quiet time in place of `distill.idle_minutes`.
    pub idle_override: Option<Duration>,
}

/// What one run did (the last [`RUNS_KEPT`] are kept in memory, for tests and diagnosis).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunReport {
    pub source_id: String,
    pub uri: String,
    /// The state the run left the source in (`done`, `due`, `waiting_judge`, …).
    pub state: String,
    /// Questions asked (0 when the gate stopped the run or nothing was worth asking).
    pub questions: usize,
    /// Memories stored.
    pub stored: usize,
    /// Memories that came back as duplicates (their idempotency key or text was stored).
    pub duplicates: usize,
    /// Kept passages not stored because the privacy rules refused their text.
    pub refused: usize,
    /// Kept passages not stored: the session's set is full, and every member they may replace is
    /// protected (a handoff, or one a person or an agent touched) or none of them is weaker.
    pub capped: usize,
    /// When the run ended (RFC 3339).
    pub at: String,
}

impl RunReport {
    fn of(src: &CaptureState) -> Self {
        RunReport {
            source_id: src.source_id.clone(),
            uri: src.uri.clone(),
            state: String::new(),
            questions: 0,
            stored: 0,
            duplicates: 0,
            refused: 0,
            capped: 0,
            at: String::new(),
        }
    }
}

/// Where distillation stands for health, `alice capture status` and the doctor (spec §8.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistillView {
    /// `off`, `ready`, `waiting_judge`, `waiting_budget`, `breaker_open` or `no_key`.
    pub state: &'static str,
    /// What is wrong (none when `off` or `ready`).
    pub warning: Option<String>,
    /// What to do about it.
    pub fix: Option<String>,
}

impl DistillView {
    fn plain(state: &'static str) -> Self {
        DistillView {
            state,
            warning: None,
            fix: None,
        }
    }

    fn warn(state: &'static str, warning: impl Into<String>, fix: &str) -> Self {
        DistillView {
            state,
            warning: Some(warning.into()),
            fix: Some(fix.into()),
        }
    }
}

/// Why the judge in force cannot distil.
enum JudgeWait {
    /// `judge.provider` is not `jev`.
    NotJev,
    /// Jev is asked for and no key is set.
    NoKey,
    /// Jev is asked for and is not in force (being built, or it could not be): its warning.
    NotReady(Option<String>),
}

/// The chunks one run reads.
#[derive(Default)]
struct Loaded {
    chunks: Vec<RunChunk>,
    /// The last main ord read; none when nothing was read from the session itself.
    main_last: Option<i64>,
    /// More of the session follows what was read.
    main_cut: bool,
    /// Each subagent read (or found with nothing left to read): its `distill_ord` after the run.
    children: Vec<(String, i64)>,
    /// More of a subagent follows what was read.
    children_cut: bool,
    /// The subagent each subagent chunk came from (an index into the run's subagents); the
    /// session's own chunks are not in it.
    owner: HashMap<String, usize>,
}

/// What a source's own project rules say about distilling it now.
enum Allowance {
    /// Allowed; the project's slug.
    Yes(String),
    /// The project is excluded, or the client sent `no_distill`.
    Excluded,
    /// The project's rules are unknown right now.
    Wait,
    /// The cloud policy keeps the project at home.
    Denied,
    /// A global capture switch is off (read after the caller's own check): not the project's
    /// rule, so nothing is skipped for good.
    Off,
}

/// The distill worker of one daemon.
pub struct DistillCtl {
    ctx: DistillCtx,
    tick: Duration,
    idle_override: Option<Duration>,
    breaker: Mutex<Breaker>,
    /// bounded(1): repeated wake-ups coalesce.
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    runs: Mutex<VecDeque<RunReport>>,
    /// `(judge.daily_budget_usd, distill.budget_share)` when a run last found the share used
    /// up: a raise of either lets the waiting sources try again the same day.
    budget_stop: Mutex<Option<(f64, f64)>>,
    /// When the cloud policy of the `skipped_cloud` sources was last asked again.
    cloud_checked: Mutex<Option<Instant>>,
    /// Turns of the worker's loop (tests check that it sleeps).
    rounds: AtomicU64,
}

impl DistillCtl {
    pub fn new(cfg: DistillCfg) -> Arc<Self> {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Arc::new(Self {
            ctx: cfg.ctx,
            tick: cfg.tick,
            idle_override: cfg.idle_override,
            breaker: Mutex::new(Breaker::default()),
            wake_tx,
            wake_rx,
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
            runs: Mutex::new(VecDeque::new()),
            budget_stop: Mutex::new(None),
            cloud_checked: Mutex::new(None),
            rounds: AtomicU64::new(0),
        })
    }

    /// Starts the worker thread (once).
    pub fn spawn(self: &Arc<Self>) -> Result<()> {
        let mut slot = self.thread.lock();
        if slot.is_some() {
            return Ok(());
        }
        let me = self.clone();
        *slot = Some(
            std::thread::Builder::new()
                .name("alice-distill".into())
                .spawn(move || me.work())?,
        );
        Ok(())
    }

    /// Asks the worker to look for due sources now.
    pub fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }

    pub fn breaker_state(&self) -> BreakerState {
        let b = *self.breaker.lock();
        let now = Instant::now();
        match b.open_until() {
            Some(until) if b.is_open(now) => BreakerState::Open {
                until: Utc::now()
                    + chrono::Duration::from_std(until - now).unwrap_or(chrono::Duration::zero()),
            },
            _ => BreakerState::Closed,
        }
    }

    /// Asks the worker to stop, without waiting.
    pub fn signal_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Stops the worker: it ends after the run in flight. Waits a moment for it (a run may be
    /// waiting for Jev), then leaves it; its writes fail once the writer has stopped.
    pub fn stop(&self) {
        self.signal_stop();
        let Some(h) = self.thread.lock().take() else {
            return;
        };
        let waited = Instant::now();
        while !h.is_finished() && waited.elapsed() < STOP_WAIT {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !h.is_finished() {
            tracing::warn!("the distill worker is waiting for Jev; stopping without it");
        } else if h.join().is_err() {
            tracing::error!("the distill worker panicked");
        }
    }

    /// How many times the worker's loop has turned (a sleeping worker turns once a tick).
    pub fn rounds(&self) -> u64 {
        self.rounds.load(Ordering::Relaxed)
    }

    /// The last runs, oldest first.
    pub fn runs(&self) -> Vec<RunReport> {
        self.runs.lock().iter().cloned().collect()
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn work(self: Arc<Self>) {
        let mut housekeeping_at = Instant::now();
        loop {
            if self.stopped() {
                return;
            }
            self.rounds.fetch_add(1, Ordering::Relaxed);
            if Instant::now() >= housekeeping_at {
                // Nothing to change while the database refuses writes; the next one is still a
                // tick away, so the loop keeps sleeping (fix round 2).
                if !self.read_only() {
                    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.housekeeping()
                    }));
                    match done {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "the distill worker could not look for quiet or waiting sessions; it tries again later")
                        }
                        Err(_) => tracing::error!("the distill worker's housekeeping panicked"),
                    }
                }
                housekeeping_at = Instant::now() + self.tick;
            }
            self.drain();
            if self.stopped() {
                return;
            }
            let wait = housekeeping_at.saturating_duration_since(Instant::now());
            let _ = self.wake_rx.recv_timeout(wait);
        }
    }

    /// Whether the database refuses writes: no run starts then, since its answer could not be
    /// kept (Task 12 fix round 1: a paid request would be thrown away every tick).
    fn read_only(&self) -> bool {
        self.ctx.writer.read_only.reason().is_some()
    }

    /// Runs the due sources, the most recently captured first, up to [`RUNS_PER_ROUND`].
    fn drain(&self) {
        let mut runs = 0;
        while runs < RUNS_PER_ROUND && !self.stopped() {
            if self.read_only() {
                return;
            }
            let now = now_rfc3339();
            let due = match self.ctx.readers.with(|c| store::due_newest(c, &now, BATCH)) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read the sessions due for distillation; trying again later");
                    return;
                }
            };
            if due.is_empty() {
                return;
            }
            for src in due {
                if self.stopped() || runs >= RUNS_PER_ROUND {
                    return;
                }
                runs += 1;
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run(&src)));
                match ran {
                    Ok(Ok(report)) => self.record(report),
                    Ok(Err(e)) => {
                        // Writer trouble (read-only, stopped): the round ends; the source was
                        // put off if that could be written. (A failed read puts the source off
                        // inside the run, and the round goes on.)
                        tracing::warn!(uri = src.uri, error = %e, "a distillation run did not finish; it is tried again later");
                        return;
                    }
                    Err(_) => {
                        tracing::error!(
                            uri = src.uri,
                            "a distillation run panicked; it is tried again later"
                        );
                        let attempts = src.distill_attempts + 1;
                        if self.put_off(&src, attempts).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }

    fn record(&self, mut report: RunReport) {
        report.at = now_rfc3339();
        let mut runs = self.runs.lock();
        if runs.len() == RUNS_KEPT {
            runs.pop_front();
        }
        runs.push_back(report);
    }

    /// One run of one due source (see the module docs). `Err` only when the writer cannot take
    /// a change; every answer, refusal and failure of the judge is a state of the source, and a
    /// read that fails puts the source off with its back-off.
    fn run(&self, src: &CaptureState) -> Result<RunReport> {
        let report = RunReport::of(src);
        // A subagent's chunks are distilled with its session's.
        if let Some(parent) = &src.parent_source_id {
            let p = match self.ctx.readers.with(|c| store::by_source(c, parent)) {
                Ok(p) => p,
                Err(e) => return self.read_failed(src, report, &e),
            };
            // Only a session at rest is woken: a failed, backfilled, skipped or waiting one keeps
            // its state (`alice capture distill` revives any of them).
            if let Some(p) = p
                && matches!(p.distill_state, DistillState::Idle | DistillState::Done)
            {
                self.set(&p.source_id, DistillState::Due, None, 0)?;
            }
            return self.leave(src, report, DistillState::Idle, None, 0);
        }
        let s = self.ctx.settings.get();
        if s.distill.provider == DistillProvider::Off {
            return self.leave(src, report, DistillState::Idle, None, src.distill_attempts);
        }
        let Some(agent) = AgentKind::parse(&src.agent) else {
            return self.leave(src, report, DistillState::SkippedExcluded, None, 0);
        };
        // The global capture switches leave the source idle, so it is distilled once they are on
        // again; only the project's own rules skip it for good (fix round 1, I2).
        if !capture_on(&s, agent) {
            return self.leave(src, report, DistillState::Idle, None, src.distill_attempts);
        }
        let judge = self.ctx.judge.current();
        if let Some(wait) = self.judge_wait(&s, judge.as_ref()) {
            if let JudgeWait::NoKey = wait {
                tracing::info!(uri = src.uri, "distillation waits: no Jev key is set");
            }
            return self.leave(
                src,
                report,
                DistillState::WaitingJudge,
                None,
                src.distill_attempts,
            );
        }
        let slug = match self.allowance(src, agent) {
            Err(e) => return self.read_failed(src, report, &e),
            Ok(Allowance::Yes(slug)) => slug,
            Ok(Allowance::Excluded) => {
                return self.leave(src, report, DistillState::SkippedExcluded, None, 0);
            }
            // A switch flipped off since the check above: idle, as there.
            Ok(Allowance::Off) => {
                return self.leave(src, report, DistillState::Idle, None, src.distill_attempts);
            }
            // The project's rules are unknown right now: try again later, not a failure.
            Ok(Allowance::Wait) => {
                return self.leave(
                    src,
                    report,
                    DistillState::Due,
                    Some(after(RETRY_FIRST)),
                    src.distill_attempts,
                );
            }
            Ok(Allowance::Denied) => {
                return self.leave(
                    src,
                    report,
                    DistillState::SkippedCloud,
                    Some(after(CLOUD_RECHECK)),
                    0,
                );
            }
        };
        if let BreakerState::Open { until } = self.breaker_state() {
            return self.leave(
                src,
                report,
                DistillState::Due,
                Some(stamp(until)),
                src.distill_attempts,
            );
        }
        // The subagents, each by its own project's rules: a subagent can work in another
        // repository, and its text goes to Jev only when that project allows it (fix round 1,
        // C1). One that is not allowed keeps its chunks and says why in its own state.
        let kids = match self
            .ctx
            .readers
            .with(|c| store::children(c, &src.source_id))
        {
            Ok(k) => k,
            Err(e) => return self.read_failed(src, report, &e),
        };
        let mut included: Vec<(CaptureState, String)> = Vec::new();
        // A subagent whose rules are unknown right now: the session is due again later for it.
        let mut child_waits = false;
        for k in kids {
            if k.next_ord <= k.distill_ord {
                continue;
            }
            let allowance = match AgentKind::parse(&k.agent) {
                Some(a) => match self.allowance(&k, a) {
                    Ok(x) => x,
                    Err(e) => return self.read_failed(src, report, &e),
                },
                None => Allowance::Excluded,
            };
            match allowance {
                // A backfilled subagent is distilled with its session once the session is asked
                // for (`alice capture distill`), so it leaves `skipped_backfill` too (Task 11b).
                Allowance::Yes(kslug) => {
                    if matches!(
                        k.distill_state,
                        DistillState::SkippedCloud
                            | DistillState::SkippedExcluded
                            | DistillState::SkippedBackfill
                    ) {
                        self.set(&k.source_id, DistillState::Idle, None, 0)?;
                    }
                    included.push((k, kslug));
                }
                Allowance::Excluded => {
                    if k.distill_state != DistillState::SkippedExcluded {
                        self.set(&k.source_id, DistillState::SkippedExcluded, None, 0)?;
                    }
                }
                Allowance::Denied => {
                    if k.distill_state != DistillState::SkippedCloud {
                        self.set(
                            &k.source_id,
                            DistillState::SkippedCloud,
                            Some(after(CLOUD_RECHECK)),
                            0,
                        )?;
                    }
                }
                // Asked again when the session is due again (fix round 2).
                Allowance::Wait | Allowance::Off => child_waits = true,
            }
        }
        // What `done` becomes while a subagent waits: due again a few minutes later.
        let rest = |state: DistillState| -> (DistillState, Option<String>) {
            match state {
                DistillState::Done if child_waits => (DistillState::Due, Some(after(RETRY_FIRST))),
                other => (other, None),
            }
        };
        let cfg = DistillConfig::from(&s.distill);
        let project_names = match self.ctx.readers.with(|c| project_names(c, &src.project_id)) {
            Ok(n) => n,
            Err(e) => return self.read_failed(src, report, &e),
        };

        let mut loaded = match self.load(src, &included) {
            Ok(l) => l,
            Err(e) => return self.read_failed(src, report, &e),
        };
        if loaded.chunks.is_empty() {
            // Nothing left to read (retention removed it, or every subagent waits): the cursors
            // move past what is gone.
            let _ = self.advance(src, &loaded, None)?;
            let (state, next) = rest(DistillState::Done);
            return self.leave(src, report, state, next, 0);
        }
        // Every project the run covers: the client gates and redacts for each one.
        let mut projects: Vec<&str> = vec![slug.as_str()];
        for (_, kslug) in &included {
            if !projects.contains(&kslug.as_str()) {
                projects.push(kslug);
            }
        }
        let input = RunInput {
            uri: src.uri.clone(),
            project: slug.clone(),
            project_names,
            closes_session: !loaded.main_cut
                && (src.ended
                    || matches!(
                        src.last_event.as_deref(),
                        Some("session-end" | "pre-compact")
                    )),
            chunks: std::mem::take(&mut loaded.chunks),
        };
        let prepared = distill::prepare(&input).within(&cfg);
        let mut report = report;
        report.questions = prepared.questions.len();
        let memories = if prepared.questions.is_empty() {
            Vec::new()
        } else {
            // A cached answer did not reach Jev: it says nothing about the breaker.
            let cached = judge.distill_cached(&projects, &prepared.state, &prepared.questions);
            match judge.ask_distill(
                &projects,
                &prepared.state,
                &prepared.questions,
                s.distill.budget_share,
            ) {
                Ok(answers) => {
                    if !cached {
                        self.breaker.lock().success();
                    }
                    distill::decide(&prepared, &answers, &src.uri, &cfg)
                }
                // Our own words: the judge error's wraps the share stop in the judge's budget.
                Err(JudgeError::Budget(_)) => {
                    *self.budget_stop.lock() =
                        Some((s.judge.daily_budget_usd, s.distill.budget_share));
                    tracing::info!(
                        uri = src.uri,
                        "distillation waits for the next UTC day: today's distillation share of the Jev budget is used up"
                    );
                    return self.leave(
                        src,
                        report,
                        DistillState::WaitingBudget,
                        Some(next_utc_midnight()),
                        src.distill_attempts,
                    );
                }
                Err(JudgeError::Denied) => {
                    return self.leave(
                        src,
                        report,
                        DistillState::SkippedCloud,
                        Some(after(CLOUD_RECHECK)),
                        0,
                    );
                }
                Err(e @ (JudgeError::Unavailable(_) | JudgeError::Failed(_))) => {
                    let opened = self.breaker.lock().failure(Instant::now());
                    let attempts = src.distill_attempts + 1;
                    tracing::warn!(uri = src.uri, attempts, error = %e, "a distillation request failed");
                    if opened && let BreakerState::Open { until } = self.breaker_state() {
                        tracing::warn!(
                            until = stamp(until),
                            "Jev failed {FAILURES_TO_OPEN} times in a row; distillation pauses until then (raw capture goes on)"
                        );
                    }
                    return self.failed(src, report, attempts);
                }
            }
        };
        // The session's best set (Task 12c): what is kept takes a free slot, or the place of a
        // weaker held memory, or is not stored.
        let placements = if memories.is_empty() {
            Vec::new()
        } else {
            let held = match self
                .ctx
                .readers
                .with(|c| store::session_held(c, &src.agent, &src.session_id))
            {
                Ok(h) => h,
                Err(e) => return self.read_failed(src, report, &e),
            };
            let n = memories.len();
            let placements = distill::place(&held, memories, &s.distill);
            report.capped = n - placements.len();
            if report.capped > 0 {
                tracing::info!(
                    uri = src.uri,
                    capped = report.capped,
                    held = held.len(),
                    protected = held.iter().filter(|h| !h.replaceable()).count(),
                    "a session's set is full: kept passages that outrank no member it may replace are not stored"
                );
            }
            placements
        };
        for pl in &placements {
            let m = &pl.memory;
            // A passage of a subagent's chunks goes to that subagent's project, with its rules.
            let owner = m
                .chunk_ids
                .iter()
                .find_map(|id| loaded.owner.get(id))
                .map_or(src, |&i| &included[i].0);
            match write_memory(&self.ctx, m, owner) {
                Ok(InsertOutcome::New { .. }) => report.stored += 1,
                Ok(InsertOutcome::Duplicate { .. }) => report.duplicates += 1,
                // The rules refuse its text (block mode), or the store refuses it: this one
                // passage is left out.
                Err(e @ (AliceError::Refused { .. } | AliceError::Invalid(_))) => {
                    report.refused += 1;
                    tracing::info!(uri = src.uri, error = %e, "a distilled passage was not stored");
                }
                Err(e) => {
                    // The writer cannot take it now: the run is repeated later, and what it
                    // stored comes back as duplicates.
                    let _ = self.failed(src, report, src.distill_attempts + 1);
                    return Err(e);
                }
            }
        }
        let cursor = self.advance(src, &loaded, Some(prepared.consumed_ord))?;
        let more = loaded.main_cut
            || loaded.children_cut
            || loaded
                .main_last
                .is_some_and(|last| prepared.consumed_ord < last)
            || self.ended_meanwhile(src, cursor);
        if report.stored > 0 || report.duplicates > 0 {
            tracing::info!(
                uri = src.uri,
                stored = report.stored,
                duplicates = report.duplicates,
                "distilled a captured session"
            );
        }
        let (state, next) = if more {
            (DistillState::Due, None)
        } else {
            rest(DistillState::Done)
        };
        self.leave(src, report, state, next, 0)
    }

    /// What `src`'s own project rules say: the client's `no_distill`, the project's exclusion
    /// (`capture.excluded_projects`, its `no_capture`; asked for every name of the project),
    /// and the cloud policy. The global capture switches are the caller's.
    fn allowance(&self, src: &CaptureState, agent: AgentKind) -> Result<Allowance> {
        if src.no_distill {
            return Ok(Allowance::Excluded);
        }
        let (verdict, _) = crate::capture::source_verdict(
            &self.ctx.readers,
            &self.ctx.settings,
            agent,
            &src.project_id,
        )?;
        match verdict {
            CaptureVerdict::Excluded(why) if switch_off(why) => return Ok(Allowance::Off),
            CaptureVerdict::Excluded(_) => return Ok(Allowance::Excluded),
            CaptureVerdict::Wait(_) => return Ok(Allowance::Wait),
            CaptureVerdict::Capture => {}
        }
        let slug = self
            .ctx
            .readers
            .with(|c| projects::get_by_id(c, &src.project_id))?
            .slug;
        Ok(if self.ctx.judge.cloud_allowed(&slug) {
            Allowance::Yes(slug)
        } else {
            Allowance::Denied
        })
    }

    /// A read the run needed failed: the source is put off with its back-off (an attempt), and
    /// the round goes on with the next source.
    fn read_failed(
        &self,
        src: &CaptureState,
        report: RunReport,
        e: &AliceError,
    ) -> Result<RunReport> {
        tracing::warn!(uri = src.uri, error = %e, "a distillation run could not read what it needs; it is tried again later");
        self.failed(src, report, src.distill_attempts + 1)
    }

    /// Whether the session ended or compacted while it was being distilled and has chunks the
    /// run did not read: the capture pass found it due already and left it so, and `done`
    /// would leave those chunks to the quiet check. It stays due instead.
    fn ended_meanwhile(&self, src: &CaptureState, cursor: i64) -> bool {
        let now = match self
            .ctx
            .readers
            .with(|c| store::by_source(c, &src.source_id))
        {
            Ok(n) => n,
            Err(e) => {
                // The quiet check finds the rest later.
                tracing::warn!(uri = src.uri, error = %e, "cannot tell whether a session ended during its distillation");
                return false;
            }
        };
        now.is_some_and(|n| {
            n.next_ord > cursor
                && n.next_ord > src.next_ord
                && (n.ended
                    || matches!(n.last_event.as_deref(), Some("session-end" | "pre-compact")))
        })
    }

    /// Why the judge in force cannot distil now; `None` when it is Jev.
    fn judge_wait(&self, s: &Settings, judge: &dyn Judge) -> Option<JudgeWait> {
        if s.judge.provider != JudgeProvider::Jev {
            return Some(JudgeWait::NotJev);
        }
        if self.ctx.judge.key_missing() {
            return Some(JudgeWait::NoKey);
        }
        (judge.provider() != "jev").then(|| JudgeWait::NotReady(self.ctx.judge.warning()))
    }

    /// A failed run: `due` again after a back-off, or `failed` after [`MAX_ATTEMPTS`].
    fn failed(&self, src: &CaptureState, report: RunReport, attempts: i64) -> Result<RunReport> {
        if attempts >= MAX_ATTEMPTS {
            tracing::warn!(
                uri = src.uri,
                "distilling this session failed {MAX_ATTEMPTS} times; `alice capture distill` tries it again"
            );
            return self.leave(src, report, DistillState::Failed, None, attempts);
        }
        self.leave(
            src,
            report,
            DistillState::Due,
            Some(after(backoff(attempts))),
            attempts,
        )
    }

    fn put_off(&self, src: &CaptureState, attempts: i64) -> Result<RunReport> {
        self.failed(src, RunReport::of(src), attempts)
    }

    /// Sets the source's distillation state and returns the report with it.
    fn leave(
        &self,
        src: &CaptureState,
        mut report: RunReport,
        state: DistillState,
        next_try_at: Option<String>,
        attempts: i64,
    ) -> Result<RunReport> {
        self.set(&src.source_id, state, next_try_at, attempts)?;
        report.state = state.as_str().into();
        Ok(report)
    }

    fn set(
        &self,
        source_id: &str,
        state: DistillState,
        next_try_at: Option<String>,
        attempts: i64,
    ) -> Result<()> {
        self.change(
            source_id,
            CaptureChange::Distill {
                state,
                next_try_at,
                attempts,
            },
        )
    }

    fn change(&self, source_id: &str, change: CaptureChange) -> Result<()> {
        self.ctx
            .writer
            .submit_blocking(WriteOp::CaptureStateUpdate {
                source_id: source_id.into(),
                change,
            })?;
        Ok(())
    }

    /// Moves the cursors after a run: the session's past `consumed` (the last main ord the run
    /// covered), each subagent's past what was read. With nothing read from the session
    /// (`main_last` none), its cursor moves to its `next_ord`: its chunks are gone. Returns the
    /// session's `distill_ord` after the run.
    fn advance(&self, src: &CaptureState, loaded: &Loaded, consumed: Option<i64>) -> Result<i64> {
        let main = match loaded.main_last {
            Some(_) => consumed.filter(|c| *c >= 0).map(|c| c + 1),
            None => (src.next_ord > src.distill_ord).then_some(src.next_ord),
        };
        if let Some(distill_ord) = main {
            self.change(&src.source_id, CaptureChange::Distilled { distill_ord })?;
        }
        for (source_id, distill_ord) in &loaded.children {
            self.change(
                source_id,
                CaptureChange::Distilled {
                    distill_ord: *distill_ord,
                },
            )?;
        }
        Ok(main.unwrap_or(src.distill_ord).max(src.distill_ord))
    }

    /// The chunks of a run: the session's from `distill_ord` (about [`LOAD_CHARS`]), then each
    /// included subagent's from its own `distill_ord`, one source after another, each in order.
    /// Every source is read below its `next_ord` as `src` and `kids` hold it: a chunk at or past
    /// it belongs to a turn whose last slice is not written yet (fix round 1, I3).
    fn load(&self, src: &CaptureState, kids: &[(CaptureState, String)]) -> Result<Loaded> {
        self.ctx.readers.with(|c| {
            let mut out = Loaded::default();
            let (main, cut) = page(c, &src.source_id, src.distill_ord, src.next_ord, LOAD_CHARS)?;
            out.main_cut = cut;
            if let (Some(first), Some(last)) = (main.first().map(|r| r.1), main.last().map(|r| r.1))
            {
                let starts = store::turn_starts(c, &src.source_id, first, last)?;
                out.main_last = Some(last);
                for (id, ord, text) in main {
                    // Supplement item 11: the session's first chunk goes by the text rule, so a
                    // session opened with a slash command does not have it quoted as a prompt.
                    let turn_start = (ord != 0).then(|| starts.contains(&ord));
                    out.chunks.push(RunChunk {
                        id,
                        ord,
                        text,
                        subagent: false,
                        turn_start,
                    });
                }
            }
            let mut budget = SUBAGENT_CHARS;
            for (i, (k, _)) in kids.iter().enumerate() {
                if budget == 0 {
                    out.children_cut = true;
                    break;
                }
                let (rows, cut) = page(c, &k.source_id, k.distill_ord, k.next_ord, budget)?;
                out.children_cut |= cut;
                let (Some(first), Some(last)) =
                    (rows.first().map(|r| r.1), rows.last().map(|r| r.1))
                else {
                    out.children.push((k.source_id.clone(), k.next_ord));
                    continue;
                };
                let starts = store::turn_starts(c, &k.source_id, first, last)?;
                for (n, (id, ord, text)) in rows.into_iter().enumerate() {
                    budget = budget.saturating_sub(text.chars().count());
                    out.owner.insert(id.clone(), i);
                    out.chunks.push(RunChunk {
                        id,
                        ord,
                        text,
                        subagent: true,
                        // The first chunk of each subagent source opens a turn: `prepare`
                        // cannot see where one source ends and the next begins.
                        turn_start: Some(n == 0 || starts.contains(&ord)),
                    });
                }
                out.children.push((k.source_id.clone(), last + 1));
            }
            Ok(out)
        })
    }

    /// The periodic part: the stale end, the quiet trigger, and sources waiting for the
    /// judge, the budget or the cloud policy that may go on now.
    fn housekeeping(&self) -> Result<()> {
        let now = Utc::now();
        let s = self.ctx.settings.get();
        // Server tenants have no sweep: a source with no capture for 24 h is ended here, and the
        // end makes its session due (spec §6.2, §10; Task 11b) when distillation is on and it,
        // or a subagent of it, captured within the last 7 days (`QUIET_RECENT`) plus the stale
        // age (8 days in all): an older one is distilled only when asked, so turning distillation
        // on makes no backlog. Locally the capture sweep owns the stale end, with the same window:
        // a final pass that closes the trailing turn and marks the session due, or for a source it
        // cannot read, the same end as here.
        if self.ctx.settings.is_tenant() {
            let stale_at = now - since(STALE_AFTER);
            let stale_before = stamp(stale_at);
            let due_since = (s.distill.provider != DistillProvider::Off)
                .then(|| stamp(stale_at - since(QUIET_RECENT)));
            for id in self
                .ctx
                .readers
                .with(|c| store::stale_open(c, &stale_before, HOUSEKEEPING_BATCH))?
            {
                tracing::info!(
                    source = id,
                    "a captured session had no capture for 24 h; marking it ended"
                );
                self.change(
                    &id,
                    CaptureChange::StaleEnd {
                        due_since: due_since.clone(),
                    },
                )?;
            }
        }
        if s.distill.provider == DistillProvider::Off {
            return Ok(());
        }
        let idle = self
            .idle_override
            .unwrap_or(Duration::from_secs(u64::from(s.distill.idle_minutes) * 60));
        // Quiet sessions captured within the last week; none while their capture is switched
        // off (they would only go back to idle).
        let quiet_since = stamp(now - since(idle));
        let recent_since = stamp(now - since(QUIET_RECENT));
        for (id, agent) in self.ctx.readers.with(|c| {
            store::quiet_undistilled(c, &quiet_since, &recent_since, HOUSEKEEPING_BATCH)
        })? {
            if AgentKind::parse(&agent).is_some_and(|a| capture_on(&s, a)) {
                self.set(&id, DistillState::Due, None, 0)?;
            }
        }
        // Waiting for the judge: due again once Jev is the judge in force.
        let judge = self.ctx.judge.current();
        if self.judge_wait(&s, judge.as_ref()).is_none() {
            for src in self.ctx.readers.with(|c| {
                store::in_state(c, DistillState::WaitingJudge, None, HOUSEKEEPING_BATCH)
            })? {
                self.set(
                    &src.source_id,
                    DistillState::Due,
                    None,
                    src.distill_attempts,
                )?;
            }
        }
        // Waiting for the budget: due at the next UTC day, or at once after a raise.
        let raised = {
            let mut stop = self.budget_stop.lock();
            let raised = stop.is_some_and(|(budget, share)| {
                s.judge.daily_budget_usd > budget || s.distill.budget_share > share
            });
            if raised {
                *stop = None;
            }
            raised
        };
        let now_s = stamp(now);
        let at = (!raised).then_some(now_s.as_str());
        for src in self
            .ctx
            .readers
            .with(|c| store::in_state(c, DistillState::WaitingBudget, at, HOUSEKEEPING_BATCH))?
        {
            self.set(
                &src.source_id,
                DistillState::Due,
                None,
                src.distill_attempts,
            )?;
        }
        // Skipped for the cloud: the policy is asked again every five ticks (5 minutes at the
        // daemon's tick), once per project.
        let every = self.tick.saturating_mul(5).min(CLOUD_RECHECK);
        let recheck = {
            let mut checked = self.cloud_checked.lock();
            let due = checked.is_none_or(|t| t.elapsed() >= every);
            if due {
                *checked = Some(Instant::now());
            }
            due
        };
        if recheck {
            let skipped = self.ctx.readers.with(|c| {
                let mut by_project: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
                for src in store::in_state(c, DistillState::SkippedCloud, None, HOUSEKEEPING_BATCH)?
                {
                    if !by_project.contains_key(&src.project_id) {
                        let slug = projects::get_by_id(c, &src.project_id)?.slug;
                        by_project.insert(src.project_id.clone(), (slug, Vec::new()));
                    }
                    if let Some((_, ids)) = by_project.get_mut(&src.project_id) {
                        ids.push(src.source_id);
                    }
                }
                Ok(by_project)
            })?;
            for (slug, ids) in skipped.into_values() {
                if self.ctx.judge.cloud_allowed(&slug) {
                    for id in ids {
                        self.set(&id, DistillState::Due, None, 0)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Where distillation stands (spec §8.4), for health, `alice capture status` and the doctor.
    pub(crate) fn view(&self, conn: &Connection) -> Result<DistillView> {
        let s = self.ctx.settings.get();
        if s.distill.provider == DistillProvider::Off {
            return Ok(DistillView::plain("off"));
        }
        let judge = self.ctx.judge.current();
        match self.judge_wait(&s, judge.as_ref()) {
            Some(JudgeWait::NotJev) => {
                return Ok(DistillView::warn(
                    "waiting_judge",
                    "distillation waits for the Jev judge: judge.provider is not jev, and the judge is what removes doubles",
                    "`alice config set judge.provider jev`",
                ));
            }
            Some(JudgeWait::NoKey) => {
                return Ok(DistillView::warn(
                    "no_key",
                    "no Jev key is set, so distillation waits",
                    "`alice config set-secret jev_api_key`",
                ));
            }
            Some(JudgeWait::NotReady(why)) => {
                let why = why.map(|w| format!(": {w}")).unwrap_or_default();
                return Ok(DistillView::warn(
                    "waiting_judge",
                    format!("distillation waits for the Jev judge to start{why}"),
                    "nothing to do while it starts; otherwise see the judge line",
                ));
            }
            None => {}
        }
        if let BreakerState::Open { until } = self.breaker_state() {
            return Ok(DistillView::warn(
                "breaker_open",
                format!(
                    "Jev failed {FAILURES_TO_OPEN} times; it is tried again at {}",
                    until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
                "nothing to do: raw capture goes on, and distillation resumes by itself",
            ));
        }
        let waiting: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM capture_state
                           WHERE distill_state = 'waiting_budget' AND distill_next_try_at > ?1)",
            [now_rfc3339()],
            |r| r.get(0),
        )?;
        if waiting {
            return Ok(DistillView::warn(
                "waiting_budget",
                "today's distillation share is used up",
                "raise distill.budget_share or judge.daily_budget_usd, or wait for the next UTC day",
            ));
        }
        Ok(DistillView::plain("ready"))
    }

    /// The capture line of the health with the distiller's part filled in (spec §8.4).
    pub(crate) fn fill_health(
        &self,
        conn: &Connection,
        health: Option<Box<alice_core::api::CaptureHealth>>,
    ) -> Result<Option<Box<alice_core::api::CaptureHealth>>> {
        let Some(mut h) = health else {
            return Ok(None);
        };
        let v = self.view(conn)?;
        h.distill_state = v.state.into();
        h.distill_warning = v.warning;
        let since = stamp(Utc::now() - chrono::Duration::hours(24));
        let (distilled, dropped) = distilled_since(conn, &since)?;
        h.distilled_24h = distilled;
        h.distill_dropped_24h = dropped;
        Ok(Some(h))
    }

    /// Marks the session or source `id` due now (`alice capture distill`, spec §8.3): a capture
    /// source's id or uri, or a session id (its main sources, of every agent). A subagent stands
    /// for its session. Any state becomes due, a backfilled or failed one too, with its
    /// attempts cleared. Returns the uris marked. Blocks (reader pool and writer).
    pub(crate) fn mark_due_now(&self, id: &str) -> Result<Vec<String>> {
        let id = id.trim();
        if id.is_empty() || id.chars().count() > 512 {
            return Err(AliceError::invalid(
                "name a captured session by its id (`alice capture status` lists them)",
            ));
        }
        let found = self.ctx.readers.with(|c| {
            let direct = match store::by_source(c, id)? {
                Some(s) => Some(s),
                None => store::by_uri(c, id)?,
            };
            let named = match direct {
                Some(s) => vec![s],
                None => store::sessions(c, id)?,
            };
            let mut out: Vec<CaptureState> = Vec::new();
            let mut seen = HashSet::new();
            for s in named {
                let s = match &s.parent_source_id {
                    Some(p) => store::by_source(c, p)?.unwrap_or(s),
                    None => s,
                };
                if seen.insert(s.source_id.clone()) {
                    out.push(s);
                }
            }
            Ok(out)
        })?;
        if found.is_empty() {
            return Err(AliceError::NotFound(format!(
                "no captured session or source \"{id}\"; `alice capture status` lists the captured sessions"
            )));
        }
        for s in &found {
            self.set(&s.source_id, DistillState::Due, None, 0)?;
        }
        self.wake();
        Ok(found.into_iter().map(|s| s.uri).collect())
    }
}

/// `(distilled, dropped)`: memories distillation stored since `since`, and distilled memories
/// trashed since then (duplicates the judge merged, and the like).
pub(crate) fn distilled_since(conn: &Connection, since: &str) -> Result<(u64, u64)> {
    let count = |sql: &str| -> Result<u64> {
        Ok(conn.query_row(sql, [since], |r| r.get::<_, i64>(0))? as u64)
    };
    Ok((
        count("SELECT count(*) FROM memories WHERE origin = 'distill' AND created_at >= ?1")?,
        count(
            "SELECT count(*) FROM memories
             WHERE origin = 'distill' AND status = 'trashed' AND updated_at >= ?1",
        )?,
    ))
}

/// Writes one distilled memory (spec §6.5) on the calling thread: the title and body redacted
/// with the rules of the source's project (a project whose rules cannot be read is screened
/// closed), then `remember` with `origin = distill`, its chunks as provenance, its idempotency
/// key, `distill.importance`, its rank inputs, and the source's agent and session as the author,
/// in the source's project (or `global` for a preference of the user's). When it takes the place
/// of a member in its session's best set, that member leaves once the judge finds this one live
/// (Task 12c review I1, option b: the writer's `ApplyJudgement`), not here.
pub(crate) fn write_memory(
    ctx: &DistillCtx,
    m: &DistilledMemory,
    src: &CaptureState,
) -> Result<InsertOutcome> {
    let (slug, tree) = ctx.readers.with(|c| {
        let slug = projects::get_by_id(c, &src.project_id)?.slug;
        let tree = projects::tree_of(c, &src.project_id);
        Ok((slug, tree))
    })?;
    let tree = tree.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "a project's rules could not be read; its distilled memories are screened closed");
        Some(ProjectTree::unreadable(slug.clone(), None))
    });
    let r = ctx.redactors.for_root(tree.as_ref());
    let body = r.check("content", &m.body)?;
    // A rule added since the capture may redact the title: it never keeps a marker (Task 12d).
    let title = distill::clean_title(&r.check("title", &m.title)?, &body);
    let input = NewMemoryInput {
        content: body,
        kind: Some(m.kind),
        title: Some(title),
        importance: Some(ctx.settings.get().distill.importance),
        idempotency_key: Some(m.idempotency_key.clone()),
        author: Author {
            agent: Some(src.agent.clone()),
            model: None,
            session: Some(src.session_id.clone()),
        },
        origin: Origin::Distill,
        provenance: m.chunk_ids.clone(),
        ..Default::default()
    };
    let project = ProjectChoice::Slug(if m.to_global { "global".into() } else { slug });
    match ctx.writer.submit_blocking(WriteOp::Distilled {
        input,
        project,
        p: m.p,
        subagent: m.subagent,
    })? {
        WriteOutcome::Remembered(out, _) => Ok(out),
        other => Err(AliceError::Other(format!(
            "unexpected writer outcome {}",
            other.name()
        ))),
    }
}

/// The other names of project `project_id` for distillation's routing guard (spec §6.5): its
/// name, and the folder and path of its working tree when it has one.
fn project_names(c: &Connection, project_id: &str) -> Result<Vec<String>> {
    let mut names = vec![projects::get_by_id(c, project_id)?.name];
    if let Some(root) = projects::root_of(c, project_id)? {
        if let Some(folder) = std::path::Path::new(&root).file_name() {
            names.push(folder.to_string_lossy().into_owned());
        }
        names.push(root);
    }
    Ok(names)
}

/// A stored chunk: `(id, ord, text)`.
type ChunkRow = (String, i64, String);

/// Chunks of `source_id` with `from <= ord < until`, about `max_chars` characters (at least one
/// chunk), and whether more of them follow.
fn page(
    c: &Connection,
    source_id: &str,
    from: i64,
    until: i64,
    max_chars: usize,
) -> Result<(Vec<ChunkRow>, bool)> {
    let mut out = Vec::new();
    let mut chars = 0usize;
    let mut next = from;
    loop {
        let rows = store::chunks_between(c, source_id, next, until, PAGE)?;
        let full = rows.len() == PAGE;
        for (id, ord, text) in rows {
            if !out.is_empty() && chars >= max_chars {
                return Ok((out, true));
            }
            chars += text.chars().count();
            next = ord + 1;
            out.push((id, ord, text));
        }
        if !full {
            return Ok((out, false));
        }
    }
}

/// Whether a capture verdict's exclusion is a global switch (not a project's rule).
fn switch_off(why: &str) -> bool {
    why == EXCLUDED_CAPTURE_OFF || why == EXCLUDED_AGENT_OFF
}

/// Whether the global capture switches are on for `agent` (`capture.enabled` and the agent's).
fn capture_on(s: &Settings, agent: AgentKind) -> bool {
    s.capture.enabled
        && match agent {
            AgentKind::ClaudeCode => s.capture.claude_code,
            AgentKind::Codex => s.capture.codex,
        }
}

/// `d` as a chrono duration (zero if it does not fit).
fn since(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::zero())
}

/// The wait after failed attempt number `attempts` (1, 2, …): 5 minutes, doubling, at most 6 h.
fn backoff(attempts: i64) -> Duration {
    let doublings = u32::try_from(attempts.saturating_sub(1).clamp(0, 16)).unwrap_or(16);
    RETRY_FIRST
        .saturating_mul(2u32.saturating_pow(doublings))
        .min(RETRY_MAX)
}

/// RFC 3339 with milliseconds and `Z`, as every stored time.
fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `d` from now, stamped.
fn after(d: Duration) -> String {
    stamp(Utc::now() + chrono::Duration::from_std(d).unwrap_or(chrono::Duration::zero()))
}

/// The start of the next UTC day, stamped.
fn next_utc_midnight() -> String {
    let tomorrow = Utc::now().date_naive() + chrono::Duration::days(1);
    stamp(
        tomorrow
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists")
            .and_utc(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §6.7: three failures in a row open the breaker for 15 minutes; after an opening,
    /// one more failure reopens it for twice as long, up to 6 h; a success closes it and
    /// starts the count again.
    #[test]
    fn the_breaker_opens_after_three_and_doubles_up_to_six_hours() {
        let t0 = Instant::now();
        let mut b = Breaker::default();
        assert!(
            !b.failure(t0) && !b.failure(t0),
            "two failures keep it closed"
        );
        assert!(!b.is_open(t0));
        assert!(b.failure(t0), "the third opens it");
        assert!(b.is_open(t0) && b.is_open(t0 + OPEN_FIRST - Duration::from_secs(1)));
        assert!(!b.is_open(t0 + OPEN_FIRST));
        assert_eq!(b.open_until(), Some(t0 + OPEN_FIRST));
        // Its time is up and the trial request fails: open again, twice as long.
        let t1 = t0 + OPEN_FIRST;
        assert!(b.failure(t1));
        assert_eq!(b.open_until(), Some(t1 + 2 * OPEN_FIRST));
        let mut t = t1;
        for _ in 0..10 {
            t += OPEN_MAX;
            b.failure(t);
        }
        assert_eq!(b.open_until(), Some(t + OPEN_MAX), "never longer than 6 h");
        b.success();
        assert!(!b.is_open(t) && b.open_until().is_none());
        assert!(!b.failure(t) && !b.failure(t), "counting starts again");
        assert!(b.failure(t));
        assert_eq!(b.open_until(), Some(t + OPEN_FIRST), "back to 15 minutes");
    }

    /// Spec §6.7 and the brief: a failed run waits 5 minutes, doubling, at most 6 h.
    #[test]
    fn a_failed_source_backs_off_from_five_minutes_to_six_hours() {
        let mins = |n: i64| backoff(n).as_secs() / 60;
        assert_eq!([mins(1), mins(2), mins(3), mins(4)], [5, 10, 20, 40]);
        assert_eq!(backoff(10), RETRY_MAX);
        assert_eq!(backoff(1_000), RETRY_MAX);
        assert_eq!(backoff(0), RETRY_FIRST);
    }

    /// Fix round 2: an exclusion by a global switch is told apart from a project's own rule
    /// (the switch leaves a session idle; the rule skips it).
    #[test]
    fn a_global_switch_is_not_a_project_rule() {
        assert!(switch_off(EXCLUDED_CAPTURE_OFF) && switch_off(EXCLUDED_AGENT_OFF));
        assert!(!switch_off("project excluded"));
        assert!(!switch_off("the project's .alice.toml says no_capture"));
    }

    #[test]
    fn the_next_utc_day_starts_at_midnight() {
        let next = next_utc_midnight();
        assert!(next.ends_with("T00:00:00.000Z"), "{next}");
        let at = DateTime::parse_from_rfc3339(&next).unwrap();
        let left = at.with_timezone(&Utc) - Utc::now();
        assert!(left > chrono::Duration::zero() && left <= chrono::Duration::days(1));
    }
}
