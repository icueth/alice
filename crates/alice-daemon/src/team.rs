//! The personal daemon's link to the team store (spec §2.10): a second daemon on the same
//! server, reached over loopback only. The link proves the team daemon with the ping proof
//! before it sends the token anywhere, and re-reads `team.token` after any failure, so a
//! rotated token needs no restart.

use crate::discovery::{read_token_file, verify_ping_proof};
use crate::http::PingResponse;
use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The ping (connect included) may take this long.
const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// Connecting for any later call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a successful ping proved: the team daemon's instance, and the token it vouched for
/// (the only token the link ever sends).
struct Verified {
    instance: String,
    token: String,
}

/// The link to the team store.
pub struct TeamLink {
    /// `http://127.0.0.1:<port>` or `http://localhost:<port>`, no path.
    base: String,
    token_file: PathBuf,
    http: reqwest::Client,
    verified: Mutex<Option<Verified>>,
    /// The latest failure, cleared by the next success (health reports it).
    problem: Mutex<Option<String>>,
}

/// Prints the URL only, never the token.
impl std::fmt::Debug for TeamLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamLink")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

fn team_error(what: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!("the team store {what}"))
}

/// 16 random bytes as 32 hex characters, new for every ping.
fn fresh_nonce() -> Option<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).ok()?;
    Some(hex::encode(buf))
}

