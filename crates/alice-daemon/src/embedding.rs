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
use alice_core::settings::{EmbeddingProvider, EmbeddingSettings};
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
    fn key(&self, s: &EmbeddingSettings) -> Result<Option<String>> {
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
        .map(|(k, _)| k.expose().to_string()))
    }

    /// May talk to the provider (the dimension is probed): off the runtime's threads.
    pub fn build(&self, s: &EmbeddingSettings) -> Result<Arc<dyn Embedder>> {
        (self.factory)(s, self.key(s)?)
    }
}

/// What [`plan`] decided for the start.
pub struct StartPlan {
    pub active: Arc<dyn Embedder>,
    /// The settings to record as the model in force (`None`: a stand-in or fallback, which is
    /// not the model in force as far as the next start is concerned).
    pub record: Option<EmbeddingSettings>,
    /// Why the model asked for could not be used, if so (shown in health).
    pub error: Option<String>,
}

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

/// Picks the embedder in force at start. It is the model `meta.active_embedding` records (the
/// local model when nothing is recorded but its vectors are there); when that is also the model
/// the settings ask for, it is built from the settings (so a new endpoint or key is used). A
/// different model asked for is carried out afterwards by [`EmbeddingCtl::reconcile`]: the old
/// one keeps serving meanwhile. If the model in force cannot be built (its provider is
/// unreachable), a stand-in keeps its vectors searchable and the daemon still starts.
pub fn plan(
    b: &Builder,
    conn: &Connection,
    want: &EmbeddingSettings,
    fallback: &dyn Fn() -> Result<Arc<dyn Embedder>>,
) -> Result<StartPlan> {
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
    let Some(stored) = stored else {
        // A new database: the model asked for is the one in force.
        return Ok(match b.build(want) {
            Ok(e) => StartPlan {
                active: e,
                record: Some(want.clone()),
                error: None,
            },
            Err(err) => StartPlan {
                active: fallback()?,
                record: None,
                error: Some(format!(
                    "cannot use {want_id}: {err}; the built-in model is used until it works"
                )),
            },
        });
    };
    let stored_id = expected_model_id(&stored);
    let settings = if stored_id == want_id {
        want.clone()
    } else {
        stored
    };
    match b.build(&settings) {
        Ok(e) => Ok(StartPlan {
            active: e,
            record: Some(settings),
            error: None,
        }),
        Err(err) => {
            let id = expected_model_id(&settings);
            let reason = format!("cannot use {id}: {err}");
            match model_dim(conn, &id) {
                // Its vectors are stored: keep them searchable until the provider is back.
                Some(dim) => Ok(StartPlan {
                    active: Arc::new(UnavailableEmbedder::new(
                        id,
                        dim,
                        settings.is_remote(),
                        reason.clone(),
                    )),
                    record: None,
                    error: Some(reason),
                }),
                // No vectors to keep: use whatever works.
                None => match b.build(want) {
                    Ok(e) => Ok(StartPlan {
                        active: e,
                        record: Some(want.clone()),
                        error: None,
                    }),
                    Err(_) => Ok(StartPlan {
                        active: fallback()?,
                        record: None,
                        error: Some(reason),
                    }),
                },
            }
        }
    }
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
        error: Option<String>,
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
            error: Arc::new(RwLock::new(error)),
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
        if target.is_remote() && self.settings.config_degraded() {
            self.set_error(Some(format!(
                "cannot switch to {want_id}: config.toml has not loaded as a valid file since the daemon started, so no text may be sent to a cloud provider; {} stays in force",
                active.model
            )));
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

/// Test support shared by the tests of this module and of `reembed`: factories that build
/// named fakes, counting embedders, and a mock OpenAI-compatible server on the loopback.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::service::{Service, ServiceOptions};
    use alice_core::embed::FakeEmbedder;
    use alice_core::settings::EmbeddingProvider as P;
    use alice_embed::http::{HttpEmbedder, HttpProvider};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    /// Named fakes: the local id has 384 dimensions, any other 64.
    pub fn fake_factory() -> EmbedderFactory {
        Arc::new(|s: &EmbeddingSettings, _key| {
            let id = expected_model_id(s);
            let dim = if id == MODEL_ID { 384 } else { 64 };
            Ok(Arc::new(FakeEmbedder::named(id, dim)) as Arc<dyn Embedder>)
        })
    }

    /// A target embedder that counts the texts it embeds, takes `delay` per batch, and notes
    /// how many embedders of this kind work at once.
    pub struct Counting {
        pub inner: FakeEmbedder,
        pub texts: Arc<AtomicUsize>,
        pub delay: Duration,
        pub remote: bool,
        /// Embedders working right now / the most there ever were (shared by a factory).
        pub busy: Arc<(AtomicUsize, AtomicUsize)>,
    }

    impl Embedder for Counting {
        fn model_id(&self) -> &str {
            self.inner.model_id()
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn embed_passages(&self, t: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
            let now = self.busy.0.fetch_add(1, Ordering::SeqCst) + 1;
            self.busy.1.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.texts.fetch_add(t.len(), Ordering::SeqCst);
            let r = self.inner.embed_passages(t);
            self.busy.0.fetch_sub(1, Ordering::SeqCst);
            r
        }
        fn embed_query(&self, q: &str) -> alice_core::Result<Vec<f32>> {
            self.inner.embed_query(q)
        }
        fn is_remote(&self) -> bool {
            self.remote
        }
    }

    /// The local model as before; any other model is a `Counting` fake.
    pub fn counting_factory(
        texts: Arc<AtomicUsize>,
        delay: Duration,
        remote: bool,
    ) -> EmbedderFactory {
        let busy: Arc<(AtomicUsize, AtomicUsize)> = Arc::default();
        counting_factory_shared(texts, delay, remote, busy)
    }

    pub fn counting_factory_shared(
        texts: Arc<AtomicUsize>,
        delay: Duration,
        remote: bool,
        busy: Arc<(AtomicUsize, AtomicUsize)>,
    ) -> EmbedderFactory {
        Arc::new(move |s: &EmbeddingSettings, _key| {
            let id = expected_model_id(s);
            if id == MODEL_ID {
                return Ok(Arc::new(FakeEmbedder::named(id, 384)) as Arc<dyn Embedder>);
            }
            Ok(Arc::new(Counting {
                inner: FakeEmbedder::named(id, 64),
                texts: texts.clone(),
                delay,
                remote,
                busy: busy.clone(),
            }) as Arc<dyn Embedder>)
        })
    }

    /// An embedder that says it is remote (the cloud policy applies), whatever its endpoint:
    /// the mock server lives on the loopback.
    pub struct ForceRemote(pub Arc<dyn Embedder>);

    impl Embedder for ForceRemote {
        fn model_id(&self) -> &str {
            self.0.model_id()
        }
        fn dim(&self) -> usize {
            self.0.dim()
        }
        fn embed_passages(&self, t: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
            self.0.embed_passages(t)
        }
        fn embed_query(&self, q: &str) -> alice_core::Result<Vec<f32>> {
            self.0.embed_query(q)
        }
        fn is_remote(&self) -> bool {
            true
        }
    }

    /// An OpenAI-compatible `/v1/embeddings` server on 127.0.0.1 that answers 8-dimension
    /// vectors and keeps every text it was sent.
    pub struct Mock {
        pub base: String,
        inputs: Arc<Mutex<Vec<String>>>,
        heads: Arc<Mutex<Vec<String>>>,
    }

    impl Mock {
        pub fn start() -> Self {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}/v1", l.local_addr().unwrap());
            let inputs: Arc<Mutex<Vec<String>>> = Arc::default();
            let heads: Arc<Mutex<Vec<String>>> = Arc::default();
            let (log, hlog) = (inputs.clone(), heads.clone());
            std::thread::spawn(move || {
                let embedder = FakeEmbedder::named("mock", 8);
                for s in l.incoming() {
                    let Ok(mut s) = s else { return };
                    let (log, hlog) = (log.clone(), hlog.clone());
                    let embedder = FakeEmbedder::named("mock", embedder.dim());
                    std::thread::spawn(move || {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 8192];
                        let (head, body) = loop {
                            let n = s.read(&mut chunk).unwrap_or(0);
                            buf.extend_from_slice(&chunk[..n]);
                            let text = String::from_utf8_lossy(&buf).to_string();
                            if let Some(i) = text.find("\r\n\r\n") {
                                let len: usize = text
                                    .lines()
                                    .find_map(|l| {
                                        l.to_ascii_lowercase()
                                            .strip_prefix("content-length: ")
                                            .map(|v| v.trim().parse().unwrap())
                                    })
                                    .unwrap_or(0);
                                if buf.len() >= i + 4 + len {
                                    break (text[..i].to_string(), text[i + 4..].to_string());
                                }
                            }
                            if n == 0 {
                                return;
                            }
                        };
                        let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let texts: Vec<String> = req["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|t| t.as_str().unwrap().to_string())
                            .collect();
                        log.lock().extend(texts.iter().cloned());
                        hlog.lock().push(head);
                        let data: Vec<serde_json::Value> = embedder
                            .embed_passages(&texts)
                            .unwrap()
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| serde_json::json!({"index": i, "embedding": v}))
                            .collect();
                        let body = serde_json::json!({"data": data}).to_string();
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                    });
                }
            });
            Self {
                base,
                inputs,
                heads,
            }
        }

        /// Every text received, the dimension probe left out.
        pub fn texts(&self) -> Vec<String> {
            self.inputs
                .lock()
                .iter()
                .filter(|t| *t != "dimension probe")
                .cloned()
                .collect()
        }

        /// Whether any text received contains `needle`.
        pub fn saw(&self, needle: &str) -> bool {
            self.texts().iter().any(|t| t.contains(needle))
        }

        /// The request heads received (they hold the headers).
        pub fn heads(&self) -> Vec<String> {
            self.heads.lock().clone()
        }
    }

    /// A factory over the mock: HTTP providers talk to the endpoint in the settings; with
    /// `force_remote` they count as remote even on the loopback. The local model is a fake.
    pub fn http_factory(
        force_remote: bool,
        keys: Arc<Mutex<Vec<Option<String>>>>,
    ) -> EmbedderFactory {
        Arc::new(move |s: &EmbeddingSettings, key| {
            keys.lock().push(key.clone());
            let provider = match s.provider {
                P::Local => {
                    return Ok(Arc::new(FakeEmbedder::named(MODEL_ID, 384)) as Arc<dyn Embedder>);
                }
                P::Openai => HttpProvider::OpenAi,
                P::Ollama => HttpProvider::Ollama,
                P::Voyage => HttpProvider::Voyage,
            };
            let e =
                HttpEmbedder::new(provider, &s.model, s.endpoint.as_deref(), key, s.batch_size)?
                    .with_backoff(Duration::from_millis(5));
            Ok(if force_remote {
                Arc::new(ForceRemote(Arc::new(e))) as Arc<dyn Embedder>
            } else {
                Arc::new(e) as Arc<dyn Embedder>
            })
        })
    }

    /// The key a test hands the factory through `ALICE_EMBEDDING_API_KEY` (never the shell's).
    pub const TEST_KEY: &str = concat!("sk-", "test-embedding-0123456789abcdef");

    /// Options with a factory, the test key in the (test-owned) environment, and fast polls.
    pub fn options(factory: EmbedderFactory) -> ServiceOptions {
        ServiceOptions {
            embedder_factory: Some(factory),
            secret_env: Some(Arc::new(|v: &str| {
                (v == "ALICE_EMBEDDING_API_KEY").then(|| TEST_KEY.to_string())
            })),
            reembed_retry_wait: Duration::from_millis(5),
            ..Default::default()
        }
    }

    /// Starts a service in `dir` (a home) whose `config.toml` holds `toml`.
    pub fn start(dir: &Path, opts: ServiceOptions, toml: &str) -> Arc<Service> {
        if !toml.is_empty() {
            std::fs::write(AliceHome::new(dir.to_path_buf()).config_file(), toml).unwrap();
        }
        Service::start(
            AliceHome::new(dir.to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::named(MODEL_ID, 384)) as Arc<dyn Embedder>),
            opts,
        )
        .unwrap()
    }

    /// Until `cond` holds, polling every 20 ms for up to `secs`.
    pub async fn until<F, Fut>(what: &str, secs: u64, mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while !cond().await {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A directory under `d` to use as a working directory (its name is the project's slug).
    pub fn repo(d: &tempfile::TempDir, name: &str) -> Option<String> {
        let repo = d.path().join(name);
        std::fs::create_dir_all(&repo).unwrap();
        Some(repo.to_string_lossy().into_owned())
    }

    /// Until no memory or chunk waits for the embedding worker or the judge.
    pub async fn settled(s: &Service) {
        until("the background work to settle", 30, || async {
            let h = s.health().await.unwrap();
            h.embed_backlog == 0 && h.judge_backlog == 0
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::service::Service;
    use alice_core::api::{IngestRequest, MatchVia, OutputFormat, RecallRequest, RememberRequest};
    use alice_core::embed::FakeEmbedder;
    use alice_core::store::sources::{ChunkInput, SourceInput};
    use std::sync::atomic::AtomicUsize;

    fn remote_toml(mock: &Mock, model: &str, extra: &str) -> String {
        format!(
            "[embedding]\nprovider = \"openai\"\nmodel = \"{model}\"\nendpoint = \"{}\"\n{extra}",
            mock.base
        )
    }

    async fn state_of(s: &Service, content_word: &str) -> String {
        let like = format!("%{content_word}%");
        s.read(move |c| {
            Ok(c.query_row(
                "SELECT embed_state FROM memories WHERE body LIKE ?1",
                [&like],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
    }

    async fn chunk_states(s: &Service) -> Vec<String> {
        s.read(|c| {
            let mut stmt = c.prepare("SELECT embed_state FROM chunks ORDER BY seq")?;
            Ok(stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<_, _>>()?)
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_denied_projects_text_never_reaches_a_remote_provider() {
        let mock = Mock::start();
        let d = tempfile::tempdir().unwrap();
        let toml = remote_toml(
            &mock,
            "m",
            "[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n",
        );
        let s = start(d.path(), options(http_factory(true, Arc::default())), &toml);
        assert_eq!(s.model(), "openai:m");
        let (open, secret) = (repo(&d, "open-proj"), repo(&d, "secret-proj"));
        for (cwd, text) in [
            (&open, "the pelican nests by the harbour"),
            (&secret, "the narwhal tusk is in the vault"),
        ] {
            s.remember(RememberRequest {
                content: text.into(),
                cwd: cwd.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        for (cwd, text) in [
            (&open, "a transcript line about a hamster wheel"),
            (&secret, "a transcript line about a walrus colony"),
        ] {
            s.ingest(IngestRequest {
                source: SourceInput {
                    uri: format!("s-{text}"),
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
        }
        settled(&s).await;
        // The open project's text went; the denied project's did not, anywhere.
        assert!(
            mock.saw("pelican") && mock.saw("hamster"),
            "{:?}",
            mock.texts()
        );
        for secret_word in ["narwhal", "walrus", "vault"] {
            assert!(!mock.saw(secret_word), "{secret_word} left the machine");
        }
        assert_eq!(state_of(&s, "narwhal").await, "skipped_cloud");
        assert_eq!(state_of(&s, "pelican").await, "done");
        let chunks = chunk_states(&s).await;
        assert!(
            chunks.contains(&"skipped_cloud".to_string()) && chunks.contains(&"done".to_string()),
            "{chunks:?}"
        );
        let h = s.health().await.unwrap();
        assert_eq!((h.embed_skipped_cloud, h.embed_backlog), (2, 0));

        // A recall in the denied project is lexical only, says so, and its query is not sent.
        let sent = mock.texts().len();
        let r = s
            .recall(RecallRequest {
                query: "narwhal tusk".into(),
                cwd: secret.clone(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            r.results.iter().any(|h| h.title.contains("narwhal")),
            "words still find it"
        );
        assert!(
            r.results
                .iter()
                .all(|h| !h.matched.contains(&MatchVia::Vector)),
            "no vector search"
        );
        assert!(
            r.notes.iter().any(|n| n.contains("word matches only")),
            "{:?}",
            r.notes
        );
        assert_eq!(mock.texts().len(), sent, "the query never left");
        assert!(!mock.saw("narwhal tusk"));
        let text = s
            .recall(RecallRequest {
                query: "narwhal tusk".into(),
                cwd: secret.clone(),
                format: OutputFormat::Text,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(text.text.unwrap().contains("word matches only"));

        // A recall in the open project sends its query, with secrets in it redacted.
        let token = concat!("gh", "p_abcdefghijklmnopqrstuvwxyz0123456789");
        let r = s
            .recall(RecallRequest {
                query: format!("where does the pelican nest {token}"),
                cwd: open.clone(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert!(
            mock.saw("where does the pelican nest"),
            "the query was sent"
        );
        assert!(
            !mock.saw(token),
            "the secret in the query was redacted before it left"
        );

        // The policy changes: the denied project is let through and its rows are embedded.
        let mut m = serde_json::Map::new();
        m.insert(
            "privacy.cloud_denied_projects".into(),
            serde_json::json!([]),
        );
        s.patch_settings(m).unwrap();
        until("the held-back rows to be embedded", 15, || async {
            s.health().await.unwrap().embed_skipped_cloud == 0 && mock.saw("narwhal")
        })
        .await;
        settled(&s).await;
        assert_eq!(state_of(&s, "narwhal").await, "done");
        assert!(mock.saw("walrus"));
        // And the key reached the provider only as a bearer header.
        let heads = mock.heads().join("\n").to_ascii_lowercase();
        assert!(
            heads.contains(&format!(
                "authorization: bearer {}",
                TEST_KEY.to_ascii_lowercase()
            )) && heads.contains(&format!("user-agent: alice/{}", env!("CARGO_PKG_VERSION")))
        );
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_loopback_ollama_endpoint_is_local_and_needs_no_cloud_policy() {
        let mock = Mock::start();
        let d = tempfile::tempdir().unwrap();
        let toml = format!(
            "[embedding]\nprovider = \"ollama\"\nmodel = \"nomic\"\nendpoint = \"{}\"\n[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n",
            mock.base
        );
        // No forced "remote": the real rule decides.
        let s = start(
            d.path(),
            options(http_factory(false, Arc::default())),
            &toml,
        );
        assert!(!s.embedder().is_remote());
        s.remember(RememberRequest {
            content: "the narwhal tusk is in the vault".into(),
            cwd: repo(&d, "secret-proj"),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        assert!(
            mock.saw("narwhal"),
            "a local server gets every project's text"
        );
        assert_eq!(state_of(&s, "narwhal").await, "done");
        let r = s
            .recall(RecallRequest {
                query: "narwhal".into(),
                cwd: repo(&d, "secret-proj"),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_degraded_configuration_sends_no_text_to_a_remote_provider() {
        let mock = Mock::start();
        let d = tempfile::tempdir().unwrap();
        // A wrong type inside the privacy section: the file is salvaged, but it may have lost
        // a privacy setting, so the configuration counts as degraded (and no cloud is used).
        let toml = remote_toml(&mock, "m", "[privacy]\ncloud_denied_projects = 5\n");
        let s = start(d.path(), options(http_factory(true, Arc::default())), &toml);
        assert!(s.settings_handle().config_degraded());
        s.remember(RememberRequest {
            content: "the heron stands in the reeds".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        assert!(!mock.saw("heron"), "{:?}", mock.texts());
        assert_eq!(state_of(&s, "heron").await, "skipped_cloud");
        let r = s
            .recall(RecallRequest {
                query: "heron".into(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(r.notes.iter().any(|n| n.contains("word matches only")));
        assert!(!mock.saw("heron"));
        s.shutdown();
    }

    /// A factory whose remote embedders are `ForceRemote` fakes of 64 dimensions that count
    /// the texts they are given.
    fn remote_fake(texts: Arc<AtomicUsize>) -> alice_embed::EmbedderFactory {
        counting_factory(texts, Duration::ZERO, true)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_to_a_remote_provider_states_what_will_be_sent_and_honours_the_policy() {
        let d = tempfile::tempdir().unwrap();
        let texts = Arc::new(AtomicUsize::new(0));
        let s = start(
            d.path(),
            options(remote_fake(texts.clone())),
            "[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n",
        );
        let (open, secret) = (repo(&d, "open-proj"), repo(&d, "secret-proj"));
        for i in 0..3 {
            s.remember(RememberRequest {
                content: format!("open memory {i}"),
                cwd: open.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        for i in 0..2 {
            s.remember(RememberRequest {
                content: format!("secret memory {i}"),
                cwd: secret.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        s.ingest(IngestRequest {
            source: SourceInput {
                uri: "s".into(),
                ..Default::default()
            },
            chunks: (0..4)
                .map(|i| ChunkInput {
                    ord: i,
                    text: format!("open chunk {i}"),
                    ..Default::default()
                })
                .collect(),
            cwd: open.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        let mut m = serde_json::Map::new();
        m.insert("embedding.provider".into(), serde_json::json!("openai"));
        m.insert(
            "embedding.model".into(),
            serde_json::json!("text-embedding-3-small"),
        );
        let out = s.patch_settings(m).unwrap();
        let note = out
            .notes
            .iter()
            .find(|n| n.contains("will be sent to api.openai.com"))
            .unwrap_or_else(|| panic!("{:?}", out.notes));
        assert!(
            note.contains("about 3 memories and 4 chunks")
                && note.contains("2 memories and 0 chunks")
                && note.contains("cloud policy")
                && note.contains("answers searches until the new index is complete"),
            "{note}"
        );
        // The same change is also announced as a loosened protection.
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("embedding.provider") && n.contains("loosened")),
            "{:?}",
            out.notes
        );
        until("the switch", 15, || async {
            s.health().await.unwrap().embedding_model == "openai:text-embedding-3-small"
        })
        .await;
        let model = "openai:text-embedding-3-small";
        let count = |sql: &'static str| {
            let s = s.clone();
            async move {
                s.read(move |c| Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))?))
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            count("SELECT count(*) FROM embeddings WHERE model = 'openai:text-embedding-3-small'")
                .await,
            7,
            "3 memories + 4 chunks of the open project"
        );
        assert_eq!(
            texts.load(Ordering::SeqCst),
            7,
            "nothing of the denied project was embedded"
        );
        assert_eq!(state_of(&s, "secret memory 0").await, "skipped_cloud");
        assert_eq!(state_of(&s, "open memory 0").await, "done");
        assert_eq!(s.model(), model);
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn going_back_to_a_local_model_queues_the_held_back_rows_again() {
        let d = tempfile::tempdir().unwrap();
        let texts = Arc::new(AtomicUsize::new(0));
        let toml = "[embedding]\nprovider = \"openai\"\nmodel = \"m\"\n[privacy]\ncloud_denied_projects = [\"secret-proj\"]\n";
        let s = start(d.path(), options(remote_fake(texts)), toml);
        assert_eq!(s.model(), "openai:m");
        s.remember(RememberRequest {
            content: "secret memory".into(),
            cwd: repo(&d, "secret-proj"),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        assert_eq!(state_of(&s, "secret memory").await, "skipped_cloud");
        let mut m = serde_json::Map::new();
        m.insert("embedding.provider".into(), serde_json::json!("local"));
        m.insert("embedding.model".into(), serde_json::json!(MODEL_ID));
        s.patch_settings(m).unwrap();
        until("the switch back", 15, || async {
            s.health().await.unwrap().embedding_model == MODEL_ID
        })
        .await;
        until("the row to be embedded locally", 15, || async {
            state_of(&s, "secret memory").await == "done"
        })
        .await;
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_provider_at_start_keeps_the_daemon_up_and_heals() {
        let d = tempfile::tempdir().unwrap();
        let up = Arc::new(AtomicBool::new(true));
        let factory: alice_embed::EmbedderFactory = {
            let up = up.clone();
            Arc::new(move |s: &EmbeddingSettings, _key| {
                let id = expected_model_id(s);
                if id != MODEL_ID && !up.load(Ordering::SeqCst) {
                    return Err(alice_core::AliceError::Other(
                        "cannot reach the provider".into(),
                    ));
                }
                Ok(Arc::new(FakeEmbedder::named(
                    id.clone(),
                    if id == MODEL_ID { 384 } else { 64 },
                )) as Arc<dyn Embedder>)
            })
        };
        let toml = "[embedding]\nprovider = \"ollama\"\nmodel = \"m\"\n";
        let s = start(d.path(), options(factory.clone()), toml);
        assert_eq!(s.model(), "ollama:m");
        s.remember(RememberRequest {
            content: "the otter slides down the bank".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        s.shutdown();
        drop(s);

        up.store(false, Ordering::SeqCst);
        let s = start(d.path(), options(factory), "");
        let h = s.health().await.unwrap();
        assert_eq!(h.embedding_model, "ollama:m", "its vectors stay in use");
        assert!(
            h.embedding_error
                .as_deref()
                .is_some_and(|e| e.contains("cannot")),
            "{h:?}"
        );
        // Searching still works (words), and a new memory waits instead of failing.
        let r = s
            .recall(RecallRequest {
                query: "otter".into(),
                format: OutputFormat::Json,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.results.len(), 1);
        s.remember(RememberRequest {
            content: "the beaver builds a lodge".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        until("the new memory to be queued for a retry", 10, || async {
            state_of(&s, "beaver").await == "retry"
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            state_of(&s, "beaver").await,
            "retry",
            "queued, not given up"
        );
        // The provider comes back: the stand-in is replaced and the queue drains.
        up.store(true, Ordering::SeqCst);
        until("the provider to be used again", 15, || async {
            let h = s.health().await.unwrap();
            h.embedding_error.is_none() && h.embed_backlog == 0
        })
        .await;
        assert_eq!(state_of(&s, "beaver").await, "done");
        s.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_set_later_switches_to_the_provider_and_a_rotated_key_is_used_without_leaking() {
        let d = tempfile::tempdir().unwrap();
        let keys: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let factory: alice_embed::EmbedderFactory = {
            let keys = keys.clone();
            Arc::new(move |s: &EmbeddingSettings, key| {
                keys.lock().push(key.clone());
                let id = expected_model_id(s);
                if id != MODEL_ID && key.is_none() {
                    return Err(alice_core::AliceError::invalid(
                        "the openai embedding provider needs an API key; set it with `alice config set-secret embedding_api_key`",
                    ));
                }
                Ok(Arc::new(FakeEmbedder::named(
                    id.clone(),
                    if id == MODEL_ID { 384 } else { 64 },
                )) as Arc<dyn Embedder>)
            })
        };
        let mut opts = options(factory);
        opts.secret_env = Some(Arc::new(|_: &str| None)); // no key anywhere, until one is set
        let toml = "[embedding]\nprovider = \"openai\"\nmodel = \"m\"\n";
        let s = start(d.path(), opts, toml);
        // No key: the built-in model serves, and health says what to do.
        assert_eq!(s.model(), MODEL_ID);
        let h = s.health().await.unwrap();
        assert!(
            h.embedding_error
                .as_deref()
                .is_some_and(|e| e.contains("embedding_api_key")),
            "{h:?}"
        );
        s.remember(RememberRequest {
            content: "a memory before the key".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        settled(&s).await;
        let key1 = concat!("sk-", "first-key-0123456789abcdef");
        s.set_secret_async(crate::secrets::SecretName::EmbeddingApiKey, key1.into())
            .await
            .unwrap();
        until("the switch to the provider", 15, || async {
            s.health().await.unwrap().embedding_model == "openai:m"
        })
        .await;
        assert!(keys.lock().iter().any(|k| k.as_deref() == Some(key1)));
        // Rotating the key builds the embedder again with the new one.
        let key2 = concat!("sk-", "second-key-0123456789abcdef");
        let before = keys.lock().len();
        s.set_secret_async(crate::secrets::SecretName::EmbeddingApiKey, key2.into())
            .await
            .unwrap();
        until("the rotated key to be used", 15, || async {
            keys.lock()[before..]
                .iter()
                .any(|k| k.as_deref() == Some(key2))
        })
        .await;
        // Deleting it stops anything being sent: the stand-in answers "unavailable".
        s.delete_secret_async(crate::secrets::SecretName::EmbeddingApiKey)
            .await
            .unwrap();
        until("the stand-in after the key was deleted", 15, || async {
            s.health()
                .await
                .unwrap()
                .embedding_error
                .is_some_and(|e| e.contains("embedding_api_key"))
        })
        .await;
        assert!(s.embedder().embed_passages(&["x".into()]).is_err());
        // The keys are in no answer the daemon gives.
        let health = serde_json::to_string(&s.health().await.unwrap()).unwrap();
        let view = serde_json::to_string(&s.settings_view()).unwrap();
        for out in [health, view] {
            assert!(
                !out.contains(key1) && !out.contains(key2),
                "a key leaked: {out}"
            );
        }
        s.shutdown();
    }
    /// H2: a switch to a remote provider typed into config.toml is a loosening, so it waits to
    /// be read the same on three polls in a row (a half-written save never sends text away).
    #[tokio::test(flavor = "multi_thread")]
    async fn editing_the_file_to_a_remote_provider_waits_for_the_loosening_ratchet() {
        let d = tempfile::tempdir().unwrap();
        let mut opts = options(fake_factory());
        opts.settings_poll = Duration::from_millis(100);
        let s = start(d.path(), opts, "");
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(
            home.config_file(),
            "[embedding]\nprovider = \"voyage\"\nmodel = \"voyage-3-lite\"\n",
        )
        .unwrap();
        // Read once or twice: not in force yet, and the settings view says why.
        tokio::time::sleep(Duration::from_millis(180)).await;
        assert_eq!(s.settings().embedding.provider, EmbeddingProvider::Local);
        assert!(s.settings_handle().loosening_waiting());
        assert_eq!(s.model(), MODEL_ID);
        until("the third identical poll", 10, || async {
            s.settings().embedding.provider == EmbeddingProvider::Voyage
        })
        .await;
        until("the switch", 15, || async {
            s.health().await.unwrap().embedding_model == "voyage:voyage-3-lite"
        })
        .await;
        s.shutdown();
    }
}
