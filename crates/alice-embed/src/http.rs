//! HTTP embedding providers (spec §4.2): OpenAI-compatible `/embeddings` (OpenAI, Ollama's
//! `/v1`, and other compatible servers) and Voyage (the same shape plus `input_type`). Vectors
//! are L2-normalised; the dimension is probed once at start.
//!
//! What goes over the wire, and what does not:
//! - The request carries the model, the texts and (Voyage) `input_type`; the key goes only in an
//!   `Authorization` header that is marked sensitive, built when a request is made. The
//!   `User-Agent` is `alice/<version>` and nothing else names this machine or its owner.
//! - An error says which provider, the status and a fixed reason. It never repeats what the
//!   server sent back (a body may echo a key or a text) and never names the key.
//! - A request is made once. Only the start-up probe (which carries no user text) is tried
//!   again, and only on 429, 5xx and timeouts. Whoever asks for passages again after a failure
//!   (the embedding worker, the re-embed) first decides again whether the texts may still be
//!   sent: a project denied while the provider was down is never sent after it is back.
//! - A search query is tried once, quickly ([`QUERY_TIMEOUT`]), and after a failure further
//!   queries are not sent for a while ([`QUERY_COOLDOWN`]), so a provider that is down never
//!   makes `recall` wait: it answers from word matches.
//! - Redirects are not followed and no proxy of the environment is used (`BlockingHttp::direct`).
//! - The key is held as a [`SecretValue`] and read only to build a request's header.
//! - Whether texts may be sent at all is the daemon's decision (`Embedder::is_remote` and the
//!   cloud policy); this module sends what it is given.

use crate::net::{BlockingHttp, HttpError};
use alice_core::embed::Embedder;
use alice_core::error::{AliceError, Result};
use alice_core::normalize::truncate_chars;
use alice_core::settings::{EmbeddingProvider, SecretValue, endpoint_is_remote};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProvider {
    OpenAi,
    Ollama,
    Voyage,
}

impl HttpProvider {
    pub fn name(self) -> &'static str {
        match self {
            HttpProvider::OpenAi => "openai",
            HttpProvider::Ollama => "ollama",
            HttpProvider::Voyage => "voyage",
        }
    }

    fn default_endpoint(self) -> &'static str {
        match self {
            HttpProvider::OpenAi => "https://api.openai.com/v1",
            HttpProvider::Ollama => "http://127.0.0.1:11434/v1",
            HttpProvider::Voyage => "https://api.voyageai.com/v1",
        }
    }

    fn needs_key(self) -> bool {
        !matches!(self, HttpProvider::Ollama)
    }

    /// Most texts one request may carry.
    fn max_batch(self) -> usize {
        match self {
            HttpProvider::Voyage => 128,
            HttpProvider::OpenAi | HttpProvider::Ollama => 2048,
        }
    }

    fn setting(self) -> EmbeddingProvider {
        match self {
            HttpProvider::OpenAi => EmbeddingProvider::Openai,
            HttpProvider::Ollama => EmbeddingProvider::Ollama,
            HttpProvider::Voyage => EmbeddingProvider::Voyage,
        }
    }
}

pub struct HttpEmbedder {
    provider: HttpProvider,
    model: String,
    id: String,
    url: String,
    key: Option<SecretValue>,
    dim: usize,
    batch: usize,
    remote: bool,
    http: BlockingHttp,
    /// Wait before the first retry (it doubles with each); tests shorten it.
    backoff: Duration,
    /// Milliseconds after `epoch` until which queries are not sent (0: none), set by a failed
    /// query.
    query_blocked_until: AtomicU64,
    epoch: Instant,
}

/// A request for passages gives up after this long.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The first request, which only finds the dimension, is shorter: it runs when a model is chosen.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// A search query waits this long for its vector, once. Below every client's recall deadline
/// (5 s), so a provider that does not answer costs a recall this and not its answer.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// After a failed query no query is sent for this long.
const QUERY_COOLDOWN: Duration = Duration::from_secs(30);
/// The probe is tried once more at most.
const PROBE_RETRIES: u32 = 1;
/// A `Retry-After` longer than this is not waited for.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
/// A text longer than this many characters is cut before it is sent (a provider's own limit is
/// in tokens and a refused text would fail the whole batch).
const MAX_TEXT_CHARS: usize = 6_000;

