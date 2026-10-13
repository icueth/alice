//! Embedding providers. The local provider runs multilingual-e5-small (int8, 118 MB) through ONNX Runtime.
//! Model files are pinned to one Hugging Face revision and verified by SHA-256.

use alice_core::error::{AliceError, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MODEL_ID: &str = "multilingual-e5-small-q8";
pub const DIM: usize = 384;
const REPO: &str = "Xenova/multilingual-e5-small";
const REVISION: &str = "761b726dd34fb83930e26aab4e9ac3899aa1fa78";
const DEFAULT_ENDPOINT: &str = "https://huggingface.co";

/// (path in the repo, local file name, size in bytes, sha256)
const FILES: &[(&str, &str, u64, &str)] = &[
    (
        "onnx/model_quantized.onnx",
        "model_quantized.onnx",
        118_308_185,
        "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193",
    ),
    (
        "tokenizer.json",
        "tokenizer.json",
        17_082_730,
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    ),
    (
        "config.json",
        "config.json",
        658,
        "cb99455288675345e1a4f411438d5d0adbba5fbd3a67ea4fb03c015433b996c1",
    ),
    (
        "special_tokens_map.json",
        "special_tokens_map.json",
        167,
        "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
    ),
    (
        "tokenizer_config.json",
        "tokenizer_config.json",
        443,
        "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    ),
];

/// reqwest is built with `rustls-no-provider`; every binary must install a crypto provider once,
/// before any HTTP client (including rmcp's) is created. Safe to call repeatedly.
pub fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn model_dir(models_dir: &Path) -> PathBuf {
    models_dir.join(MODEL_ID)
}

pub fn is_model_present(models_dir: &Path) -> bool {
    let dir = model_dir(models_dir);
    FILES
        .iter()
        .all(|(_, name, size, _)| std::fs::metadata(dir.join(name)).is_ok_and(|m| m.len() == *size))
}

/// Download URL of one model file at the pinned revision. `endpoint` is `HF_ENDPOINT` (a mirror) if set.
fn file_url(endpoint: Option<&str>, remote: &str) -> String {
    let base = endpoint
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .unwrap_or(DEFAULT_ENDPOINT)
        .trim_end_matches('/');
    format!("{base}/{REPO}/resolve/{REVISION}/{remote}")
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Network limits for model downloads, so a stalled connection fails instead of hanging start-up.
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    connect: Duration,
    read: Duration,
}

const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(30),
    read: Duration::from_secs(60),
};

/// Download missing or damaged model files. `progress(file, bytes_done, bytes_total)` is called while downloading.
/// `HF_ENDPOINT`, if set, replaces `https://huggingface.co` (a mirror serving the same pinned revision).
pub async fn fetch_model(
    models_dir: &Path,
    progress: &(dyn Fn(&str, u64, u64) + Sync),
) -> Result<()> {
    install_tls_provider();
    let endpoint = std::env::var("HF_ENDPOINT").ok();
    fetch_files(
        &model_dir(models_dir),
        endpoint.as_deref(),
        FILES,
        TIMEOUTS,
        progress,
    )
    .await
}

/// Lock file in the model directory, held for a whole fetch so concurrent fetches run one after another.
const LOCK_FILE: &str = ".fetch.lock";

/// Waits for the exclusive fetch lock on `dir` without blocking the async runtime.
/// The lock is released when the returned file is dropped.
async fn lock_dir(dir: &Path) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Fetches each file that is missing or fails its checksum into `dir`. Holds the directory's fetch lock
/// throughout, so a second fetch waits and then finds the files valid. Each download goes to a temp file
/// unique to this call and is renamed into place only after its size and SHA-256 match; a failed download
/// leaves nothing behind.
async fn fetch_files(
    dir: &Path,
    endpoint: Option<&str>,
    files: &[(&str, &str, u64, &str)],
    timeouts: Timeouts,
    progress: &(dyn Fn(&str, u64, u64) + Sync),
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let _lock = lock_dir(dir).await?;
    // Under the lock no other fetch is writing, so any temp file is left over from a crashed run.
    for entry in std::fs::read_dir(dir)?.flatten() {
        if entry.file_name().to_string_lossy().ends_with(".part") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("alice/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.read)
        .build()
        .map_err(|e| AliceError::Other(format!("http client: {e}")))?;
    for (remote, name, size, sha) in files {
        let dest = dir.join(name);
        if dest.exists() && sha256_file(&dest)? == *sha {
            continue;
        }
        let part = dir.join(format!(
            "{name}.{}-{}.part",
            std::process::id(),
            alice_core::ids::new_id()
        ));
        let url = file_url(endpoint, remote);
        if let Err(e) = download(&client, &url, &part, name, *size, sha, progress).await {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &dest)?;
    }
    Ok(())
}

/// Error for a failed request or body read, with the whole cause chain (reqwest's own text omits it,
/// e.g. "operation timed out") and what to do next.
fn network_error(name: &str, e: &reqwest::Error) -> AliceError {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !msg.contains(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = cause.source();
    }
    AliceError::Other(format!(
        "downloading {name} failed: {msg}. Check the network or set HF_ENDPOINT to a mirror, then run \"alice model fetch\" again"
    ))
}

async fn download(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    name: &str,
    size: u64,
    sha: &str,
    progress: &(dyn Fn(&str, u64, u64) + Sync),
) -> Result<()> {
    let resp = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| network_error(name, &e))?;
    let mut file = std::fs::File::create(part)?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| network_error(name, &e))?;
        done += chunk.len() as u64;
        if done > size {
            return Err(AliceError::Other(format!(
                "downloading {name} failed: the server sent more than the expected {size} bytes. Check HF_ENDPOINT"
            )));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        progress(name, done, size);
    }
    file.sync_all()?;
    let got = hex::encode(hasher.finalize());
    if got != sha {
        return Err(AliceError::Other(format!(
            "checksum mismatch for {name}: expected {sha}, got {got}. Run \"alice model fetch\" again; if it keeps failing, check HF_ENDPOINT"
        )));
    }
    Ok(())
}

#[cfg(feature = "local")]
mod local {
    use super::*;
    use alice_core::embed::Embedder;
    use fastembed::{
        InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
        UserDefinedEmbeddingModel,
    };
    use parking_lot::{Mutex, MutexGuard};

    /// Longest input the model reads, in tokens; longer texts are truncated.
    const MAX_TOKENS: usize = 512;
    /// Most texts per inference call.
    const MAX_BATCH: usize = 32;
    /// Padded tokens (texts × longest text) per inference call. ONNX Runtime keeps the memory of
    /// its largest call: one batch of 32 texts of 512 tokens grew the process by 1.4 GB, batches
    /// of 1,024 tokens by about 100 MB (4,096: about 450 MB). It also bounds how long a query
    /// waits for the model behind the worker: one batch, ~115 ms for two 512-token texts on an
    /// M4 Pro and less for short ones. Short texts are still batched up to 32. Batches are
    /// planned from the tokenizer's own counts: a characters/3 estimate undercounted identifiers,
    /// JSON and hex ids by up to 2x.
    const BATCH_TOKENS: usize = 1_024;

    /// Consecutive ranges over texts with these token `counts` such that each range holds at
    /// most `max_batch` texts and its padded size (texts × longest) stays within `budget` tokens;
    /// a single text always forms a range of its own. Order is kept.
    pub(super) fn token_batches(
        counts: &[usize],
        max_batch: usize,
        budget: usize,
    ) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let (mut start, mut longest) = (0, 0);
        for (i, &n_tokens) in counts.iter().enumerate() {
            let n = i - start;
            if n > 0 && (n >= max_batch || (n + 1) * longest.max(n_tokens) > budget) {
                out.push(start..i);
                (start, longest) = (i, 0);
            }
            longest = longest.max(n_tokens);
        }
        if start < counts.len() {
            out.push(start..counts.len());
        }
        out
    }

    fn passage(text: &str) -> String {
        format!("passage: {text}")
    }

    /// `TextEmbedding::embed` needs `&mut self`, so calls are serialised by a mutex. One instance
    /// costs about 600 MB (the tokenizer of the 250k-token multilingual vocabulary ~250 MB, the
    /// ONNX session ~265 MB), so the daemon shares one between the embedding worker and queries.
    /// Passages are embedded in token-bounded batches and the lock is handed over fairly after
    /// each, so a query waits for one batch at most, never for a whole worker run.
    pub struct FastEmbedder {
        model: Mutex<TextEmbedding>,
    }

    impl FastEmbedder {
        pub fn new(models_dir: &Path) -> Result<Self> {
            if !is_model_present(models_dir) {
                return Err(AliceError::Other(
                    "the embedding model is not downloaded; run \"alice model fetch\"".into(),
                ));
            }
            let dir = model_dir(models_dir);
            let rd = |f: &str| std::fs::read(dir.join(f));
            let model = UserDefinedEmbeddingModel::new(
                rd("model_quantized.onnx")?,
                TokenizerFiles {
                    tokenizer_file: rd("tokenizer.json")?,
                    config_file: rd("config.json")?,
                    special_tokens_map_file: rd("special_tokens_map.json")?,
                    tokenizer_config_file: rd("tokenizer_config.json")?,
                },
            )
            .with_pooling(Pooling::Mean)
            // Static: dynamic quantization forbids batching.
            .with_quantization(QuantizationMode::Static);
            let te = TextEmbedding::try_new_from_user_defined(
                model,
                InitOptionsUserDefined::new().with_max_length(MAX_TOKENS),
            )
            .map_err(|e| {
                AliceError::Other(format!(
                    "loading the embedding model failed: {e}. Run \"alice model fetch\" to repair the model files"
                ))
            })?;
            Ok(Self {
                model: Mutex::new(te),
            })
        }
    }

    impl FastEmbedder {
        /// Tokens the model reads for each passage (prefix and special tokens included, at most
        /// 512), counted with its own tokenizer. Takes the model lock while counting.
        #[doc(hidden)]
        pub fn token_counts(&self, texts: &[String]) -> Vec<usize> {
            let model = self.model.lock();
            let counts = texts
                .iter()
                .map(|t| {
                    model
                        .tokenizer
                        .encode(passage(t), true)
                        .map_or(MAX_TOKENS, |e| e.len())
                        .min(MAX_TOKENS)
                })
                .collect();
            MutexGuard::unlock_fair(model);
            counts
        }

        /// How `embed_passages` splits `texts` into inference calls.
        #[doc(hidden)]
        pub fn passage_batches(&self, texts: &[String]) -> Vec<std::ops::Range<usize>> {
            token_batches(&self.token_counts(texts), MAX_BATCH, BATCH_TOKENS)
        }
    }

    impl Embedder for FastEmbedder {
        fn model_id(&self) -> &str {
            MODEL_ID
        }
        fn dim(&self) -> usize {
            DIM
        }
        fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for range in self.passage_batches(texts) {
                let inputs: Vec<String> = texts[range].iter().map(|t| passage(t)).collect();
                let mut model = self.model.lock();
                let vectors = model
                    .embed(inputs, None)
                    .map_err(|e| AliceError::Other(format!("embedding failed: {e}")));
                // A waiting query goes next, before this loop takes the lock again.
                MutexGuard::unlock_fair(model);
                out.extend(vectors?);
            }
            Ok(out)
        }
        fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
            let mut v = self
                .model
                .lock()
                .embed(vec![format!("query: {text}")], None)
                .map_err(|e| AliceError::Other(format!("embedding failed: {e}")))?;
            v.pop()
                .ok_or_else(|| AliceError::Other("embedding returned nothing".into()))
        }
    }
}

