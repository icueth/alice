//! Find, verify, start and call the daemon.
//!
//! The token is sent only to a process that proves it knows it: `/api/v1/ping?nonce=<fresh
//! random>` must answer with the instance id written in daemon.json and a proof
//! (`discovery::ping_proof`) for that same instance. A stale daemon.json pointing at a port
//! another program now owns, even one that relays the nonce to the real daemon, never
//! receives the token.

use crate::discovery::{read_info, read_token, verify_ping_proof};
use crate::http::PingResponse;
use crate::paths::AliceHome;
use crate::spool::{self, SpoolEntry};
use crate::startup;
use alice_core::api::*;
use alice_core::error::{AliceError, Result};
use alice_core::redact::Redactor;
use alice_core::settings::{ProjectSettings, load_project};
use alice_core::store::kg::FactOutcome;
use alice_core::store::memories::{self, NewMemoryInput};
use alice_core::store::projects::ResolvedProject;
use alice_core::store::reviews::ReviewStatus;
use alice_core::store::sources::IngestOutcome;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a maintenance job or a backup requested by hand may take.
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(3 * 3_600);
/// A daemon that does not answer the ping this fast counts as absent.
const PING_TIMEOUT: Duration = Duration::from_millis(300);
/// How long a write waits for a starting daemon before it is spooled.
pub(crate) const WRITE_WAIT: Duration = Duration::from_millis(1_500);
/// How often `connect` looks for the daemon it started.
const POLL: Duration = Duration::from_millis(50);
/// Every "the daemon cannot be reached" error starts with this; writes are spooled on them.
const UNAVAILABLE: &str = "memory daemon ";

#[derive(Debug, Clone)]
pub enum Launcher {
    /// Never start a daemon (tests, explicit opt-out).
    None,
    /// Start `<exe> daemon run` detached.
    Exe(PathBuf),
}

/// A daemon that proved it holds the token. Cheap to clone (the HTTP pool is shared).
#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base: String,
    token: String,
    /// The instance that proved the token; `is_live` checks the daemon is still that one.
    instance: String,
}

/// Never prints the token.
impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("base", &self.base)
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

fn unavailable(what: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!("{UNAVAILABLE}{what}"))
}

/// Why there is no daemon to call. Its text (an `AliceError::Other` starting with "memory
/// daemon ", so writes are spooled on it) is what a read reports; `queued_note` is what a
/// write queued for it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// None runs and this client may not start one (`Launcher::None`).
    NotRunning,
    /// One was started (or is starting) and did not serve within the wait; what it is doing,
    /// if it said (`startup::StartProgress::describe`).
    Starting(Option<String>),
    /// The last start failed; the daemon's own explanation (from daemon.error).
    StartFailed(String),
    /// The daemon program could not be run, or no HTTP client could be made.
    CannotStart(String),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(UNAVAILABLE)?;
        match self {
            Unavailable::NotRunning => {
                f.write_str("is not running; start it with \"alice daemon start\"")
            }
            Unavailable::Starting(None) => f.write_str("is starting; retry in a few seconds"),
            Unavailable::Starting(Some(doing)) => write!(
                f,
                "is starting ({doing}); retry when it is done, or run \"alice daemon start\" to wait for it"
            ),
            Unavailable::StartFailed(reason) => write!(f, "could not start: {reason}"),
            Unavailable::CannotStart(why) => f.write_str(why),
        }
    }
}

impl From<Unavailable> for AliceError {
    fn from(u: Unavailable) -> Self {
        AliceError::Other(u.to_string())
    }
}

impl Unavailable {
    /// What a write queued because of this says: why, and when it is saved.
    pub fn queued_note(&self) -> String {
        match self {
            Unavailable::NotRunning => "the memory daemon is not running; the memory is saved once it runs (\"alice daemon start\")".into(),
            Unavailable::Starting(None) => {
                "the memory daemon is starting; the memory is saved within seconds".into()
            }
            Unavailable::Starting(Some(doing)) => format!(
                "the memory daemon is starting ({doing}); the memory is saved as soon as it is ready"
            ),
            Unavailable::StartFailed(reason) => format!(
                "the memory daemon could not start: {reason}. The memory is saved once it starts"
            ),
            Unavailable::CannotStart(_) => format!("{self}. The memory is saved once a daemon runs"),
        }
    }
}

fn transport(e: reqwest::Error) -> AliceError {
    if e.is_timeout() {
        unavailable("did not answer in time; it may be busy. Retry in a few seconds")
    } else {
        unavailable(format!(
            "unreachable: {e}. It restarts automatically; retry in a few seconds"
        ))
    }
}

/// Whether a failed write should be spooled: the daemon is down, starting, slow or read-only.
fn is_unavailable(e: &AliceError) -> bool {
    matches!(e, AliceError::Other(m) if m.starts_with(UNAVAILABLE))
        || matches!(e, AliceError::ReadOnly(_))
}

/// Loopback only: no proxy (it would see the token) and no redirects (they could carry it away).
fn http_client() -> Result<reqwest::Client> {
    crate::install_tls();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AliceError::Other(format!("cannot create an HTTP client: {e}")))
}

/// 16 random bytes as 32 hex characters, new for every ping.
fn fresh_nonce() -> Option<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).ok()?;
    Some(hex::encode(buf))
}

/// Whether the process at `base` is daemon `instance` and knows `token`. Sends no token.
async fn proves(http: &reqwest::Client, base: &str, token: &str, instance: &str) -> bool {
    let Some(nonce) = fresh_nonce() else {
        return false;
    };
    let Ok(resp) = http
        .get(format!("{base}/api/v1/ping?nonce={nonce}"))
        .timeout(PING_TIMEOUT)
        .send()
        .await
    else {
        return false;
    };
    let Ok(ping) = resp.json::<PingResponse>().await else {
        return false;
    };
    // Checked against the instance daemon.json names, never the one the answer echoes.
    ping.service == "alice"
        && ping.instance == instance
        && ping
            .proof
            .is_some_and(|p| verify_ping_proof(token, &nonce, instance, &p))
}

/// The JSON error body (`{"error", "kind"}`) as the error the service raised.
fn error_from(status: reqwest::StatusCode, body: &serde_json::Value) -> AliceError {
    let Some(msg) = body["error"].as_str() else {
        return AliceError::Other(format!(
            "the memory daemon answered {status} without an explanation; it may be an older version. Run \"alice daemon stop\", then retry"
        ));
    };
    match body["kind"].as_str() {
        Some("invalid") => AliceError::Invalid(msg.to_string()),
        Some("not_found") => {
            AliceError::NotFound(msg.strip_prefix("not found: ").unwrap_or(msg).to_string())
        }
        Some("read_only") => {
            // The text is `AliceError::ReadOnly`'s display: keep only the reason.
            let reason = msg.strip_prefix("memory is read-only: ").unwrap_or(msg);
            let reason = reason
                .strip_suffix(alice_core::error::READ_ONLY_HINT)
                .and_then(|r| r.strip_suffix(". "))
                .unwrap_or(reason);
            AliceError::ReadOnly(reason.to_string())
        }
        Some("unauthorized" | "forbidden") => AliceError::Other(format!(
            "the memory daemon refused this client: the token or host check failed ({msg}). Restart it with \"alice daemon stop\", then retry"
        )),
        _ => AliceError::Other(msg.to_string()),
    }
}

