//! Test support shared by the tests of `embedding` and `reembed`: factories that build named
//! fakes, counting embedders, and a mock OpenAI-compatible server on the loopback.

use super::*;
use crate::service::{Service, ServiceOptions};
use alice_core::embed::FakeEmbedder;
use alice_core::settings::EmbeddingProvider as P;
use alice_embed::http::{HttpEmbedder, HttpProvider};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::AtomicUsize;

/// Named fakes: the local id has 384 dimensions, any other 64.
pub fn fake_factory() -> EmbedderFactory {
    Arc::new(|s: &EmbeddingSettings, _key| {
        let id = expected_model_id(s);
        let dim = if id == MODEL_ID { 384 } else { 64 };
        Ok(Arc::new(FakeEmbedder::named(id, dim)) as Arc<dyn Embedder>)
    })
}

/// A target embedder that counts the texts it embeds, takes `delay` per batch, and notes
/// how many embedders of this kind work at once.
pub struct Counting {
    pub inner: FakeEmbedder,
    pub texts: Arc<AtomicUsize>,
    pub delay: Duration,
    pub remote: bool,
    /// Embedders working right now / the most there ever were (shared by a factory).
    pub busy: Arc<(AtomicUsize, AtomicUsize)>,
}

impl Embedder for Counting {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed_passages(&self, t: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
        let now = self.busy.0.fetch_add(1, Ordering::SeqCst) + 1;
        self.busy.1.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        self.texts.fetch_add(t.len(), Ordering::SeqCst);
        let r = self.inner.embed_passages(t);
        self.busy.0.fetch_sub(1, Ordering::SeqCst);
        r
    }
    fn embed_query(&self, q: &str) -> alice_core::Result<Vec<f32>> {
        self.inner.embed_query(q)
    }
    fn is_remote(&self) -> bool {
        self.remote
    }
}

/// The local model as before; any other model is a `Counting` fake.
pub fn counting_factory(texts: Arc<AtomicUsize>, delay: Duration, remote: bool) -> EmbedderFactory {
    let busy: Arc<(AtomicUsize, AtomicUsize)> = Arc::default();
    counting_factory_shared(texts, delay, remote, busy)
}

pub fn counting_factory_shared(
    texts: Arc<AtomicUsize>,
    delay: Duration,
    remote: bool,
    busy: Arc<(AtomicUsize, AtomicUsize)>,
) -> EmbedderFactory {
    Arc::new(move |s: &EmbeddingSettings, _key| {
        let id = expected_model_id(s);
        if id == MODEL_ID {
            return Ok(Arc::new(FakeEmbedder::named(id, 384)) as Arc<dyn Embedder>);
        }
        Ok(Arc::new(Counting {
            inner: FakeEmbedder::named(id, 64),
            texts: texts.clone(),
            delay,
            remote,
            busy: busy.clone(),
        }) as Arc<dyn Embedder>)
    })
}

/// An embedder that says it is remote (the cloud policy applies), whatever its endpoint:
/// the mock server lives on the loopback.
pub struct ForceRemote(pub Arc<dyn Embedder>);

impl Embedder for ForceRemote {
    fn model_id(&self) -> &str {
        self.0.model_id()
    }
    fn dim(&self) -> usize {
        self.0.dim()
    }
    fn embed_passages(&self, t: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
        self.0.embed_passages(t)
    }
    fn embed_query(&self, q: &str) -> alice_core::Result<Vec<f32>> {
        self.0.embed_query(q)
    }
    fn max_batch(&self) -> usize {
        self.0.max_batch()
    }
    fn is_remote(&self) -> bool {
        true
    }
}

/// An OpenAI-compatible `/v1/embeddings` server on 127.0.0.1 that answers 8-dimension
/// vectors and keeps every text it was sent.
pub struct Mock {
    pub base: String,
    inputs: Arc<Mutex<Vec<String>>>,
    heads: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    pub fn start() -> Self {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let inputs: Arc<Mutex<Vec<String>>> = Arc::default();
        let heads: Arc<Mutex<Vec<String>>> = Arc::default();
        let (log, hlog) = (inputs.clone(), heads.clone());
        std::thread::spawn(move || {
            let embedder = FakeEmbedder::named("mock", 8);
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let (log, hlog) = (log.clone(), hlog.clone());
                let embedder = FakeEmbedder::named("mock", embedder.dim());
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some(i) = text.find("\r\n\r\n") {
                            let len: usize = text
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .map(|v| v.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            if buf.len() >= i + 4 + len {
                                break (text[..i].to_string(), text[i + 4..].to_string());
                            }
                        }
                        if n == 0 {
                            return;
                        }
                    };
                    let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                    let texts: Vec<String> = req["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t.as_str().unwrap().to_string())
                        .collect();
                    log.lock().extend(texts.iter().cloned());
                    hlog.lock().push(head);
                    let data: Vec<serde_json::Value> = embedder
                        .embed_passages(&texts)
                        .unwrap()
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| serde_json::json!({"index": i, "embedding": v}))
                        .collect();
                    let body = serde_json::json!({"data": data}).to_string();
                    let _ = s.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                });
            }
        });
        Self {
            base,
            inputs,
            heads,
        }
    }

    /// Every text received, the dimension probe left out.
    pub fn texts(&self) -> Vec<String> {
        self.inputs
            .lock()
            .iter()
            .filter(|t| *t != "dimension probe")
            .cloned()
            .collect()
    }

    /// Whether any text received contains `needle`.
    pub fn saw(&self, needle: &str) -> bool {
        self.texts().iter().any(|t| t.contains(needle))
    }

    /// The request heads received (they hold the headers).
    pub fn heads(&self) -> Vec<String> {
        self.heads.lock().clone()
    }
}

