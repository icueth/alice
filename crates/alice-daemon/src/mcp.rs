//! The MCP server. The same `AliceMcp` runs inside the daemon (Streamable HTTP at /mcp,
//! backend = the in-process Service) and in `alice mcp` (stdio, backend = HTTP to the daemon).

#![allow(deprecated)] // roots are deprecated by SEP-2577 but current clients still provide them

use crate::client::{DaemonClient, Launcher, Unavailable, WRITE_WAIT, remember_via};
use crate::paths::AliceHome;
use crate::service::Service;
use alice_core::api::*;
use alice_core::error::AliceError;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams,
    PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, Role, ServerCapabilities, ServerConfig,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, prompt, prompt_handler,
    prompt_router, tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Not imported as `Result`: the rmcp handler macros expand to an unqualified `Result<_, McpError>`.
type AliceResult<T> = alice_core::error::Result<T>;

pub const INSTRUCTIONS: &str = "Alice is your long-term memory, shared with the user's other agents and sessions.\n\
- When you start work on a project, call `context` unless a memory briefing is already in the conversation.\n\
- Before answering about past work, decisions, bugs or preferences, call `recall`.\n\
- Call `remember` for decisions (with the reason), bug fixes (symptom, cause, fix), facts, how-tos, warnings and user preferences. One idea per memory; set `kind`.\n\
- When a new memory replaces an old one, pass the old id in `supersedes`.\n\
- Before ending a long task, `remember` a `handoff`: what is done, what is next, open questions.\n\
- Use `assert_fact` and `query_facts` for stable relationships (X uses Y, service runs on host).\n\
- Never store secrets.";

/// How long a read waits for a daemon that is starting.
const READ_WAIT: Duration = Duration::from_secs(5);
/// How long the client gets to answer `roots/list`.
const ROOTS_TIMEOUT: Duration = Duration::from_millis(300);

pub struct RemoteBackend {
    home: AliceHome,
    launcher: Launcher,
    cwd: Option<String>,
    client: tokio::sync::Mutex<Option<DaemonClient>>,
}

impl RemoteBackend {
    pub fn new(home: AliceHome, launcher: Launcher, cwd: Option<String>) -> Self {
        Self {
            home,
            launcher,
            cwd,
            client: tokio::sync::Mutex::new(None),
        }
    }

    /// Connect lazily (keeps `alice mcp` start-up fast). A cached client is re-checked with
    /// the proof ping, which touches no database, so a restarted daemon is found again.
    async fn client(&self, wait: Duration) -> AliceResult<DaemonClient> {
        Ok(self.try_client(wait).await?)
    }

    /// `client`, with why there is no daemon (a queued write's reply says it).
    async fn try_client(&self, wait: Duration) -> Result<DaemonClient, Unavailable> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref()
            && c.is_live().await
        {
            return Ok(c.clone());
        }
        *slot = None;
        let c = DaemonClient::try_connect(&self.home, &self.launcher, wait).await?;
        *slot = Some(c.clone());
        Ok(c)
    }
}

#[derive(Clone)]
pub enum Backend {
    Local(Arc<Service>),
    Remote(Arc<RemoteBackend>),
}