/// A name that goes into a URL path (a secret, a provider): letters, digits, `_` and `-` only.
fn path_word(word: &str) -> Result<&str> {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Ok(word)
    } else {
        // Not echoed: it may be a secret typed where the name goes.
        Err(AliceError::invalid(
            "that is not a name; use letters, digits, _ and - only (see `alice config list`)",
        ))
    }
}

/// Detach so the daemon survives the caller (agent hooks, shims, terminals).
pub fn spawn_detached(exe: &Path, args: &[&str], log: &Path) -> std::io::Result<u32> {
    spawn_detached_with_env(exe, args, log, &[])
}

fn spawn_detached_with_env(
    exe: &Path,
    args: &[&str],
    log: &Path,
    env: &[(&str, &OsStr)],
) -> std::io::Result<u32> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.current_dir("/");
        // SAFETY: setsid is async-signal-safe and called in the child before exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    // A program written a moment ago can be "busy" (ETXTBSY) while another thread's fork still
    // holds a copy of its write handle; that ends at the fork's exec, so retry briefly.
    let mut child = {
        let mut tries = 0;
        loop {
            match cmd.spawn() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 40 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => break other?,
            }
        }
    };
    let pid = child.id();
    // Reap it if it exits while we are alive (no zombie); does not keep it attached.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// The query string of `GET /api/v1/reviews` for `q` (empty when `q` sets nothing).
fn review_query_string(q: &ReviewQuery) -> String {
    let mut qs = url::form_urlencoded::Serializer::new(String::new());
    if let Some(s) = q.status {
        qs.append_pair(
            "status",
            match s {
                ReviewStatus::Open => "open",
                ReviewStatus::Resolved => "resolved",
            },
        );
    }
    if let Some(t) = q.review_type {
        qs.append_pair("review_type", t.as_str());
    }
    if let Some(b) = &q.before {
        qs.append_pair("before", b);
    }
    if let Some(l) = q.limit {
        qs.append_pair("limit", &l.to_string());
    }
    qs.finish()
}

