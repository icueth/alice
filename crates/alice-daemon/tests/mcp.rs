#![allow(deprecated)] // roots are deprecated by SEP-2577 but current clients still provide them

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_daemon::client::Launcher;
use alice_daemon::discovery::{DaemonInfo, write_info};
use alice_daemon::mcp::{AliceMcp, Backend, INSTRUCTIONS, RemoteBackend};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, GetPromptRequestParams,
    Implementation, ListRootsResult, ProtocolVersion, ReadResourceRequestParams, Root,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, ErrorData as McpError, RoleClient,
    ServiceExt,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct TestClient;
impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("test-agent", "1.0"),
        )
    }
}

/// A client that offers one workspace root, as editors do.
#[derive(Clone)]
struct RootsClient(String);
impl ClientHandler for RootsClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::builder().enable_roots().build(),
            Implementation::new("roots-agent", "1.0"),
        )
    }
    async fn list_roots(
        &self,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, McpError> {
        Ok(ListRootsResult::new(vec![Root::new(self.0.clone())]))
    }
}

fn args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

fn call(name: &'static str, v: serde_json::Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name).with_arguments(args(v))
}

fn first_text(r: &rmcp::model::CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

/// An in-process daemon published like `daemon::run` does it; `stop` ends it.
struct Daemon {
    port: u16,
    token: String,
    svc: Arc<Service>,
    stop: CancellationToken,
    served: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start(home: &AliceHome) -> Daemon {
        alice_daemon::install_tls();
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions::default(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = alice_daemon::discovery::load_or_create_token(home).unwrap();
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
        let app = alice_daemon::http::router(svc.clone(), token.clone(), port, stop.clone());
        let ct = stop.clone();
        let served = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct.cancelled().await })
                .await
                .unwrap()
        });
        Daemon {
            port,
            token,
            svc,
            stop,
            served,
        }
    }

    /// Stop serving and close the store, as a stopping daemon does.
    async fn stop(self) {
        self.stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.served).await;
        let svc = self.svc;
        tokio::task::spawn_blocking(move || svc.shutdown())
            .await
            .unwrap();
    }

    fn mcp_url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}

async fn daemon() -> (tempfile::TempDir, AliceHome, Daemon) {
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().to_path_buf());
    let daemon = Daemon::start(&home).await;
    (d, home, daemon)
}

