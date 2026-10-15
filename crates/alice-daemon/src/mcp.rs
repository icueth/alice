//! The MCP server. The same `AliceMcp` runs inside the daemon (Streamable HTTP at /mcp,
//! backend = the in-process Service) and in `alice mcp` (stdio, backend = HTTP to the daemon).

#![allow(deprecated)] // roots are deprecated by SEP-2577 but current clients still provide them

use crate::client::{DaemonClient, Launcher, Unavailable, WRITE_WAIT, remember_via};
use crate::paths::AliceHome;
use crate::service::Service;
use alice_core::api::*;
use alice_core::error::AliceError;
use alice_core::model::Author;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListResourcesResult, ListToolsResult, PaginatedRequestParams, PromptMessage,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResultType, Role, ServerCapabilities, ServerConfig, Tool,
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
/// The tools that are offered only while `mcp.admin_tools` is on.
const ADMIN_TOOLS: [&str; 4] = ["review_list", "review_resolve", "merge_memories", "stats"];
/// How long `alice mcp` trusts the daemon's last answer about the admin-tools switch.
const ADMIN_SWITCH_TTL: Duration = Duration::from_secs(1);
/// How long the client gets to answer `roots/list`.
const ROOTS_TIMEOUT: Duration = Duration::from_millis(300);

pub struct RemoteBackend {
    home: AliceHome,
    launcher: Launcher,
    cwd: Option<String>,
    client: tokio::sync::Mutex<Option<DaemonClient>>,
    /// The daemon's last answer about `mcp.admin_tools`, and when it came.
    admin_switch: parking_lot::Mutex<Option<(std::time::Instant, bool)>>,
    /// How long that answer is trusted.
    admin_ttl: Duration,
}

impl RemoteBackend {
    pub fn new(home: AliceHome, launcher: Launcher, cwd: Option<String>) -> Self {
        Self {
            home,
            launcher,
            cwd,
            client: tokio::sync::Mutex::new(None),
            admin_switch: parking_lot::Mutex::new(None),
            admin_ttl: ADMIN_SWITCH_TTL,
        }
    }

    /// Trust the daemon's answer about the admin tools for `ttl` (tests use zero).
    pub fn with_admin_ttl(mut self, ttl: Duration) -> Self {
        self.admin_ttl = ttl;
        self
    }

    /// `mcp.admin_tools` as the running daemon has it (its ratchet and its audit apply), asked
    /// at most once per `admin_ttl`. No daemon, or one that does not answer: off. This never
    /// starts a daemon.
    async fn admin_tools(&self) -> bool {
        if let Some((at, on)) = *self.admin_switch.lock()
            && at.elapsed() < self.admin_ttl
        {
            return on;
        }
        // The answer is as old as the moment the question was asked, not the moment it came
        // back: a slow answer must not look fresh for a whole `admin_ttl` more.
        let asked = std::time::Instant::now();
        // The small flags endpoint (no read of the secrets' status, a short timeout), not the
        // whole settings view.
        let on = match DaemonClient::try_existing(&self.home).await {
            Some(c) => c.admin_tools_flag().await.unwrap_or(false),
            None => false,
        };
        *self.admin_switch.lock() = Some((asked, on));
        on
    }