impl HttpEmbedder {
    /// `endpoint` is the base URL (without `/embeddings`); the provider's own when `None`.
    /// Asks the provider for one vector to learn the dimension, so a wrong key, model or
    /// endpoint fails here and not on the first memory.
    pub fn new(
        provider: HttpProvider,
        model: &str,
        endpoint: Option<&str>,
        key: Option<SecretValue>,
        batch: usize,
    ) -> Result<Self> {
        if provider.needs_key() && key.as_ref().is_none_or(|k| k.expose().trim().is_empty()) {
            return Err(AliceError::invalid(format!(
                "the {} embedding provider needs an API key; set it with `alice config set-secret embedding_api_key`",
                provider.name()
            )));
        }
        let mut base = endpoint
            .unwrap_or(provider.default_endpoint())
            .trim()
            .trim_end_matches('/')
            .to_string();
        // Ollama serves the OpenAI shape under /v1: `http://localhost:11434` means that.
        if provider == HttpProvider::Ollama
            && base
                .split_once("://")
                .is_some_and(|(_, rest)| !rest.contains('/'))
        {
            base.push_str("/v1");
        }
        let mut e = Self {
            provider,
            model: model.into(),
            id: format!("{}:{model}", provider.name()),
            url: format!("{base}/embeddings"),
            key,
            dim: 0,
            batch: batch.clamp(1, provider.max_batch()),
            remote: endpoint_is_remote(provider.setting(), endpoint),
            http: BlockingHttp::direct(concat!("alice/", env!("CARGO_PKG_VERSION")))
                .map_err(AliceError::Io)?,
            backoff: Duration::from_millis(500),
            query_blocked_until: AtomicU64::new(0),
            epoch: Instant::now(),
        };
        e.dim = e
            .call(
                &["dimension probe".to_string()],
                "query",
                PROBE_TIMEOUT,
                PROBE_RETRIES,
            )?
            .pop()
            .map(|v| v.len())
            .unwrap_or(0);
        if e.dim == 0 {
            return Err(AliceError::Other(format!(
                "{} returned an empty vector; check embedding.model",
                e.id
            )));
        }
        Ok(e)
    }

    /// The wait before the first retry (it doubles with each). For tests.
    #[must_use]
    pub fn with_backoff(mut self, first: Duration) -> Self {
        self.backoff = first;
        self
    }

    /// `texts` (cut to a size providers accept) as the request's `input`.
    fn inputs(texts: &[String]) -> Vec<String> {
        texts
            .iter()
            .map(|t| {
                if t.trim().is_empty() {
                    // Providers refuse an empty string.
                    "(empty)".to_string()
                } else {
                    truncate_chars(t, MAX_TEXT_CHARS)
                }
            })
            .collect()
    }

    /// One request. `retries` (429, 5xx and timeouts only) is for the probe: texts are asked
    /// once per call, and whoever asks again (the embedding worker, the re-embed) decides first,
    /// every time, whether the text may still be sent.
    fn call(
        &self,
        texts: &[String],
        input_type: &str,
        timeout: Duration,
        retries: u32,
    ) -> Result<Vec<Vec<f32>>> {
        let mut body = serde_json::json!({"model": self.model, "input": Self::inputs(texts)});
        if self.provider == HttpProvider::Voyage {
            body["input_type"] = serde_json::Value::String(input_type.into());
        }
        let bytes = serde_json::to_vec(&body).expect("json");
        let mut last = String::new();
        for attempt in 0..=retries {
            let mut wait = self.backoff.saturating_mul(2u32.saturating_pow(attempt));
            // The key is read only to make this request's header.
            let bearer = self.key.as_ref().map(|k| k.expose());
            match self
                .http
                .post_json(&self.url, bearer, bytes.clone(), timeout)
            {
                Ok(r) if r.status == 200 => return self.parse(&r.body, texts.len()),
                Ok(r) if r.status == 401 || r.status == 403 => {
                    return Err(AliceError::Unavailable(format!(
                        "{} refused the API key ({}); set it with `alice config set-secret embedding_api_key`",
                        self.id, r.status
                    )));
                }
                Ok(r) if r.status == 429 || r.status >= 500 => {
                    last = format!("{} answered {}", self.id, r.status);
                    if let Some(after) = retry_after(&r.headers) {
                        wait = after.min(MAX_RETRY_AFTER);
                    }
                }
                Ok(r) => {
                    // Nothing the server said is repeated: a body may echo a key or a text.
                    return Err(AliceError::Other(format!(
                        "{} answered {}; check embedding.model and embedding.endpoint",
                        self.id, r.status
                    )));
                }
                Err(HttpError::Timeout) => last = format!("{} did not answer in time", self.id),
                // Not retried: a refused connection will not change in a moment.
                Err(HttpError::Connect(_)) => {
                    return Err(AliceError::Unavailable(format!(
                        "cannot reach {}; memories stay searchable by words and are embedded later",
                        self.id
                    )));
                }
                Err(HttpError::Build(_)) => {
                    return Err(AliceError::Other(format!(
                        "cannot send the request to {}; check embedding.endpoint and the API key",
                        self.id
                    )));
                }
                Err(HttpError::Other(_)) => {
                    return Err(AliceError::Unavailable(format!(
                        "the connection to {} failed; memories stay searchable by words and are embedded later",
                        self.id
                    )));
                }
            }
            if attempt < retries {
                std::thread::sleep(wait);
            }
        }
        Err(AliceError::Unavailable(format!(
            "{last}; memories stay searchable by words and are embedded later"
        )))
    }

