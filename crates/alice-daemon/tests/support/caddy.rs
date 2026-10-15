//! An in-process stand-in for the Caddy front of a server (spec §2.3): one listener, many
//! tenants. `/u/<name>` is stripped, only `/api/v1/*` (not `admin`), `/mcp` and `/mcp/*` are
//! forwarded, `Host` is rewritten to the tenant's fake port and `Origin` is dropped.

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

/// One request the emulator received: method, path and query, whether `Authorization` was
/// present, and the body.
pub type Seen = (String, String, bool, Vec<u8>);

pub struct Tenant {
    pub name: String,
    pub home: AliceHome,
    pub svc: Arc<Service>,
    pub token: String,
    pub port: u16,
}

pub struct Server {
    pub base: String,
    pub tenants: Vec<Tenant>,
    /// The team store, when started: its own loopback listener and no route in the emulator.
    pub team: Option<Tenant>,
    recorders: Vec<(String, Arc<Mutex<Vec<Seen>>>)>,
    paused: Arc<AtomicBool>,
    stop: CancellationToken,
    _dirs: Vec<tempfile::TempDir>,
}

#[derive(Clone)]
struct Front {
    port: u16,
    recorder: Arc<Mutex<Vec<Seen>>>,
    paused: Arc<AtomicBool>,
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<html><body><h1>404 Not Found</h1></body></html>"),
    )
        .into_response()
}

/// Runs inside `/u/<name>`, so the path is already stripped.
async fn front(State(f): State<Front>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    f.recorder.lock().push((
        parts.method.to_string(),
        pq,
        parts.headers.contains_key(header::AUTHORIZATION),
        bytes.to_vec(),
    ));
    let path = parts.uri.path();
    let routed = (path.starts_with("/api/v1/") && !path.starts_with("/api/v1/admin/"))
        || path == "/mcp"
        || path.starts_with("/mcp/");
    if !routed {
        return not_found();
    }
    if f.paused.load(Ordering::SeqCst) {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    parts.headers.remove(header::ORIGIN);
    parts.headers.insert(
        header::HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{}", f.port)).unwrap(),
    );
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

impl Server {
    /// Tenants `names` (tenant mode on, fake embedder, tokens from `load_or_create_token`)
    /// behind one emulator on 127.0.0.1:0.
    pub async fn start(names: &[&str]) -> Server {
        Self::start_with(names, false).await
    }

    /// Like [`Server::start`], plus a team store: a `--team-store` daemon on its own
    /// `127.0.0.1:<port>` that the emulator does not route (`/u/team/...` is 404). Every tenant
    /// gets its URL and a `team.token` file.
    pub async fn start_with_team(names: &[&str]) -> Server {
        Self::start_with(names, true).await
    }

    async fn start_with(names: &[&str], with_team: bool) -> Server {
        alice_daemon::install_tls();
        let stop = CancellationToken::new();
        let paused = Arc::new(AtomicBool::new(false));
        let mut app = Router::new();
        let mut tenants = Vec::new();
        let mut recorders = Vec::new();
        let mut dirs = Vec::new();
        let mut team = None;
        let mut team_url = None;
        if with_team {
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
            let token = alice_daemon::discovery::load_or_create_token(&home).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let app =
                alice_daemon::http::router(svc.clone(), token.clone(), port, stop.child_token());
            let ct = stop.clone();
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move { ct.cancelled().await })
                    .await
                    .unwrap()
            });
            team_url = Some(format!("http://127.0.0.1:{port}"));
            team = Some(Tenant {
                name: "team".into(),
                home,
                svc,
                token,
                port,
            });
            dirs.push(dir);
        }
        for (i, name) in names.iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let home = AliceHome::new(dir.path().to_path_buf());
            home.ensure_dirs().unwrap();
            if let Some(t) = &team {
                std::fs::write(home.team_token_file(), &t.token).unwrap();
            }
            let svc = Service::start(
                home.clone(),
                &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
                ServiceOptions {
                    tenant: true,
                    team_url: team_url.clone(),
                    tenant_name: Some(name.to_string()),
                    ..ServiceOptions::default()
                },
            )
            .unwrap();
            let token = alice_daemon::discovery::load_or_create_token(&home).unwrap();
            let port = 47801 + i as u16;
            let inner =
                alice_daemon::http::router(svc.clone(), token.clone(), port, stop.child_token());
            let recorder = Arc::new(Mutex::new(Vec::new()));
            let f = Front {
                port,
                recorder: recorder.clone(),
                paused: paused.clone(),
            };
            app = app.nest(
                &format!("/u/{name}"),
                inner.layer(middleware::from_fn_with_state(f, front)),
            );
            recorders.push((name.to_string(), recorder));
            tenants.push(Tenant {
                name: name.to_string(),
                home,
                svc,
                token,
                port,
            });
            dirs.push(dir);
        }
        let app = app.fallback(|| async { not_found() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ct = stop.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct.cancelled().await })
                .await
                .unwrap()
        });
        Server {
            base: format!("http://{addr}"),
            tenants,
            team,
            recorders,
            paused,
            stop,
            _dirs: dirs,
        }
    }

    /// `{base}/u/{name}`.
    pub fn url(&self, name: &str) -> String {
        format!("{}/u/{name}", self.base)
    }

    pub fn tenant(&self, name: &str) -> &Tenant {
        self.tenants.iter().find(|t| t.name == name).unwrap()
    }

    /// Every request the emulator received for `name`, oldest first.
    pub fn seen(&self, name: &str) -> Vec<Seen> {
        self.recorders
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r.lock().clone())
            .unwrap_or_default()
    }

    /// While paused, routed requests get a 502, as when a tenant's daemon is down.
    pub fn pause(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