    /// The last answer, without asking again (off before the first).
    fn admin_tools_cached(&self) -> bool {
        self.admin_switch.lock().is_some_and(|(_, on)| on)
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
    /// Whether the admin tools are on right now: the daemon's live setting, which `alice mcp`
    /// asks the daemon for (off when none answers). Asked per request.
    async fn admin_tools(&self) -> bool {
        match self {
            Backend::Local(s) => s.settings().mcp.admin_tools,
            Backend::Remote(b) => b.admin_tools().await,
        }
    }
    /// [`admin_tools`](Self::admin_tools) without waiting for an answer (`get_tool` is not async).
    fn admin_tools_cached(&self) -> bool {
        match self {
            Backend::Local(s) => s.settings().mcp.admin_tools,
            Backend::Remote(b) => b.admin_tools_cached(),
        }
    }
    async fn reviews(&self, q: ReviewQuery) -> AliceResult<Vec<ReviewItem>> {
        match self {
            Backend::Local(s) => s.reviews(q).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.reviews(&q).await,
        }
    }
    async fn resolve_review(&self, r: ResolveRequest) -> AliceResult<ResolveResponse> {
        match self {
            Backend::Local(s) => s.resolve_review(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.resolve_review(&r).await,
        }
    }
    async fn merge(&self, r: MergeRequest) -> AliceResult<MergeResponse> {
        match self {
            Backend::Local(s) => s.merge(r).await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.merge(&r).await,
        }
    }
    async fn stats(&self) -> AliceResult<Stats> {
        match self {
            Backend::Local(s) => s.stats().await,
            Backend::Remote(b) => b.client(READ_WAIT).await?.stats().await,
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
            // Every tool is routed; the admin ones are listed and callable only while the
            // setting is on (see `admin_visible`).
            tool_router: Self::tool_router() + Self::admin_tool_router(),
            prompt_router: Self::prompt_router(),
            roots: Default::default(),
        }
    }

    /// Whether `tool` may be called now: every tool but the admin ones always; those only while
    /// `mcp.admin_tools` is on, read at each request so a change applies to connections that
    /// are already open.
    async fn tool_allowed(&self, tool: &str) -> bool {
        !ADMIN_TOOLS.contains(&tool) || self.backend.admin_tools().await
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
        description = "Search long-term memory: decisions, bug fixes, facts, how-tos, preferences and past work. Use before answering about anything done before. By default a configured model judge (judge.provider = jev) grades the top results, so recall is precise (slower, about half a second); pass mode \"fast\" to skip the judge, or mode \"precise\" to force it.",
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
        Parameters(mut req): Parameters<UpdateRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // The change is the client's: history records it as `agent:<client name>`.
        if req.author.agent.is_none() {
            req.author.agent = Self::agent(&ctx);
        }
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
        Parameters(mut req): Parameters<AssertFactRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if req.author.agent.is_none() {
            req.author.agent = Self::agent(&ctx);
        }
        Ok(match self.backend.assert_fact(req).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveArgs {
    /// The review item's id (from `review_list`).
    pub id: String,
    /// One of the item's `choices`.
    pub choice: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergeArgs {
    /// The memory that stays.
    pub survivor: String,
    /// Memories that become its duplicates.
    pub others: Vec<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[tool_router(router = admin_tool_router)]
impl AliceMcp {
    #[tool(
        description = "List review items Alice could not decide alone (duplicates, updates, contradictions), with their choices. Items span all projects.",
        annotations(read_only_hint = true)
    )]
    async fn review_list(
        &self,
        Parameters(q): Parameters<ReviewQuery>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.reviews(q).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }

    #[tool(description = "Resolve a review item with one of its choices.")]
    async fn review_resolve(
        &self,
        Parameters(a): Parameters<ResolveArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let author = Author {
            agent: Self::agent(&ctx),
            ..Default::default()
        };
        Ok(
            match self
                .backend
                .resolve_review(ResolveRequest {
                    id: a.id,
                    choice: a.choice,
                    author,
                })
                .await
            {
                Ok(r) => text(r.message),
                Err(e) => tool_err(e),
            },
        )
    }

    #[tool(
        description = "Merge duplicate memories into one survivor; the others are trashed and point to it."
    )]
    async fn merge_memories(
        &self,
        Parameters(a): Parameters<MergeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let author = Author {
            agent: Self::agent(&ctx),
            ..Default::default()
        };
        Ok(
            match self
                .backend
                .merge(MergeRequest {
                    survivor: a.survivor,
                    others: a.others,
                    author,
                })
                .await
            {
                Ok(r) => json(&r),
                Err(e) => tool_err(e),
            },
        )
    }

    #[tool(
        description = "Memory counts, review backlog, judge backlog and costs.",
        annotations(read_only_hint = true)
    )]
    async fn stats(&self, Parameters(_): Parameters<NoArgs>) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.stats().await {
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

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // Off: an admin tool answers as a tool that does not exist.
        if !self.tool_allowed(&request.name).await {
            return Err(McpError::invalid_params("tool not found", None));
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let cache_hints = context
            .protocol_version()
            .is_some_and(|v| v >= ProtocolVersion::V_2026_07_28);
        let admin = self.backend.admin_tools().await;
        let tools = self
            .tool_router
            .list_all()
            .into_iter()
            .filter(|t| admin || !ADMIN_TOOLS.contains(&t.name.as_ref()))
            .collect();
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: cache_hints.then_some(0),
            cache_scope: cache_hints.then_some(CacheScope::Public),
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if ADMIN_TOOLS.contains(&name) && !self.backend.admin_tools_cached() {
            return None;
        }
        self.tool_router.get(name).cloned()
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
