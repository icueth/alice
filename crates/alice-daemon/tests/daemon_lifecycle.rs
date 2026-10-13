//! The daemon lifecycle in-process: single instance, discovery file, spool drain at start,
//! clean shutdown and restart (review focus #3: no hang, exactly one daemon, no lost write).

use alice_core::api::*;
use alice_daemon::daemon::{DaemonOptions, run};
use alice_daemon::discovery::{DaemonInfo, read_info, read_token, verify_ping_proof, write_info};
use alice_daemon::http::PingResponse;
use alice_daemon::paths::AliceHome;
use alice_daemon::spool::{self, SpoolEntry};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Running = JoinHandle<anyhow::Result<()>>;

fn spawn(home: &AliceHome) -> Running {
    alice_daemon::install_tls();
    let home = home.clone();
    std::thread::spawn(move || {
        run(
            home,
            DaemonOptions {
                port: 0,
                fake_embedder: true,
            },
        )
    })
}

/// Wait (without blocking the test runtime) for `run` to return.
async fn finished(h: Running, within: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    while !h.is_finished() {
        assert!(
            Instant::now() < deadline,
            "daemon::run did not return within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.join().expect("daemon thread panicked")
}

struct Daemon {
    http: reqwest::Client,
    base: String,
    token: String,
    info: DaemonInfo,
}

impl Daemon {
    /// Like a client: trust daemon.json only once ping answers with its instance id.
    async fn connect(home: &AliceHome) -> Daemon {
        let http = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let (Some(info), Some(token)) = (read_info(home), read_token(home)) {
                let base = format!("http://127.0.0.1:{}", info.port);
                let nonce = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
                let ping = http
                    .get(format!("{base}/api/v1/ping?nonce={nonce}"))
                    .timeout(Duration::from_millis(300))
                    .send()
                    .await;
                if let Ok(r) = ping
                    && let Ok(p) = r.json::<PingResponse>().await
                    && p.instance == info.instance
                    && p.proof.is_some_and(|proof| {
                        verify_ping_proof(&token, nonce, &info.instance, &proof)
                    })
                {
                    return Daemon {
                        http,
                        base,
                        token,
                        info,
                    };
                }
            }
            assert!(Instant::now() < deadline, "the daemon did not come up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn health(&self) -> Health {
        self.http
            .get(format!("{}/api/v1/health", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn remember(&self, content: &str) -> RememberResponse {
        self.http
            .post(format!("{}/api/v1/remember", self.base))
            .bearer_auth(&self.token)
            .json(&RememberRequest {
                content: content.into(),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn shutdown(&self) {
        let r = self
            .http
            .post(format!("{}/api/v1/admin/shutdown", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202);
    }
}

fn home() -> (tempfile::TempDir, AliceHome) {
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().to_path_buf());
    (d, home)
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledged_writes_survive_shutdown_and_restart() {
    let (_d, home) = home();
    let running = spawn(&home);
    let daemon = Daemon::connect(&home).await;
    assert_eq!(daemon.info.pid, std::process::id());
    assert_eq!(daemon.info.instance.len(), 26);
    for i in 0..3 {
        let r = daemon.remember(&format!("acknowledged write {i}")).await;
        assert_eq!(r.status, RememberStatus::New);
    }
    daemon.shutdown().await;
    finished(running, Duration::from_secs(15)).await.unwrap();
    assert_eq!(read_info(&home), None, "a clean stop removes daemon.json");

    // The lock was released: a new daemon starts, with a new instance id and the same token.
    let running = spawn(&home);
    let again = Daemon::connect(&home).await;
    assert_ne!(again.info.instance, daemon.info.instance);
    assert_eq!(again.token, daemon.token);
    assert_eq!(again.health().await.memories, 3);
    again.shutdown().await;
    finished(running, Duration::from_secs(15)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_on_the_same_home_exits_at_once() {
    let (_d, home) = home();
    let first = spawn(&home);
    let daemon = Daemon::connect(&home).await;
    let started = Instant::now();
    finished(spawn(&home), Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a published daemon means no waiting: {:?}",
        started.elapsed()
    );
    assert_eq!(
        read_info(&home),
        Some(daemon.info.clone()),
        "the loser left daemon.json alone"
    );
    assert_eq!(daemon.health().await.instance, daemon.info.instance);
    daemon.shutdown().await;
    finished(first, Duration::from_secs(15)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_discovery_file_is_replaced_and_the_spool_drains_at_start() {
    let (_d, home) = home();
    home.ensure_dirs().unwrap();
    // A daemon killed with -9: its daemon.json names a port nobody serves any more.
    let dead_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    write_info(
        &home,
        &DaemonInfo {
            pid: 1,
            port: dead_port,
            instance: "STALE".into(),
            version: "0".into(),
            started_at: "t".into(),
        },
    )
    .unwrap();
    // Writes clients queued while no daemon was up.
    for c in ["queued while down one", "queued while down two"] {
        spool::append(
            &home.spool_dir(),
            &SpoolEntry::Remember(RememberRequest {
                content: c.into(),
                ..Default::default()
            }),
        )
        .unwrap();
    }
    let running = spawn(&home);
    let daemon = Daemon::connect(&home).await;
    assert_ne!(daemon.info.instance, "STALE");
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.health().await.memories < 2 {
        assert!(Instant::now() < deadline, "the spool was not drained");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(spool::pending_count(&home.spool_dir()), 0);
    daemon.shutdown().await;
    finished(running, Duration::from_secs(15)).await.unwrap();
}

/// A restart: the old daemon still holds the lock while it saves, and has already withdrawn
/// daemon.json. The new one waits for the lock instead of giving up.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_daemon_waits_for_a_stopping_one() {
    let (_d, home) = home();
    home.ensure_dirs().unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.lock_file())
        .unwrap();
    held.try_lock().unwrap();
    let running = spawn(&home);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!running.is_finished(), "it is waiting, not giving up");
    assert_eq!(read_info(&home), None, "it has not started");
    drop(held);
    let daemon = Daemon::connect(&home).await;
    daemon.shutdown().await;
    finished(running, Duration::from_secs(15)).await.unwrap();
}

/// `alice daemon stop` then an immediate start, while a stuck client holds a request open:
/// the old daemon withdraws daemon.json at once, drops the stuck request after the grace
/// period instead of hanging, and the new daemon waits for its lock and takes over.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_during_a_stuck_request_ends_with_one_running_daemon() {
    use std::io::Write;
    let (_d, home) = home();
    let old = spawn(&home);
    let daemon = Daemon::connect(&home).await;
    // Headers promise a body that never comes, so graceful shutdown cannot finish by itself.
    let mut stuck = std::net::TcpStream::connect(("127.0.0.1", daemon.info.port)).unwrap();
    write!(
        stuck,
        "POST /api/v1/remember HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{",
        daemon.info.port, daemon.token
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped_at = Instant::now();
    daemon.shutdown().await;
    while read_info(&home).is_some() {
        assert!(
            stopped_at.elapsed() < Duration::from_secs(1),
            "daemon.json not withdrawn"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!old.is_finished(), "the old daemon is still stopping");
    let new = spawn(&home);
    finished(old, Duration::from_secs(10)).await.unwrap();
    assert!(
        stopped_at.elapsed() >= Duration::from_millis(1_500),
        "the stuck request was given its grace period: {:?}",
        stopped_at.elapsed()
    );
    let again = Daemon::connect(&home).await;
    assert_ne!(again.info.instance, daemon.info.instance);
    assert!(!new.is_finished());
    again.shutdown().await;
    finished(new, Duration::from_secs(15)).await.unwrap();
    drop(stuck);
}

/// The lock holder knows any daemon.json it finds is stale, and drops it before starting the
/// service, so clients never ping a port that may belong to someone else by now. Shown with a
/// daemon whose start then fails: the stale file must already be gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_discovery_file_is_cleared_as_soon_as_the_lock_is_taken() {
    let (_d, home) = home();
    home.ensure_dirs().unwrap();
    write_info(
        &home,
        &DaemonInfo {
            pid: 1,
            port: 9,
            instance: "STALE".into(),
            version: "0".into(),
            started_at: "t".into(),
        },
    )
    .unwrap();
    // A directory where the database file should be: Service::start fails.
    std::fs::create_dir(home.db()).unwrap();
    let r = finished(spawn(&home), Duration::from_secs(10)).await;
    assert!(r.is_err(), "the service cannot start on a directory");
    assert_eq!(read_info(&home), None, "the stale daemon.json was removed");
}
