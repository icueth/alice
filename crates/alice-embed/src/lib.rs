//! Embedding providers. The local provider runs multilingual-e5-small (int8, 118 MB) through ONNX Runtime.
//! Model files are pinned to one Hugging Face revision and verified by SHA-256.

use alice_core::error::{AliceError, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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

/// Download missing or damaged model files. `progress(file, bytes_done, bytes_total)` is called while downloading.
/// `HF_ENDPOINT`, if set, replaces `https://huggingface.co` (a mirror serving the same pinned revision).
pub async fn fetch_model(
    models_dir: &Path,
    progress: &(dyn Fn(&str, u64, u64) + Sync),
) -> Result<()> {
    install_tls_provider();
    let endpoint = std::env::var("HF_ENDPOINT").ok();
    fetch_files(&model_dir(models_dir), endpoint.as_deref(), FILES, progress).await
}

/// Fetches each file that is missing or fails its checksum into `dir`. A download goes to `<name>.part`
/// and is renamed into place only after its size and SHA-256 match; a failed download leaves nothing behind.
async fn fetch_files(
    dir: &Path,
    endpoint: Option<&str>,
    files: &[(&str, &str, u64, &str)],
    progress: &(dyn Fn(&str, u64, u64) + Sync),
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("alice/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| AliceError::Other(format!("http client: {e}")))?;
    for (remote, name, size, sha) in files {
        let dest = dir.join(name);
        if dest.exists() && sha256_file(&dest)? == *sha {
            continue;
        }
        let part = dir.join(format!("{name}.part"));
        let url = file_url(endpoint, remote);
        if let Err(e) = download(&client, &url, &part, name, *size, sha, progress).await {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &dest)?;
    }
    Ok(())
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
        .map_err(|e| {
            AliceError::Other(format!(
                "downloading {name} failed: {e}. Check the network or set HF_ENDPOINT to a mirror"
            ))
        })?;
    let mut file = std::fs::File::create(part)?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| AliceError::Other(format!("downloading {name} failed: {e}")))?;
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
    use parking_lot::Mutex;

    /// `TextEmbedding::embed` needs `&mut self`, so calls are serialised by a mutex.
    /// The daemon creates two instances (worker and queries) so they never wait on each other.
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
                InitOptionsUserDefined::new().with_max_length(512),
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

    impl Embedder for FastEmbedder {
        fn model_id(&self) -> &str {
            MODEL_ID
        }
        fn dim(&self) -> usize {
            DIM
        }
        fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let inputs: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
            self.model
                .lock()
                .embed(inputs, Some(32))
                .map_err(|e| AliceError::Other(format!("embedding failed: {e}")))
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

    /// Minimal HTTP/1.1 server: answers each request whose path ends with a route's suffix with its body,
    /// anything else with 404. Returns the base URL and a request counter.
    async fn serve(
        routes: Vec<(&'static str, &'static [u8])>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head);
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let reply = match routes.iter().find(|(p, _)| path.ends_with(p)) {
                    Some((_, body)) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = sock.write_all(&reply).await;
                let _ = sock.shutdown().await;
            }
        });
        (base, hits)
    }

    fn sha_hex(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    #[tokio::test]
    async fn fetch_verifies_each_file_and_skips_files_already_valid() {
        install_tls_provider();
        let (base, hits) = serve(vec![("/a.json", b"hello"), ("/b.json", b"world!")]).await;
        let d = tempfile::tempdir().unwrap();
        let (sa, sb) = (sha_hex(b"hello"), sha_hex(b"world!"));
        let files = [
            ("a.json", "a.json", 5, sa.as_str()),
            ("sub/b.json", "b.json", 6, sb.as_str()),
        ];
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |f: &str, done: u64, total: u64| {
            seen.lock().unwrap().push((f.to_string(), done, total));
        };
        fetch_files(d.path(), Some(&base), &files, &progress)
            .await
            .unwrap();
        assert_eq!(std::fs::read(d.path().join("a.json")).unwrap(), b"hello");
        assert_eq!(std::fs::read(d.path().join("b.json")).unwrap(), b"world!");
        assert!(!d.path().join("a.json.part").exists());
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.last(), Some(&("b.json".to_string(), 6, 6)));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);

        // A damaged file is fetched again; a valid one is not.
        std::fs::write(d.path().join("b.json"), b"worle!").unwrap();
        fetch_files(d.path(), Some(&base), &files, &|_, _, _| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(d.path().join("b.json")).unwrap(), b"world!");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn fetch_rejects_wrong_or_oversized_files_and_leaves_nothing_behind() {
        install_tls_provider();
        let (base, _) = serve(vec![
            ("/wrong.json", b"hellx"),
            ("/big.json", b"hello world"),
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
            let err = fetch_files(d.path(), Some(&base), &files, &|_, _, _| {})
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{name}: {err}");
            let left: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
            assert!(left.is_empty(), "{name} left files behind: {left:?}");
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