impl Backend {
    fn default_cwd(&self) -> Option<String> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(r) => r.cwd.clone(),
        }
    }
    async fn recall(&self, r: RecallRequest) -> AliceResult<RecallResponse> {
        match self {
            Backend::Local(s) => s.recall(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.recall(&r).await,
        }
    }
    /// Never fails because the daemon is down or read-only: the write is spooled instead.
    async fn remember(&self, r: RememberRequest) -> AliceResult<RememberResponse> {
        match self {
            Backend::Local(s) => s.remember_or_spool(r).await,
            Backend::Remote(b) => remember_via(&b.home, b.try_client(WRITE_WAIT).await, r).await,
        }
    }
    async fn context(&self, r: ContextRequest) -> AliceResult<ContextResponse> {
        match self {
            Backend::Local(s) => s.context(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.context(&r).await,
        }
    }
    async fn get(&self, r: GetRequest) -> AliceResult<GetResponse> {
        match self {
            Backend::Local(s) => s.get(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.get(&r).await,
        }
    }
    async fn update(&self, r: UpdateRequest) -> AliceResult<MemoryFull> {
        match self {
            Backend::Local(s) => s.update(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.update(&r).await,
        }
    }
    async fn facts(&self, r: FactsRequest) -> AliceResult<FactsResponse> {
        match self {
            Backend::Local(s) => s.facts(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.facts(&r).await,
        }
    }
    async fn assert_fact(
        &self,
        r: AssertFactRequest,
    ) -> AliceResult<alice_core::store::kg::FactOutcome> {
        match self {
            Backend::Local(s) => s.assert_fact(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.assert_fact(&r).await,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallPromptArgs {
    /// What to look for.
    pub query: String,
}

/// The client's first root for one connection.
#[derive(Default)]
struct RootCache {
    /// Bumped by `roots/list_changed`, so an answer to an older question is not kept.
    generation: u64,
    /// `Some(dir)` once the client answered; `dir` is `None` when it named no local directory.
    first: Option<Option<String>>,
}

pub struct AliceMcp {
    backend: Backend,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
    /// rmcp keeps one handler per connection (the HTTP service clones one per session).
    roots: parking_lot::Mutex<RootCache>,
}

impl Clone for AliceMcp {
    /// A clone serves another connection, so it starts without a cached root.
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            tool_router: self.tool_router.clone(),
            prompt_router: self.prompt_router.clone(),
            roots: Default::default(),
        }
    }
}

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s.into())])
}

fn tool_err(e: AliceError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}

/// The reply to a queued `remember`: why it was queued, and when it is saved.
fn queued_text(reason: Option<&str>) -> String {
    match reason {
        Some(r) => format!("Queued: {r}."),
        None => {
            "Queued: the memory service was not available; this memory is saved when it is.".into()
        }
    }
}

fn json<T: serde::Serialize>(v: &T) -> CallToolResult {
    text(serde_json::to_string_pretty(v).unwrap_or_default())
}

fn resource_err(e: AliceError) -> McpError {
    match e {
        AliceError::Invalid(m) => McpError::invalid_params(m, None),
        e @ AliceError::NotFound(_) => McpError::resource_not_found(e.to_string(), None),
        e => McpError::internal_error(e.to_string(), None),
    }
}

/// A client on the 2026-07-28 protocol: no handshake, its identity and capabilities travel in
/// each request's `_meta`, and the server cannot send it requests (such as `roots/list`).
fn per_request(ctx: &RequestContext<RoleServer>) -> bool {
    ctx.protocol_version().is_some_and(|v| !v.has_initialize())
}

impl AliceMcp {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
            roots: Default::default(),
        }
    }

    /// Working directory for project detection (spec §8.1): the client's first root when it
    /// offers roots, else the shim's cwd, else none. An explicit `project` is applied later by
    /// the service and wins over both.
    async fn cwd(&self, ctx: &RequestContext<RoleServer>) -> Option<String> {
        if let Some(root) = self.first_root(ctx).await {
            return Some(root);
        }
        self.backend.default_cwd()
    }

    /// The client's first root as a local directory, asked once per connection and again after
    /// `roots/list_changed`. A timeout or error is not cached: the next request asks again.
    async fn first_root(&self, ctx: &RequestContext<RoleServer>) -> Option<String> {
        let offers_roots = ctx
            .peer
            .peer_info()
            .is_some_and(|i| i.capabilities.roots.is_some());
        if per_request(ctx) || !offers_roots {
            return None;
        }
        let generation = {
            let cache = self.roots.lock();
            if let Some(first) = &cache.first {
                return first.clone();
            }
            cache.generation
        };
        let roots = tokio::time::timeout(ROOTS_TIMEOUT, ctx.peer.list_roots())
            .await
            .ok()?
            .ok()?;
        let first = roots.roots.first().and_then(|r| {
            url::Url::parse(&r.uri)
                .ok()?
                .to_file_path()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
        let mut cache = self.roots.lock();
        if cache.generation == generation {
            cache.first = Some(first.clone());
        }
        first
    }

    /// The client's name, recorded as the memory's author.
    fn agent(ctx: &RequestContext<RoleServer>) -> Option<String> {
        let name = if per_request(ctx) {
            // The handshake-less path fills `peer_info` with placeholder client details.
            ctx.meta.client_info().map(|i| i.name)
        } else {
            ctx.peer.peer_info().map(|i| i.client_info.name.clone())
        };
        name.filter(|n| !n.trim().is_empty())
    }
}

#[tool_router]
impl AliceMcp {
    #[tool(
        description = "Search long-term memory: decisions, bug fixes, facts, how-tos, preferences and past work. Use before answering about anything done before.",
        annotations(read_only_hint = true)
    )]
    async fn recall(
        &self,
        Parameters(mut req): Parameters<RecallRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        req.cwd = self.cwd(&ctx).await;
        let format = req.format;
        Ok(match self.backend.recall(req).await {
            Ok(r) if format == OutputFormat::Json => json(&r),
            Ok(r) => text(r.text.unwrap_or_default()),
            Err(e) => tool_err(e),
        })
    }

    #[tool(
        description = "Save one memory worth keeping: a decision (with why), bug fix (symptom, cause, fix), fact, how-to, preference, warning or end-of-task handoff. Set kind; pass supersedes to replace older memories."
    )]
    async fn remember(
        &self,
        Parameters(mut req): Parameters<RememberRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        req.cwd = self.cwd(&ctx).await;
        if req.author.agent.is_none() {
            req.author.agent = Self::agent(&ctx);
        }
        Ok(match self.backend.remember(req).await {
            Ok(r) => match (r.status, r.id) {
                (RememberStatus::Spooled, _) => text(queued_text(r.reason.as_deref())),
                (RememberStatus::Duplicate, Some(id)) => {
                    text(format!("Already remembered as {id} in {}.", r.project))
                }
                (_, Some(id)) => text(format!("Saved {id} in {}.", r.project)),
                (_, None) => text("Saved."),
            },
            Err(e) => tool_err(e),
        })
    }

    #[tool(
        description = "Briefing for the current project: key memories, warnings, recent decisions, last handoff and the user's preferences. Call at the start of work.",
        annotations(read_only_hint = true)
    )]
    async fn context(
        &self,
        Parameters(mut req): Parameters<ContextRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        req.cwd = self.cwd(&ctx).await;
        Ok(match self.backend.context(req).await {
            Ok(r) => text(r.text),
            Err(e) => tool_err(e),
        })
    }

    #[tool(
        description = "Fetch full memories by id, with links and optionally the raw session chunks they came from.",
        annotations(read_only_hint = true)
    )]
    async fn get_memory(
        &self,
        Parameters(req): Parameters<GetRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.get(req).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }

    #[tool(
        description = "Change a memory: action edit (title/body/kind/topics/importance), supersede (superseded_by), archive, forget (soft delete) or restore."
    )]
    async fn update_memory(
        &self,
        Parameters(req): Parameters<UpdateRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.update(req).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }

    #[tool(
        description = "Facts about an entity from the knowledge graph: current ones, as of a date, or the full timeline.",
        annotations(read_only_hint = true)
    )]
    async fn query_facts(
        &self,
        Parameters(req): Parameters<FactsRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.facts(req).await {
            Ok(r) => text(r.text),
            Err(e) => tool_err(e),
        })
    }

    #[tool(
        description = "Record a fact (subject, predicate, object). action add, supersede (replace the current value) or end (it stopped being true)."
    )]
    async fn assert_fact(
        &self,
        Parameters(req): Parameters<AssertFactRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.assert_fact(req).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }
}