    fn parse(&self, body: &[u8], expected: usize) -> Result<Vec<Vec<f32>>> {
        #[derive(serde::Deserialize)]
        struct Item {
            index: usize,
            embedding: Vec<f32>,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<Item>,
        }
        let mut r: Resp = serde_json::from_slice(body).map_err(|_| {
            AliceError::Other(format!(
                "{} sent an answer that could not be read; check embedding.endpoint",
                self.id
            ))
        })?;
        if r.data.len() != expected {
            return Err(AliceError::Other(format!(
                "{} returned {} vectors for {expected} texts",
                self.id,
                r.data.len()
            )));
        }
        r.data.sort_by_key(|i| i.index);
        let mut out = Vec::with_capacity(expected);
        for (i, item) in r.data.into_iter().enumerate() {
            if item.index != i {
                return Err(AliceError::Other(format!(
                    "{} numbered its vectors wrongly",
                    self.id
                )));
            }
            if (self.dim != 0 && item.embedding.len() != self.dim)
                || item.embedding.iter().any(|x| !x.is_finite())
            {
                return Err(AliceError::Other(format!(
                    "{} returned a vector of another size or with invalid numbers; check embedding.model",
                    self.id
                )));
            }
            let n = item
                .embedding
                .iter()
                .map(|x| x * x)
                .sum::<f32>()
                .sqrt()
                .max(1e-12);
            out.push(item.embedding.into_iter().map(|x| x / n).collect());
        }
        Ok(out)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }
}