fn http_transport(d: &Daemon) -> StreamableHttpClientTransport<reqwest::Client> {
    let cfg =
        StreamableHttpClientTransportConfig::with_uri(d.mcp_url()).auth_header(d.token.clone());
    StreamableHttpClientTransport::from_config(cfg)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_mcp_exposes_exactly_seven_tools_and_works() {
    let (_d, _home, d) = daemon().await;
    let client = TestClient.serve(http_transport(&d)).await.unwrap();
    let info = client.peer_info().unwrap();
    assert!(
        info.instructions
            .as_deref()
            .unwrap()
            .contains("long-term memory")
    );
    assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));

    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "assert_fact",
            "context",
            "get_memory",
            "query_facts",
            "recall",
            "remember",
            "update_memory"
        ]
    );
    for t in &tools {
        let read_only = t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true);
        let expected = matches!(
            t.name.as_ref(),
            "recall" | "context" | "get_memory" | "query_facts"
        );
        assert_eq!(read_only, expected, "read-only hint of {}", t.name);
        let props = t
            .input_schema
            .get("properties")
            .cloned()
            .unwrap_or_default();
        assert!(
            props.get("cwd").is_none() && props.get("author").is_none(),
            "{} exposes daemon-filled fields",
            t.name
        );
    }

    let saved = client
        .call_tool(call(
            "remember",
            serde_json::json!({"content": "MCP over HTTP stores memories", "kind": "fact"}),
        ))
        .await
        .unwrap();
    assert!(
        first_text(&saved).starts_with("Saved "),
        "{}",
        first_text(&saved)
    );
    let id = first_text(&saved)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    let again = client
        .call_tool(call(
            "remember",
            serde_json::json!({"content": "MCP over HTTP stores memories", "kind": "fact"}),
        ))
        .await
        .unwrap();
    assert!(
        first_text(&again).starts_with(&format!("Already remembered as {id}")),
        "{}",
        first_text(&again)
    );
    let found = client
        .call_tool(call(
            "recall",
            serde_json::json!({"query": "MCP over HTTP"}),
        ))
        .await
        .unwrap();
    assert!(first_text(&found).contains("MCP over HTTP stores memories"));
    let found = client
        .call_tool(call(
            "recall",
            serde_json::json!({"query": "MCP over HTTP", "format": "json"}),
        ))
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&first_text(&found)).unwrap();
    assert_eq!(json["results"][0]["id"], id.as_str());

    let bad = client
        .call_tool(call("remember", serde_json::json!({"content": "  "})))
        .await
        .unwrap();
    assert_eq!(bad.is_error, Some(true));
    assert!(
        first_text(&bad).contains("content is empty"),
        "{}",
        first_text(&bad)
    );
    let missing = client
        .call_tool(call(
            "update_memory",
            serde_json::json!({"id": "01K7NOSUCHMEMORY0000000000", "action": "archive"}),
        ))
        .await
        .unwrap();
    assert_eq!(missing.is_error, Some(true));

    let mut prompts: Vec<String> = client
        .list_all_prompts()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    prompts.sort();
    assert_eq!(prompts, ["recall", "remember-session"]);
    let p = client
        .get_prompt(GetPromptRequestParams::new("remember-session"))
        .await
        .unwrap();
    assert!(!p.messages.is_empty());
    let p = client
        .get_prompt(
            GetPromptRequestParams::new("recall")
                .with_arguments(args(serde_json::json!({"query": "the lease"}))),
        )
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&p.messages)
            .unwrap()
            .contains("the lease")
    );

    let resources: Vec<String> = client
        .list_all_resources()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.uri.clone())
        .collect();
    assert_eq!(resources, ["memory://context"]);
    let ctx = client
        .read_resource(ReadResourceRequestParams::new("memory://context"))
        .await
        .unwrap();
    assert!(!ctx.contents.is_empty());
    let m = client
        .read_resource(ReadResourceRequestParams::new(format!("memory://m/{id}")))
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&m.contents)
            .unwrap()
            .contains("MCP over HTTP stores memories")
    );
    let e = client
        .read_resource(ReadResourceRequestParams::new(
            "memory://m/01K7NOSUCHMEMORY0000000000",
        ))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("01K7NOSUCHMEMORY0000000000"), "{e}");
    assert!(
        client
            .read_resource(ReadResourceRequestParams::new("memory://elsewhere"))
            .await
            .is_err()
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_without_token_is_rejected() {
    let (_d, _home, d) = daemon().await;
    let r = reqwest::Client::new()
        .post(d.mcp_url())
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_style_remote_backend_uses_the_daemon_and_its_cwd() {
    let (dir, home, _d) = daemon().await;
    let repo = dir.path().join("shimproj");
    std::fs::create_dir(&repo).unwrap();
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(
        home,
        Launcher::None,
        Some(repo.to_string_lossy().into_owned()),
    ))));
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    let client = TestClient.serve(b).await.unwrap();
    let saved = client
        .call_tool(call(
            "remember",
            serde_json::json!({"content": "remote backend writes"}),
        ))
        .await
        .unwrap();
    assert!(
        first_text(&saved).contains("in shimproj"),
        "{}",
        first_text(&saved)
    );
    let found = client
        .call_tool(call(
            "recall",
            serde_json::json!({"query": "remote backend writes"}),
        ))
        .await
        .unwrap();
    assert!(
        first_text(&found).contains("remote backend writes"),
        "{}",
        first_text(&found)
    );
    let ctx = client
        .call_tool(call("context", serde_json::json!({})))
        .await
        .unwrap();
    assert_ne!(ctx.is_error, Some(true), "{}", first_text(&ctx));
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_backend_spools_when_no_daemon() {
    alice_daemon::install_tls();
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().to_path_buf());
    home.ensure_dirs().unwrap();
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(
        home.clone(),
        Launcher::None,
        None,
    ))));
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    let client = TestClient.serve(b).await.unwrap();
    let r = client
        .call_tool(call(
            "remember",
            serde_json::json!({"content": "saved later"}),
        ))
        .await
        .unwrap();
    let queued = first_text(&r);
    assert!(queued.starts_with("Queued"), "{queued}");
    // Deferred item #93: it says why it was queued, and "starting" only when it is.
    assert!(
        queued.contains("not running") && !queued.contains("starting"),
        "{queued}"
    );
    assert_eq!(alice_daemon::spool::pending_count(&home.spool_dir()), 1);
    // Reads cannot be queued: they say what to do.
    let r = client
        .call_tool(call("recall", serde_json::json!({"query": "anything"})))
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert!(
        first_text(&r).contains("alice daemon start"),
        "{}",
        first_text(&r)
    );
    client.cancel().await.unwrap();
}