#[prompt_router]
impl AliceMcp {
    #[prompt(name = "recall", description = "Search Alice memory for a topic")]
    async fn recall_prompt(
        &self,
        Parameters(a): Parameters<RecallPromptArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Search my memory for: {}. Use the recall tool, then summarise what is relevant and cite memory ids.",
                a.query
            ),
        )]
    }

    #[prompt(
        name = "remember-session",
        description = "Save what this session learned"
    )]
    async fn remember_session(&self) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            "Review this session and save what is worth keeping with the remember tool: decisions (with reasons), bug fixes (symptom, cause, fix), facts, how-tos, warnings and my preferences. One idea per memory, with kind set. Then save a handoff memory: what is done, what is next, open questions.",
        )]
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for AliceMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("alice", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _r: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let r = Resource::new("memory://context", "Project memory briefing")
            .with_description("Alice context for the current project")
            .with_mime_type("text/plain");
        Ok(ListResourcesResult::with_all_items(vec![r]))
    }

    async fn on_roots_list_changed(&self, _ctx: NotificationContext<RoleServer>) {
        let mut cache = self.roots.lock();
        cache.generation += 1;
        cache.first = None;
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = request.uri.clone();
        let body = if uri == "memory://context" {
            let cwd = self.cwd(&ctx).await;
            let r = self
                .backend
                .context(ContextRequest {
                    cwd,
                    ..Default::default()
                })
                .await
                .map_err(resource_err)?;
            r.text
        } else if let Some(id) = uri.strip_prefix("memory://m/") {
            let r = self
                .backend
                .get(GetRequest {
                    ids: vec![id.to_string()],
                    include_provenance: true,
                })
                .await
                .map_err(resource_err)?;
            if r.memories.is_empty() {
                return Err(McpError::resource_not_found(
                    format!("no memory {id}; find memory ids with the recall tool"),
                    None,
                ));
            }
            serde_json::to_string_pretty(&r).unwrap_or_default()
        } else {
            return Err(McpError::resource_not_found(
                format!("unknown resource {uri}; read memory://context or memory://m/<id>"),
                None,
            ));
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(body, uri)]).into())
    }
}