/// `id` as one URL path segment.
fn path_segment(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// `ALICE_HOME` for a daemon this client starts. The daemon runs from `/`, so a relative home
/// (`ALICE_HOME=./mem`) is resolved against this process's directory first.
fn daemon_home(home: &AliceHome) -> PathBuf {
    std::path::absolute(home.root()).unwrap_or_else(|_| home.root().to_path_buf())
}

impl DaemonClient {
    /// The daemon daemon.json names, if it answers and proves the token.
    pub async fn try_existing(home: &AliceHome) -> Option<DaemonClient> {
        let http = http_client().ok()?;
        Self::verified(home, &http).await
    }

    async fn verified(home: &AliceHome, http: &reqwest::Client) -> Option<DaemonClient> {
        let info = read_info(home)?;
        let token = read_token(home)?;
        let base = format!("http://127.0.0.1:{}", info.port);
        proves(http, &base, &token, &info.instance)
            .await
            .then(|| DaemonClient {
                http: http.clone(),
                base,
                token,
                instance: info.instance,
            })
    }

    /// The running daemon; with `Launcher::Exe`, start one and wait up to `wait` for it.
    pub async fn connect(
        home: &AliceHome,
        launcher: &Launcher,
        wait: Duration,
    ) -> Result<DaemonClient> {
        Ok(Self::try_connect(home, launcher, wait).await?)
    }

    /// `connect`, with why there is no daemon. A daemon this client started that fails says
    /// why in daemon.error, and that is reported at once; when the wait runs out, the reason a
    /// start failed, or what a daemon still starting is doing (downloading the model, say).
    pub async fn try_connect(
        home: &AliceHome,
        launcher: &Launcher,
        wait: Duration,
    ) -> std::result::Result<DaemonClient, Unavailable> {
        let http = http_client().map_err(|e| Unavailable::CannotStart(e.to_string()))?;
        if let Some(c) = Self::verified(home, &http).await {
            return Ok(c);
        }
        let Launcher::Exe(exe) = launcher else {
            return Err(Unavailable::NotRunning);
        };
        // `alice restore` holds the daemon lock for the swap: a daemon started now would only
        // wait for it. Writes spool at once instead of waiting for one.
        if crate::restore::in_progress(home) {
            return Err(Unavailable::Starting(Some(
                "being replaced by `alice restore`".into(),
            )));
        }
        let since = startup::unix_ms();
        // The daemon reads its home from ALICE_HOME: make it serve the one we wait on.
        let child = spawn_detached_with_env(
            exe,
            &["daemon", "run"],
            &home.logs_dir().join("daemon.log"),
            &[("ALICE_HOME", daemon_home(home).as_os_str())],
        )
        .map_err(|e| {
            Unavailable::CannotStart(format!(
                "could not be started with {}: {e}. Run \"alice daemon run\" in a terminal to see why",
                exe.display()
            ))
        })?;
        let deadline = Instant::now() + wait;
        loop {
            tokio::time::sleep(POLL).await;
            if let Some(c) = Self::verified(home, &http).await {
                return Ok(c);
            }
            // A failure since this call started (by the daemon it started, or by one another
            // client started meanwhile) is the answer; an older one may be about to be cleared
            // by a daemon that is starting.
            let failed = startup::read_error(home);
            if let Some(e) = &failed
                && (e.pid == child || e.unix_ms >= since)
            {
                return Err(Unavailable::StartFailed(e.error.clone()));
            }
            if Instant::now() >= deadline {
                return Err(match failed {
                    Some(e) => Unavailable::StartFailed(e.error),
                    None => {
                        Unavailable::Starting(startup::read_progress(home).map(|p| p.describe()))
                    }
                });
            }
        }
    }

    /// Whether the daemon this client verified still answers and proves the token.
    pub async fn is_live(&self) -> bool {
        proves(&self.http, &self.base, &self.token, &self.instance).await
    }

    async fn call<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
        &self,
        path: &str,
        req: &Req,
        timeout: Duration,
    ) -> Result<Resp> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .timeout(timeout)
            .json(req)
            .send()
            .await
            .map_err(transport)?;
        Self::decode(resp).await
    }

    async fn get_json<Resp: DeserializeOwned>(&self, path: &str) -> Result<Resp> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(transport)?;
        Self::decode(resp).await
    }

    async fn decode<Resp: DeserializeOwned>(resp: reqwest::Response) -> Result<Resp> {
        let status = resp.status();
        if status.is_success() {
            return resp.json().await.map_err(|e| {
                if e.is_decode() {
                    AliceError::Other(format!(
                        "unexpected answer from the memory daemon ({e}); it may be another version. Run \"alice daemon stop\", then retry"
                    ))
                } else {
                    transport(e)
                }
            });
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        Err(error_from(status, &body))
    }

    pub async fn health(&self) -> Result<Health> {
        self.get_json("/api/v1/health").await
    }
    pub async fn projects(&self) -> Result<Vec<ResolvedProject>> {
        self.get_json("/api/v1/projects").await
    }
    pub async fn remember(&self, r: &RememberRequest) -> Result<RememberResponse> {
        self.call("/api/v1/remember", r, Duration::from_secs(5))
            .await
    }
    pub async fn recall(&self, r: &RecallRequest) -> Result<RecallResponse> {
        self.call("/api/v1/recall", r, Duration::from_secs(5)).await
    }
    pub async fn context(&self, r: &ContextRequest) -> Result<ContextResponse> {
        self.call("/api/v1/context", r, Duration::from_secs(3))
            .await
    }
    pub async fn get(&self, r: &GetRequest) -> Result<GetResponse> {
        self.call("/api/v1/get", r, Duration::from_secs(5)).await
    }
    pub async fn update(&self, r: &UpdateRequest) -> Result<MemoryFull> {
        self.call("/api/v1/update", r, Duration::from_secs(5)).await
    }
    pub async fn facts(&self, r: &FactsRequest) -> Result<FactsResponse> {
        self.call("/api/v1/facts/query", r, Duration::from_secs(5))
            .await
    }
    pub async fn assert_fact(&self, r: &AssertFactRequest) -> Result<FactOutcome> {
        self.call("/api/v1/facts/assert", r, Duration::from_secs(5))
            .await
    }
    pub async fn ingest(&self, r: &IngestRequest) -> Result<IngestOutcome> {
        self.call("/api/v1/ingest", r, Duration::from_secs(30))
            .await
    }
    /// Recorded changes, newest first.
    pub async fn events(&self, q: &EventsRequest) -> Result<Vec<EventRow>> {
        let mut qs = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in [
            ("target_id", q.target_id.clone()),
            ("actor", q.actor.clone()),
            ("before", q.before.clone()),
            ("limit", q.limit.map(|l| l.to_string())),
        ] {
            if let Some(v) = value {
                qs.append_pair(key, &v);
            }
        }
        let qs = qs.finish();
        if qs.is_empty() {
            self.get_json("/api/v1/events").await
        } else {
            self.get_json(&format!("/api/v1/events?{qs}")).await
        }
    }
    pub async fn undo(&self, r: &UndoRequest) -> Result<UndoResponse> {
        self.call("/api/v1/undo", r, Duration::from_secs(10)).await
    }
    /// Review items, newest first (open ones unless the query says otherwise).
    pub async fn reviews(&self, q: &ReviewQuery) -> Result<Vec<ReviewItem>> {
        let qs = review_query_string(q);
        if qs.is_empty() {
            self.get_json("/api/v1/reviews").await
        } else {
            self.get_json(&format!("/api/v1/reviews?{qs}")).await
        }
    }
    pub async fn review(&self, id: &str) -> Result<ReviewItem> {
        self.get_json(&format!("/api/v1/reviews/{}", path_segment(id)))
            .await
    }
    pub async fn resolve_review(&self, r: &ResolveRequest) -> Result<ResolveResponse> {
        self.call(
            &format!("/api/v1/reviews/{}/resolve", path_segment(&r.id)),
            &serde_json::json!({"choice": r.choice, "author": r.author}),
            Duration::from_secs(10),
        )
        .await
    }
    pub async fn merge(&self, r: &MergeRequest) -> Result<MergeResponse> {
        self.call("/api/v1/memories/merge", r, Duration::from_secs(10))
            .await
    }
    pub async fn stats(&self) -> Result<Stats> {
        self.get_json("/api/v1/stats").await
    }
    /// Backups in the backup directory, newest first.
    pub async fn backups(&self) -> Result<Vec<crate::backup::BackupInfo>> {
        self.get_json("/api/v1/backups").await
    }
    /// Copy the database into the backup directory now (a copy of a large database takes minutes).
    pub async fn backup_now(&self) -> Result<crate::backup::BackupInfo> {
        self.call(
            "/api/v1/backups",
            &serde_json::json!({}),
            MAINTENANCE_TIMEOUT,
        )
        .await
    }
    /// When each maintenance job last ran.
    pub async fn maintenance_status(&self) -> Result<Vec<JobStatus>> {
        self.get_json("/api/v1/maintenance").await
    }
    /// Run one maintenance job now, to its end (a job on a large database takes minutes).
    pub async fn run_job(
        &self,
        job: crate::maintenance::Job,
    ) -> Result<crate::maintenance::JobReport> {
        self.call(
            "/api/v1/maintenance/run",
            &serde_json::json!({ "job": job.name() }),
            MAINTENANCE_TIMEOUT,
        )
        .await
    }
    /// Start importing a MemPalace palace; the answer is the job's first status.
    pub async fn start_import(
        &self,
        r: &alice_core::import::ImportRequest,
    ) -> Result<alice_core::import::ImportStatus> {
        self.call("/api/v1/import/mempalace", r, Duration::from_secs(30))
            .await
    }
    /// One `mempalace_*` tool call (HTTP 409 while `compat.mempalace` is off).
    pub async fn compat_call(
        &self,
        r: &crate::compat::CompatCall,
    ) -> Result<crate::compat::CompatResult> {
        self.call("/api/v1/compat/call", r, Duration::from_secs(10))
            .await
    }
    /// The running import's progress, or the last one's outcome.
    pub async fn import_status(&self) -> Result<alice_core::import::ImportStatus> {
        self.get_json("/api/v1/import/status").await
    }
    /// Every setting with its source, the problems, the secrets' status (never a value).
    pub async fn settings(&self) -> Result<SettingsView> {
        self.get_json("/api/v1/settings").await
    }
    /// The JSON Schema of the settings.
    pub async fn settings_schema(&self) -> Result<serde_json::Value> {
        self.get_json("/api/v1/settings/schema").await
    }
    /// Change settings (`null` removes a key). Validation failures come back as
    /// `AliceError::Invalid` with every `key: message` on its own line.
    pub async fn patch_settings(
        &self,
        changes: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PatchResponse> {
        let resp = self
            .http
            .patch(format!("{}/api/v1/settings", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .json(changes)
            .send()
            .await
            .map_err(transport)?;
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let lines: Vec<String> = body["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    format!(
                        "{}: {}",
                        e["key"].as_str().unwrap_or("?"),
                        e["message"].as_str().unwrap_or("")
                    )
                })
                .collect();
            return Err(AliceError::Invalid(if lines.is_empty() {
                body["error"].as_str().unwrap_or("invalid settings").into()
            } else {
                lines.join("\n")
            }));
        }
        Self::decode(resp).await
    }
    pub async fn set_secret(&self, name: &str, value: &str) -> Result<SecretStatus> {
        let name = path_word(name)?;
        let resp = self
            .http
            .put(format!("{}/api/v1/secrets/{name}", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(15))
            .json(&SecretRequest {
                value: value.into(),
            })
            .send()
            .await
            .map_err(transport)?;
        Self::decode(resp).await
    }
    /// `true` when there was a secret to delete.
    pub async fn delete_secret(&self, name: &str) -> Result<bool> {
        let name = path_word(name)?;
        let resp = self
            .http
            .delete(format!("{}/api/v1/secrets/{name}", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(transport)?;
        let v: serde_json::Value = Self::decode(resp).await?;
        Ok(v["deleted"].as_bool().unwrap_or(false))
    }
    pub async fn test_provider(&self, which: &str) -> Result<ProviderTest> {
        let which = path_word(which)?;
        self.call(
            &format!("/api/v1/settings/test/{which}"),
            &serde_json::json!({}),
            Duration::from_secs(30),
        )
        .await
    }
    pub async fn shutdown(&self) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/api/v1/admin/shutdown", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(transport)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        Err(error_from(status, &body))
    }
}

/// The checks the daemon would make, so a write it will refuse is refused now, not queued.
fn check_remember(req: &RememberRequest) -> Result<()> {
    let input = NewMemoryInput {
        content: req.content.clone(),
        kind: req.kind,
        title: req.title.clone(),
        topics: req.topics.clone(),
        importance: req.importance,
        ..Default::default()
    };
    let somewhere = ResolvedProject {
        id: String::new(),
        slug: String::new(),
        name: String::new(),
    };
    memories::prepare(input, &somewhere, String::new()).map(|_| ())
}

/// The redactor for a write that is spooled because the daemon is away (so its settings and
/// project rules are not at hand): `config.toml` and the `ALICE_*` variables through the same
/// rule the daemon loads them by, and, when the project root can be found the way the daemon
/// finds it (from `cwd`, unless the caller named a project), that project's `.alice.toml`. A
/// project file that is invalid or unreadable counts as `block_secrets` on, as in the daemon, and
/// so does a `config.toml` that is degraded.
/// A named project has no root to find without the database, so only the global rules apply
/// to it here; the daemon applies the project's own rules again when it drains the spool.
///
/// Runs git and reads files: call it on the blocking pool.
fn spool_redactor(home: &AliceHome, project: Option<&str>, cwd: Option<&str>) -> Redactor {
    let effective = crate::config::load_effective(home);
    let mut global = effective.resolved.settings.privacy;
    if effective.degraded {
        // A leaf of config.toml was left out and may have been a privacy one: fail closed, as
        // the daemon does, rather than spool with rules that may be weaker than the person's.
        global.redact = true;
        global.redact_mode = alice_core::settings::RedactMode::Block;
    }
    let named = project.is_some_and(|p| !p.trim().is_empty());
    let root = cwd
        .filter(|_| !named)
        .and_then(|c| alice_core::project::detect_project(Path::new(c)).root);
    let privacy = match root {
        Some(root) => match load_project(&root) {
            Ok(p) => p.apply_to(&global).privacy,
            Err(_) => ProjectSettings::fail_closed().apply_to(&global).privacy,
        },
        None => global,
    };
    Redactor::new(&privacy).unwrap_or_else(|_| Redactor::fallback(&privacy))
}

/// `req` ready for the spool: secrets redacted, or an error naming the detector in `block` mode.
async fn redacted_for_spool(home: &AliceHome, mut req: RememberRequest) -> Result<RememberRequest> {
    let home = home.clone();
    tokio::task::spawn_blocking(move || {
        let r = spool_redactor(&home, req.project.as_deref(), req.cwd.as_deref());
        req.content = r.check("content", &req.content)?;
        if let Some(t) = &req.title {
            req.title = Some(r.check("title", t)?);
        }
        req.topics = req
            .topics
            .iter()
            .map(|t| r.check("topic", t))
            .collect::<Result<_>>()?;
        Ok(req)
    })
    .await
    .map_err(|e| AliceError::Other(format!("redaction stopped: {e}")))?
}

/// The same for an ingest: the source uri and every chunk.
async fn redacted_ingest_for_spool(
    home: &AliceHome,
    mut req: IngestRequest,
) -> Result<IngestRequest> {
    let home = home.clone();
    tokio::task::spawn_blocking(move || {
        let r = spool_redactor(&home, req.project.as_deref(), req.cwd.as_deref());
        req.source.uri = r.check("source uri", &req.source.uri)?;
        for c in &mut req.chunks {
            c.text = r.check("chunk text", &c.text)?;
        }
        Ok(req)
    })
    .await
    .map_err(|e| AliceError::Other(format!("redaction stopped: {e}")))?
}

/// Queue a write for the daemon to apply when it is back; if even that fails, say both.
async fn spool_entry(home: &AliceHome, entry: SpoolEntry, cause: &str) -> Result<()> {
    let dir = home.spool_dir();
    let queued = tokio::task::spawn_blocking(move || spool::append(&dir, &entry))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()));
    match queued {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) | Err(e) => Err(AliceError::Other(format!(
            "{cause}; the write could not be queued in {} either ({e}). Check the disk, then retry",
            home.spool_dir().display()
        ))),
    }
}

