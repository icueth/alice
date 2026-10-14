//! The embedding model in force and changes of it (spec §4.2).
//!
//! The service keeps the model in force in an [`ActiveSlot`]. [`plan`] picks it at start: the
//! model `meta.active_embedding` records (so a model change made while the daemon was stopped
//! is a *change* to carry out, not a silent switch). [`EmbeddingCtl::reconcile`] then compares
//! it with `[embedding]` in the settings, on a thread of its own: a different model starts a
//! background re-embed (`reembed`), the same model with another endpoint or key rebuilds the
//! embedder in place, and a stand-in embedder (the provider was unreachable at start) is
//! replaced as soon as the real one can be built. It runs at start, on every change of
//! `embedding.*`, when the embedding key is set or deleted, and once a minute.

use crate::config::SettingsHandle;
use crate::maintenance::Activity;
use crate::paths::AliceHome;
use crate::reembed::{self, ACTIVE_KEY, Progress, ReembedConfig, TARGET_KEY};
use crate::secrets::{self, SecretName, SecretStore};
use crate::service::{Active, ActiveSlot, IndexSlot, SecretEnv, snapshot};
use crate::writer::{WriteOp, WriterHandle};
use alice_core::api::ReembedStatus;
use alice_core::embed::{Embedder, UnavailableEmbedder};
use alice_core::error::Result;
use alice_core::judge::jev::JevPolicy;
use alice_core::settings::{EmbeddingProvider, EmbeddingSettings, SecretValue};
use alice_core::store::ReaderPool;
use alice_core::store::maintenance::get_meta;
use alice_embed::{EmbedderFactory, MODEL_ID, expected_model_id};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the control thread compares the settings with the model in force on its own.
const RECONCILE_EVERY: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(60)
};

/// Builds embedders from settings: the factory plus the `embedding_api_key` secret.
#[derive(Clone)]
pub struct Builder {
    pub factory: EmbedderFactory,
    pub secrets: Arc<dyn SecretStore>,
    pub env: SecretEnv,
}

impl Builder {
    /// The key for `s`: only a provider that needs one is given it (a key set for OpenAI is
    /// never sent to a local Ollama). Read from the store each time, so a rotated key is used.
    fn key(&self, s: &EmbeddingSettings) -> Result<Option<SecretValue>> {
        if !matches!(
            s.provider,
            EmbeddingProvider::Openai | EmbeddingProvider::Voyage
        ) {
            return Ok(None);
        }
        Ok(secrets::resolve_with(
            self.secrets.as_ref(),
            SecretName::EmbeddingApiKey,
            self.env.as_ref(),
        )?
        .map(|(k, _)| k))
    }

    /// May talk to the provider (the dimension is probed): off the runtime's threads.
    pub fn build(&self, s: &EmbeddingSettings) -> Result<Arc<dyn Embedder>> {
        (self.factory)(s, self.key(s)?)
    }
}

/// What [`plan`] decided for the start.
pub struct StartPlan {
    pub active: Arc<dyn Embedder>,
    /// The settings to record as the model in force (`None`: a stand-in, which is not the
    /// model in force as far as the next start is concerned).
    pub record: Option<EmbeddingSettings>,
    /// Why the model could not be built (shown in health), if so.
    pub error: Option<String>,
}

/// The model id of the embedder that stands in when nothing is known about the model to use
/// (a new database whose provider is not reached yet): it has no vectors and answers nothing.
pub const PLACEHOLDER_MODEL: &str = "pending-embedder";

fn parse(json: &str) -> Option<EmbeddingSettings> {
    serde_json::from_str(json).ok()
}

fn model_dim(conn: &Connection, model: &str) -> Option<usize> {
    conn.query_row(
        "SELECT dim FROM embeddings WHERE model = ?1 LIMIT 1",
        [model],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|d| d as usize)
}

