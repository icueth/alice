//! JSON API over loopback HTTP. The guard (token + Host + Origin) is the code verified in the spike.

use crate::secrets::SecretName;
use crate::service::{Scoped, Service};
use alice_core::api::*;
use alice_core::error::AliceError;
use alice_core::settings::SettingError;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

/// Largest request body. An ingest at its limits (1,000 chunks of 32 KB) is about 32 MB of JSON.
const BODY_LIMIT: usize = 40 * 1024 * 1024;
/// A comment line on an idle event stream, so proxies and clients keep it open.
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The API token, switchable while the daemon runs (`alice token rotate`): the guard and the
/// ping proof both read the current value for every request.
pub type SharedToken = Arc<parking_lot::RwLock<Arc<str>>>;

#[derive(Clone)]
pub struct Guard {
    token: SharedToken,
    allowed_hosts: Arc<[String; 2]>,
    allowed_origin: Arc<str>,
}

impl Guard {
    pub fn new(token: SharedToken, port: u16) -> Self {
        Self {
            token,
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
    /// Present when a write was refused for secret-shaped content.
    #[serde(skip_serializing_if = "Option::is_none")]
    refused: Option<RefusedBody>,
    /// The project's rules were fail-closed when the refusal was made: the same write may be
    /// accepted later, so a replayed spool entry is kept, not withheld.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    retry_later: bool,
}

/// The field and the detectors of a refused write.
#[derive(Debug, Serialize)]
struct RefusedBody {
    field: String,
    detectors: Vec<String>,
}

fn refuse(status: StatusCode, kind: &'static str, error: String) -> Response {
    (
        status,
        Json(ErrorBody {
            error,
            kind,
            refused: None,
            retry_later: false,
        }),
    )
        .into_response()
}

/// The answer to a refused write: `invalid`, with the field and detectors, and whether a retry
/// may succeed later.
fn refusal_response(e: AliceError, retry_later: bool) -> Response {
    let refused = match &e {
        AliceError::Refused { field, detectors } => Some(RefusedBody {
            field: field.clone(),
            detectors: detectors.clone(),
        }),
        _ => None,
    };
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: e.to_string(),
            kind: "invalid",
            refused,
            retry_later,
        }),
    )
        .into_response()
}