/// Why a write went to the spool: the error (for when even the spool fails) and the note its
/// reply carries. `None` when `e` is not a reason to spool.
fn spool_cause(e: &AliceError) -> Option<(String, String)> {
    match e {
        AliceError::ReadOnly(reason) => Some((e.to_string(), spool::read_only_note(reason))),
        e if is_unavailable(e) => Some((
            e.to_string(),
            "the memory daemon stopped answering; the memory is saved when it is back".into(),
        )),
        _ => None,
    }
}

/// `remember` through `client` (or the reason there is none), spooled when the daemon is
/// unavailable; a spooled reply says why.
pub(crate) async fn remember_via(
    home: &AliceHome,
    client: std::result::Result<DaemonClient, Unavailable>,
    req: RememberRequest,
) -> Result<RememberResponse> {
    let (cause, note) = match client {
        Ok(c) => match c.remember(&req).await {
            Err(e) => match spool_cause(&e) {
                Some(cause) => cause,
                None => return Err(e),
            },
            ok => return ok,
        },
        Err(u) => (u.to_string(), u.queued_note()),
    };
    // Redacted first: the daemon checks the length of the redacted text, and so does this.
    let req = redacted_for_spool(home, req).await?;
    check_remember(&req)?;
    spool_entry(home, SpoolEntry::Remember(req), &cause).await?;
    Ok(RememberResponse {
        id: None,
        status: RememberStatus::Spooled,
        project: String::new(),
        reason: Some(note),
    })
}

/// Writes never fail because the daemon is down: they are spooled and applied when it is back.
pub async fn remember_or_spool(
    home: &AliceHome,
    launcher: &Launcher,
    req: RememberRequest,
) -> Result<RememberResponse> {
    let client = DaemonClient::try_connect(home, launcher, WRITE_WAIT).await;
    remember_via(home, client, req).await
}

