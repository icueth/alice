//! JSON API over loopback HTTP. The guard (token + Host + Origin) is the code verified in the spike.

use crate::service::Service;
use alice_core::api::*;
use alice_core::error::AliceError;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Largest request body. An ingest at its limits (1,000 chunks of 32 KB) is about 32 MB of JSON.
const BODY_LIMIT: usize = 40 * 1024 * 1024;

#[derive(Clone)]
pub struct Guard {
    token: Arc<str>,
    allowed_hosts: Arc<[String; 2]>,
    allowed_origin: Arc<str>,
}

impl Guard {
    pub fn new(token: impl Into<Arc<str>>, port: u16) -> Self {
        Self {
            token: token.into(),
            allowed_hosts: Arc::new([format!("127.0.0.1:{port}"), format!("localhost:{port}")]),
            allowed_origin: format!("http://127.0.0.1:{port}").into(),
        }
    }
}

/// Constant-time equality (the length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn host_of(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, tok) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(tok.trim())
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    kind: &'static str,
}

fn refuse(status: StatusCode, kind: &'static str, error: String) -> Response {
    (status, Json(ErrorBody { error, kind })).into_response()
}

/// The refusal for a request whose Host or Origin is not this daemon's, if it is refused.
fn host_or_origin_refusal(g: &Guard, req: &Request) -> Option<Response> {
    match host_of(req) {
        Some(h) if g.allowed_hosts.iter().any(|a| a == h) => {}
        _ => {
            return Some(refuse(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!(
                    "forbidden: the Host header must be {} or {}; connect to the daemon on 127.0.0.1 directly",
                    g.allowed_hosts[0], g.allowed_hosts[1]
                ),
            ));
        }
    }
    if let Some(o) = req.headers().get(header::ORIGIN)
        && o.as_bytes() != g.allowed_origin.as_bytes()
    {
        return Some(refuse(
            StatusCode::FORBIDDEN,
            "forbidden",
            "forbidden: cross-origin requests are refused; call the API from a local program, not a web page".into(),
        ));
    }
    None
}

pub async fn guard(State(g): State<Guard>, req: Request, next: Next) -> Response {
    if let Some(r) = host_or_origin_refusal(&g, &req) {
        return r;
    }
    match bearer(req.headers()) {
        Some(t) if ct_eq(t.as_bytes(), g.token.as_bytes()) => next.run(req).await,
        _ => {
            let mut r = refuse(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "unauthorized: send \"Authorization: Bearer <token>\" with the token from the `token` file in the Alice home directory".into(),
            );
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
            r
        }
    }
}

/// For the public ping: Host and Origin only.
pub async fn host_guard(State(g): State<Guard>, req: Request, next: Next) -> Response {
    match host_or_origin_refusal(&g, &req) {
        Some(r) => r,
        None => next.run(req).await,
    }
}

pub struct ApiError(pub AliceError);

impl From<AliceError> for ApiError {
    fn from(e: AliceError) -> Self {
        Self(e)
    }
}

/// A body that is not JSON of the right shape is the caller's to fix, like any `Invalid` error.
impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        let msg = if r.status() == StatusCode::PAYLOAD_TOO_LARGE {
            format!(
                "request body is over the {} MB limit; send fewer or smaller chunks per request",
                BODY_LIMIT / (1024 * 1024)
            )
        } else {
            format!(
                "{}; send a JSON body (Content-Type: application/json) shaped like the API request",
                r.body_text()
            )
        };
        Self(AliceError::Invalid(msg))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, kind) = match &self.0 {
            AliceError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
            AliceError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            AliceError::ReadOnly(_) => (StatusCode::SERVICE_UNAVAILABLE, "read_only"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        refuse(status, kind, self.0.to_string())
    }
}

