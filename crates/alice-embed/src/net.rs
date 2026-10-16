//! Blocking HTTP usable from any thread — worker threads, `spawn_blocking`, plain tests — and
//! never on an async runtime thread's own executor: requests run on a private one-thread
//! runtime and the caller waits on a std channel. (Starting or blocking on a runtime from
//! inside another runtime panics; this does neither.)

use std::time::Duration;

pub struct BlockingHttp {
    rt: Option<tokio::runtime::Runtime>,
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// Lowercase header names.
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Timeout,
    Connect(String),
    /// The request could not be made (a bad header value or URL): nothing was sent.
    Build(String),
    /// Something failed after the request was sent.
    Other(String),
}

impl BlockingHttp {
    /// `user_agent` is sent as it is: pass `alice/<version>` and nothing that names a person or
    /// a machine.
    pub fn new(user_agent: &str) -> std::io::Result<Self> {
        Self::build(user_agent, false, false)
    }

    /// For a request to an endpoint the person named (an embedding provider): redirects are not
    /// followed (the body and the key would be posted to wherever a server points) and no
    /// proxy from the environment reroutes the request.
    pub fn direct(user_agent: &str) -> std::io::Result<Self> {
        Self::build(user_agent, true, true)
    }

    /// Like [`new`](Self::new) but redirects are not followed (a redirect would post the
    /// body, and the key, to wherever a server points); the environment's proxy is kept, for
    /// a remote API a corporate network may require one for.
    pub fn no_redirect(user_agent: &str) -> std::io::Result<Self> {
        Self::build(user_agent, true, false)
    }

    fn build(user_agent: &str, no_redirect: bool, no_proxy: bool) -> std::io::Result<Self> {
        crate::install_tls_provider();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("alice-http")
            .enable_all()
            .build()?;
        let mut builder = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(Duration::from_secs(10));
        if no_redirect {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        if no_proxy {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self {
            rt: Some(rt),
            client,
        })
    }

    /// POSTs `body` as JSON. A `bearer` token goes into a header marked sensitive, so it is
    /// left out of any debug output of the request.
    pub fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut req = self
            .client
            .post(url)
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(k) = bearer {
            req = req.header(reqwest::header::AUTHORIZATION, bearer_header(k)?);
        }
        self.rt
            .as_ref()
            .expect("runtime lives until drop")
            .spawn(async move {
                let r = async {
                    let resp = req.send().await?;
                    let status = resp.status().as_u16();
                    let headers = resp
                        .headers()
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.as_str().to_ascii_lowercase(),
                                v.to_str().unwrap_or("").to_string(),
                            )
                        })
                        .collect();
                    let body = resp.bytes().await?.to_vec();
                    Ok::<_, reqwest::Error>(HttpResponse {
                        status,
                        body,
                        headers,
                    })
                }
                .await;
                let _ = tx.send(r);
            });
        match rx.recv_timeout(timeout + Duration::from_secs(5)) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) if e.is_timeout() => Err(HttpError::Timeout),
            Ok(Err(e)) if e.is_connect() => Err(HttpError::Connect(without_url(e))),
            Ok(Err(e)) if e.is_builder() => Err(HttpError::Build(without_url(e))),
            Ok(Err(e)) => Err(HttpError::Other(without_url(e))),
            Err(_) => Err(HttpError::Timeout),
        }
    }
}

/// `Authorization: Bearer <k>`, built by hand (not `bearer_auth`) so the value is marked
/// sensitive and stays out of any debug output. The error names no part of the key.
fn bearer_header(k: &str) -> Result<reqwest::header::HeaderValue, HttpError> {
    let mut value =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {k}")).map_err(|_| {
            HttpError::Build("the API key holds characters a header cannot carry".into())
        })?;
    value.set_sensitive(true);
    Ok(value)
}

