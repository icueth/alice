//! Run with: ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release -- --ignored
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine};
use alice_embed::{FastEmbedder, fetch_model};
use std::path::PathBuf;

#[tokio::test]
#[ignore = "downloads the real model"]
async fn real_model_ranks_the_thai_match_first() {
    alice_embed::install_tls_provider();
    let dir = std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("alice-models"));
    fetch_model(&dir, &|_, _, _| {}).await.unwrap();
    let e = FastEmbedder::new(&dir).unwrap();
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