/// Picks the embedder in force at start, **without calling any provider**: the daemon always
/// starts, however long a provider takes to answer or whether it answers at all. It is the model
/// `meta.active_embedding` records (the local model when nothing is recorded but its vectors are
/// there); when that is also the model the settings ask for, it is built from the settings (so a
/// new endpoint is used). A different model asked for is carried out afterwards by
/// [`EmbeddingCtl::reconcile`], the old one serving meanwhile.
///
/// The local model is built here (it is a file load). An HTTP provider's embedder is built by
/// the control thread: until then a stand-in keeps the model's stored vectors searchable (its
/// dimension is known from them) and queues new text; a new database has no vectors to keep, so
/// a placeholder stands in until the control thread switches to the model asked for.
pub fn plan(b: &Builder, conn: &Connection, want: &EmbeddingSettings) -> Result<StartPlan> {
    let want_id = expected_model_id(want);
    // Process-level knobs always come from the current settings.
    let knobs = |mut s: EmbeddingSettings| {
        s.idle_unload = want.idle_unload;
        s.batch_size = want.batch_size;
        s
    };
    let stored = get_meta(conn, ACTIVE_KEY)?
        .as_deref()
        .and_then(parse)
        .map(knobs)
        .or_else(|| {
            // An M1 or early-M2 database: the local model's vectors, nothing recorded.
            model_dim(conn, MODEL_ID).map(|_| knobs(EmbeddingSettings::default()))
        });
    let settings = match stored {
        Some(st) if expected_model_id(&st) != want_id => st,
        _ => want.clone(),
    };
    let id = expected_model_id(&settings);
    let dim = model_dim(conn, &id);
    let stand_in = |reason: String, remote: bool, error: Option<String>| {
        let e: Arc<dyn Embedder> = match dim {
            Some(dim) => Arc::new(UnavailableEmbedder::new(id.clone(), dim, remote, reason)),
            None => Arc::new(UnavailableEmbedder::new(PLACEHOLDER_MODEL, 1, true, reason)),
        };
        StartPlan {
            active: e,
            record: None,
            error,
        }
    };
    if settings.provider != EmbeddingProvider::Local {
        return Ok(stand_in(
            format!("connecting to {id}; searches use word matches only until it answers"),
            settings.is_remote(),
            None,
        ));
    }
    Ok(match b.build(&settings) {
        Ok(e) => StartPlan {
            active: e,
            record: Some(settings),
            error: None,
        },
        Err(err) => {
            let reason = format!("cannot use {id}: {err}");
            stand_in(reason.clone(), false, Some(reason))
        }
    })
}

struct Running {
    target_model: String,
    stop: Arc<AtomicBool>,
    progress: Arc<Progress>,
    handle: JoinHandle<()>,
}

/// Runs the changes of the embedding model; see the module docs.
pub struct EmbeddingCtl {
    pub builder: Option<Builder>,
    pub home: AliceHome,
    pub active: Arc<ActiveSlot>,
    pub index: Arc<IndexSlot>,
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub settings: Arc<SettingsHandle>,
    pub policy: Arc<dyn JevPolicy>,
    pub activity: Arc<Activity>,
    /// Told to the main embedding worker (see `WorkerConfig`).
    pub main_paused: Arc<AtomicBool>,
    pub parked: Arc<AtomicBool>,
    pub recheck_skipped: Arc<AtomicBool>,
    /// Why the model asked for is not in force (a re-embed stopped, a provider cannot be built).
    pub error: Arc<RwLock<Option<String>>>,
    /// The embedding key was set or deleted: the embedder is built again.
    pub rebuild: AtomicBool,
    /// The service is stopping.
    pub stop: Arc<AtomicBool>,
    pub retry_wait: Duration,
    running: Mutex<Option<Running>>,
    wake: Sender<()>,
}