/// A capability of this daemon the client may rely on: it accepts `client_project` and the
/// project's `.alice.toml` text.
pub const CAP_CLIENT_PROJECT: &str = "client-project";
/// The daemon serves one person on a shared server (`--tenant`).
pub const CAP_TENANT: &str = "tenant";

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
    // The current token, copied out so the lock is not held across the handler.
    let current = g.token.read().clone();
    match bearer(req.headers()) {
        Some(t) if ct_eq(t.as_bytes(), current.as_bytes()) => next.run(req).await,
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
            AliceError::Invalid(_) | AliceError::Refused { .. } => {
                (StatusCode::BAD_REQUEST, "invalid")
            }
            AliceError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            AliceError::ReadOnly(_) => (StatusCode::SERVICE_UNAVAILABLE, "read_only"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        if matches!(self.0, AliceError::Refused { .. }) {
            return refusal_response(self.0, false);
        }
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
    /// The same token the guard checks (the ping proof signs with it).
    token: SharedToken,
    /// One rotation at a time, so the file and the token in memory end up the same.
    rotating: Arc<tokio::sync::Mutex<()>>,
    /// Ends every event stream opened under the current token: cancelled on a rotation (and a
    /// child of `shutdown`, so a stopping daemon ends them too).
    streams: Arc<parking_lot::Mutex<CancellationToken>>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PingResponse {
    pub service: String,
    pub instance: String,
    pub version: String,
    /// `discovery::ping_proof(token, nonce, instance)` when the ping carried `?nonce=`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
    /// What this daemon supports beyond the base API (`CAP_CLIENT_PROJECT`, `CAP_TENANT`); an
    /// older daemon sends none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
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
        Some(n) if valid_nonce(&n) => {
            let token = st.token.read().clone();
            Some(crate::discovery::ping_proof(&token, &n, st.svc.instance()))
        }
        Some(_) => {
            return Err(ApiError(AliceError::invalid(
                "the nonce must be 16 to 64 hex characters; send a fresh random one",
            )));
        }
    };
    let mut capabilities = vec![CAP_CLIENT_PROJECT.to_string()];
    if st.svc.is_tenant() {
        capabilities.push(CAP_TENANT.to_string());
    }
    Ok(Json(PingResponse {
        service: "alice".into(),
        instance: st.svc.instance().to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        proof,
        capabilities,
    }))
}
async fn health(State(st): State<AppState>) -> ApiResult<Health> {
    Ok(Json(st.svc.health().await?))
}
/// What `alice doctor` asks the daemon: the checks only it can make.
async fn doctor(State(st): State<AppState>) -> ApiResult<crate::doctor::DoctorReport> {
    Ok(Json(crate::doctor::DoctorReport {
        checks: crate::doctor::daemon_checks(&st.svc).await?,
    }))
}
async fn projects(
    State(st): State<AppState>,
) -> ApiResult<Vec<alice_core::store::projects::ResolvedProject>> {
    Ok(Json(st.svc.projects().await?))
}
/// A project hint is set in-process by the MCP handlers and never comes from the wire: every
/// handler that takes a request carrying one clears it on entry, through this function. The one
/// exception is the team store (`--team-store`), whose callers are the personal daemons on the
/// same server (they hold its token) and name the shared project in the body.
fn clear_wire_hint(svc: &Service, hint: &mut Option<String>) {
    if !svc.is_team_store() {
        *hint = None;
    }
}

async fn merge_projects(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<alice_core::api::ProjectMergeRequest>,
) -> ApiResult<alice_core::api::ProjectMergeResponse> {
    Ok(Json(st.svc.merge_projects(req).await?))
}
/// A read-only database never loses a `remember`: it is spooled (202) and applied after repair.
async fn remember(
    State(st): State<AppState>,
    ApiJson(mut req): ApiJson<RememberRequest>,
) -> Result<Response, ApiError> {
    clear_wire_hint(&st.svc, &mut req.project_hint);
    let scope_req = req.clone();
    let r = match st.svc.remember_or_spool(req).await {
        Ok(r) => r,
        Err(e) if alice_core::redact::is_refusal(&e) => {
            let later = st.svc.rules_fail_closed(scope_req.scope()).await;
            return Ok(refusal_response(e, later));
        }
        Err(e) => return Err(e.into()),
    };
    let status = match r.status {
        RememberStatus::Spooled => StatusCode::ACCEPTED,
        _ => StatusCode::OK,
    };
    Ok((status, Json(r)).into_response())
}
async fn recall(
    State(st): State<AppState>,
    ApiJson(mut req): ApiJson<RecallRequest>,
) -> ApiResult<RecallResponse> {
    clear_wire_hint(&st.svc, &mut req.project_hint);
    Ok(Json(st.svc.recall(req).await?))
}
async fn context(
    State(st): State<AppState>,
    ApiJson(mut req): ApiJson<ContextRequest>,
) -> ApiResult<ContextResponse> {
    clear_wire_hint(&st.svc, &mut req.project_hint);
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
    ApiJson(mut req): ApiJson<IngestRequest>,
) -> Result<Response, ApiError> {
    clear_wire_hint(&st.svc, &mut req.project_hint);
    let scope_req = req.clone();
    match st.svc.ingest(req).await {
        Ok(o) => Ok(Json(o).into_response()),
        Err(e) if alice_core::redact::is_refusal(&e) => {
            let later = st.svc.rules_fail_closed(scope_req.scope()).await;
            Ok(refusal_response(e, later))
        }
        Err(e) => Err(e.into()),
    }
}
async fn admin_shutdown(State(st): State<AppState>) -> StatusCode {
    st.shutdown.cancel();
    StatusCode::ACCEPTED
}

/// Replace the API token: the new one goes to the token file (privately, atomically) and takes
/// effect at once, so the old one is refused from the next request on. The new token is never
/// returned; clients read it from `token` (they find out through the ping proof, which the old
/// token can no longer make).
async fn rotate_token(State(st): State<AppState>) -> ApiResult<serde_json::Value> {
    let _one_at_a_time = st.rotating.lock().await;
    let home = st.svc.home().clone();
    let file = home.token_file();
    let new = tokio::task::spawn_blocking(move || crate::discovery::rotate_token(&home))
        .await
        .map_err(|e| AliceError::Other(format!("the token rotation task failed: {e}")))?
        .map_err(|e| {
            AliceError::Other(format!(
                "cannot write the new token to {}: {e}; the old token stays in force. Check the permissions of the Alice home directory",
                file.display()
            ))
        })?;
    *st.token.write() = Arc::from(new);
    // A stream opened with the old token must not outlive it.
    let fresh = st.shutdown.child_token();
    std::mem::replace(&mut *st.streams.lock(), fresh).cancel();
    st.svc.record_token_rotation();
    Ok(Json(serde_json::json!({ "rotated": true })))
}
async fn events(
    State(st): State<AppState>,
    query: Result<Query<EventsRequest>, QueryRejection>,
) -> ApiResult<Vec<EventRow>> {
    let Query(q) = query.map_err(|r| {
        ApiError(AliceError::Invalid(format!(
            "{}; use /api/v1/events?target_id=<id>&actor=<actor>&before=<event id>&limit=<1-500>",
            r.body_text()
        )))
    })?;
    Ok(Json(st.svc.events(q).await?))
}
async fn undo(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<UndoRequest>,
) -> ApiResult<UndoResponse> {
    Ok(Json(st.svc.undo(req).await?))
}

async fn list_reviews(
    State(st): State<AppState>,
    query: Result<Query<alice_core::store::reviews::ReviewQuery>, QueryRejection>,
) -> ApiResult<Vec<alice_core::store::reviews::ReviewItem>> {
    let Query(q) = query.map_err(|r| {
        ApiError(AliceError::Invalid(format!(
            "{}; use /api/v1/reviews?status=<open|resolved>&review_type=<type>&before=<review id>&limit=<1-500>",
            r.body_text()
        )))
    })?;
    Ok(Json(st.svc.reviews(q).await?))
}
/// The query of `GET /api/v1/reviews/counts`.
#[derive(serde::Deserialize)]
struct ReviewCountsQuery {
    review_type: Option<alice_core::store::reviews::ReviewType>,
    provider: Option<String>,
}

async fn review_counts(
    State(st): State<AppState>,
    query: Result<Query<ReviewCountsQuery>, QueryRejection>,
) -> ApiResult<alice_core::store::reviews::ReviewCounts> {
    let Query(q) = query.map_err(|r| {
        ApiError(AliceError::Invalid(format!(
            "{}; use /api/v1/reviews/counts?review_type=<type>&provider=<provider>",
            r.body_text()
        )))
    })?;
    Ok(Json(st.svc.review_counts(q.review_type, q.provider).await?))
}
async fn dismiss_reviews(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<DismissRequest>,
) -> ApiResult<DismissResponse> {
    Ok(Json(st.svc.dismiss_reviews(req).await?))
}
async fn get_review(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<alice_core::store::reviews::ReviewItem> {
    Ok(Json(st.svc.review(id).await?))
}

/// The body of `POST /api/v1/reviews/{id}/resolve`; the id comes from the path.
#[derive(serde::Deserialize)]
struct ResolveBody {
    choice: String,
    #[serde(default)]
    author: alice_core::model::Author,
}

async fn resolve_review(
    State(st): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ResolveBody>,
) -> ApiResult<ResolveResponse> {
    Ok(Json(
        st.svc
            .resolve_review(ResolveRequest {
                id,
                choice: body.choice,
                author: body.author,
            })
            .await?,
    ))
}
async fn merge(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<MergeRequest>,
) -> ApiResult<MergeResponse> {
    Ok(Json(st.svc.merge(req).await?))
}
async fn stats(State(st): State<AppState>) -> ApiResult<Stats> {
    Ok(Json(st.svc.stats().await?))
}

async fn list_backups(State(st): State<AppState>) -> ApiResult<Vec<crate::backup::BackupInfo>> {
    Ok(Json(st.svc.backups().await?))
}
async fn make_backup(State(st): State<AppState>) -> ApiResult<crate::backup::BackupInfo> {
    Ok(Json(st.svc.backup_now().await?))
}
async fn maintenance_status(State(st): State<AppState>) -> ApiResult<Vec<JobStatus>> {
    Ok(Json(st.svc.maintenance_status().await?))
}

/// The body of `POST /api/v1/maintenance/run`.
#[derive(serde::Deserialize)]
struct RunJobBody {
    job: String,
}

async fn run_maintenance(
    State(st): State<AppState>,
    ApiJson(body): ApiJson<RunJobBody>,
) -> ApiResult<crate::maintenance::JobReport> {
    let job = crate::maintenance::Job::parse_or_err(&body.job)?;
    Ok(Json(st.svc.run_job(job).await?))
}

/// Starts an import; the answer is its first status (poll `/api/v1/import/status`).
async fn start_import(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<alice_core::import::ImportRequest>,
) -> Result<(StatusCode, Json<alice_core::import::ImportStatus>), ApiError> {
    let svc = st.svc.clone();
    // The folder checks touch the disk: off the runtime threads.
    let status = tokio::task::spawn_blocking(move || svc.start_import(req))
        .await
        .map_err(|e| ApiError(AliceError::Other(format!("internal task failed: {e}"))))??;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

/// A `mempalace_*` tool call. While `compat.mempalace` is off the answer is 409 with the way
/// to turn it on (the MCP adapter says the same).
async fn compat_call(
    State(st): State<AppState>,
    ApiJson(mut call): ApiJson<crate::compat::CompatCall>,
) -> Response {
    clear_wire_hint(&st.svc, &mut call.project_hint);
    if !st.svc.settings().compat.mempalace {
        return refuse(
            StatusCode::CONFLICT,
            "conflict",
            crate::compat::OFF_MESSAGE.to_string(),
        );
    }
    match st.svc.compat_call(call).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}

async fn import_status(State(st): State<AppState>) -> Json<alice_core::import::ImportStatus> {
    let mut status = st.svc.import_status();
    // How much of the imported text still waits for a vector (0 when it cannot be read now).
    status.embedding_backlog = st.svc.embed_backlog().await.unwrap_or(0);
    Json(status)
}

#[derive(serde::Deserialize, Default)]
struct UploadQuery {
    /// The same switch as `--allow-remote-embedding`.
    #[serde(default)]
    allow_remote_embedding: bool,
}

fn upload_refusal(e: crate::service::UploadError) -> Response {
    use crate::service::UploadError;
    match e {
        UploadError::Conflict(m) => refuse(StatusCode::CONFLICT, "conflict", m),
        UploadError::TooLarge(m) => refuse(StatusCode::PAYLOAD_TOO_LARGE, "too_large", m),
        UploadError::Other(e) => ApiError(e).into_response(),
    }
}

/// `POST /api/v1/import/upload` (server tenants): the body is a gzip tar of the person's
/// MemPalace, streamed to a file in `import-inbox/` and never held in memory. The answer is 202
/// once it is stored; unpacking and importing go on in the background
/// (`GET /api/v1/import/status`).
async fn upload_import(
    State(st): State<AppState>,
    query: Result<Query<UploadQuery>, QueryRejection>,
    req: Request,
) -> Response {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    if !st.svc.is_tenant() {
        return refuse(
            StatusCode::NOT_FOUND,
            "not_found",
            "not found: uploads are only for a server tenant".into(),
        );
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(r) => {
            return ApiError(AliceError::Invalid(format!(
                "{}; the only option is ?allow_remote_embedding=true",
                r.body_text()
            )))
            .into_response();
        }
    };
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    // Admitted (and the one import slot taken) before the first byte of the body is read.
    let mut ticket = match st.svc.upload_begin(declared, q.allow_remote_embedding) {
        Ok(t) => t,
        Err(e) => return upload_refusal(e),
    };
    // Declared after the ticket: dropped first, so the file is closed before it is deleted.
    let file = {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(ticket.archive_path())
    };
    let mut file = match file {
        Ok(f) => tokio::fs::File::from_std(f),
        Err(e) => {
            ticket.fail(format!("the server cannot store the upload ({e})"));
            return ApiError(AliceError::Io(e)).into_response();
        }
    };
    let mut body = req.into_body().into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                ticket.fail(format!("the upload broke off before it was complete ({e})"));
                return ApiError(AliceError::invalid(format!(
                    "the upload broke off before it was complete ({e})"
                )))
                .into_response();
            }
        };
        if let Err(e) = ticket.account(chunk.len() as u64) {
            return upload_refusal(e);
        }
        if let Err(e) = file.write_all(&chunk).await {
            ticket.fail(format!("the server cannot store the upload ({e})"));
            return ApiError(AliceError::Io(e)).into_response();
        }
    }
    if let Err(e) = file.flush().await {
        ticket.fail(format!("the server cannot store the upload ({e})"));
        return ApiError(AliceError::Io(e)).into_response();
    }
    drop(file);
    match ticket.finish() {
        Ok(status) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Err(e) => upload_refusal(e),
    }
}

