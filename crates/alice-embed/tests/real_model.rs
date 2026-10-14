//! Run with: ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release -- --ignored
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine};
use alice_embed::{OnnxEmbedder, OnnxOptions, fetch_model};
use std::path::PathBuf;

#[tokio::test]
#[ignore = "downloads the real model"]
async fn real_model_ranks_the_thai_match_first() {
    alice_embed::install_tls_provider();
    let dir = std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("alice-models"));
    fetch_model(&dir, &|_, _, _| {}).await.unwrap();
    let e = OnnxEmbedder::new(&dir, OnnxOptions::default()).unwrap();
    let passages = vec![
        "ระบบความจำสำหรับเอเจนต์เขียนโค้ด".to_string(),
        "The deploy target is Easypanel".to_string(),
        "สูตรต้มยำกุ้ง".to_string(),
    ];
    let p = e.embed_passages(&passages).unwrap();
    let q = e.embed_query("ความจำของเอเจนต์").unwrap();
    assert_eq!(q.len(), 384);
    let scores: Vec<f32> = p.iter().map(|v| cosine(&q, v)).collect();
    assert!(scores[0] > scores[1] && scores[0] > scores[2], "{scores:?}");
}

/// Review finding M1: batches are planned from the model's own token counts, so text that
/// tokenizes densely (identifiers, JSON, hex ids) never makes a batch above 1,024 padded
/// tokens. A characters/3 estimate undercounted such text by up to 2x.
#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn passage_batches_respect_the_token_budget_for_dense_text() {
    let dir = std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".alice/models")))
        .unwrap();
    let e = OnnxEmbedder::new(&dir, OnnxOptions::default()).unwrap();
    let hex = |i: usize, n: usize| -> String {
        (0..n)
            .map(|j| format!("{:032x}", (i * 7919 + j) as u128 * 0x9e37_79b9_7f4a_7c15))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut texts: Vec<String> = Vec::new();
    for i in 0..40 {
        texts.push(match i % 4 {
            0 => hex(i, 1 + i % 9),
            1 => format!(
                "{{\"level\":\"warn\",\"ts\":\"2026-10-13T09:44:23.391Z\",\"id\":\"01K7EXAMPLE{i:015}\",\"msg\":\"retry {i}\"}} "
            )
            .repeat(1 + i % 6),
            2 => "fn upsert_batch(&self, items: &[(VecKey, Vec<f32>)]) -> Result<()> { self.put(&items) } "
                .repeat(1 + i % 7),
            _ => "short plain sentence".to_string(),
        });
    }
    let counts = e.token_counts(&texts);
    let dense = counts
        .iter()
        .zip(&texts)
        .filter(|(n, t)| **n > t.chars().count() / 3 + 4)
        .count();
    assert!(
        dense > 0,
        "the inputs should tokenize more densely than chars/3"
    );
    let batches = e.passage_batches(&texts);
    assert_eq!(batches.first().map(|r| r.start), Some(0));
    assert_eq!(batches.last().map(|r| r.end), Some(texts.len()));
    assert!(batches.windows(2).all(|w| w[0].end == w[1].start));
    for r in &batches {
        let longest = counts[r.clone()].iter().max().copied().unwrap();
        assert!(r.len() <= 32, "{r:?}");
        assert!(
            r.len() == 1 || r.len() * longest <= 1_024,
            "{r:?}: {} texts x {longest} tokens",
            r.len()
        );
    }
    // The old plan, from a characters/3 estimate, overran the budget on the same texts.
    let estimate = |t: &String| (t.chars().count() / 3 + 4).min(512);
    let (mut start, mut longest, mut overran) = (0, 0, false);
    for (i, t) in texts.iter().enumerate() {
        let n = i - start;
        if n > 0 && (n >= 32 || (n + 1) * longest.max(estimate(t)) > 1_024) {
            let real = counts[start..i].iter().max().unwrap() * n;
            overran |= real > 1_024;
            (start, longest) = (i, 0);
        }
        longest = longest.max(estimate(t));
    }
    assert!(overran, "the estimate-based plan stayed within the budget");
    let vectors = e.embed_passages(&texts).unwrap();
    assert_eq!(vectors.len(), texts.len());
}

/// Resident memory of this process in MB, as `ps` reports it.
fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

/// Idle unload with the real model: the session is dropped after the idle time, a query
/// then fails at once with `MODEL_LOADING` (search uses word matches) and starts a reload in
/// the background, passages wait for the model, and the reloaded model gives the same vectors.
#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn the_real_model_is_unloaded_when_idle_and_reloaded_on_demand() {
    use alice_embed::onnx::MODEL_LOADING;
    use std::time::{Duration, Instant};
    let dir = std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".alice/models")))
        .unwrap();
    let base = rss_mb();
    let e = OnnxEmbedder::new(
        &dir,
        OnnxOptions {
            idle_unload: Some(Duration::from_millis(500)),
            intra_threads: None,
        },
    )
    .unwrap();
    let before = e.embed_query("writer lease timeout").unwrap();
    let p = e
        .embed_passages(&["writer lease held during mining".into()])
        .unwrap();
    let loaded = rss_mb();
    let deadline = Instant::now() + Duration::from_secs(10);
    while e.is_loaded() {
        assert!(Instant::now() < deadline, "not unloaded after 10 s");
        std::thread::sleep(Duration::from_millis(50));
    }
    let unloaded = rss_mb();
    let t = Instant::now();
    let err = e
        .embed_query("writer lease timeout")
        .unwrap_err()
        .to_string();
    assert!(err.starts_with(MODEL_LOADING), "{err}");
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "the query waited {:?}",
        t.elapsed()
    );
    let reload = Instant::now();
    while !e.is_loaded() {
        assert!(
            reload.elapsed() < Duration::from_secs(30),
            "no background reload"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let reload_ms = reload.elapsed().as_millis();
    assert_eq!(e.embed_query("writer lease timeout").unwrap(), before);
    // Passages wait for a load instead of failing.
    while e.is_loaded() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let t = Instant::now();
    assert_eq!(
        e.embed_passages(&["writer lease held during mining".into()])
            .unwrap(),
        p
    );
    println!(
        "RSS: {base:.0} MB before, {loaded:.0} MB loaded, {unloaded:.0} MB unloaded; \
         background reload {reload_ms} ms; passages after an unload {} ms (load included)",
        t.elapsed().as_millis()
    );
}