/// A factory over the mock: HTTP providers talk to the endpoint in the settings; with
/// `force_remote` they count as remote even on the loopback. The local model is a fake.
pub fn http_factory(force_remote: bool, keys: Arc<Mutex<Vec<Option<String>>>>) -> EmbedderFactory {
    Arc::new(move |s: &EmbeddingSettings, key| {
        keys.lock()
            .push(key.as_ref().map(|k| k.expose().to_string()));
        let provider = match s.provider {
            P::Local => {
                return Ok(Arc::new(FakeEmbedder::named(MODEL_ID, 384)) as Arc<dyn Embedder>);
            }
            P::Openai => HttpProvider::OpenAi,
            P::Ollama => HttpProvider::Ollama,
            P::Voyage => HttpProvider::Voyage,
        };
        let e = HttpEmbedder::new(provider, &s.model, s.endpoint.as_deref(), key, s.batch_size)?
            .with_backoff(Duration::from_millis(5));
        Ok(if force_remote {
            Arc::new(ForceRemote(Arc::new(e))) as Arc<dyn Embedder>
        } else {
            Arc::new(e) as Arc<dyn Embedder>
        })
    })
}

/// The key a test hands the factory through `ALICE_EMBEDDING_API_KEY` (never the shell's).
pub const TEST_KEY: &str = concat!("sk-", "test-embedding-0123456789abcdef");

/// Options with a factory, the test key in the (test-owned) environment, and fast polls.
pub fn options(factory: EmbedderFactory) -> ServiceOptions {
    ServiceOptions {
        embedder_factory: Some(factory),
        secret_env: Some(Arc::new(|v: &str| {
            (v == "ALICE_EMBEDDING_API_KEY").then(|| TEST_KEY.to_string())
        })),
        reembed_retry_wait: Duration::from_millis(5),
        ..Default::default()
    }
}

/// Starts a service in `dir` (a home) whose `config.toml` holds `toml`.
pub fn start(dir: &Path, opts: ServiceOptions, toml: &str) -> Arc<Service> {
    if !toml.is_empty() {
        std::fs::write(AliceHome::new(dir.to_path_buf()).config_file(), toml).unwrap();
    }
    Service::start(
        AliceHome::new(dir.to_path_buf()),
        &|| Ok(Arc::new(FakeEmbedder::named(MODEL_ID, 384)) as Arc<dyn Embedder>),
        opts,
    )
    .unwrap()
}

/// Until `cond` holds, polling every 20 ms for up to `secs`.
pub async fn until<F, Fut>(what: &str, secs: u64, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while !cond().await {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A directory under `d` to use as a working directory (its name is the project's slug).
pub fn repo(d: &tempfile::TempDir, name: &str) -> Option<String> {
    let repo = d.path().join(name);
    std::fs::create_dir_all(&repo).unwrap();
    Some(repo.to_string_lossy().into_owned())
}

/// Until no memory or chunk waits for the embedding worker or the judge.
pub async fn settled(s: &Service) {
    until("the background work to settle", 30, || async {
        let h = s.health().await.unwrap();
        h.embed_backlog == 0 && h.judge_backlog == 0
    })
    .await;
}

/// A remote fake that is down while `up` is false (every call fails as "unavailable") and
/// notes every text it is handed, down or up.
pub struct Gate {
    pub inner: FakeEmbedder,
    pub up: Arc<AtomicBool>,
    pub seen: Arc<Mutex<Vec<String>>>,
}

impl Embedder for Gate {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed_passages(&self, t: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
        self.seen.lock().extend(t.iter().cloned());
        if !self.up.load(Ordering::SeqCst) {
            return Err(alice_core::AliceError::Unavailable(
                "scripted outage".into(),
            ));
        }
        self.inner.embed_passages(t)
    }
    fn embed_query(&self, q: &str) -> alice_core::Result<Vec<f32>> {
        self.inner.embed_query(q)
    }
    fn is_remote(&self) -> bool {
        true
    }
}

/// The local model is a fake; any other model is a [`Gate`].
pub fn gate_factory(up: Arc<AtomicBool>, seen: Arc<Mutex<Vec<String>>>) -> EmbedderFactory {
    Arc::new(move |s: &EmbeddingSettings, _key| {
        let id = expected_model_id(s);
        if id == MODEL_ID {
            return Ok(Arc::new(FakeEmbedder::named(id, 384)) as Arc<dyn Embedder>);
        }
        Ok(Arc::new(Gate {
            inner: FakeEmbedder::named(id, 64),
            up: up.clone(),
            seen: seen.clone(),
        }) as Arc<dyn Embedder>)
    })
}

/// A remote fake with a request size (like Voyage's 128): it refuses a call of more texts, notes
/// every text it is handed, and runs `after_first` once after its first call.
pub struct Capped {
    pub inner: FakeEmbedder,
    pub cap: usize,
    pub seen: Arc<Mutex<Vec<String>>>,
    pub after_first: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Embedder for Capped {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed_passages(&self, t: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
        assert!(
            t.len() <= self.cap,
            "a call of {} texts for a cap of {}",
            t.len(),
            self.cap
        );
        self.seen.lock().extend(t.iter().cloned());
        let r = self.inner.embed_passages(t);
        if let Some(f) = self.after_first.lock().take() {
            f();
        }
        r
    }
    fn embed_query(&self, q: &str) -> alice_core::Result<Vec<f32>> {
        self.inner.embed_query(q)
    }
    fn max_batch(&self) -> usize {
        self.cap
    }
    fn is_remote(&self) -> bool {
        true
    }
}