/// A refused settings change: the first problem as `error` (like every refusal), and all of
/// them as `errors`, each with the key it is about.
#[derive(Serialize)]
struct SettingsErrors {
    error: String,
    kind: &'static str,
    errors: Vec<SettingError>,
}

async fn get_settings(State(st): State<AppState>) -> Json<SettingsView> {
    Json(st.svc.settings_view_async().await)
}

async fn patch_settings(
    State(st): State<AppState>,
    ApiJson(body): ApiJson<serde_json::Map<String, serde_json::Value>>,
) -> Response {
    match st.svc.patch_settings_async(body).await {
        Ok(r) => Json(r).into_response(),
        Err(errors) => (
            StatusCode::BAD_REQUEST,
            Json(SettingsErrors {
                error: errors
                    .first()
                    .map(|e| format!("{}: {}", e.key, e.message))
                    .unwrap_or_default(),
                kind: "invalid",
                errors,
            }),
        )
            .into_response(),
    }
}

/// The switches other processes read often (`alice mcp` asks before each tool listing): small,
/// and without the secrets' status, which may wait on a keychain prompt.
async fn settings_flags(State(st): State<AppState>) -> Json<serde_json::Value> {
    let settings = st.svc.settings();
    Json(serde_json::json!({
        "mcp.admin_tools": settings.mcp.admin_tools,
        "compat.mempalace": settings.compat.mempalace,
    }))
}