impl TeamLink {
    /// From `ALICE_TEAM_URL` (loopback http only) and `<home>/team.token`; `Err(reason)` disables the link.
    pub fn from_env(
        home: &AliceHome,
        url: Option<&str>,
    ) -> std::result::Result<Option<Arc<TeamLink>>, String> {
        let Some(raw) = url.map(str::trim).filter(|u| !u.is_empty()) else {
            return Ok(None);
        };
        // The URL is not echoed: it may carry a password by mistake.
        let u = url::Url::parse(raw)
            .map_err(|_| "ALICE_TEAM_URL is not a URL; use http://127.0.0.1:<port>".to_string())?;
        if u.scheme() != "http" {
            return Err("ALICE_TEAM_URL must be http://127.0.0.1:<port>: the team store is reached over loopback, without TLS".into());
        }
        let loopback = match u.host() {
            Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            _ => false,
        };
        if !loopback {
            return Err("ALICE_TEAM_URL must name 127.0.0.1 or localhost: the team store is never reached over the network".into());
        }
        let Some(port) = u.port() else {
            return Err("ALICE_TEAM_URL needs the port of the team store".into());
        };
        if !u.username().is_empty()
            || u.password().is_some()
            || !matches!(u.path(), "" | "/")
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err("ALICE_TEAM_URL must be only http://127.0.0.1:<port>".into());
        }
        let host = u.host_str().unwrap_or("127.0.0.1").to_ascii_lowercase();
        let token_file = home.team_token_file();
        if read_token_file(&token_file).is_none() {
            return Err(format!(
                "{} is missing or empty; ask the admin to provision the team store for this tenant",
                token_file.display()
            ));
        }
        crate::install_tls();
        // No proxy (it would see the token) and no redirects (they could carry it away).
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("cannot create an HTTP client: {e}"))?;
        Ok(Some(Arc::new(TeamLink {
            base: format!("http://{host}:{port}"),
            token_file,
            http,
            verified: Mutex::new(None),
            problem: Mutex::new(None),
        })))
    }

    /// The latest failure of the link, if the latest attempt failed.
    pub fn problem(&self) -> Option<String> {
        self.problem.lock().clone()
    }

    fn fail(&self, e: AliceError) -> AliceError {
        *self.problem.lock() = Some(e.to_string());
        e
    }

    fn forget(&self) {
        *self.verified.lock() = None;
    }

    /// Ping with a fresh nonce, check `service == "alice"`, verify the proof with the token (re-read from the file) and the
    /// answer's instance; cached until a call fails with 401 or a transport error.
    pub async fn ensure_verified(&self) -> Result<()> {
        if self.verified.lock().is_some() {
            return Ok(());
        }
        self.verify().await.map_err(|e| self.fail(e))
    }

    async fn verify(&self) -> Result<()> {
        let path = self.token_file.clone();
        let token = tokio::task::spawn_blocking(move || read_token_file(&path))
            .await
            .map_err(|e| AliceError::Other(format!("internal task failed: {e}")))?
            .ok_or_else(|| {
                team_error(format!(
                    "link has no token: {} is missing or empty. Ask the admin to provision it",
                    self.token_file.display()
                ))
            })?;
        let nonce = fresh_nonce().ok_or_else(|| team_error("link could not make a nonce"))?;
        let ping: PingResponse = async {
            let resp = self
                .http
                .get(format!("{}/api/v1/ping?nonce={nonce}", self.base))
                .timeout(PING_TIMEOUT)
                .send()
                .await?;
            resp.json().await
        }
        .await
        .map_err(|e| team_error(format!("did not answer a ping: {}", e.without_url())))?;
        // The proof vouches for the instance in the answer; only a holder of the token can make it.
        let proven = ping.service == "alice"
            && ping
                .proof
                .as_deref()
                .is_some_and(|p| verify_ping_proof(&token, &nonce, &ping.instance, p));
        if !proven {
            return Err(team_error(
                "did not prove that it holds this tenant's token, so no request was sent to it. Ask the admin to check team.token and the team store's token",
            ));
        }
        *self.verified.lock() = Some(Verified {
            instance: ping.instance,
            token,
        });
        Ok(())
    }

    /// POST/GET a JSON API path on the team store with the bearer token, after `ensure_verified`; a 401 or transport error
    /// clears the verification so the next call re-reads the token.
    pub async fn call<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Req>,
        timeout: Duration,
    ) -> Result<Resp> {
        // Only the JSON API: the token never goes to another path or, through "//", another host.
        if !path.starts_with("/api/v1/") || path.contains("//") || path.contains('\\') {
            return Err(AliceError::invalid(
                "a team store call must name a path under /api/v1/",
            ));
        }
        self.ensure_verified().await?;
        let token = match self.verified.lock().as_ref() {
            Some(v) => v.token.clone(),
            None => return Err(team_error("link lost its verification; retry")),
        };
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .timeout(timeout);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                self.forget();
                // Only a connection that never opened means the request was not sent.
                let why = if e.is_timeout() {
                    "did not answer in time".to_string()
                } else if e.is_connect() {
                    format!("is unreachable: {}", e.without_url())
                } else {
                    format!("broke off its answer: {}", e.without_url())
                };
                return Err(self.fail(team_error(why)));
            }
        };
        let status = resp.status();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                self.forget();
                return Err(self.fail(team_error(format!(
                    "broke off its answer: {}",
                    e.without_url()
                ))));
            }
        };
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            self.forget();
            return Err(self.fail(team_error(
                "refused this tenant's token or host. The token is read again on the next call; if it keeps failing, ask the admin to provision the team store again",
            )));
        }
        if !status.is_success() {
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            return Err(crate::client::error_from(status, &json));
        }
        *self.problem.lock() = None;
        let text: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
        serde_json::from_slice(text)
            .map_err(|e| team_error(format!("sent an answer this daemon cannot read: {e}")))
    }

    /// The instance of the team daemon the link last verified.
    pub fn verified_instance(&self) -> Option<String> {
        self.verified.lock().as_ref().map(|v| v.instance.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Service, ServiceOptions};
    use alice_core::embed::{Embedder, FakeEmbedder};
    use axum::extract::{Request, State};
    use axum::middleware::{self, Next};
    use axum::response::Response;
    use parking_lot::Mutex;
    use tokio_util::sync::CancellationToken;

    type Seen = Arc<Mutex<Vec<(String, bool)>>>;

    struct Team {
        port: u16,
        home: AliceHome,
        token: String,
        seen: Seen,
        _dir: tempfile::TempDir,
        _svc: Arc<Service>,
    }

    async fn record(State(seen): State<Seen>, req: Request, next: Next) -> Response {
        seen.lock().push((
            req.uri().path().to_string(),
            req.headers()
                .contains_key(axum::http::header::AUTHORIZATION),
        ));
        next.run(req).await
    }

    async fn team() -> Team {
        crate::install_tls();
        let dir = tempfile::tempdir().unwrap();
        let home = AliceHome::new(dir.path().to_path_buf());
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant: true,
                team_store: true,
                ..ServiceOptions::default()
            },
        )
        .unwrap();
        let token = crate::discovery::load_or_create_token(&home).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Seen = Arc::default();
        let app = crate::http::router(svc.clone(), token.clone(), port, CancellationToken::new())
            .layer(middleware::from_fn_with_state(seen.clone(), record));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Team {
            port,
            home,
            token,
            seen,
            _dir: dir,
            _svc: svc,
        }
    }

    fn personal(token: &str) -> (tempfile::TempDir, AliceHome) {
        let dir = tempfile::tempdir().unwrap();
        let home = AliceHome::new(dir.path().to_path_buf());
        home.ensure_dirs().unwrap();
        std::fs::write(home.team_token_file(), token).unwrap();
        (dir, home)
    }

    fn link(t: &Team, home: &AliceHome) -> Arc<TeamLink> {
        TeamLink::from_env(home, Some(&format!("http://127.0.0.1:{}", t.port)))
            .unwrap()
            .unwrap()
    }

    const T: Duration = Duration::from_secs(5);

    #[tokio::test(flavor = "multi_thread")]
    async fn it_verifies_the_proof_and_calls() {
        let t = team().await;
        let (_d, home) = personal(&t.token);
        let l = link(&t, &home);
        l.ensure_verified().await.unwrap();
        let h: serde_json::Value = l
            .call(reqwest::Method::GET, "/api/v1/health", None::<&()>, T)
            .await
            .unwrap();
        assert_eq!(h["service"], "alice");
        let seen = t.seen.lock().clone();
        assert_eq!(seen[0], ("/api/v1/ping".to_string(), false));
        assert!(seen.contains(&("/api/v1/health".to_string(), true)));
    }

    #[test]
    fn a_non_loopback_or_https_url_is_refused() {
        let (_d, home) = personal("tok");
        for url in [
            "http://203.0.113.7:47900",
            "https://127.0.0.1:47900",
            "http://example.com:47900",
            "http://127.0.0.1",
            "http://user:pw@127.0.0.1:47900",
            "not a url",
        ] {
            let e = TeamLink::from_env(&home, Some(url)).unwrap_err();
            assert!(!e.contains("pw@"), "{e}");
        }
        assert!(TeamLink::from_env(&home, None).unwrap().is_none());
        assert!(
            TeamLink::from_env(&home, Some("http://localhost:47900"))
                .unwrap()
                .is_some()
        );
        // No token file: the link is disabled with a reason.
        let d = tempfile::tempdir().unwrap();
        let bare = AliceHome::new(d.path().to_path_buf());
        assert!(TeamLink::from_env(&bare, Some("http://127.0.0.1:47900")).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_token_is_never_sent() {
        let t = team().await;
        let secret = "0".repeat(64);
        let (_d, home) = personal(&secret);
        let l = link(&t, &home);
        let e = l.ensure_verified().await.unwrap_err().to_string();
        assert!(!e.contains(&secret), "{e}");
        let r: Result<serde_json::Value> = l
            .call(reqwest::Method::GET, "/api/v1/health", None::<&()>, T)
            .await;
        assert!(r.is_err());
        assert!(t.seen.lock().iter().all(|(_, auth)| !auth));
        assert!(!format!("{l:?}").contains(&secret));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rotated_token_works_after_the_file_is_rewritten() {
        let t = team().await;
        let (_d, home) = personal(&t.token);
        let l = link(&t, &home);
        l.ensure_verified().await.unwrap();
        let c = reqwest::Client::new();
        let r = c
            .post(format!(
                "http://127.0.0.1:{}/api/v1/admin/token/rotate",
                t.port
            ))
            .bearer_auth(&t.token)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        // The old token is refused: the call fails and clears the verification.
        let r: Result<serde_json::Value> = l
            .call(reqwest::Method::GET, "/api/v1/health", None::<&()>, T)
            .await;
        assert!(r.is_err());
        let new = crate::discovery::read_token(&t.home).unwrap();
        std::fs::write(home.team_token_file(), &new).unwrap();
        let h: serde_json::Value = l
            .call(reqwest::Method::GET, "/api/v1/health", None::<&()>, T)
            .await
            .unwrap();
        assert_eq!(h["service"], "alice");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_outside_the_api_is_refused() {
        let t = team().await;
        let (_d, home) = personal(&t.token);
        let l = link(&t, &home);
        for p in ["/mcp", "//evil.example/api/v1/x", "api/v1/health"] {
            let r: Result<serde_json::Value> =
                l.call(reqwest::Method::GET, p, None::<&()>, T).await;
            assert!(r.is_err(), "{p}");
        }
        assert!(t.seen.lock().iter().all(|(_, auth)| !auth));
    }
}
