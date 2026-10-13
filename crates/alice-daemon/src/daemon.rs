//! `alice daemon run`: single instance, token, discovery file, HTTP server, spool drain, clean shutdown.

use crate::discovery::{DaemonInfo, load_or_create_token, read_info, remove_info_if, write_info};
use crate::paths::AliceHome;
use crate::service::{Service, ServiceOptions};
use crate::spool;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::ids::now_rfc3339;
use anyhow::Context;
use std::fs::{File, OpenOptions, TryLockError};
use std::future::IntoFuture;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How long a new daemon waits for the lock while no daemon.json is published: the holder is
/// then either starting (it publishes soon) or stopping (it releases the lock once saved).
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// Open connections (an MCP stream, say) may hold up a stop this long; then they are dropped.
const GRACE: Duration = Duration::from_secs(2);
/// How often spooled writes are retried.
const DRAIN_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    /// 0 = pick a free port.
    pub port: u16,
    /// Tests and CI: deterministic embeddings, no model download (`ALICE_EMBEDDER=fake`).
    pub fake_embedder: bool,
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
    // Always printed (not subject to ALICE_LOG): process tests count this line to prove a single instance.
    eprintln!(
        "alice daemon started pid={} home={}",
        std::process::id(),
        home.root().display()
    );

    let token = load_or_create_token(&home)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let models = home.models_dir();
    if !opts.fake_embedder && !alice_embed::is_model_present(&models) {
        tracing::info!("downloading the embedding model (first run, ~136 MB)");
        runtime.block_on(alice_embed::fetch_model(&models, &|_, _, _| {}))?;
    }
    let fake = opts.fake_embedder;
    let make = move || -> alice_core::Result<Arc<dyn Embedder>> {
        if fake {
            Ok(Arc::new(FakeEmbedder::new(384)))
        } else {
            Ok(Arc::new(alice_embed::FastEmbedder::new(&models)?))
        }
    };
    let svc = Service::start(home.clone(), &make, ServiceOptions::default())?;

    let ct = CancellationToken::new();
    let served = runtime.block_on(serve(&home, &svc, token, opts.port, ct.clone()));
    ct.cancel();
    // Drop whatever still runs (a request past the grace period, a drain in progress) so that
    // nothing reaches the writer after it stops; such a client sees a closed connection and
    // spools or retries, and a write it had already sent is committed by `shutdown` below.
    runtime.shutdown_timeout(GRACE);
    svc.shutdown();
    // Still holding the lock, so no newer daemon can have published in the meantime.
    remove_info_if(&home, svc.instance());
    drop(lock);
    tracing::info!("daemon stopped");
    served
}

async fn serve(
    home: &AliceHome,
    svc: &Arc<Service>,
    token: String,
    port: u16,
    ct: CancellationToken,
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