/// `alice mcp` start-up (< 20 ms budget) must not wait for, look for or start the daemon:
/// the handshake and the listings answer from the shim alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_stdio_server_answers_the_handshake_without_touching_the_daemon() {
    alice_daemon::install_tls();
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().to_path_buf());
    let launcher = Launcher::Exe(d.path().join("would-start-a-daemon"));
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(
        home.clone(),
        launcher,
        None,
    ))));
    let (a, b) = tokio::io::duplex(1 << 16);
    let started = Instant::now();
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    let client = TestClient.serve(b).await.unwrap();
    let handshake = started.elapsed();
    assert_eq!(client.list_all_tools().await.unwrap().len(), 7);
    assert_eq!(client.list_all_prompts().await.unwrap().len(), 2);
    assert_eq!(client.list_all_resources().await.unwrap().len(), 1);
    eprintln!("in-process handshake took {handshake:?}");
    assert!(handshake < Duration::from_secs(2), "{handshake:?}");
    assert!(
        !home.logs_dir().join("daemon.log").exists(),
        "a daemon start was attempted before any tool call"
    );
    client.cancel().await.unwrap();
}

/// A daemon restart (new port, new instance) between two tool calls is followed, not spooled.
#[tokio::test(flavor = "multi_thread")]
async fn remote_backend_follows_a_restarted_daemon() {
    let (dir, home, first) = daemon().await;
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(
        home.clone(),
        Launcher::None,
        Some(dir.path().to_string_lossy().into_owned()),
    ))));
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    let client = TestClient.serve(b).await.unwrap();
    let r = client
        .call_tool(call("recall", serde_json::json!({"query": "nothing yet"})))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{}", first_text(&r));
    first.stop().await;
    let second = Daemon::start(&home).await;
    let saved = client
        .call_tool(call(
            "remember",
            serde_json::json!({"content": "written after the restart"}),
        ))
        .await
        .unwrap();
    assert!(
        first_text(&saved).starts_with("Saved "),
        "{}",
        first_text(&saved)
    );
    let found = client
        .call_tool(call(
            "recall",
            serde_json::json!({"query": "written after the restart"}),
        ))
        .await
        .unwrap();
    assert!(
        first_text(&found).contains("written after the restart"),
        "{}",
        first_text(&found)
    );
    assert_eq!(alice_daemon::spool::pending_count(&home.spool_dir()), 0);
    client.cancel().await.unwrap();
    second.stop().await;
}

/// Over HTTP the daemon has no working directory of its own: the client's first root picks the project.
#[tokio::test(flavor = "multi_thread")]
async fn http_clients_first_root_chooses_the_project() {
    let (dir, _home, d) = daemon().await;
    let repo = dir.path().join("rootproj");
    std::fs::create_dir(&repo).unwrap();
    let root = url::Url::from_directory_path(&repo).unwrap().to_string();
    let client = RootsClient(root).serve(http_transport(&d)).await.unwrap();
    let saved = client
        .call_tool(call(
            "remember",
            serde_json::json!({"content": "rooted memory"}),
        ))
        .await
        .unwrap();
    assert!(
        first_text(&saved).contains("in rootproj"),
        "{}",
        first_text(&saved)
    );
    client.cancel().await.unwrap();
}

