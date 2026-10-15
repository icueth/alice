//! Recall and remember during a large import, and the `-wal` file the import leaves behind
//! (Task 20).
//!
//! `measure_recall_during_import` is the A/B harness behind `docs/benchmarks/m2.md`: it
//! bulk-ingests chunks while 20 clients remember and recall in a loop (5 ms apart, as the M3
//! import test does), and prints the p95 of each plus the `-wal` size over time. It is ignored by default; run it in release:
//!
//! ```text
//! ALICE_TEST_BULK=100000 ALICE_TEST_PAUSE_MS=0 cargo test --release -p alice-daemon \
//!     --test wal_during_import -- --ignored --nocapture measure
//! ```
//!
//! `ALICE_TEST_BULK` is the number of chunks, `ALICE_TEST_PAUSE_MS` a pause between the
//! 1,000-chunk ingest calls (a throttled import), `ALICE_TEST_KEEP_HOME` a directory to keep
//! the database in (with `ALICE_TEST_BULK=0` the clients run for `ALICE_TEST_LINGER_MS` on the
//! database already there: the no-import baseline). `ALICE_TEST_SAMPLE_MS` is the `-wal` sampling
//! interval.
//!
//! The checks that the writer keeps the log bounded and the pool hygiene hold live in
//! `writer.rs` and `alice-core/src/store/mod.rs`; this file is the measurement.

use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// About `bytes` of transcript-like text (`ALICE_TEST_CHUNK_BYTES`, default 750: the size of a
/// MemPalace drawer); every 50th chunk is 40 times longer, as in `service_concurrency`.
fn chunk_text(batch: usize, i: usize, bytes: usize) -> String {
    let target = if i.is_multiple_of(50) {
        (bytes * 40).min(32_000)
    } else {
        bytes
    };
    // `zq{batch}q{i}` is a word only this chunk holds: the rare word a recall looks for.
    let mut t = format!("transcript line zq{batch}q{i} about lease timeouts and sqlite");
    let seed = batch * 1_000 + i;
    let mut w = 0;
    while t.len() < target {
        t.push_str(&format!(" w{}", (seed * 31 + w * 17) % 5_003));
        w += 1;
    }
    t
}

fn p95(mut v: Vec<Duration>) -> Duration {
    if v.is_empty() {
        return Duration::ZERO;
    }
    v.sort();
    v[((v.len() as f64 * 0.95) as usize).saturating_sub(1)]
}