/// `Json<T>` whose rejections use the API error body.
struct ApiJson<T>(T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(v) = Json::<T>::from_request(req, state).await?;
        Ok(Self(v))
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Clone)]
struct AppState {
    svc: Arc<Service>,
    shutdown: CancellationToken,
    /// For the ping proof only; the guard keeps its own copy.
    token: Arc<str>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PingResponse {
    pub service: String,
    pub instance: String,
    pub version: String,
    /// `discovery::ping_proof(token, nonce, instance)` when the ping carried `?nonce=`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct PingQuery {
    nonce: Option<String>,
}

fn valid_nonce(n: &str) -> bool {
    (16..=64).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Public, but proves the token without revealing it: with `?nonce=<16–64 hex chars>` the
/// answer carries `proof = discovery::ping_proof(token, nonce, instance)`.
async fn ping(
    State(st): State<AppState>,
    query: Result<Query<PingQuery>, QueryRejection>,
) -> Result<Json<PingResponse>, ApiError> {
    let Query(q) = query.map_err(|r| {
        ApiError(AliceError::Invalid(format!(
            "{}; send /api/v1/ping or /api/v1/ping?nonce=<16 to 64 hex characters>",
            r.body_text()
        )))
    })?;
    let proof = match q.nonce {
        None => None,
        Some(n) if valid_nonce(&n) => Some(crate::discovery::ping_proof(
            &st.token,
            &n,
            st.svc.instance(),
        )),
        Some(_) => {
            return Err(ApiError(AliceError::invalid(
                "the nonce must be 16 to 64 hex characters; send a fresh random one",
            )));
        }
    };
    Ok(Json(PingResponse {
        service: "alice".into(),
        instance: st.svc.instance().to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        proof,
    }))
}
async fn health(State(st): State<AppState>) -> ApiResult<Health> {
    Ok(Json(st.svc.health().await?))
}
async fn projects(
    State(st): State<AppState>,
) -> ApiResult<Vec<alice_core::store::projects::ResolvedProject>> {
    Ok(Json(st.svc.projects().await?))
}
/// A read-only database never loses a `remember`: it is spooled (202) and applied after repair.
async fn remember(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<RememberRequest>,
) -> Result<Response, ApiError> {
    let r = st.svc.remember_or_spool(req).await?;
    let status = match r.status {
        RememberStatus::Spooled => StatusCode::ACCEPTED,
        _ => StatusCode::OK,
    };
    Ok((status, Json(r)).into_response())
}
async fn recall(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<RecallRequest>,
) -> ApiResult<RecallResponse> {
    Ok(Json(st.svc.recall(req).await?))
}
async fn context(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<ContextRequest>,
) -> ApiResult<ContextResponse> {
    Ok(Json(st.svc.context(req).await?))
}
async fn get_memories(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<GetRequest>,
) -> ApiResult<GetResponse> {
    Ok(Json(st.svc.get(req).await?))
}
async fn update(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<UpdateRequest>,
) -> ApiResult<MemoryFull> {
    Ok(Json(st.svc.update(req).await?))
}
async fn facts(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<FactsRequest>,
) -> ApiResult<FactsResponse> {
    Ok(Json(st.svc.facts(req).await?))
}
async fn assert_fact(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<AssertFactRequest>,
) -> ApiResult<alice_core::store::kg::FactOutcome> {
    Ok(Json(st.svc.assert_fact(req).await?))
}
async fn ingest(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<IngestRequest>,
) -> ApiResult<alice_core::store::sources::IngestOutcome> {
    Ok(Json(st.svc.ingest(req).await?))
}
async fn admin_shutdown(State(st): State<AppState>) -> StatusCode {
    st.shutdown.cancel();
    StatusCode::ACCEPTED
}

/// Build the router: the public ping, and everything else (the JSON API and the MCP service
/// at `/mcp`) behind the token, Host and Origin guard.
pub fn router(svc: Arc<Service>, token: String, port: u16, shutdown: CancellationToken) -> Router {
    let token: Arc<str> = token.into();
    let g = Guard::new(token.clone(), port);
    let state = AppState {
        svc,
        shutdown,
        token,
    };
    let public = Router::new()
        .route("/api/v1/ping", get(ping))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(g.clone(), host_guard));
    let private = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/projects", get(projects))
        .route("/api/v1/remember", post(remember))
        .route("/api/v1/recall", post(recall))
        .route("/api/v1/context", post(context))
        .route("/api/v1/get", post(get_memories))
        .route("/api/v1/update", post(update))
        .route("/api/v1/facts/query", post(facts))
        .route("/api/v1/facts/assert", post(assert_fact))
        .route("/api/v1/ingest", post(ingest))
        .route("/api/v1/admin/shutdown", post(admin_shutdown))
        // Before the guard layer, so `/mcp` is covered by it.
        .nest_service(
            "/mcp",
            crate::mcp::http_service(state.svc.clone(), port, state.shutdown.clone()),
        )
        .with_state(state)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(g, guard));
    public.merge(private)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AliceHome;
    use crate::service::ServiceOptions;
    use alice_core::embed::{Embedder, FakeEmbedder};

    /// Review focus #3 (lose no writes): a read-only database spools `remember` and says so.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_only_remember_is_spooled_and_ingest_says_read_only() {
        crate::install_tls();
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap();
        svc.force_read_only("the disk is full");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = router(svc, "k".repeat(64), port, CancellationToken::new());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let r = http
            .post(format!("{base}/api/v1/remember"))
            .bearer_auth("k".repeat(64))
            .json(&RememberRequest {
                content: "kept for later".into(),
                cwd: Some("/tmp/somewhere".into()),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202);
        let body: RememberResponse = r.json().await.unwrap();
        assert_eq!(body.status, RememberStatus::Spooled);
        let why = body.reason.unwrap_or_default();
        assert!(
            why.contains("read-only") && why.contains("the disk is full"),
            "the reply says why it was queued: {why}"
        );
        assert_eq!(crate::spool::pending_count(&home.spool_dir()), 1);
        let file = std::fs::read_dir(home.spool_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|x| x == "json"))
            .unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(
            raw["cwd"], "/tmp/somewhere",
            "the project is resolved when it is applied"
        );

        let r = http
            .post(format!("{base}/api/v1/ingest"))
            .bearer_auth("k".repeat(64))
            .json(&IngestRequest {
                source: alice_core::store::sources::SourceInput {
                    uri: "s".into(),
                    ..Default::default()
                },
                chunks: vec![alice_core::store::sources::ChunkInput {
                    ord: 0,
                    text: "x".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 503);
        let e: serde_json::Value = r.json().await.unwrap();
        assert_eq!(e["kind"], "read_only");
        assert!(e["error"].as_str().unwrap().contains("alice status"), "{e}");
    }

    /// When the spool cannot be written either (the disk is full, say), the caller gets the
    /// read-only error that says what to do, not an opaque I/O failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unwritable_spool_returns_the_read_only_error() {
        crate::install_tls();
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap();
        svc.force_read_only("the disk is full");
        std::fs::remove_dir_all(home.spool_dir()).unwrap();
        std::fs::write(
            home.spool_dir(),
            b"a file where the spool directory should be",
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = router(svc, "k".repeat(64), port, CancellationToken::new());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let r = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/v1/remember"))
            .bearer_auth("k".repeat(64))
            .json(&RememberRequest {
                content: "nowhere to keep this".into(),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 503);
        let e: serde_json::Value = r.json().await.unwrap();
        assert_eq!(e["kind"], "read_only");
        assert!(
            e["error"].as_str().unwrap().contains("the disk is full"),
            "{e}"
        );
    }
}