/// The writing agent is recorded both for session clients (initialize) and for clients on
/// the 2026-07-28 per-request protocol, whose identity travels in each request's `_meta`.
#[tokio::test(flavor = "multi_thread")]
async fn the_writing_agent_is_recorded_for_session_and_per_request_clients() {
    let (_dir, _home, d) = daemon().await;
    let session = TestClient.serve(http_transport(&d)).await.unwrap();
    let per_request = TestClient
        .serve_with_lifecycle(
            http_transport(&d),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    for (client, content) in [
        (&session, "written in a session"),
        (&per_request, "written per request"),
    ] {
        let saved = client
            .call_tool(call("remember", serde_json::json!({"content": content})))
            .await
            .unwrap();
        assert!(
            first_text(&saved).starts_with("Saved "),
            "{}",
            first_text(&saved)
        );
        let id = first_text(&saved)
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_string();
        let got = client
            .call_tool(call("get_memory", serde_json::json!({"ids": [id]})))
            .await
            .unwrap();
        let got: serde_json::Value = serde_json::from_str(&first_text(&got)).unwrap();
        assert_eq!(
            got["memories"][0]["memory"]["author"]["agent"], "test-agent",
            "{content}: {got}"
        );
    }
    session.cancel().await.unwrap();
    per_request.cancel().await.unwrap();
}

// ---- Spec §8.1: the client's workspace (first root) decides the project, then the shim's cwd.

/// A client offering roots whose first root can change; it counts `roots/list` requests and
/// leaves the first `hang_first` of them unanswered.
#[derive(Clone)]
struct WorkspaceClient {
    root: Arc<std::sync::Mutex<String>>,
    asked: Arc<std::sync::atomic::AtomicUsize>,
    hang_first: usize,
    /// When set, the first answer is the root at the time of asking, sent once this is notified.
    hold_first: Option<Arc<tokio::sync::Notify>>,
}

impl WorkspaceClient {
    fn new(dir: &std::path::Path, hang_first: usize) -> Self {
        Self {
            root: Arc::new(std::sync::Mutex::new(dir_uri(dir))),
            asked: Arc::default(),
            hang_first,
            hold_first: None,
        }
    }
    fn move_to(&self, dir: &std::path::Path) {
        *self.root.lock().unwrap() = dir_uri(dir);
    }
    fn asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ClientHandler for WorkspaceClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::builder().enable_roots().build(),
            Implementation::new("workspace-agent", "1.0"),
        )
    }
    async fn list_roots(
        &self,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, McpError> {
        let n = self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < self.hang_first {
            std::future::pending::<()>().await;
        }
        let root = self.root.lock().unwrap().clone();
        if n == 0
            && let Some(gate) = &self.hold_first
        {
            gate.notified().await;
        }
        Ok(ListRootsResult::new(vec![Root::new(root)]))
    }
}

fn dir_uri(dir: &std::path::Path) -> String {
    url::Url::from_directory_path(dir).unwrap().to_string()
}

fn mkdir(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
    let d = parent.join(name);
    std::fs::create_dir(&d).unwrap();
    d
}

/// A stdio shim (cwd `shimside`) serving `client` over an in-memory pipe.
async fn shim<C: ClientHandler>(
    home: &AliceHome,
    shim_cwd: &std::path::Path,
    client: C,
) -> rmcp::service::RunningService<RoleClient, C> {
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(
        home.clone(),
        Launcher::None,
        Some(shim_cwd.to_string_lossy().into_owned()),
    ))));
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    client.serve(b).await.unwrap()
}