fn wal_len(db: &Path) -> u64 {
    let mut p = db.as_os_str().to_owned();
    p.push("-wal");
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

struct Report {
    remember_p95: Duration,
    recall_p95: Duration,
    recalls: usize,
    wal_peak: u64,
    wal_end: u64,
    /// (seconds since the start, wal bytes), every sample interval.
    wal_series: Vec<(f64, u64)>,
    import_secs: f64,
}

/// Ingest `total` chunks in calls of 1,000 (pausing `pause` between calls) while 20 clients
/// remember and recall until the import ends.
async fn import_with_clients(
    home: &Path,
    total: usize,
    pause: Duration,
    sample: Duration,
    linger: Duration,
) -> Report {
    let svc = Service::start(
        AliceHome::new(home.to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions::default(),
    )
    .unwrap();
    let db = AliceHome::new(home.to_path_buf()).db();
    let chunk_bytes = env_num("ALICE_TEST_CHUNK_BYTES", 750) as usize;
    let started = Instant::now();
    let done = Arc::new(AtomicBool::new(false));

    let sampler = {
        let done = done.clone();
        let db = db.clone();
        tokio::spawn(async move {
            let mut series = Vec::new();
            while !done.load(Ordering::Relaxed) {
                series.push((started.elapsed().as_secs_f64(), wal_len(&db)));
                tokio::time::sleep(sample).await;
            }
            series.push((started.elapsed().as_secs_f64(), wal_len(&db)));
            series
        })
    };

    let bulk = {
        let svc = svc.clone();
        let done = done.clone();
        tokio::spawn(async move {
            for batch in 0..(total / 1_000) {
                let chunks = (0..1_000)
                    .map(|i| ChunkInput {
                        ord: (batch * 1_000 + i) as i64,
                        text: chunk_text(batch, i, chunk_bytes),
                        ..Default::default()
                    })
                    .collect();
                svc.ingest(IngestRequest {
                    source: SourceInput {
                        uri: "bulk".into(),
                        ..Default::default()
                    },
                    chunks,
                    ..Default::default()
                })
                .await
                .unwrap();
                if !pause.is_zero() {
                    tokio::time::sleep(pause).await;
                }
            }
            if total == 0 {
                // No import: the clients alone, on a database that is already there.
                tokio::time::sleep(linger).await;
            }
            done.store(true, Ordering::Relaxed);
            started.elapsed().as_secs_f64()
        })
    };

    let mut clients = Vec::new();
    for c in 0..env_num("ALICE_TEST_CLIENTS", 20) as usize {
        let svc = svc.clone();
        let done = done.clone();
        clients.push(tokio::spawn(async move {
            let mut w = Vec::new();
            let mut r = Vec::new();
            let mut i = 0usize;
            while !done.load(Ordering::Relaxed) {
                let t = Instant::now();
                svc.remember(RememberRequest {
                    content: format!("client {c} decision {i}: keep the writer short"),
                    ..Default::default()
                })
                .await
                .unwrap();
                w.push(t.elapsed());
                // Alternate a client's own memory with a word that only imported chunks hold.
                let query = if i.is_multiple_of(2) {
                    format!("client {c} decision")
                } else {
                    format!("zq{}q{}", (c + i) % 4, (c * 37 + i) % 1_000)
                };
                let t = Instant::now();
                svc.recall(RecallRequest {
                    query,
                    ..Default::default()
                })
                .await
                .unwrap();
                r.push(t.elapsed());
                i += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            (w, r)
        }));
    }
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    for h in clients {
        let (w, r) = h.await.unwrap();
        writes.extend(w);
        reads.extend(r);
    }
    let import_secs = bulk.await.unwrap();
    let wal_series = sampler.await.unwrap();
    let wal_peak = wal_series.iter().map(|s| s.1).max().unwrap_or(0);
    let wal_end = wal_len(&db);
    let recalls = reads.len();
    Report {
        remember_p95: p95(writes),
        recall_p95: p95(reads),
        recalls,
        wal_peak,
        wal_end,
        wal_series,
        import_secs,
    }
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn load_average() -> String {
    std::process::Command::new("uptime")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measurement, not a check: see the module comment"]
async fn measure_recall_during_import() {
    let total = env_num("ALICE_TEST_BULK", 100_000) as usize;
    let pause = Duration::from_millis(env_num("ALICE_TEST_PAUSE_MS", 0));
    let sample = Duration::from_millis(env_num("ALICE_TEST_SAMPLE_MS", 500));
    let keep = std::env::var("ALICE_TEST_KEEP_HOME").ok();
    let tmp;
    let home = match &keep {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            tmp = tempfile::tempdir().unwrap();
            tmp.path().to_path_buf()
        }
    };
    let before = load_average();
    let linger = Duration::from_millis(env_num("ALICE_TEST_LINGER_MS", 30_000));
    let r = import_with_clients(&home, total, pause, sample, linger).await;
    let after = load_average();
    let mut e = std::io::stderr();
    let _ = writeln!(e, "load before: {before}\nload after:  {after}");
    let _ = writeln!(
        e,
        "wal_during_import: chunks {total}, pause {pause:?}, import {:.1}s, recalls {}, remember p95 {:?}, recall p95 {:?}, wal peak {:.1} MiB, wal at end {:.1} MiB",
        r.import_secs,
        r.recalls,
        r.remember_p95,
        r.recall_p95,
        r.wal_peak as f64 / 1048576.0,
        r.wal_end as f64 / 1048576.0
    );
    let step = (r.wal_series.len() / 12).max(1);
    for (t, b) in r.wal_series.iter().step_by(step) {
        let _ = writeln!(e, "  wal t={t:6.1}s {:8.1} MiB", *b as f64 / 1048576.0);
    }
}