/// Streamable HTTP service mounted at `/mcp` inside the token-guarded router.
pub fn http_service(
    svc: Arc<Service>,
    port: u16,
    ct: CancellationToken,
) -> StreamableHttpService<AliceMcp, LocalSessionManager> {
    let server = AliceMcp::new(Backend::Local(svc));
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts([format!("127.0.0.1:{port}"), format!("localhost:{port}")])
            .with_allowed_origins([format!("http://127.0.0.1:{port}")])
            .with_cancellation_token(ct.child_token()),
    )
}

/// `alice mcp`: stdio server for agents that launch MCP servers as child processes.
/// Logs must go to stderr; stdout carries the protocol.
pub async fn run_stdio(home: AliceHome, launcher: Launcher) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(
        home, launcher, cwd,
    ))));
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ServiceOptions;
    use alice_core::embed::{Embedder, FakeEmbedder};
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };

    /// Review finding m-3: over Streamable HTTP too, a read-only database queues `remember`
    /// in the spool (as the JSON API and the stdio shim do), and the reply says why.
    #[tokio::test(flavor = "multi_thread")]
    async fn http_mcp_remember_is_spooled_while_read_only() {
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
        let token = "k".repeat(64);
        let app = crate::http::router(svc, token.clone(), port, CancellationToken::new());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg =
            StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp"))
                .auth_header(token);
        let client = ().serve(StreamableHttpClientTransport::from_config(cfg)).await.unwrap();
        let mut args = serde_json::Map::new();
        args.insert("content".into(), "kept while read-only".into());
        let r = client
            .call_tool(CallToolRequestParams::new("remember").with_arguments(args))
            .await
            .unwrap();
        let text = r
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default();
        assert_ne!(r.is_error, Some(true), "{text}");
        assert!(text.starts_with("Queued: "), "{text}");
        assert!(
            text.contains("read-only") && text.contains("the disk is full"),
            "{text}"
        );
        assert!(!text.contains("starting"), "{text}");
        assert_eq!(crate::spool::pending_count(&home.spool_dir()), 1);
        client.cancel().await.unwrap();
    }
}