async fn remember_text<C: ClientHandler>(
    client: &rmcp::service::RunningService<RoleClient, C>,
    content: &str,
) -> String {
    let r = client
        .call_tool(call("remember", serde_json::json!({"content": content})))
        .await
        .unwrap();
    first_text(&r)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_clients_first_root_wins_over_the_shim_cwd_and_follows_list_changed() {
    let (dir, home, _d) = daemon().await;
    let shim_cwd = mkdir(dir.path(), "shimside");
    let workspace = mkdir(dir.path(), "rootside");
    let later = mkdir(dir.path(), "laterside");
    let agent = WorkspaceClient::new(&workspace, 0);
    let client = shim(&home, &shim_cwd, agent.clone()).await;

    let saved = remember_text(&client, "filed under the workspace").await;
    assert!(saved.contains("in rootside"), "{saved}");
    let saved = remember_text(&client, "filed under the workspace again").await;
    assert!(saved.contains("in rootside"), "{saved}");
    assert_eq!(
        agent.asked(),
        1,
        "the first root is cached for the connection"
    );

    // The editor switches workspace and says so.
    agent.move_to(&later);
    client.notify_roots_list_changed().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ctx = client
            .call_tool(call("context", serde_json::json!({})))
            .await
            .unwrap();
        if first_text(&ctx).starts_with("# Context · laterside") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never followed: {}",
            first_text(&ctx)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let saved = remember_text(&client, "filed under the new workspace").await;
    assert!(saved.contains("in laterside"), "{saved}");
    assert_eq!(
        agent.asked(),
        2,
        "asked once more after list_changed, then cached"
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_without_roots_uses_the_shim_cwd_and_is_recorded_as_author() {
    let (dir, home, _d) = daemon().await;
    let shim_cwd = mkdir(dir.path(), "shimside");
    let client = shim(&home, &shim_cwd, TestClient).await;
    let saved = remember_text(&client, "filed under the shim directory").await;
    assert!(saved.contains("in shimside"), "{saved}");
    let id = saved.split_whitespace().nth(1).unwrap().to_string();
    let got = client
        .call_tool(call("get_memory", serde_json::json!({"ids": [id]})))
        .await
        .unwrap();
    let got: serde_json::Value = serde_json::from_str(&first_text(&got)).unwrap();
    assert_eq!(
        got["memories"][0]["memory"]["author"]["agent"], "test-agent",
        "{got}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_never_answers_roots_falls_back_to_the_shim_cwd_in_time() {
    let (dir, home, _d) = daemon().await;
    let shim_cwd = mkdir(dir.path(), "shimside");
    let workspace = mkdir(dir.path(), "rootside");
    let agent = WorkspaceClient::new(&workspace, 1);
    let client = shim(&home, &shim_cwd, agent.clone()).await;

    let t = Instant::now();
    let saved = remember_text(&client, "written while the client is silent").await;
    let took = t.elapsed();
    assert!(saved.contains("in shimside"), "{saved}");
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(2),
        "{took:?}"
    );
    // The failure is not cached: the next request asks again and gets the workspace.
    let saved = remember_text(&client, "written once the client answers").await;
    assert!(saved.contains("in rootside"), "{saved}");
    assert_eq!(agent.asked(), 2);
    client.cancel().await.unwrap();
}

/// The root is per connection: two HTTP sessions on one daemon keep their own workspaces.
#[tokio::test(flavor = "multi_thread")]
async fn each_http_session_keeps_its_own_root() {
    let (dir, _home, d) = daemon().await;
    let one = mkdir(dir.path(), "sessionone");
    let two = mkdir(dir.path(), "sessiontwo");
    let first = WorkspaceClient::new(&one, 0)
        .serve(http_transport(&d))
        .await
        .unwrap();
    let second = WorkspaceClient::new(&two, 0)
        .serve(http_transport(&d))
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(
            remember_text(
                &first,
                &format!("one {}", Instant::now().elapsed().as_nanos())
            )
            .await
            .contains("in sessionone")
        );
        assert!(
            remember_text(
                &second,
                &format!("two {}", Instant::now().elapsed().as_nanos())
            )
            .await
            .contains("in sessiontwo")
        );
    }
    first.cancel().await.unwrap();
    second.cancel().await.unwrap();
}

/// An answer the client gave for its old workspace, arriving after it announced a new one,
/// serves that one request but is not kept for later ones.
#[tokio::test(flavor = "multi_thread")]
async fn a_root_answer_that_predates_list_changed_is_not_kept() {
    let (dir, home, _d) = daemon().await;
    let shim_cwd = mkdir(dir.path(), "shimside");
    let old = mkdir(dir.path(), "oldside");
    let new = mkdir(dir.path(), "newside");
    let gate = Arc::new(tokio::sync::Notify::new());
    let agent = WorkspaceClient {
        hold_first: Some(gate.clone()),
        ..WorkspaceClient::new(&old, 0)
    };
    let client = shim(&home, &shim_cwd, agent.clone()).await;
    let peer = client.peer().clone();
    let first = tokio::spawn(async move {
        peer.call_tool(call(
            "remember",
            serde_json::json!({"content": "asked before the switch"}),
        ))
        .await
        .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while agent.asked() == 0 {
        assert!(
            Instant::now() < deadline,
            "the server never asked for roots"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    agent.move_to(&new);
    client.notify_roots_list_changed().await.unwrap();
    // Let the server take the notification before the stale answer arrives (if it is slower,
    // the stale answer is cached and then cleared, which passes too).
    tokio::time::sleep(Duration::from_millis(100)).await;
    gate.notify_one();
    let r = first.await.unwrap();
    // The old root, or the shim's cwd if a loaded machine let the 300 ms roots timeout pass.
    assert!(
        first_text(&r).contains("in oldside") || first_text(&r).contains("in shimside"),
        "{}",
        first_text(&r)
    );
    let saved = remember_text(&client, "asked after the switch").await;
    assert!(saved.contains("in newside"), "{saved}");
    assert_eq!(agent.asked(), 2);
    client.cancel().await.unwrap();
}