/// Like `remember_or_spool`; a spooled ingest reports `inserted: 0`.
pub async fn ingest_or_spool(
    home: &AliceHome,
    launcher: &Launcher,
    req: IngestRequest,
) -> Result<IngestOutcome> {
    alice_core::store::sources::validate_chunks(&req.chunks)?;
    let cause = match DaemonClient::try_connect(home, launcher, WRITE_WAIT).await {
        Ok(c) => match c.ingest(&req).await {
            Err(e) => match spool_cause(&e) {
                Some((cause, _)) => cause,
                None => return Err(e),
            },
            ok => return ok,
        },
        Err(u) => u.to_string(),
    };
    let req = redacted_ingest_for_spool(home, req).await?;
    alice_core::store::sources::validate_chunks(&req.chunks)?;
    spool_entry(home, SpoolEntry::Ingest(req), &cause).await?;
    Ok(IngestOutcome {
        source_id: String::new(),
        inserted: 0,
        skipped: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{DaemonInfo, load_or_create_token, ping_proof, write_info};
    use crate::service::{Service, ServiceOptions};
    use crate::spool;
    use alice_core::embed::{Embedder, FakeEmbedder};
    use alice_core::store::sources::{ChunkInput, SourceInput};
    use axum::response::IntoResponse;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn home() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        (d, home)
    }

    struct Running {
        svc: Arc<Service>,
        stop: CancellationToken,
    }

    /// An in-process daemon published the way `daemon::run` does it: token file, daemon.json, router.
    async fn daemon(home: &AliceHome) -> Running {
        crate::install_tls();
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = load_or_create_token(home).unwrap();
        write_info(
            home,
            &DaemonInfo {
                pid: std::process::id(),
                port,
                instance: svc.instance().into(),
                version: "t".into(),
                started_at: "t".into(),
            },
        )
        .unwrap();
        let stop = CancellationToken::new();
        let app = crate::http::router(svc.clone(), token, port, stop.clone());
        let ct = stop.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct.cancelled().await })
                .await
                .unwrap()
        });
        Running { svc, stop }
    }

    const STALE: &str = "01K7STALEINSTANCE000000000";

    /// `(token, nonce) -> proof` the impostor answers the ping with.
    type Proof = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

    #[derive(Default)]
    struct Seen {
        nonces: Vec<String>,
        authorization: Vec<String>,
    }

    /// Another program on the port a stale daemon.json names. It echoes that file's instance,
    /// answers the ping with `proof`, accepts any other request, and records what reaches it.
    async fn impostor(home: &AliceHome, proof: Proof) -> Arc<Mutex<Seen>> {
        let token = load_or_create_token(home).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        write_info(
            home,
            &DaemonInfo {
                pid: 1,
                port,
                instance: STALE.into(),
                version: "t".into(),
                started_at: "t".into(),
            },
        )
        .unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let (seen, proof, token) = (s.clone(), proof.clone(), token.clone());
            async move {
                if let Some(a) = req.headers().get("authorization") {
                    let a = a.to_str().unwrap_or("?").to_string();
                    seen.lock().unwrap().authorization.push(a);
                }
                if req.uri().path() == "/api/v1/ping" {
                    let nonce = req
                        .uri()
                        .query()
                        .and_then(|q| q.strip_prefix("nonce="))
                        .unwrap_or("")
                        .to_string();
                    seen.lock().unwrap().nonces.push(nonce.clone());
                    let mut body = serde_json::json!({"service": "alice", "instance": STALE, "version": "t"});
                    if let Some(p) = proof(&token, &nonce) {
                        body["proof"] = p.into();
                    }
                    return axum::Json(body).into_response();
                }
                axum::Json(serde_json::json!({"id": "01K7STOLEN0000000000000000", "status": "new", "project": "stolen"}))
                    .into_response()
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        seen
    }

    fn remember_req(content: &str) -> RememberRequest {
        RememberRequest {
            content: content.into(),
            ..Default::default()
        }
    }

    fn ingest_req(text: String) -> IngestRequest {
        IngestRequest {
            source: SourceInput {
                uri: "session-1".into(),
                ..Default::default()
            },
            chunks: vec![ChunkInput {
                ord: 0,
                text,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Review focus #3: a stale daemon.json pointing at a port another program now answers on.
    /// The token goes nowhere until the ping proves it for the instance daemon.json names.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_impostor_that_cannot_prove_the_token_never_receives_it() {
        crate::install_tls();
        let refused: Vec<(&str, Proof)> = vec![
            ("no proof", Arc::new(|_, _| None)),
            ("a garbage proof", Arc::new(|_, _| Some("00".repeat(32)))),
            (
                "a proof made with another token",
                Arc::new(|_, n| Some(ping_proof(&"f".repeat(64), n, STALE))),
            ),
            (
                "a proof relayed from the real daemon (another instance)",
                Arc::new(|t, n| Some(ping_proof(t, n, "01K7REALDAEMON000000000000"))),
            ),
            (
                "a proof for another nonce",
                Arc::new(|t, _| Some(ping_proof(t, "0123456789abcdef0123456789abcdef", STALE))),
            ),
        ];
        for (case, proof) in refused {
            let (_d, home) = home();
            let seen = impostor(&home, proof).await;
            assert!(DaemonClient::try_existing(&home).await.is_none(), "{case}");
            let e = DaemonClient::connect(&home, &Launcher::None, Duration::from_millis(200))
                .await
                .unwrap_err();
            assert!(e.to_string().contains("alice daemon start"), "{case}: {e}");
            let r = remember_or_spool(&home, &Launcher::None, remember_req("keep me"))
                .await
                .unwrap();
            assert_eq!(r.status, RememberStatus::Spooled, "{case}");
            assert_eq!(spool::pending_count(&home.spool_dir()), 1, "{case}");
            let seen = seen.lock().unwrap();
            assert!(
                seen.authorization.is_empty(),
                "{case}: the token reached an impostor: {:?}",
                seen.authorization
            );
            assert_eq!(seen.nonces.len(), 3, "{case}: one ping per attempt");
            for n in &seen.nonces {
                assert!(
                    n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{case}: nonce {n:?}"
                );
            }
            let mut unique = seen.nonces.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                seen.nonces.len(),
                "{case}: a nonce was reused"
            );
        }

        // The same harness with a correct proof is accepted, so the refusals above are real.
        let (_d, home) = home();
        let seen = impostor(&home, Arc::new(|t, n| Some(ping_proof(t, n, STALE)))).await;
        let c = DaemonClient::try_existing(&home)
            .await
            .expect("a correct proof is accepted");
        let r = c.remember(&remember_req("x")).await.unwrap();
        assert_eq!(r.project, "stolen");
        assert_eq!(seen.lock().unwrap().authorization.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_daemon_is_verified_and_every_call_round_trips() {
        let (_d, home) = home();
        let running = daemon(&home).await;
        let c = DaemonClient::try_existing(&home)
            .await
            .expect("the real daemon proves the token");
        assert!(c.is_live().await);
        assert_eq!(c.health().await.unwrap().instance, running.svc.instance());

        let repo = home.root().join("clientproj");
        std::fs::create_dir(&repo).unwrap();
        let saved = c
            .remember(&RememberRequest {
                content: "the client round trip works".into(),
                kind: Some(alice_core::model::Kind::Fact),
                cwd: Some(repo.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            (saved.status, saved.project.as_str()),
            (RememberStatus::New, "clientproj")
        );
        let id = saved.id.unwrap();
        assert!(
            c.projects()
                .await
                .unwrap()
                .iter()
                .any(|p| p.slug == "clientproj")
        );
        let found = c
            .recall(&RecallRequest {
                query: "client round trip".into(),
                project: Some("clientproj".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(found.text.unwrap().contains("the client round trip works"));
        let ctx = c
            .context(&ContextRequest {
                project: Some("clientproj".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ctx.project, "clientproj");
        let got = c
            .get(&GetRequest {
                ids: vec![id.clone()],
                include_provenance: false,
            })
            .await
            .unwrap();
        assert_eq!(got.memories.len(), 1);
        let mut update: UpdateRequest =
            serde_json::from_value(serde_json::json!({"id": id, "action": "archive"})).unwrap();
        update.author.agent = Some("my agent/2".into());
        let updated = c.update(&update).await.unwrap();
        assert_eq!(updated.memory.id, id);
        let event = updated.event_id.expect("the update's event");
        // Filters that need escaping in a query string arrive intact.
        let mine = c
            .events(&EventsRequest {
                target_id: Some(id.clone()),
                actor: Some("agent:my agent/2".into()),
                before: None,
                limit: Some(5),
            })
            .await
            .unwrap();
        assert_eq!(
            mine.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            [event.as_str()]
        );
        let all = c.events(&EventsRequest::default()).await.unwrap();
        assert_eq!(all[0].id, event, "newest first");
        let undone = c
            .undo(&UndoRequest {
                event_id: event.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            undone.undone && undone.reverted == [event.clone()],
            "{undone:?}"
        );
        let again = c
            .undo(&UndoRequest {
                event_id: event,
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&again, AliceError::Invalid(m) if m.contains("already undone")),
            "{again}"
        );
        let fact: AssertFactRequest = serde_json::from_value(
            serde_json::json!({"subject": "alice", "predicate": "written in", "object": "rust"}),
        )
        .unwrap();
        assert!(c.assert_fact(&fact).await.unwrap().relation_id.is_some());
        let facts = c
            .facts(&serde_json::from_value(serde_json::json!({"entity": "alice"})).unwrap())
            .await
            .unwrap();
        assert!(facts.text.contains("rust"), "{}", facts.text);
        assert_eq!(
            c.ingest(&ingest_req("a raw chunk".into()))
                .await
                .unwrap()
                .inserted,
            1
        );
        let r = remember_or_spool(
            &home,
            &Launcher::None,
            remember_req("written straight through"),
        )
        .await
        .unwrap();
        assert_eq!(r.status, RememberStatus::New);
        let mut other_source = ingest_req("another raw chunk".into());
        other_source.source.uri = "session-2".into();
        let o = ingest_or_spool(&home, &Launcher::None, other_source)
            .await
            .unwrap();
        assert_eq!((o.inserted, o.source_id.is_empty()), (1, false));
        assert_eq!(spool::pending_count(&home.spool_dir()), 0);

        c.shutdown().await.unwrap();
        assert!(running.stop.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_errors_keep_their_kind_and_read_once() {
        let (_d, home) = home();
        let running = daemon(&home).await;
        let c = DaemonClient::try_existing(&home).await.unwrap();
        let e = c.remember(&remember_req("   ")).await.unwrap_err();
        assert!(
            matches!(&e, AliceError::Invalid(m) if m.contains("content is empty")),
            "{e:?}"
        );
        let update: UpdateRequest = serde_json::from_value(
            serde_json::json!({"id": "01K7NOSUCHMEMORY0000000000", "action": "archive"}),
        )
        .unwrap();
        let e = c.update(&update).await.unwrap_err();
        assert!(matches!(e, AliceError::NotFound(_)), "{e:?}");
        assert_eq!(e.to_string().matches("not found").count(), 1, "{e}");
        running.svc.force_read_only("the disk is full");
        let e = c.ingest(&ingest_req("x".into())).await.unwrap_err();
        assert!(
            matches!(&e, AliceError::ReadOnly(m) if m.contains("the disk is full")),
            "{e:?}"
        );
        assert_eq!(e.to_string().matches("read-only").count(), 1, "{e}");
        assert_eq!(e.to_string().matches("alice status").count(), 1, "{e}");
        assert!(!e.to_string().contains("doctor"), "{e}");
    }

    #[test]
    fn every_daemon_error_kind_maps_to_an_actionable_error() {
        use reqwest::StatusCode as S;
        let body = |kind: &str, error: &str| serde_json::json!({"kind": kind, "error": error});

        let e = error_from(
            S::BAD_REQUEST,
            &body("invalid", "content is empty; send the text"),
        );
        assert!(
            matches!(&e, AliceError::Invalid(m) if m == "content is empty; send the text"),
            "{e:?}"
        );

        let original = AliceError::NotFound("memory 01X; check the id".into());
        let e = error_from(S::NOT_FOUND, &body("not_found", &original.to_string()));
        assert!(
            matches!(&e, AliceError::NotFound(m) if m == "memory 01X; check the id"),
            "{e:?}"
        );
        assert_eq!(e.to_string(), original.to_string());

        let original = AliceError::ReadOnly("the disk is full".into());
        let e = error_from(
            S::SERVICE_UNAVAILABLE,
            &body("read_only", &original.to_string()),
        );
        assert!(
            matches!(&e, AliceError::ReadOnly(m) if m == "the disk is full"),
            "{e:?}"
        );
        assert_eq!(e.to_string(), original.to_string());
        assert!(is_unavailable(&e), "read-only writes are spooled");

        for (status, kind) in [
            (S::UNAUTHORIZED, "unauthorized"),
            (S::FORBIDDEN, "forbidden"),
        ] {
            let e = error_from(status, &body(kind, &format!("{kind}: details")));
            let m = e.to_string();
            assert!(matches!(e, AliceError::Other(_)), "{kind}");
            assert!(m.contains("token or host check failed"), "{m}");
            assert!(m.contains("alice daemon stop"), "{m}");
            assert!(m.contains(&format!("{kind}: details")), "{m}");
        }

        let e = error_from(
            S::INTERNAL_SERVER_ERROR,
            &body("internal", "database error: disk I/O error"),
        );
        assert!(
            matches!(&e, AliceError::Other(m) if m == "database error: disk I/O error"),
            "{e:?}"
        );
        assert!(
            !is_unavailable(&e),
            "a failing write is not spooled: it would block the spool"
        );

        // Not the daemon's JSON (an older daemon without the route, or another program).
        let e = error_from(S::NOT_FOUND, &serde_json::Value::Null);
        let m = e.to_string();
        assert!(m.contains("404") && m.contains("alice daemon stop"), "{m}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_daemon_valid_writes_are_spooled_and_invalid_ones_refused() {
        crate::install_tls();
        let (_d, home) = home();
        let r = remember_or_spool(&home, &Launcher::None, remember_req("saved later"))
            .await
            .unwrap();
        assert_eq!((r.status, r.id), (RememberStatus::Spooled, None));
        let e = remember_or_spool(&home, &Launcher::None, remember_req("   "))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, AliceError::Invalid(m) if m.contains("content is empty")),
            "{e:?}"
        );
        let e = remember_or_spool(
            &home,
            &Launcher::None,
            RememberRequest {
                importance: Some(9),
                ..remember_req("too important")
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(e, AliceError::Invalid(_)), "{e:?}");
        let o = ingest_or_spool(
            &home,
            &Launcher::None,
            ingest_req("captured offline".into()),
        )
        .await
        .unwrap();
        assert_eq!((o.inserted, o.skipped), (0, 0));
        let e = ingest_or_spool(&home, &Launcher::None, ingest_req("x".repeat(40_000)))
            .await
            .unwrap_err();
        assert!(matches!(e, AliceError::Invalid(_)), "{e:?}");
        assert_eq!(spool::pending_count(&home.spool_dir()), 2);

        // The daemon applies them once it runs.
        let running = daemon(&home).await;
        let drained = spool::drain(&home.spool_dir(), &running.svc).await;
        assert_eq!((drained.processed, drained.rejected), (2, 0));
    }

    /// All the bytes of the files in a directory, as one string (lossy).
    fn spool_text(home: &AliceHome) -> String {
        std::fs::read_dir(home.spool_dir())
            .unwrap()
            .flatten()
            .map(|e| String::from_utf8_lossy(&std::fs::read(e.path()).unwrap()).into_owned())
            .collect()
    }

    const GITHUB: &str = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");

    /// The daemon is away: what is queued holds no secret (redacted with the settings read from
    /// the home), `block` mode refuses instead of queueing, and so does a project file, found
    /// from the working directory, that is broken.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_spooled_without_a_daemon_is_redacted_or_refused_in_block_mode() {
        crate::install_tls();
        let (d, home) = home();
        let r = remember_or_spool(
            &home,
            &Launcher::None,
            RememberRequest {
                title: Some(format!("key {GITHUB}")),
                topics: vec![GITHUB.into()],
                ..remember_req(&format!("deploy with {GITHUB}"))
            },
        )
        .await
        .unwrap();
        assert_eq!(r.status, RememberStatus::Spooled);
        let o = ingest_or_spool(
            &home,
            &Launcher::None,
            ingest_req(format!("export TOKEN={GITHUB}")),
        )
        .await
        .unwrap();
        assert_eq!(o.inserted, 0);
        let text = spool_text(&home);
        assert!(!text.contains(GITHUB) && !text.contains("aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"));
        assert!(text.contains("[REDACTED:github_token]"), "{text}");
        assert_eq!(spool::pending_count(&home.spool_dir()), 2);

        // The project file of the working directory applies to a write without a named project.
        let work = d.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join(".alice.toml"), "[privacy\nblock_secrets = tru").unwrap();
        let cwd = Some(work.to_string_lossy().into_owned());
        let e = remember_or_spool(
            &home,
            &Launcher::None,
            RememberRequest {
                cwd: cwd.clone(),
                ..remember_req(&format!("again {GITHUB}"))
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&e, AliceError::Refused { detectors, .. } if detectors == &["github_token"])
                && !e.to_string().contains(GITHUB),
            "{e:?}"
        );
        let e = ingest_or_spool(
            &home,
            &Launcher::None,
            IngestRequest {
                cwd,
                ..ingest_req(format!("again {GITHUB}"))
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(&e, AliceError::Refused { .. }), "{e:?}");
        assert_eq!(
            spool::pending_count(&home.spool_dir()),
            2,
            "nothing more was queued"
        );

        // `block` in config.toml refuses everywhere.
        std::fs::write(home.config_file(), "[privacy]\nredact_mode = \"block\"\n").unwrap();
        let e = remember_or_spool(&home, &Launcher::None, remember_req(&format!("x {GITHUB}")))
            .await
            .unwrap_err();
        assert!(matches!(&e, AliceError::Refused { .. }), "{e:?}");
        assert!(!spool_text(&home).contains(GITHUB));
    }

    /// The length is checked on the redacted text (as the daemon does): a request that redaction
    /// makes too long is refused now, not queued and rejected at the drain.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_spool_checks_the_length_after_redaction() {
        crate::install_tls();
        let (_d, home) = home();
        let key = concat!("AKIA", "IOSFODNN7EXAMPLE");
        let content = vec![key; 3_000].join(" ");
        assert!(content.len() < 65_536);
        let e = remember_or_spool(&home, &Launcher::None, remember_req(&content))
            .await
            .unwrap_err();
        assert!(matches!(e, AliceError::Invalid(_)), "{e:?}");
        assert_eq!(spool::pending_count(&home.spool_dir()), 0);
    }

    /// A `config.toml` with a bad leaf may have lost a privacy setting: the client then blocks
    /// secrets instead of spooling with rules that may be weaker than the person's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_degraded_config_blocks_secrets_in_the_spool() {
        crate::install_tls();
        let (_d, home) = home();
        std::fs::write(
            home.config_file(),
            "[privacy]\nredact = true\n[search]\nrelevance_floor = \"x\"\n",
        )
        .unwrap();
        let e = remember_or_spool(&home, &Launcher::None, remember_req(&format!("k {GITHUB}")))
            .await
            .unwrap_err();
        assert!(matches!(e, AliceError::Refused { .. }), "{e:?}");
        let ok = remember_or_spool(&home, &Launcher::None, remember_req("no secret"))
            .await
            .unwrap();
        assert_eq!(ok.status, RememberStatus::Spooled);
    }

    /// The daemon runs from `/`: a relative home must reach it as the same directory.
    #[test]
    fn the_started_daemon_gets_this_home_as_an_absolute_path() {
        let rel = AliceHome::new(PathBuf::from("rel").join("home"));
        let given = daemon_home(&rel);
        assert!(given.is_absolute(), "{}", given.display());
        assert_eq!(
            given,
            std::env::current_dir().unwrap().join("rel").join("home")
        );
        let abs = AliceHome::new(std::env::temp_dir().join("alice-abs"));
        assert_eq!(daemon_home(&abs), abs.root());
    }

    /// A stand-in for the `alice` binary: records how it was started, then exits.
    #[cfg(unix)]
    fn fake_exe(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let record = dir.join("started.txt");
        let exe = dir.join("fake-alice");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\necho \"$* home=$ALICE_HOME cwd=$(pwd)\" >> '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        (exe, record)
    }

    #[cfg(unix)]
    async fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // Unix only (M1 ruling R3): the stand-in for the `alice` binary is a `#!/bin/sh` script,
    // which Windows cannot run.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_starts_a_daemon_for_this_home_and_waits_until_it_serves() {
        crate::install_tls();
        let (_d, home) = home();
        let tools = tempfile::tempdir().unwrap();
        let (exe, record) = fake_exe(tools.path());
        // The daemon comes up a moment after it was started, as a real one does.
        let starter = {
            let (home, record) = (home.clone(), record.clone());
            tokio::spawn(async move {
                wait_for(&record).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                daemon(&home).await
            })
        };
        let c = DaemonClient::connect(&home, &Launcher::Exe(exe), Duration::from_secs(10))
            .await
            .unwrap();
        let running = starter.await.unwrap();
        assert_eq!(c.health().await.unwrap().instance, running.svc.instance());
        let started = std::fs::read_to_string(&record).unwrap();
        assert_eq!(
            started.trim(),
            format!("daemon run home={} cwd=/", home.root().display()),
            "started once, as `daemon run`, for this home, outside the caller's directory"
        );
        assert!(home.logs_dir().join("daemon.log").exists());
    }

    // Unix only (M1 ruling R3): the stand-in for the `alice` binary is a `#!/bin/sh` script,
    // which Windows cannot run.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_gives_up_after_the_wait_and_says_what_to_do() {
        crate::install_tls();
        let (_d, home) = home();
        let tools = tempfile::tempdir().unwrap();
        let (exe, record) = fake_exe(tools.path());
        let t = Instant::now();
        let e = DaemonClient::connect(
            &home,
            &Launcher::Exe(exe.clone()),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(
            e.to_string().contains("starting; retry in a few seconds"),
            "{e}"
        );
        assert!(t.elapsed() >= Duration::from_millis(300) && t.elapsed() < Duration::from_secs(5));
        wait_for(&record).await;

        let missing = tools.path().join("no-such-alice");
        let e = DaemonClient::connect(&home, &Launcher::Exe(missing), Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("could not be started"), "{e}");

        // Writes survive both.
        let r = remember_or_spool(&home, &Launcher::Exe(exe), remember_req("not lost"))
            .await
            .unwrap();
        assert_eq!(r.status, RememberStatus::Spooled);
    }

    /// A stand-in for `alice daemon run` that runs `body` (a shell snippet; `$$` is its pid).
    #[cfg(unix)]
    fn script_exe(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("scripted-alice");
        std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    /// Review finding I-1: the daemon this client started fails and leaves its reason in
    /// daemon.error; `connect` reports that reason at once instead of waiting out the wait and
    /// saying "starting". A write is still queued, and its reply says why.
    // Unix only (M1 ruling R3): the stand-in for the `alice` binary is a `#!/bin/sh` script,
    // which Windows cannot run.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_reports_why_the_started_daemon_failed() {
        crate::install_tls();
        let (_d, home) = home();
        let tools = tempfile::tempdir().unwrap();
        let reason = "alice.db is not a usable database; move it aside";
        let exe = script_exe(
            tools.path(),
            &format!(
                "printf '{{\"pid\": %s, \"at\": \"t\", \"unix_ms\": 1, \"error\": \"{reason}\"}}' $$ > '{}'",
                home.root().join("daemon.error").display()
            ),
        );
        let t = Instant::now();
        let e = DaemonClient::connect(&home, &Launcher::Exe(exe.clone()), Duration::from_secs(30))
            .await
            .unwrap_err();
        // At once, not after the 30 s wait.
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "waited {:?}",
            t.elapsed()
        );
        assert_eq!(
            e.to_string(),
            format!("memory daemon could not start: {reason}")
        );
        let r = remember_or_spool(&home, &Launcher::Exe(exe), remember_req("not lost"))
            .await
            .unwrap();
        assert_eq!(r.status, RememberStatus::Spooled);
        let why = r.reason.unwrap();
        assert!(why.contains(reason) && !why.contains("starting"), "{why}");
        assert_eq!(spool::pending_count(&home.spool_dir()), 1);
    }

    /// Review finding I-1: when the wait runs out while the daemon downloads the model, the
    /// error (and a queued write's reason) says so.
    // Unix only (M1 ruling R3): the stand-in for the `alice` binary is a `#!/bin/sh` script,
    // which Windows cannot run.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_says_when_the_daemon_is_still_downloading_the_model() {
        crate::install_tls();
        let (_d, home) = home();
        let tools = tempfile::tempdir().unwrap();
        let exe = script_exe(
            tools.path(),
            &format!(
                "printf '{{\"pid\": %s, \"unix_ms\": 1, \"step\": \"downloading the embedding model\", \"file\": \"model_quantized.onnx\", \"done\": 29577047, \"total\": 118308185}}' $$ > '{}'\nexec sleep 10",
                home.root().join("daemon.starting").display()
            ),
        );
        let announced = || {
            let raw = std::fs::read_to_string(home.root().join("daemon.starting")).unwrap();
            serde_json::from_str::<serde_json::Value>(&raw).unwrap()["pid"]
                .as_i64()
                .unwrap() as libc::pid_t
        };
        // The stand-in can be slow to start on a loaded machine (a one-off OS check on the first
        // run of a new executable on macOS, a starved process elsewhere): ask again until it
        // has announced itself.
        let mut tries = 0;
        let e = loop {
            tries += 1;
            let e =
                DaemonClient::connect(&home, &Launcher::Exe(exe.clone()), Duration::from_secs(3))
                    .await
                    .unwrap_err()
                    .to_string();
            if e.contains("downloading") || tries >= 10 {
                break e;
            }
        };
        let first = announced();
        assert!(
            e.starts_with("memory daemon is starting")
                && e.contains("downloading the embedding model")
                && e.contains("25%"),
            "{e}"
        );
        let r = remember_or_spool(&home, &Launcher::Exe(exe), remember_req("not lost"))
            .await
            .unwrap();
        let why = r.reason.unwrap();
        assert!(why.contains("downloading the embedding model"), "{why}");
        for pid in [first, announced()] {
            // SAFETY: ending the stand-ins this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    // Unix only: checks the setsid session (a Windows process has none) and kills with signals.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_detached_runs_in_its_own_session_with_output_in_the_log() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("logs").join("daemon.log");
        let pid = spawn_detached(
            Path::new("/bin/sh"),
            &[
                "-c",
                "echo started in $(pwd); read line; echo stdin=${line:-closed}; exec sleep 30",
            ],
            &log,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = std::fs::read_to_string(&log).unwrap_or_default();
            if out.contains("stdin=closed") {
                assert!(out.contains("started in /\n"), "{out}");
                break;
            }
            assert!(Instant::now() < deadline, "log: {out:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid = pid as libc::pid_t;
        // SAFETY: plain syscalls on a pid we started.
        unsafe {
            assert_eq!(libc::getsid(pid), pid, "the child leads its own session");
            assert_ne!(libc::getsid(pid), libc::getsid(0));
            libc::kill(pid, libc::SIGTERM);
        }
    }
}
