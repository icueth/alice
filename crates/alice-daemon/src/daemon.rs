//! `alice daemon run`: single instance, token, discovery file, HTTP server, spool drain, clean shutdown.

use crate::config::SettingsHandle;
use crate::discovery::{DaemonInfo, load_or_create_token, read_info, remove_info_if, write_info};
use crate::paths::AliceHome;
use crate::service::{Service, ServiceOptions};
use crate::spool;
use crate::startup;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::error::AliceError;
use alice_core::ids::now_rfc3339;
use anyhow::Context;
use rusqlite::ErrorCode;
use std::fs::{File, OpenOptions, TryLockError};
use std::future::IntoFuture;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How long a new daemon waits for the lock while no daemon.json is published: the holder is
/// then either starting (it publishes soon) or stopping (it releases the lock once saved).
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// Open connections (an MCP stream, say) may hold up a stop this long; then they are dropped.
const GRACE: Duration = Duration::from_secs(2);
/// How often spooled writes are retried.
const DRAIN_EVERY: Duration = Duration::from_secs(5);
/// Download progress is written to daemon.starting at most this often.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(1);
#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    /// 0 = the `server.port` setting (itself 0 = pick a free port).
    pub port: u16,
    /// Tests and CI: deterministic embeddings, no model download (`ALICE_EMBEDDER=fake`).
    pub fake_embedder: bool,
    /// The `ALICE_*` variables the settings see; `None`: the process's. Tests pass an empty
    /// list so a developer's shell cannot change them.
    pub settings_env: Option<Vec<(String, String)>>,
}

