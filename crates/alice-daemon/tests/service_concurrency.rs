//! The MemPalace failure as a regression test: many clients writing and reading while a bulk ingest runs.

use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Transcript-like chunk text: mostly about 2 KB, every 50th about 32 KB (the limit is 32,768
/// bytes). FTS work grows with text, so short lines would hide a writer held up by big slices.
fn chunk_text(batch: usize, i: usize) -> String {
    let target = if i.is_multiple_of(50) { 32_000 } else { 2_000 };
    let mut t = format!("transcript line {batch}-{i} about lease timeouts and sqlite");
    let seed = batch * 1_000 + i;
    let mut w = 0;
    while t.len() < target {
        t.push_str(&format!(" w{}", (seed * 31 + w * 17) % 5_003));
        w += 1;
    }
    t
}

fn p95(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[(v.len() as f64 * 0.95) as usize - 1]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn writes_and_reads_stay_fast_during_bulk_ingest() {
    let d = tempfile::tempdir().unwrap();
    let svc = Service::start(
        AliceHome::new(d.path().to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
        ServiceOptions::default(),
    )
    .unwrap();
    let chunks_total: usize = std::env::var("ALICE_TEST_BULK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);

    let bulk = {
        let svc = svc.clone();
        tokio::spawn(async move {
            for batch in 0..(chunks_total / 1_000) {
                let chunks = (0..1_000)
                    .map(|i| ChunkInput {
                        ord: (batch * 1_000 + i) as i64,
                        text: chunk_text(batch, i),
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
            }
        })
    };

    let mut clients = Vec::new();
    for c in 0..20 {
        let svc = svc.clone();
        clients.push(tokio::spawn(async move {
            let mut w = Vec::new();
            let mut r = Vec::new();
            let mut ids = Vec::new();
            for i in 0..25 {
                let t = Instant::now();
                let resp = svc
                    .remember(RememberRequest {
                        content: format!("client {c} decision {i}: keep the writer short"),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                w.push(t.elapsed());
                ids.push(resp.id.unwrap());
                let t = Instant::now();
                svc.recall(RecallRequest {
                    query: format!("client {c} decision"),
                    ..Default::default()
                })
                .await
                .unwrap();
                r.push(t.elapsed());
            }
            (w, r, ids)
        }));
    }
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    let mut all_ids = Vec::new();
    for h in clients {
        let (w, r, ids) = h.await.unwrap();
        writes.extend(w);
        reads.extend(r);
        all_ids.extend(ids);
    }
    let overlapped = !bulk.is_finished();
    bulk.await.unwrap();

    let (wp, rp) = (p95(writes), p95(reads));
    eprintln!(
        "remember p95 {wp:?}, recall p95 {rp:?}, bulk ingest still running when the clients finished: {overlapped}"
    );
    // The budgets (remember 50 ms, recall 200 ms) hold in release builds, where this test and
    // `alice bench` enforce them. An unoptimised build on a busy machine measured remember p95
    // at 250 ms, so debug builds only catch stalls (a write waiting behind a whole bulk ingest
    // takes seconds); the correctness checks below apply to both.
    let (write_bound, read_bound) = if cfg!(debug_assertions) {
        (Duration::from_secs(2), Duration::from_secs(4))
    } else {
        (Duration::from_millis(50), Duration::from_millis(200))
    };
    assert!(
        wp < write_bound,
        "remember p95 {wp:?} (bound {write_bound:?})"
    );
    assert!(rp < read_bound, "recall p95 {rp:?} (bound {read_bound:?})");
    let got = svc
        .get(GetRequest {
            ids: all_ids.clone(),
            include_provenance: false,
        })
        .await
        .unwrap();
    assert_eq!(
        got.memories.len(),
        all_ids.len(),
        "no acknowledged write is lost"
    );
    assert!(got.missing.is_empty());
}