impl EmbeddingCtl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        builder: Option<Builder>,
        home: AliceHome,
        active: Arc<ActiveSlot>,
        index: Arc<IndexSlot>,
        readers: Arc<ReaderPool>,
        writer: WriterHandle,
        settings: Arc<SettingsHandle>,
        policy: Arc<dyn JevPolicy>,
        activity: Arc<Activity>,
        main_paused: Arc<AtomicBool>,
        parked: Arc<AtomicBool>,
        recheck_skipped: Arc<AtomicBool>,
        error: Arc<RwLock<Option<String>>>,
        stop: Arc<AtomicBool>,
        retry_wait: Duration,
    ) -> (Arc<Self>, Receiver<()>) {
        let (wake, rx) = crossbeam_channel::bounded(1);
        let ctl = Arc::new(Self {
            builder,
            home,
            active,
            index,
            readers,
            writer,
            settings,
            policy,
            activity,
            main_paused,
            parked,
            recheck_skipped,
            error,
            rebuild: AtomicBool::new(false),
            stop,
            retry_wait,
            running: Mutex::new(None),
            wake,
        });
        (ctl, rx)
    }

    /// Ask the control thread to compare the settings with the model in force now.
    pub fn poke(&self) {
        let _ = self.wake.try_send(());
    }

    /// The control thread: [`reconcile`](Self::reconcile) on every poke and once a minute.
    pub fn spawn(self: &Arc<Self>, rx: Receiver<()>) -> std::io::Result<JoinHandle<()>> {
        let ctl = self.clone();
        std::thread::Builder::new()
            .name("alice-embedding".into())
            .spawn(move || {
                while !ctl.stop.load(Ordering::SeqCst) {
                    match rx.recv_timeout(RECONCILE_EVERY) {
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    }
                    if ctl.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    ctl.reconcile();
                }
            })
    }

    /// Progress of the re-embed running now, if one is.
    pub fn status(&self) -> Option<ReembedStatus> {
        self.running
            .lock()
            .as_ref()
            .filter(|r| !r.progress.is_finished())
            .map(|r| r.progress.status())
    }

    /// Why the model asked for is not in force, or why the embedder in force is a stand-in.
    pub fn problem(&self) -> Option<String> {
        self.error
            .read()
            .clone()
            .or_else(|| self.active.read().embedder.problem())
    }

    /// Stops the re-embed running now and waits for its thread. A batch in flight finishes
    /// first (a remote one may take up to its timeout).
    pub fn stop_run(&self) {
        let run = self.running.lock().take();
        if let Some(run) = run {
            run.stop.store(true, Ordering::SeqCst);
            if run.handle.join().is_err() {
                tracing::error!("the re-embed thread panicked");
            }
            self.main_paused.store(false, Ordering::SeqCst);
        }
    }

    /// Asks the re-embed running now to stop, without waiting for it (the service is dropped
    /// without a `shutdown`).
    pub fn signal_stop(&self) {
        if let Some(run) = self.running.lock().as_ref() {
            run.stop.store(true, Ordering::SeqCst);
        }
        self.poke();
    }

    /// Stops a re-embed that is no longer wanted and forgets its target.
    fn cancel_run(&self) {
        if self.running.lock().is_some() {
            self.stop_run();
        }
        if meta_target(&self.readers).is_some() {
            let _ = self.writer.submit_blocking(WriteOp::SetMeta {
                key: TARGET_KEY.into(),
                value: String::new(),
            });
            let _ = self.writer.submit_blocking(WriteOp::SetReembedTarget(None));
        }
    }

    fn set_error(&self, msg: Option<String>) {
        *self.error.write() = msg;
    }

    /// Makes the model in force match `[embedding]`.
    pub fn reconcile(&self) {
        let Some(b) = &self.builder else { return };
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        let want = self.settings.get().embedding.clone();
        let want_id = expected_model_id(&want);
        let (active, _) = snapshot(&self.active, &self.index);
        // Without a valid config.toml the settings in force are defaults: they say nothing
        // about the model to use, and no text may go to a cloud provider. Keep the model in
        // force (an interrupted re-embed is left as it is).
        if self.settings.config_degraded() {
            self.set_error(Some(format!(
                "config.toml has not loaded as a valid file since the daemon started, so the embedding settings are not applied; {} stays in force until it does",
                active.model
            )));
            return;
        }
        // Collect a finished run (it has switched, or given up).
        {
            let mut running = self.running.lock();
            if running.as_ref().is_some_and(|r| r.handle.is_finished())
                && let Some(r) = running.take()
            {
                let _ = r.handle.join();
            }
            if let Some(r) = running.as_ref()
                && r.target_model == want_id
            {
                return; // this very target is being filled
            }
        }
        if active.model == want_id {
            self.cancel_run();
            self.refresh_in_place(b, &want, &active);
            return;
        }
        // Another model is asked for than the one in force. Whatever re-embed runs is for
        // some third model.
        let resume = meta_target(&self.readers).is_some_and(|t| expected_model_id(&t) == want_id);
        self.stop_run();
        let target = match b.build(&want) {
            Ok(t) => t,
            Err(e) => {
                self.set_error(Some(format!(
                    "cannot switch to {want_id}: {e}; {} stays in force",
                    active.model
                )));
                return;
            }
        };
        // A build can be slow (the provider is called): the service may be stopping by now.
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        self.start_run(target, &want, resume);
    }

    /// The model in force is the one asked for: rebuild its embedder when the endpoint changed,
    /// the key was set or deleted, or it is a stand-in for an unreachable provider.
    fn refresh_in_place(&self, b: &Builder, want: &EmbeddingSettings, active: &Arc<Active>) {
        let key_changed = self.rebuild.swap(false, Ordering::SeqCst);
        let stand_in = active.embedder.problem().is_some();
        // The endpoint (or provider) changed under the same model id: the settings recorded
        // for the model in force are those it was built from.
        let moved = recorded_active(&self.readers)
            .is_none_or(|r| r.provider != want.provider || r.endpoint != want.endpoint);
        if !(key_changed || stand_in || moved) {
            self.set_error(None);
            return;
        }
        match b.build(want) {
            Ok(e) if e.model_id() == active.model && e.dim() == active.embedder.dim() => {
                *self.active.write() = Arc::new(Active {
                    embedder: e,
                    model: active.model.clone(),
                    vector_path: active.vector_path.clone(),
                });
                let _ = self.writer.submit_blocking(WriteOp::SetMeta {
                    key: ACTIVE_KEY.into(),
                    value: serde_json::to_string(want).expect("settings serialise"),
                });
                self.set_error(None);
                self.recheck_skipped.store(true, Ordering::SeqCst);
                tracing::info!(model = %active.model, "rebuilt the embedder");
            }
            Ok(_) => {
                self.set_error(Some(format!(
                    "{} now answers with vectors of another size; set embedding.model to a different model so its vectors are made again",
                    active.model
                )));
            }
            Err(e) => {
                // Nothing keeps being sent with a key that was removed, or to an endpoint that
                // no longer answers: the model's vectors stay searchable, texts stay queued.
                let reason = format!("cannot use {}: {e}", active.model);
                if key_changed || moved {
                    *self.active.write() = Arc::new(Active {
                        embedder: Arc::new(UnavailableEmbedder::new(
                            active.model.clone(),
                            active.embedder.dim(),
                            want.is_remote(),
                            reason.clone(),
                        )),
                        model: active.model.clone(),
                        vector_path: active.vector_path.clone(),
                    });
                }
                self.set_error(Some(reason));
            }
        }
    }

    fn start_run(&self, target: Arc<dyn Embedder>, want: &EmbeddingSettings, resume: bool) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        let model = target.model_id().to_string();
        let progress = Arc::new(Progress::new(&model));
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = ReembedConfig {
            readers: self.readers.clone(),
            writer: self.writer.clone(),
            target,
            target_settings: serde_json::to_string(want).expect("settings serialise"),
            vector_path: self.home.vector_file(&model),
            active: self.active.clone(),
            index: self.index.clone(),
            main_paused: self.main_paused.clone(),
            parked: self.parked.clone(),
            stop: stop.clone(),
            progress: progress.clone(),
            batch: want.batch_size.max(1),
            error: self.error.clone(),
            policy: Some(self.policy.clone()),
            activity: self.activity.clone(),
            resume,
            recheck_skipped: self.recheck_skipped.clone(),
            retry_wait: self.retry_wait,
        };
        let handle = reembed::spawn_reembed(cfg);
        *self.running.lock() = Some(Running {
            target_model: model,
            stop,
            progress,
            handle,
        });
    }
}

/// The settings recorded for the model in force.
fn recorded_active(readers: &ReaderPool) -> Option<EmbeddingSettings> {
    readers
        .with(|c| get_meta(c, ACTIVE_KEY))
        .ok()
        .flatten()
        .as_deref()
        .and_then(parse)
}

/// The settings of an interrupted re-embed, if one is recorded.
fn meta_target(readers: &ReaderPool) -> Option<EmbeddingSettings> {
    readers
        .with(|c| get_meta(c, TARGET_KEY))
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
        .as_deref()
        .and_then(parse)
}

#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;