/// The single-instance lock, or `None` when another daemon holds it.
fn acquire_lock(home: &AliceHome) -> std::io::Result<Option<File>> {
    // The lock lives in its own file because Windows locks are mandatory.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.lock_file())?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {
                // A published daemon.json (even a stale one) means the holder is serving or about
                // to: give way at once, so a crowd of racing starts ends quickly. Without one the
                // holder may be stopping (a restart): wait for it rather than leave no daemon.
                if read_info(home).is_some() || Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

pub fn run(home: AliceHome, opts: DaemonOptions) -> anyhow::Result<()> {
    crate::install_tls();
    home.ensure_dirs()?;
    let Some(mut lock) = acquire_lock(&home)? else {
        eprintln!(
            "alice daemon is already running for {}",
            home.root().display()
        );
        return Ok(());
    };
    lock.set_len(0)?;
    writeln!(lock, "{}", std::process::id())?;
    // Whatever daemon.json exists now is stale (its writer no longer holds the lock): drop it
    // before the slow part of starting, so no client pings a port that may be someone else's.
    let _ = std::fs::remove_file(home.daemon_json());
    // A new attempt: the last failed start's reason no longer applies.
    startup::clear_error(&home);
    // Always printed (not subject to ALICE_LOG): process tests count this line to prove a single instance.
    eprintln!(
        "alice daemon started pid={} home={}",
        std::process::id(),
        home.root().display()
    );
    let published = AtomicBool::new(false);
    let result = start_and_serve(&home, &opts, &published);
    startup::clear_progress(&home);
    if let Err(e) = &result
        && !published.load(Ordering::SeqCst)
    {
        // Clients cannot read the log reliably: leave the reason where they look, before the
        // lock is released (a client that finds the lock free then finds the reason too).
        let reason = format!("{e:#}");
        tracing::error!(error = %reason, "the daemon could not start");
        if let Err(w) = startup::write_error(&home, &reason) {
            tracing::warn!(error = %w, "cannot write daemon.error");
        }
    }
    drop(lock);
    result
}

/// The start-up error for a service that cannot open the database or load the model, worded
/// with the cause and what to do (the message clients show).
fn start_error(home: &AliceHome, e: AliceError) -> anyhow::Error {
    let db = home.db();
    match &e {
        AliceError::Db(err)
            if matches!(
                err.sqlite_error_code(),
                Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
            ) =>
        {
            anyhow::anyhow!(
                "{} is not a usable database ({err}); restore it from a backup or move it aside (a new, empty one is then created), then retry",
                db.display()
            )
        }
        AliceError::Db(err) => anyhow::anyhow!(
            "cannot open the database {}: {err}; check the file and its permissions, then retry",
            db.display()
        ),
        // A schema from a newer build: the reason says to upgrade.
        AliceError::ReadOnly(reason) => {
            anyhow::anyhow!("cannot open the database {}: {reason}", db.display())
        }
        _ => anyhow::Error::new(e),
    }
}

/// Download the model with progress announced in daemon.starting (at most once a second) and
/// logged every 10%.
fn fetch_model(home: &AliceHome, runtime: &tokio::runtime::Runtime) -> anyhow::Result<()> {
    const STEP: &str = "downloading the embedding model";
    tracing::info!("{STEP} (first run, ~136 MB)");
    let _ = startup::write_progress(home, &startup::StartProgress::step(STEP));
    // (when last announced, file, percent)
    let last: parking_lot::Mutex<(Option<Instant>, String, u64)> = Default::default();
    let announce = |file: &str, done: u64, total: u64| {
        let pct = done.saturating_mul(100) / total.max(1);
        let mut l = last.lock();
        let same_file = l.1 == file;
        if same_file && pct / 10 == l.2 / 10 && l.0.is_some_and(|t| t.elapsed() < ANNOUNCE_EVERY) {
            return;
        }
        if !same_file || pct / 10 != l.2 / 10 {
            tracing::info!(file, pct, "{STEP}");
        }
        *l = (Some(Instant::now()), file.to_string(), pct);
        let mut p = startup::StartProgress::step(STEP);
        (p.file, p.done, p.total) = (Some(file.to_string()), Some(done), Some(total));
        let _ = startup::write_progress(home, &p);
    };
    runtime
        .block_on(alice_embed::fetch_model(&home.models_dir(), &announce))
        .map_err(|e| anyhow::anyhow!("the embedding model could not be downloaded: {e}"))
}

/// Everything after the lock: the model, the service, then serving until stopped. Sets
/// `published` once daemon.json is written (from then on a failure is not a start-up error).
fn start_and_serve(
    home: &AliceHome,
    opts: &DaemonOptions,
    published: &AtomicBool,
) -> anyhow::Result<()> {
    let token = load_or_create_token(home)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let models = home.models_dir();
    let _ = startup::write_progress(
        home,
        &startup::StartProgress::step("opening the database and loading the embedding model"),
    );
    // The settings come first: the model's idle unload and the port are read from them.
    let settings = match &opts.settings_env {
        Some(env) => SettingsHandle::load_with_env(home, env.clone()),
        None => SettingsHandle::load(home),
    };
    let started = settings.get();
    // The local model is downloaded when it is the one asked for (a remote provider needs none).
    if !opts.fake_embedder
        && started.embedding.provider == alice_core::settings::EmbeddingProvider::Local
        && !alice_embed::is_model_present(&models)
    {
        fetch_model(home, &runtime)?;
    }
    // `embedding.idle_unload` (0 keeps the local model loaded) reaches the model through the
    // factory; queries use word matches while a model unloaded for being idle reloads.
    let port = if opts.port != 0 {
        opts.port
    } else {
        started.server.port
    };
    let fake = opts.fake_embedder;
    // The factory builds embedders from `[embedding]`: the local model or an HTTP provider.
    // Without it (the fake embedder) nothing switches.
    let factory = (!fake).then(|| alice_embed::factory(models.clone()));
    // What `Service::start` falls back to when the model in force cannot be built: the
    // built-in model (or the fake one).
    let make = {
        let factory = factory.clone();
        let embedding = started.embedding.clone();
        move || -> alice_core::Result<Arc<dyn Embedder>> {
            match &factory {
                None => Ok(Arc::new(FakeEmbedder::new(384))),
                Some(f) => {
                    let mut s = embedding.clone();
                    s.provider = alice_core::settings::EmbeddingProvider::Local;
                    s.model = alice_embed::MODEL_ID.to_string();
                    s.endpoint = None;
                    f(&s, None)
                }
            }
        }
    };
    let svc = Service::start(
        home.clone(),
        &make,
        ServiceOptions {
            settings: Some(settings),
            embedder_factory: factory,
            // The daemon reads the real environment (the secrets' status shows a variable).
            ..ServiceOptions::process_env()
        },
    )
    .map_err(|e| start_error(home, e))?;

    let ct = CancellationToken::new();
    let served = runtime.block_on(serve(home, &svc, token, port, ct.clone(), published));
    ct.cancel();
    // Drop whatever still runs (a request past the grace period, a drain in progress) so that
    // nothing reaches the writer after it stops; such a client sees a closed connection and
    // spools or retries, and a write it had already sent is committed by `shutdown` below.
    runtime.shutdown_timeout(GRACE);
    svc.shutdown();
    // Still holding the lock, so no newer daemon can have published in the meantime.
    remove_info_if(home, svc.instance());
    tracing::info!("daemon stopped");
    served
}

async fn serve(
    home: &AliceHome,
    svc: &Arc<Service>,
    token: String,
    port: u16,
    ct: CancellationToken,
    published: &AtomicBool,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .with_context(|| {
            format!("cannot listen on 127.0.0.1:{port}; choose another port or stop the program using it")
        })?;
    let port = listener.local_addr()?.port();
    write_info(
        home,
        &DaemonInfo {
            pid: std::process::id(),
            port,
            instance: svc.instance().to_string(),
            version: env!("CARGO_PKG_VERSION").into(),
            started_at: now_rfc3339(),
        },
    )?;
    published.store(true, Ordering::SeqCst);
    startup::clear_error(home);
    startup::clear_progress(home);
    tokio::spawn(drain_spool(home.spool_dir(), svc.clone(), ct.clone()));
    tokio::spawn(stop_on_signal(ct.clone()));
    let app = crate::http::router(svc.clone(), token, port, ct.clone());
    tracing::info!(port, "listening on 127.0.0.1");

    let stopping = {
        let (ct, home, instance) = (ct.clone(), home.clone(), svc.instance().to_string());
        async move {
            ct.cancelled().await;
            // Withdraw at once: clients stop finding a daemon that is going away, and a daemon
            // started meanwhile waits for the lock instead of giving way to this one.
            remove_info_if(&home, &instance);
        }
    };
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(stopping)
        .into_future();
    tokio::select! {
        r = server => r?,
        _ = async { ct.cancelled().await; tokio::time::sleep(GRACE).await } => {
            tracing::warn!("graceful shutdown timed out; dropping open connections");
        }
    }
    Ok(())
}

/// Apply spooled writes now and every few seconds. A read-only database lasts until a restart
/// after repair, so the spool waits for that.
async fn drain_spool(dir: PathBuf, svc: Arc<Service>, ct: CancellationToken) {
    loop {
        if svc.read_only().is_none() {
            let r = spool::drain(&dir, &svc).await;
            if r.processed + r.rejected > 0 {
                tracing::info!(?r, "spool drained");
            }
        }
        tokio::select! {
            _ = ct.cancelled() => break,
            _ = tokio::time::sleep(DRAIN_EVERY) => {}
        }
    }
}

/// Resolves on Ctrl-C; never, if that signal cannot be watched.
async fn interrupted() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn stop_on_signal(ct: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = interrupted() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot watch SIGTERM; stop with \"alice daemon stop\"");
                interrupted().await;
            }
        }
    }
    #[cfg(not(unix))]
    interrupted().await;
    ct.cancel();
}