/// `Retry-After` in seconds (the date form is not honoured).
fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    headers
        .iter()
        .find(|(k, _)| k == "retry-after")
        .and_then(|(_, v)| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

impl Embedder for HttpEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // One call, one request: the caller asked the cloud policy once, just before this call,
        // so a second request a minute later would send what that answer no longer covers.
        if texts.len() > self.batch {
            return Err(AliceError::Other(format!(
                "{} was given {} texts for a request of at most {}; cut the batch to Embedder::max_batch",
                self.id,
                texts.len(),
                self.batch
            )));
        }
        self.call(texts, "document", TIMEOUT, 0)
    }

    fn max_batch(&self) -> usize {
        self.batch
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        if self.query_blocked_until.load(Ordering::Relaxed) > self.now_ms() {
            return Err(AliceError::Unavailable(format!(
                "{} did not answer a moment ago; this search used word matches only",
                self.id
            )));
        }
        let r = self
            .call(&[text.to_string()], "query", QUERY_TIMEOUT, 0)
            .and_then(|mut v| v.pop().ok_or_else(|| AliceError::Other("no vector".into())));
        if r.is_err() {
            self.query_blocked_until.store(
                self.now_ms() + QUERY_COOLDOWN.as_millis() as u64,
                Ordering::Relaxed,
            );
        }
        r
    }

    fn is_remote(&self) -> bool {
        self.remote
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// One request the mock saw.
    #[derive(Clone)]
    struct Seen {
        head: String,
        body: serde_json::Value,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    /// A mock OpenAI-compatible server on a thread of its own. `reply(call_index, head, request)`
    /// answers `(status, body)`; the default one (`mock`) answers 3-dimension vectors
    /// `[len(text), 1, 0]`, out of order (to check `index` is honoured).
    fn mock_with(
        reply: impl Fn(usize, &str, &serde_json::Value) -> (u16, String) + Send + Sync + 'static,
    ) -> (String, Log) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let seen: Log = Arc::default();
        let log = seen.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reply = Arc::new(reply);
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let (log, calls, reply) = (log.clone(), calls.clone(), reply.clone());
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
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
                    let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    log.lock().unwrap().push(Seen {
                        head: head.clone(),
                        body: req.clone(),
                    });
                    let (status, body) = reply(n, &head, &req);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        (base, seen)
    }

    fn vectors(req: &serde_json::Value) -> String {
        let inputs = req["input"].as_array().unwrap();
        let data: Vec<serde_json::Value> = inputs
            .iter()
            .enumerate()
            .rev()
            .map(|(i, t)| {
                serde_json::json!({"index": i, "embedding": [t.as_str().unwrap().len() as f32, 1.0, 0.0]})
            })
            .collect();
        serde_json::json!({"data": data}).to_string()
    }

    /// 401 for the key "bad"; 429 for the first call when asked to.
    fn mock(first_429: bool) -> (String, Log) {
        mock_with(move |n, head, req| {
            if head.to_ascii_lowercase().contains("bearer bad") {
                (401, "{\"error\":\"bad key\"}".to_string())
            } else if first_429 && n == 0 {
                (429, "{}".to_string())
            } else {
                (200, vectors(req))
            }
        })
    }

    fn quick(
        provider: HttpProvider,
        model: &str,
        base: &str,
        key: Option<&str>,
        batch: usize,
    ) -> Result<HttpEmbedder> {
        HttpEmbedder::new(
            provider,
            model,
            Some(base),
            key.map(SecretValue::new),
            batch,
        )
        .map(|e| e.with_backoff(Duration::from_millis(5)))
    }

    #[test]
    fn embeds_in_order_normalised_with_a_probed_dimension() {
        let (base, seen) = mock(true);
        let e = quick(HttpProvider::Voyage, "voyage-3-lite", &base, Some("k"), 2).unwrap();
        // Callers cut to `max_batch` (2): one request each.
        let mut v = e.embed_passages(&["a".into(), "bbb".into()]).unwrap();
        v.extend(e.embed_passages(&["cc".into()]).unwrap());
        let v = vec![v[0].clone(), v[1].clone(), v[2].clone()];
        let q = e.embed_query("dddd").unwrap();
        assert_eq!((e.model_id(), e.dim()), ("voyage:voyage-3-lite", 3));
        let norm = |x: f32, y: f32| x / (x * x + y * y).sqrt();
        assert!(
            (v[1][0] - norm(3.0, 1.0)).abs() < 1e-6,
            "order follows `index`: {v:?}"
        );
        assert!((q[0] - norm(4.0, 1.0)).abs() < 1e-6);
        let reqs = seen.lock().unwrap();
        assert!(reqs.iter().any(|r| r.body["input_type"] == "document"));
        assert!(reqs.iter().any(|r| r.body["input_type"] == "query"));
        assert!(
            reqs.iter()
                .all(|r| r.body["input"].as_array().unwrap().len() <= 2),
            "batches of batch_size"
        );
    }

    #[test]
    fn a_rejected_key_says_which_secret_to_set_and_ollama_needs_none() {
        let (base, _) = mock(false);
        let err = quick(HttpProvider::OpenAi, "m", &base, Some("bad"), 8)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("embedding_api_key"), "{err}");
        assert!(
            !err.contains("bad key"),
            "the server's words are not repeated: {err}"
        );
        let ok = quick(HttpProvider::Ollama, "nomic-embed-text", &base, None, 8)
            .map(|e| e.model_id().to_string())
            .unwrap();
        assert_eq!(ok, "ollama:nomic-embed-text");
        assert!(
            HttpEmbedder::new(
                HttpProvider::OpenAi,
                "m",
                Some("http://127.0.0.1:9"),
                None,
                8
            )
            .err()
            .unwrap()
            .to_string()
            .contains("embedding_api_key"),
            "openai needs a key"
        );
    }

    #[test]
    fn the_key_goes_in_a_bearer_header_with_our_user_agent_and_nowhere_else() {
        let key = concat!("sk-", "emb-header-check-0123456789");
        let (base, seen) = mock(false);
        let e = quick(HttpProvider::OpenAi, "m", &base, Some(key), 8).unwrap();
        e.embed_passages(&["hello".into()]).unwrap();
        for r in seen.lock().unwrap().iter() {
            let head = r.head.to_ascii_lowercase();
            assert!(
                head.contains(&format!(
                    "authorization: bearer {}",
                    key.to_ascii_lowercase()
                )),
                "{head}"
            );
            assert!(
                head.contains(&format!("user-agent: alice/{}", env!("CARGO_PKG_VERSION"))),
                "{head}"
            );
            assert!(
                !r.body.to_string().contains(key),
                "the key is not in the body"
            );
            assert!(
                !head.starts_with("post /v1/embeddings?"),
                "no key in the URL: {head}"
            );
        }
    }

    #[test]
    fn errors_carry_a_status_and_a_fixed_reason_never_the_servers_words() {
        // A 400 whose body echoes a key and a text.
        let echo = concat!("sk-", "echoed-by-the-server-9876543210");
        let (base, _) = mock_with(move |n, _, req| {
            if n == 0 {
                (200, vectors(req))
            } else {
                (
                    400,
                    format!("{{\"error\":\"bad input {echo} secret text\"}}"),
                )
            }
        });
        let e = quick(HttpProvider::OpenAi, "m", &base, Some("k"), 8).unwrap();
        let err = e.embed_passages(&["x".into()]).unwrap_err().to_string();
        assert!(
            err.contains("400") && err.contains("embedding.model"),
            "{err}"
        );
        assert!(!err.contains(echo) && !err.contains("secret text"), "{err}");
        // An answer that is not JSON: fixed text, not the parser's.
        let (base, _) = mock_with(|n, _, req| {
            if n == 0 {
                (200, vectors(req))
            } else {
                (200, "<html>nope</html>".into())
            }
        });
        let e = quick(HttpProvider::OpenAi, "m", &base, Some("k"), 8).unwrap();
        let err = e.embed_passages(&["x".into()]).unwrap_err().to_string();
        assert!(
            err.contains("could not be read") && !err.contains("html"),
            "{err}"
        );
    }

    #[test]
    fn a_text_is_asked_once_and_only_the_probe_is_tried_again() {
        // Always 500: the probe tries twice (one retry) and fails.
        let (base, seen) = mock_with(|_, _, _| (500, "{}".into()));
        let err = quick(HttpProvider::Ollama, "m", &base, None, 8)
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("500") && err.contains("searchable by words"),
            "{err}"
        );
        assert_eq!(seen.lock().unwrap().len(), 1 + PROBE_RETRIES as usize);
        // Passages: 429, 5xx, 404 and a refused key are each asked once and say whether the
        // provider is unavailable (the text stays queued) or the request is wrong.
        for (status, unavailable) in [(500, true), (429, true), (401, true), (404, false)] {
            let (base, seen) = mock_with(move |n, _, req| match n {
                0 => (200, vectors(req)),
                _ => (status, "{}".into()),
            });
            let e = quick(HttpProvider::OpenAi, "m", &base, Some("k"), 8).unwrap();
            let err = e.embed_passages(&["x".into()]).unwrap_err();
            assert_eq!(
                matches!(err, AliceError::Unavailable(_)),
                unavailable,
                "{status}: {err}"
            );
            assert_eq!(seen.lock().unwrap().len(), 2, "{status} is asked once");
        }
        // A refused connection is unavailable at once, without a retry.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = quick(
            HttpProvider::Ollama,
            "m",
            &format!("http://127.0.0.1:{port}/v1"),
            None,
            8,
        )
        .err()
        .unwrap();
        assert!(matches!(err, AliceError::Unavailable(_)), "{err}");
    }

    /// A provider that takes the connection and never answers costs a search the query
    /// timeout, below the clients' recall deadline of 5 s, and then nothing at all.
    #[test]
    fn a_black_holed_endpoint_falls_back_to_words_within_the_client_deadline() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let first = first.swap(false, std::sync::atomic::Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    let n = s.read(&mut buf).unwrap_or(0);
                    if first && n > 0 {
                        let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
                        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
                    } else {
                        std::thread::sleep(Duration::from_secs(30));
                    }
                });
            }
        });
        let e = quick(HttpProvider::Ollama, "m", &base, None, 8).unwrap();
        let started = Instant::now();
        assert!(e.embed_query("anything").is_err());
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        let again = Instant::now();
        assert!(e.embed_query("anything").is_err());
        assert!(
            again.elapsed() < Duration::from_millis(100),
            "the cool-down answers at once"
        );
    }

    #[test]
    fn a_failed_query_is_not_retried_and_blocks_queries_for_a_while() {
        let (base, seen) = mock_with(|n, _, req| match n {
            0 => (200, vectors(req)),
            _ => (503, "{}".into()),
        });
        let e = quick(HttpProvider::Ollama, "m", &base, None, 8).unwrap();
        let started = Instant::now();
        assert!(e.embed_query("one").is_err());
        assert_eq!(seen.lock().unwrap().len(), 2, "one try, no retries");
        let err = e.embed_query("two").unwrap_err().to_string();
        assert!(err.contains("word matches"), "{err}");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "nothing was sent during the cool-down"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        // Passages are not held back by it.
        assert!(e.embed_passages(&["x".into()]).is_err());
        assert!(seen.lock().unwrap().len() > 2);
    }

    #[test]
    fn long_and_empty_texts_are_made_acceptable_and_a_changed_dimension_is_refused() {
        let (base, seen) = mock(false);
        let e = quick(HttpProvider::OpenAi, "m", &base, Some("k"), 8).unwrap();
        e.embed_passages(&["x".repeat(20_000), "  ".into()])
            .unwrap();
        let last = seen.lock().unwrap().last().unwrap().body.clone();
        let inputs = last["input"].as_array().unwrap();
        assert!(inputs[0].as_str().unwrap().chars().count() <= MAX_TEXT_CHARS + 1);
        assert_eq!(inputs[1], "(empty)");
        // The server starts answering 2 numbers where it answered 3.
        let (base, _) = mock_with(|n, _, req| {
            if n == 0 {
                (200, vectors(req))
            } else {
                (
                    200,
                    serde_json::json!({"data": [{"index": 0, "embedding": [1.0, 2.0]}]})
                        .to_string(),
                )
            }
        });
        let e = quick(HttpProvider::OpenAi, "m", &base, Some("k"), 8).unwrap();
        let err = e.embed_passages(&["x".into()]).unwrap_err().to_string();
        assert!(err.contains("another size"), "{err}");
    }

    /// N1: a call of more texts than one request holds is refused before anything is sent.
    #[test]
    fn a_call_is_one_request_and_a_bigger_batch_is_refused_unsent() {
        let (base, seen) = mock(false);
        let e = quick(HttpProvider::Voyage, "v", &base, Some("k"), 200).unwrap();
        assert_eq!(e.max_batch(), 128, "Voyage takes 128 texts a request");
        let before = seen.lock().unwrap().len();
        let texts: Vec<String> = (0..129).map(|i| format!("t{i}")).collect();
        assert!(e.embed_passages(&texts).is_err());
        assert_eq!(seen.lock().unwrap().len(), before, "nothing was sent");
        assert_eq!(e.embed_passages(&texts[..128]).unwrap().len(), 128);
        assert_eq!(seen.lock().unwrap().len(), before + 1, "one request");
    }

    #[test]
    fn a_loopback_endpoint_is_local_and_the_providers_own_hosts_are_remote() {
        let (base, _) = mock(false);
        let local = quick(HttpProvider::OpenAi, "m", &base, Some("k"), 8).unwrap();
        assert!(!local.is_remote(), "a server on this machine is local");
        assert!(endpoint_is_remote(EmbeddingProvider::Openai, None));
        assert!(endpoint_is_remote(EmbeddingProvider::Voyage, None));
        assert!(!endpoint_is_remote(EmbeddingProvider::Ollama, None));
        assert!(endpoint_is_remote(
            EmbeddingProvider::Ollama,
            Some("http://192.168.1.20:11434")
        ));
    }

    #[test]
    fn ollama_without_v1_in_its_endpoint_still_reaches_the_embeddings_route() {
        let (base, seen) = mock(false);
        let root = base.trim_end_matches("/v1").to_string();
        let e = quick(HttpProvider::Ollama, "m", &root, None, 8).unwrap();
        assert!(e.url.ends_with("/v1/embeddings"), "{}", e.url);
        assert!(
            seen.lock().unwrap()[0]
                .head
                .starts_with("POST /v1/embeddings")
        );
    }
}