/// The error's text without the URL it carries (a URL may hold a query token).
fn without_url(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

impl Drop for BlockingHttp {
    /// `shutdown_background` is allowed inside an async context, unlike dropping a runtime.
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A one-route HTTP/1.1 server on a thread of its own: `/ok` answers 200 with a body and a
    /// `Retry-After` header, `/stall` reads the request and never answers. Records each request
    /// head.
    fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let log = heads.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { return };
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    log.lock().unwrap().push(head.clone());
                    if head.contains("POST /moved") {
                        let _ = sock.write_all(
                            b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    }
                    if head.contains("POST /stall") {
                        std::thread::sleep(Duration::from_secs(30));
                        return;
                    }
                    let body = b"{\"ok\":true}";
                    let _ = sock.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nRetry-After: 7\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    let _ = sock.write_all(body);
                });
            }
        });
        (base, heads)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn posts_from_a_blocking_task_and_times_out_on_a_stalled_route() {
        let (base, heads) = serve();
        let http = Arc::new(BlockingHttp::new("alice/0.0.0-test").unwrap());
        let h = http.clone();
        let url = format!("{base}/ok");
        let r = tokio::task::spawn_blocking(move || {
            h.post_json(
                &url,
                Some("test-token"),
                b"{\"a\":1}".to_vec(),
                Duration::from_secs(5),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{\"ok\":true}");
        assert!(r.headers.contains(&("retry-after".into(), "7".into())));
        let head = heads.lock().unwrap()[0].to_ascii_lowercase();
        assert!(head.contains("user-agent: alice/0.0.0-test"), "{head}");
        assert!(head.contains("authorization: bearer test-token"), "{head}");
        assert!(head.contains("content-type: application/json"), "{head}");

        let h = http.clone();
        let url = format!("{base}/stall");
        let started = std::time::Instant::now();
        let r = tokio::task::spawn_blocking(move || {
            h.post_json(&url, None, Vec::new(), Duration::from_millis(300))
        })
        .await
        .unwrap();
        assert_eq!(r.unwrap_err(), HttpError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(5));

        // Dropping inside the async test must not panic.
        drop(http);
    }

    #[test]
    fn a_refused_connection_is_a_connect_error_without_the_url() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let http = BlockingHttp::new("alice/0.0.0-test").unwrap();
        let e = http
            .post_json(
                &format!("http://127.0.0.1:{port}/secret-path?token=abc"),
                None,
                Vec::new(),
                // Windows tries a refused loopback connection again for about two seconds
                // before it reports the refusal: a shorter deadline ends in `Timeout` there.
                Duration::from_secs(15),
            )
            .unwrap_err();
        match e {
            HttpError::Connect(m) => {
                assert!(!m.contains("secret-path") && !m.contains("abc"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_bearer_header_is_sensitive_and_hidden_from_debug_output() {
        let key = concat!("ts_", "live_header_test_key");
        let v = bearer_header(key).unwrap();
        assert!(v.is_sensitive());
        assert!(!format!("{v:?}").contains("header_test_key"), "{v:?}");
        assert_eq!(v.to_str().unwrap(), format!("Bearer {key}"));
    }

    #[test]
    fn a_key_a_header_cannot_carry_is_refused_without_naming_it() {
        let http = BlockingHttp::new("alice/0.0.0-test").unwrap();
        let bad = concat!("ts_", "live_bad\nkey");
        let e = http
            .post_json(
                "http://127.0.0.1:1/",
                Some(bad),
                Vec::new(),
                Duration::from_secs(1),
            )
            .unwrap_err();
        match e {
            HttpError::Build(m) => assert!(!m.contains("live_bad"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// An embedding request is not re-posted where a server points it (the text and the key
    /// would follow), and no proxy of the environment reroutes it.
    #[test]
    fn a_direct_client_does_not_follow_a_redirect() {
        let (base, heads) = serve();
        let body = || b"{\"secret\":true}".to_vec();
        let direct = BlockingHttp::direct("alice/0.0.0-test").unwrap();
        let r = direct
            .post_json(
                &format!("{base}/moved"),
                Some("test-token"),
                body(),
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(r.status, 307, "the redirect is the answer, not followed");
        assert_eq!(heads.lock().unwrap().len(), 1, "nothing was posted again");
        let nr = BlockingHttp::no_redirect("alice/0.0.0-test").unwrap();
        let r = nr
            .post_json(
                &format!("{base}/moved"),
                None,
                body(),
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(r.status, 307, "the Jev client does not follow it either");
        // The ordinary client follows it (this is why the embedding one must not).
        let plain = BlockingHttp::new("alice/0.0.0-test").unwrap();
        let r = plain
            .post_json(
                &format!("{base}/moved"),
                None,
                body(),
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(r.status, 200);
    }
}