/// The JSON Schema of the settings: the web UI renders its forms and help from it.
async fn settings_schema() -> Json<serde_json::Value> {
    Json(
        serde_json::to_value(schemars::schema_for!(alice_core::settings::Settings))
            .unwrap_or_default(),
    )
}

fn secret_name(name: &str) -> Result<SecretName, ApiError> {
    SecretName::parse(name).ok_or_else(|| {
        // The name is not echoed: a secret pasted by mistake where the name goes would be.
        ApiError(AliceError::invalid(format!(
            "unknown secret name; use one of: {}",
            SecretName::ALL
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    })
}

/// The body is read as plain JSON, not as a typed request: a type error from serde would quote
/// the value it found, and the value is a secret.
async fn put_secret(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> ApiResult<SecretStatus> {
    let name = secret_name(&name)?;
    let Some(value) = body.get("value").and_then(|v| v.as_str()) else {
        return Err(ApiError(AliceError::invalid(
            "send a JSON body like {\"value\": \"<the secret>\"} with the secret as text",
        )));
    };
    Ok(Json(
        st.svc.set_secret_async(name, value.to_string()).await?,
    ))
}

async fn delete_secret(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    let deleted = st.svc.delete_secret_async(secret_name(&name)?).await?;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

async fn test_provider(
    State(st): State<AppState>,
    axum::extract::Path(provider): axum::extract::Path<String>,
) -> ApiResult<ProviderTest> {
    Ok(Json(st.svc.test_provider(&provider).await?))
}

/// What the event stream reads between messages.
struct Feed {
    rx: tokio::sync::broadcast::Receiver<StreamItem>,
    svc: Arc<Service>,
    tick: tokio::time::Interval,
    /// The health last sent, without the fields that change on every reading.
    last: Option<serde_json::Value>,
    /// The last health reading failed (logged once until it works again).
    health_failing: bool,
    shutdown: CancellationToken,
}

/// The items of one event-stream client: committed changes as they are broadcast, `Lagged`
/// when it fell too far behind to get them all, and the health on connect and whenever it
/// changes (checked every `health_tick`, one reading shared by every client). It ends when
/// `shutdown` is cancelled.
pub(crate) fn feed(
    svc: Arc<Service>,
    shutdown: CancellationToken,
) -> impl futures::Stream<Item = StreamItem> {
    let mut tick = tokio::time::interval(svc.health_tick());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires at once: the stream opens with the current health.
    let feed = Feed {
        rx: svc.subscribe(),
        svc,
        tick,
        last: None,
        health_failing: false,
        shutdown,
    };
    futures::stream::unfold(feed, |mut f| async move {
        loop {
            tokio::select! {
                _ = f.shutdown.cancelled() => return None,
                item = f.rx.recv() => match item {
                    Ok(item) => return Some((item, f)),
                    // The oldest items were dropped for this client; it can list them again.
                    Err(RecvError::Lagged(missed)) => {
                        return Some((StreamItem::Lagged { missed }, f));
                    }
                    Err(RecvError::Closed) => return None,
                },
                _ = f.tick.tick() => {
                    let health = match f.svc.stream_health().await {
                        Ok(h) => {
                            if f.health_failing {
                                tracing::info!("the event stream reads the health again");
                                f.health_failing = false;
                            }
                            h
                        }
                        Err(e) => {
                            if !f.health_failing {
                                tracing::warn!(error = %e, "the event stream cannot read the health; it is sent again once it can be read");
                                f.health_failing = true;
                            }
                            continue;
                        }
                    };
                    // Memory use changes on every reading; it alone is not a change.
                    let mut key = serde_json::to_value(&health).unwrap_or_default();
                    if let Some(o) = key.as_object_mut() {
                        o.remove("memory_bytes");
                    }
                    if f.last.as_ref() != Some(&key) {
                        f.last = Some(key);
                        return Some((StreamItem::Health { health }, f));
                    }
                }
            }
        }
    })
}

fn sse_event(item: &StreamItem) -> Event {
    Event::default().data(serde_json::to_string(item).unwrap_or_default())
}

/// Server-sent events: `feed`, one JSON `StreamItem` per message. The stream ends when the
/// daemon stops, so a graceful shutdown never waits for a client that stays connected.
async fn stream(
    State(st): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    use futures::StreamExt;
    let ends = st.streams.lock().clone();
    let s = feed(st.svc.clone(), ends).map(|item| Ok(sse_event(&item)));
    Sse::new(s).keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE))
}

/// Ends a response body (an MCP session's event stream) when the token it was opened with is
/// rotated. A request is checked against the token before it gets here; this cuts the ones that
/// were already open.
async fn end_on_rotation(State(st): State<AppState>, req: Request, next: Next) -> Response {
    use futures::StreamExt;
    let ends = st.streams.lock().clone();
    let (parts, body) = next.run(req).await.into_parts();
    let stream = body.into_data_stream().take_until(ends.cancelled_owned());
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

/// Build the router: the public ping, and everything else (the JSON API and the MCP service
/// at `/mcp`) behind the token, Host and Origin guard.
pub fn router(svc: Arc<Service>, token: String, port: u16, shutdown: CancellationToken) -> Router {
    let token: SharedToken = Arc::new(parking_lot::RwLock::new(Arc::from(token)));
    let g = Guard::new(token.clone(), port);
    let state = AppState {
        svc,
        streams: Arc::new(parking_lot::Mutex::new(shutdown.child_token())),
        shutdown,
        token,
        rotating: Arc::new(tokio::sync::Mutex::new(())),
    };
    let public = Router::new()
        .route("/api/v1/ping", get(ping))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(g.clone(), host_guard));
    let private = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/doctor", get(doctor))
        .route("/api/v1/projects", get(projects))
        .route("/api/v1/projects/merge", post(merge_projects))
        .route("/api/v1/remember", post(remember))
        .route("/api/v1/recall", post(recall))
        .route("/api/v1/context", post(context))
        .route("/api/v1/get", post(get_memories))
        .route("/api/v1/update", post(update))
        .route("/api/v1/facts/query", post(facts))
        .route("/api/v1/facts/assert", post(assert_fact))
        .route("/api/v1/ingest", post(ingest))
        .route("/api/v1/events", get(events))
        .route("/api/v1/undo", post(undo))
        .route("/api/v1/reviews", get(list_reviews))
        .route("/api/v1/reviews/counts", get(review_counts))
        .route("/api/v1/reviews/dismiss", post(dismiss_reviews))
        .route("/api/v1/reviews/{id}", get(get_review))
        .route("/api/v1/reviews/{id}/resolve", post(resolve_review))
        .route("/api/v1/memories/merge", post(merge))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/backups", get(list_backups).post(make_backup))
        .route("/api/v1/maintenance", get(maintenance_status))
        .route("/api/v1/maintenance/run", post(run_maintenance))
        .route("/api/v1/events/stream", get(stream))
        .route("/api/v1/settings", get(get_settings).patch(patch_settings))
        .route("/api/v1/settings/schema", get(settings_schema))
        .route("/api/v1/settings/flags", get(settings_flags))
        .route("/api/v1/settings/test/{provider}", post(test_provider))
        .route(
            "/api/v1/secrets/{name}",
            put(put_secret).delete(delete_secret),
        )
        .route("/api/v1/import/mempalace", post(start_import))
        .route("/api/v1/import/status", get(import_status))
        // The body is the archive itself, streamed to disk: the JSON limit does not apply.
        .route(
            "/api/v1/import/upload",
            post(upload_import).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/v1/compat/call", post(compat_call))
        .route("/api/v1/admin/shutdown", post(admin_shutdown))
        .route("/api/v1/admin/token/rotate", post(rotate_token))
        .with_state(state.clone())
        // Before the guard layer, so `/mcp` is covered by it. Its streams end on a rotation too.
        .merge(
            Router::new()
                .nest_service(
                    "/mcp",
                    crate::mcp::http_service(state.svc.clone(), port, state.shutdown.clone()),
                )
                .layer(middleware::from_fn_with_state(state, end_on_rotation)),
        )
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

    #[tokio::test(flavor = "multi_thread")]
    async fn the_ping_lists_capabilities_and_a_tenant_says_so() {
        crate::install_tls();
        for tenant in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let svc = Service::start(
                AliceHome::new(d.path().to_path_buf()),
                &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
                ServiceOptions {
                    tenant,
                    ..ServiceOptions::default()
                },
            )
            .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let app = router(svc, "t".repeat(64), port, CancellationToken::new());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let p: PingResponse = reqwest::get(format!("http://127.0.0.1:{port}/api/v1/ping"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(p.capabilities.contains(&CAP_CLIENT_PROJECT.to_string()));
            assert_eq!(p.capabilities.contains(&CAP_TENANT.to_string()), tenant);
        }
    }

    #[test]
    fn a_refusal_carries_its_field_and_detectors() {
        let r = ApiError(AliceError::Refused {
            field: "content".into(),
            detectors: vec!["github_token".into()],
        })
        .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let body =
            futures::executor::block_on(axum::body::to_bytes(r.into_body(), 1 << 20)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["kind"], "invalid");
        assert_eq!(v["refused"]["field"], "content");
        assert_eq!(v["refused"]["detectors"][0], "github_token");
        assert!(v.get("retry_later").is_none());
    }

    /// A client that falls more than the stream's capacity behind is told how many items it
    /// missed (it can list `/api/v1/events` again), instead of a silent gap.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lagging_client_is_told_how_many_items_it_missed() {
        use futures::StreamExt;
        let d = tempfile::tempdir().unwrap();
        let svc = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                health_tick: std::time::Duration::from_secs(3_600),
                ..Default::default()
            },
        )
        .unwrap();
        let mut items = Box::pin(feed(svc.clone(), CancellationToken::new()));
        assert!(matches!(
            items.next().await,
            Some(StreamItem::Health { .. })
        ));
        let writes = (0..300).map(|i| {
            svc.remember(RememberRequest {
                content: format!("burst {i}"),
                ..Default::default()
            })
        });
        for r in futures::future::join_all(writes).await {
            r.unwrap();
        }
        match items.next().await {
            Some(StreamItem::Lagged { missed }) => assert_eq!(missed, 300 - 256),
            other => panic!("{other:?}"),
        }
        assert!(matches!(items.next().await, Some(StreamItem::Event { .. })));
    }

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
