//! Direct MCP for server tenants, through the in-process Caddy emulator (spec §2.3, §2.8).
#![allow(deprecated)] // roots are deprecated by SEP-2577 but current clients still provide them

mod support;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    ListRootsResult, Root,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, ErrorData as McpError, RoleClient, ServiceExt};
use std::path::Path;
use std::time::Duration;
use support::caddy::Server;

/// An agent that may offer one workspace root.
#[derive(Clone)]
struct Agent(Option<String>);
impl ClientHandler for Agent {
    fn get_info(&self) -> ClientConfig {
        let caps = if self.0.is_some() {
            ClientCapabilities::builder().enable_roots().build()
        } else {
            ClientCapabilities::default()
        };
        ClientConfig::new(caps, Implementation::new("team-agent", "1.0"))
    }
    async fn list_roots(
        &self,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, McpError> {
        let root = self.0.clone().unwrap_or_default();
        Ok(ListRootsResult::new(vec![Root::new(root)]))
    }
}

type Conn = RunningService<RoleClient, Agent>;

async fn connect(url: &str, token: Option<&str>, root: Option<&Path>) -> Result<Conn, String> {
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if let Some(t) = token {
        cfg = cfg.auth_header(t.to_string());
    }
    let uri = root.map(|p| url::Url::from_directory_path(p).unwrap().to_string());
    tokio::time::timeout(
        Duration::from_secs(10),
        Agent(uri).serve(StreamableHttpClientTransport::from_config(cfg)),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|e| e.to_string())
}

async fn tool(c: &Conn, name: &'static str, args: serde_json::Value) -> CallToolResult {
    let a = args.as_object().unwrap().clone();
    c.call_tool(CallToolRequestParams::new(name).with_arguments(a))
        .await
        .unwrap()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

/// `(project slug, root_path)` of every stored memory, read from the tenant's database.
fn stored(home: &alice_daemon::paths::AliceHome) -> Vec<(String, Option<String>)> {
    let db = rusqlite::Connection::open_with_flags(
        home.db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut st = db
        .prepare(
            "SELECT p.slug, p.root_path FROM memories m JOIN projects p ON p.id = m.project_id",
        )
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// Whether any file under `dir` holds `needle`.
fn tree_contains(dir: &Path, needle: &str) -> bool {
    std::fs::read_dir(dir).unwrap().any(|e| {
        let p = e.unwrap().path();
        if p.is_dir() {
            tree_contains(&p, needle)
        } else {
            std::fs::read(&p)
                .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
                .unwrap_or(false)
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_with_the_bearer_token_recalls_and_remembers() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let url = format!("{}/mcp?project=scratch", s.url("ann"));
    let c = connect(&url, Some(&t.token), None).await.unwrap();
    let r = tool(
        &c,
        "remember",
        serde_json::json!({"content": "The staging deploy uses blue green switching", "kind": "fact"}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert!(text(&r).contains("scratch"), "{}", text(&r));
    let r = tool(
        &c,
        "recall",
        serde_json::json!({"query": "staging deploy blue green"}),
    )
    .await;
    assert!(text(&r).contains("blue green"), "{}", text(&r));
    let rows = stored(&t.home);
    assert_eq!(rows, vec![("scratch".to_string(), None)]);
    c.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_without_or_with_another_tenants_token_is_refused() {
    let s = Server::start(&["ann", "bob"]).await;
    let url = format!("{}/mcp?project=scratch", s.url("ann"));
    assert!(connect(&url, None, None).await.is_err());
    assert!(
        connect(&url, Some(&s.tenant("bob").token), None)
            .await
            .is_err()
    );
    // Nothing reached ann's store, and the emulator saw the attempts.
    assert!(stored(&s.tenant("ann").home).is_empty());
    assert!(!s.seen("ann").is_empty());
    assert!(s.seen("bob").is_empty());
    let http = reqwest::Client::new();
    let code = |req: reqwest::RequestBuilder| async move { req.send().await.unwrap().status() };
    let ann = format!("{}/api/v1/projects", s.url("ann"));
    assert_eq!(code(http.get(&ann)).await, 401);
    assert_eq!(
        code(http.get(&ann).bearer_auth(&s.tenant("bob").token)).await,
        401
    );
    assert_eq!(
        code(http.get(&ann).bearer_auth(&s.tenant("ann").token)).await,
        200
    );
    // The emulator routes only the API and MCP; admin and the rest are a 404.
    for p in ["/api/v1/admin/shutdown", "/other", "/"] {
        let r = http
            .get(format!("{}{p}", s.url("ann")))
            .bearer_auth(&s.tenant("ann").token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{p}");
        assert!(r.text().await.unwrap().contains("<html"), "{p}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_project_precedence() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let remember = |c: Conn, content: &'static str, args: serde_json::Value| async move {
        let mut a = args;
        a["content"] = content.into();
        let r = tool(&c, "remember", a).await;
        c.cancel().await.ok();
        assert_ne!(r.is_error, Some(true), "{}", text(&r));
        text(&r)
    };
    // The argument (an existing slug) beats ?project=.
    let c = connect(&format!("{base}/mcp?project=alpha"), Some(&t.token), None)
        .await
        .unwrap();
    remember(c, "alpha has the first note", serde_json::json!({})).await;
    let c = connect(&format!("{base}/mcp?project=beta"), Some(&t.token), None)
        .await
        .unwrap();
    let out = remember(
        c,
        "alpha also has the second note",
        serde_json::json!({"project": "alpha"}),
    )
    .await;
    assert!(out.contains("alpha"), "{out}");
    // ?project= beats a root.
    let root = Path::new("/srv/clientside/work/Root Repo");
    let c = connect(
        &format!("{base}/mcp?project=queried"),
        Some(&t.token),
        Some(root),
    )
    .await
    .unwrap();
    let out = remember(c, "the query wins over the root", serde_json::json!({})).await;
    assert!(out.contains("queried"), "{out}");
    // A root alone: its name.
    let c = connect(&format!("{base}/mcp"), Some(&t.token), Some(root))
        .await
        .unwrap();
    let out = remember(c, "the root names the project", serde_json::json!({})).await;
    assert!(out.contains("root-repo"), "{out}");
    // Neither: global.
    let c = connect(&format!("{base}/mcp"), Some(&t.token), None)
        .await
        .unwrap();
    let out = remember(c, "nothing names the project", serde_json::json!({})).await;
    assert!(out.contains("global"), "{out}");

    let mut slugs: Vec<String> = stored(&t.home).into_iter().map(|r| r.0).collect();
    slugs.sort();
    assert_eq!(slugs, ["alpha", "alpha", "global", "queried", "root-repo"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stores_no_client_path() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let root = Path::new("/home/dev/clientside-unique-dirname/app");
    let c = connect(&format!("{}/mcp", s.url("ann")), Some(&t.token), Some(root))
        .await
        .unwrap();
    let r = tool(
        &c,
        "remember",
        serde_json::json!({"content": "This note comes from a client with a private path"}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    c.cancel().await.ok();
    assert_eq!(stored(&t.home), vec![("app".to_string(), None)]);
    assert!(!tree_contains(t.home.root(), "clientside-unique-dirname"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stores_a_secret_redacted() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let secret = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let c = connect(
        &format!("{}/mcp?project=scratch", s.url("ann")),
        Some(&t.token),
        None,
    )
    .await
    .unwrap();
    let r = tool(
        &c,
        "remember",
        serde_json::json!({"content": format!("Deploys read the token {secret} from the vault")}),
    )
    .await;
    assert!(text(&r).starts_with("Saved "), "{}", text(&r));
    c.cancel().await.ok();
    // The memory is kept, so the absence below is redaction, not a refused write.
    assert!(tree_contains(t.home.root(), "Deploys read the token"));
    assert!(!tree_contains(t.home.root(), secret));
    assert!(!tree_contains(t.home.root(), &secret[4..]));
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_pass_the_emulator_unbuffered() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let mut resp = reqwest::Client::new()
        .get(format!("{}/api/v1/events/stream", s.url("ann")))
        .bearer_auth(&t.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    async fn next_data(resp: &mut reqwest::Response, within: Duration) -> Option<String> {
        tokio::time::timeout(within, async {
            loop {
                let chunk = resp.chunk().await.ok()??;
                let s = String::from_utf8_lossy(&chunk).into_owned();
                if s.contains("data:") {
                    return Some(s);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
    assert!(next_data(&mut resp, Duration::from_secs(2)).await.is_some());
    let c = connect(
        &format!("{}/mcp?project=scratch", s.url("ann")),
        Some(&t.token),
        None,
    )
    .await
    .unwrap();
    tool(
        &c,
        "remember",
        serde_json::json!({"content": "A note that makes the stream speak"}),
    )
    .await;
    assert!(next_data(&mut resp, Duration::from_secs(1)).await.is_some());
    c.cancel().await.ok();
}

/// Waits until the service's import job has ended.
async fn wait_import(
    svc: &std::sync::Arc<alice_daemon::service::Service>,
) -> alice_core::import::ImportStatus {
    let t = std::time::Instant::now();
    loop {
        let st = svc.import_status();
        if st.state != alice_core::import::ImportState::Running {
            return st;
        }
        assert!(t.elapsed() < Duration::from_secs(300), "import did not end");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_import_matches_a_local_import() {
    use alice_core::embed::{Embedder, FakeEmbedder};
    use alice_core::import::{ImportRequest, ImportState};
    use alice_daemon::paths::AliceHome;
    use alice_daemon::service::{Service, ServiceOptions};
    use std::sync::Arc;

    let src = tempfile::tempdir().unwrap();
    let palace = alice_core::import::fixture::synthetic_palace(&src.path().join("palace"), 600);
    let start = |home: &Path, tenant: bool| {
        Service::start(
            AliceHome::new(home.to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions {
                tenant,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let local = start(&a.path().join("alice"), false);
    let tenant = start(&b.path().join("alice"), true);

    local
        .start_import(ImportRequest {
            palace: Some(palace.dir.display().to_string()),
            ..Default::default()
        })
        .unwrap();
    let inbox = b.path().join("alice/import-inbox/j1/palace");
    copy_tree(&palace.dir, &inbox);
    tenant
        .start_import(ImportRequest {
            palace: Some(inbox.display().to_string()),
            ..Default::default()
        })
        .unwrap();
    let (l, t) = (wait_import(&local).await, wait_import(&tenant).await);
    assert_eq!(l.state, ImportState::Done, "{:?}", l.error);
    assert_eq!(t.state, ImportState::Done, "{:?}", t.error);
    let (l, t) = (l.report.unwrap(), t.report.unwrap());
    assert!(l.drawers_seen >= 600);
    assert_eq!(l.tiers, t.tiers);
    assert_eq!(l.projects, t.projects);
    assert_eq!(l.duplicates, t.duplicates);
    assert_eq!(l.facts, t.facts);
    assert_eq!(l.skipped, t.skipped);
    assert_eq!(l.drawers_seen, t.drawers_seen);
    assert_eq!(l.drawers_accounted, t.drawers_accounted);
}
