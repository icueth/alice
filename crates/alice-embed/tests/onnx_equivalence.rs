//! The compact tokenizer and the direct ONNX Runtime session give the vectors of M1's
//! fastembed build: identical token ids on a multilingual corpus, cosine ≥ 0.9999 per vector.
//!   ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release \
//!     --test onnx_equivalence -- --ignored --nocapture
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine};
use alice_embed::tokenizer::UnigramTokenizer;
use alice_embed::{OnnxEmbedder, OnnxOptions, model_dir};
use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use std::path::{Path, PathBuf};

fn models_dir() -> PathBuf {
    std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".alice/models")))
        .expect("set ALICE_MODELS_DIR")
}

/// M1's `FastEmbedder` construction, verbatim.
fn fastembed(dir: &Path) -> TextEmbedding {
    let rd = |f: &str| std::fs::read(dir.join(f)).unwrap();
    let model = UserDefinedEmbeddingModel::new(
        rd("model_quantized.onnx"),
        TokenizerFiles {
            tokenizer_file: rd("tokenizer.json"),
            config_file: rd("config.json"),
            special_tokens_map_file: rd("special_tokens_map.json"),
            tokenizer_config_file: rd("tokenizer_config.json"),
        },
    )
    .with_pooling(Pooling::Mean)
    .with_quantization(QuantizationMode::Static);
    TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new().with_max_length(512),
    )
    .unwrap()
}

/// Edge cases first, then lines from the repo's docs and sources.
fn corpus() -> Vec<String> {
    let mut v: Vec<String> = [
        "hello world",
        "  leading and trailing  ",
        "tabs\tand\nnewlines\r\nhere",
        "ภาษาไทยไม่มีช่องว่างระหว่างคำ",
        "แก้บั๊ก writer lease หมดเวลา 120 วินาที",
        "HostLinkReconnect writer_lease internal/teacher/jev/client.go",
        "ｆｕｌｌｗｉｄｔｈ ｔｅｘｔ and ﬁ ligature",
        "emoji 🎉👍🏽 and CJK 漢字かな",
        "combining e\u{0301} and ำ vs \u{0E4D}\u{0E32}",
        "private use \u{E000}\u{E001} chars",
        "a ▁ literal metaspace",
        "UPPER lower MiXeD 12345 3.14 v2.0.1",
        "no\u{00A0}break\u{3000}ideographic\u{2003}em spaces",
        "zero\u{200B}width\u{200D}joiner and\u{0000}nul\u{0007}bell",
        "x",
        "Ünïcödé Ελληνικά Кириллица עברית العربية हिन्दी",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.push("long text ".repeat(400));
    v.push("ภาษาไทย".repeat(300));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for f in [
        "README.md",
        "README.th.md",
        "docs/superpowers/specs/2026-10-13-alice-core-design.md",
    ] {
        for line in std::fs::read_to_string(root.join(f)).unwrap().lines() {
            let t = line.trim();
            if t.len() > 10
                && !["<s>", "</s>", "<pad>", "<unk>", "<mask>"]
                    .iter()
                    .any(|s| t.contains(s))
            {
                v.push(t.to_string());
            }
        }
    }
    v.truncate(2_000);
    v
}

#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn token_ids_match_hugging_face() {
    let dir = model_dir(&models_dir());
    let ours = UnigramTokenizer::from_file(&dir.join("tokenizer.json")).unwrap();
    let theirs = fastembed(&dir);
    let texts = corpus();
    let mut mismatches = Vec::new();
    for t in &texts {
        for text in [format!("passage: {t}"), format!("query: {t}")] {
            let want: Vec<u32> = theirs
                .tokenizer
                .encode(text.as_str(), true)
                .unwrap()
                .get_ids()
                .to_vec();
            let got = ours.encode(&text, 512);
            if got != want {
                mismatches.push(format!("{text:?}\n  ours   {got:?}\n  theirs {want:?}"));
            }
        }
    }
    println!(
        "token ids: {} mismatches over {} texts ({} encodings)",
        mismatches.len(),
        texts.len(),
        texts.len() * 2
    );
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The worst cosine between pairs of vectors.
fn worst(a: &[Vec<f32>], b: &[Vec<f32>]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| cosine(x, y))
        .fold(1.0f32, f32::min)
}

/// The int8 model quantizes its activations per call (`DynamicQuantizeLinear`, one scale for
/// the whole batch, padding included), so a vector depends a little on the texts it was
/// batched with. M1 called fastembed once per token-budget batch (`passage_batches`), so that
/// is the comparison asserted; batches of 8 and texts one at a time are printed for scale.
#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn vectors_match_fastembed() {
    let models = models_dir();
    let ours = OnnxEmbedder::new(&models, OnnxOptions::default()).unwrap();
    let mut theirs = fastembed(&model_dir(&models));
    let texts: Vec<String> = corpus().into_iter().take(200).collect();
    let prefixed: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
    let a = ours.embed_passages(&texts).unwrap();
    let batches = ours.passage_batches(&texts);
    let m1: Vec<Vec<f32>> = batches
        .iter()
        .flat_map(|r| theirs.embed(&prefixed[r.clone()], None).unwrap())
        .collect();
    let same_batches = worst(&a, &m1);
    println!(
        "worst passage cosine {same_batches:.6} over {} passages in M1's {} batches",
        texts.len(),
        batches.len()
    );
    let by_eight = worst(&a, &theirs.embed(prefixed.clone(), Some(8)).unwrap());
    println!("  (fastembed in batches of 8 instead: worst {by_eight:.6})");
    let alone_ours: Vec<Vec<f32>> = texts
        .iter()
        .flat_map(|t| ours.embed_passages(std::slice::from_ref(t)).unwrap())
        .collect();
    let alone_theirs: Vec<Vec<f32>> = prefixed
        .iter()
        .flat_map(|t| theirs.embed(vec![t.clone()], None).unwrap())
        .collect();
    let alone = worst(&alone_ours, &alone_theirs);
    println!("  (each text alone in both: worst {alone:.6})");
    assert!(same_batches >= 0.9999, "worst cosine {same_batches}");
    assert!(alone >= 0.9999, "worst cosine, one text per call: {alone}");
    let mut worst_query = 1.0f32;
    for t in texts.iter().take(20) {
        let q = ours.embed_query(t).unwrap();
        let r = theirs
            .embed(vec![format!("query: {t}")], None)
            .unwrap()
            .remove(0);
        let c = cosine(&q, &r);
        worst_query = worst_query.min(c);
        assert!(c >= 0.9999, "{t:?}: {c}");
    }
    println!("worst query cosine {worst_query:.6} over 20 queries");
}