#[cfg(feature = "local")]
pub use local::FastEmbedder;

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates every model file at its pinned size (sparse, so no real disk use).
    fn fake_model_files(models_dir: &Path) {
        let dir = model_dir(models_dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (_, name, size, _) in FILES {
            std::fs::File::create(dir.join(name))
                .unwrap()
                .set_len(*size)
                .unwrap();
        }
    }

    #[cfg(feature = "local")]
    #[test]
    fn passages_are_batched_by_padded_tokens_in_order() {
        use super::local::token_batches;
        // Short texts (14 tokens each): up to 32 per batch.
        let short = vec![14; 70];
        assert_eq!(
            token_batches(&short, 32, 1_024),
            vec![0..32, 32..64, 64..70]
        );
        // Texts at the 512-token cap: two per batch of 1,024.
        let long = vec![512; 5];
        assert_eq!(token_batches(&long, 32, 1_024), vec![0..2, 2..4, 4..5]);
        // One long text after short ones closes the batch: padding would exceed the budget.
        let mut mixed = vec![24; 10];
        mixed.extend([512, 24]);
        assert_eq!(token_batches(&mixed, 32, 1_024), vec![0..10, 10..12]);
        // A text over the budget on its own still gets a batch; nothing is lost or reordered.
        assert_eq!(
            token_batches(&long, 32, 100),
            vec![0..1, 1..2, 2..3, 3..4, 4..5]
        );
        assert!(token_batches(&[], 32, 1_024).is_empty());
        for counts in [&short, &long, &mixed] {
            let ranges = token_batches(counts, 32, 1_024);
            assert_eq!(ranges.first().map(|r| r.start), Some(0));
            assert_eq!(ranges.last().map(|r| r.end), Some(counts.len()));
            assert!(ranges.windows(2).all(|w| w[0].end == w[1].start));
            for r in &ranges {
                let longest = counts[r.clone()].iter().max().copied().unwrap();
                assert!(r.len() == 1 || r.len() * longest <= 1_024, "{r:?}");
            }
        }
    }

    #[test]
    fn model_dir_is_named_after_the_model_id() {
        assert_eq!(
            model_dir(Path::new("/m")),
            Path::new("/m").join("multilingual-e5-small-q8")
        );
    }

    #[test]
    fn model_is_present_only_when_every_file_has_its_pinned_size() {
        let d = tempfile::tempdir().unwrap();
        assert!(!is_model_present(d.path()), "empty directory");
        fake_model_files(d.path());
        assert!(is_model_present(d.path()));
        let tok = model_dir(d.path()).join("tokenizer.json");
        std::fs::File::options()
            .write(true)
            .open(&tok)
            .unwrap()
            .set_len(10)
            .unwrap();
        assert!(!is_model_present(d.path()), "truncated download");
        std::fs::remove_file(&tok).unwrap();
        assert!(!is_model_present(d.path()), "missing file");
    }

    #[test]
    fn download_urls_pin_the_revision_and_honour_a_mirror() {
        assert_eq!(
            file_url(None, "onnx/model_quantized.onnx"),
            "https://huggingface.co/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78/onnx/model_quantized.onnx"
        );
        assert_eq!(
            file_url(Some("https://hf-mirror.example/"), "config.json"),
            "https://hf-mirror.example/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78/config.json"
        );
        assert!(file_url(Some("  "), "config.json").starts_with("https://huggingface.co/"));
    }

    const TEST_TIMEOUTS: Timeouts = Timeouts {
        connect: Duration::from_secs(2),
        read: Duration::from_millis(300),
    };

    #[derive(Clone, Copy)]
    enum Reply {
        /// The whole body at once.
        Body(&'static [u8]),
        /// The body in four pieces, 50 ms apart.
        Slow(&'static [u8]),
        /// Headers announcing the whole body, half of it, then silence.
        StallBody(&'static [u8]),
        /// Reads the request and never answers.
        StallHead,
    }

    struct Server {
        base: String,
        paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Server {
        fn hits(&self, suffix: &str) -> usize {
            let paths = self.paths.lock().unwrap();
            paths.iter().filter(|p| p.ends_with(suffix)).count()
        }
        fn total(&self) -> usize {
            self.paths.lock().unwrap().len()
        }
    }

    /// Minimal HTTP/1.1 server: answers a request whose path ends with a route's suffix as that route says,
    /// anything else with 404. Records every requested path.
    async fn serve(routes: Vec<(&'static str, Reply)>) -> Server {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = paths.clone();
        let routes = std::sync::Arc::new(routes);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, routes) = (log.clone(), routes.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head);
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    log.lock().unwrap().push(path.clone());
                    let ok = |len: usize| {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                        )
                        .into_bytes()
                    };
                    let stall = Duration::from_secs(60);
                    match routes
                        .iter()
                        .find(|(p, _)| path.ends_with(p))
                        .map(|(_, r)| *r)
                    {
                        Some(Reply::Body(b)) => {
                            let _ = sock.write_all(&[ok(b.len()), b.to_vec()].concat()).await;
                        }
                        Some(Reply::Slow(b)) => {
                            let _ = sock.write_all(&ok(b.len())).await;
                            for piece in b.chunks(b.len().div_ceil(4).max(1)) {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                let _ = sock.write_all(piece).await;
                            }
                        }
                        Some(Reply::StallBody(b)) => {
                            let _ = sock
                                .write_all(&[ok(b.len()), b[..b.len() / 2].to_vec()].concat())
                                .await;
                            tokio::time::sleep(stall).await;
                        }
                        Some(Reply::StallHead) => tokio::time::sleep(stall).await,
                        None => {
                            let _ = sock
                                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                        }
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        Server { base, paths }
    }

    fn sha_hex(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    /// Files a fetch left in `dir`, other than installed ones and the lock file.
    fn leftovers(dir: &Path, installed: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != ".fetch.lock" && !installed.contains(&n.as_str()))
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn fetch_verifies_each_file_and_skips_files_already_valid() {
        install_tls_provider();
        let s = serve(vec![
            ("/a.json", Reply::Body(b"hello")),
            ("/b.json", Reply::Body(b"world!")),
        ])
        .await;
        let d = tempfile::tempdir().unwrap();
        let (sa, sb) = (sha_hex(b"hello"), sha_hex(b"world!"));
        let files = [
            ("a.json", "a.json", 5, sa.as_str()),
            ("sub/b.json", "b.json", 6, sb.as_str()),
        ];
        std::fs::write(d.path().join("a.json.1-CRASHED.part"), b"he").unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |f: &str, done: u64, total: u64| {
            seen.lock().unwrap().push((f.to_string(), done, total));
        };
        fetch_files(d.path(), Some(&s.base), &files, TEST_TIMEOUTS, &progress)
            .await
            .unwrap();
        assert_eq!(std::fs::read(d.path().join("a.json")).unwrap(), b"hello");
        assert_eq!(std::fs::read(d.path().join("b.json")).unwrap(), b"world!");
        assert_eq!(
            leftovers(d.path(), &["a.json", "b.json"]),
            Vec::<String>::new(),
            "a temp file left by a crashed fetch is removed"
        );
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.last(), Some(&("b.json".to_string(), 6, 6)));
        assert_eq!(s.total(), 2);

        // A damaged file is fetched again; a valid one is not.
        std::fs::write(d.path().join("b.json"), b"worle!").unwrap();
        fetch_files(
            d.path(),
            Some(&s.base),
            &files,
            TEST_TIMEOUTS,
            &|_, _, _| {},
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(d.path().join("b.json")).unwrap(), b"world!");
        assert_eq!(s.total(), 3);
    }

    #[tokio::test]
    async fn fetch_rejects_wrong_or_oversized_files_and_leaves_nothing_behind() {
        install_tls_provider();
        let s = serve(vec![
            ("/wrong.json", Reply::Body(b"hellx")),
            ("/big.json", Reply::Body(b"hello world")),
        ])
        .await;
        let sha = sha_hex(b"hello");
        let cases = [
            ("wrong.json", "checksum mismatch"),
            ("big.json", "more than the expected 5 bytes"),
            ("missing.json", "Check the network"),
        ];
        for (name, want) in cases {
            let d = tempfile::tempdir().unwrap();
            let files = [(name, name, 5, sha.as_str())];
            let err = fetch_files(
                d.path(),
                Some(&s.base),
                &files,
                TEST_TIMEOUTS,
                &|_, _, _| {},
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(err.contains(want), "{name}: {err}");
            assert_eq!(leftovers(d.path(), &[]), Vec::<String>::new(), "{name}");
        }
    }

    #[tokio::test]
    async fn concurrent_fetches_download_each_file_once_and_install_it_intact() {
        install_tls_provider();
        let (a, b): (&[u8], &[u8]) = (b"alpha alpha alpha alpha", b"beta beta beta beta beta");
        let s = serve(vec![("/a.bin", Reply::Slow(a)), ("/b.bin", Reply::Slow(b))]).await;
        let d = tempfile::tempdir().unwrap();
        let (sa, sb) = (sha_hex(a), sha_hex(b));
        let files = [
            ("a.bin", "a.bin", a.len() as u64, sa.as_str()),
            ("b.bin", "b.bin", b.len() as u64, sb.as_str()),
        ];
        let none = |_: &str, _: u64, _: u64| {};
        let (r1, r2) = tokio::join!(
            fetch_files(d.path(), Some(&s.base), &files, TEST_TIMEOUTS, &none),
            fetch_files(d.path(), Some(&s.base), &files, TEST_TIMEOUTS, &none),
        );
        r1.unwrap();
        r2.unwrap();
        assert_eq!(sha_hex(&std::fs::read(d.path().join("a.bin")).unwrap()), sa);
        assert_eq!(sha_hex(&std::fs::read(d.path().join("b.bin")).unwrap()), sb);
        assert_eq!(
            (s.hits("/a.bin"), s.hits("/b.bin")),
            (1, 1),
            "the second fetch waits, then skips"
        );
        assert_eq!(
            leftovers(d.path(), &["a.bin", "b.bin"]),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn a_stalled_download_times_out_with_a_next_step_and_leaves_nothing_behind() {
        install_tls_provider();
        let body: &[u8] = b"0123456789";
        let s = serve(vec![
            ("/stall-body.json", Reply::StallBody(body)),
            ("/stall-head.json", Reply::StallHead),
        ])
        .await;
        let sha = sha_hex(body);
        for name in ["stall-body.json", "stall-head.json"] {
            let d = tempfile::tempdir().unwrap();
            let files = [(name, name, body.len() as u64, sha.as_str())];
            let fetch = fetch_files(
                d.path(),
                Some(&s.base),
                &files,
                TEST_TIMEOUTS,
                &|_, _, _| {},
            );
            let err = tokio::time::timeout(Duration::from_secs(10), fetch)
                .await
                .unwrap_or_else(|_| panic!("{name}: the fetch hung on a stalled connection"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(name), "{err}");
            assert!(err.contains("timed out"), "{err}");
            assert!(err.contains("Check the network"), "{err}");
            assert_eq!(leftovers(d.path(), &[]), Vec::<String>::new(), "{name}");
        }
    }

    #[cfg(feature = "local")]
    #[test]
    fn loading_without_the_model_says_how_to_fetch_it() {
        let d = tempfile::tempdir().unwrap();
        let Err(e) = FastEmbedder::new(d.path()) else {
            panic!("loaded a model that is not there")
        };
        assert!(e.to_string().contains("alice model fetch"), "{e}");
    }
}
