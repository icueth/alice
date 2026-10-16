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
    connect_with_header(url, token, root, None).await
}

/// `connect`, sending `X-Alice-Project: <project>` on every request when one is given (what
/// Claude Code's `headersHelper` does).
async fn connect_with_header(
    url: &str,
    token: Option<&str>,
    root: Option<&Path>,
    project: Option<&str>,
) -> Result<Conn, String> {
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if let Some(t) = token {
        cfg = cfg.auth_header(t.to_string());
    }
    if let Some(p) = project {
        cfg.custom_headers.insert(
            axum::http::HeaderName::from_static("x-alice-project"),
            axum::http::HeaderValue::from_str(p).unwrap(),
        );
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
    // The X-Alice-Project header beats a root; ?project= beats the header.
    let c = connect_with_header(
        &format!("{base}/mcp"),
        Some(&t.token),
        Some(root),
        Some("Header App"),
    )
    .await
    .unwrap();
    let out = remember(c, "the header wins over the root", serde_json::json!({})).await;
    assert!(out.contains("header-app"), "{out}");
    let c = connect_with_header(
        &format!("{base}/mcp?project=queried"),
        Some(&t.token),
        None,
        Some("Header App"),
    )
    .await
    .unwrap();
    let out = remember(c, "the query wins over the header", serde_json::json!({})).await;
    assert!(out.contains("queried"), "{out}");
    // Over the limit, the header fails the call as the query does.
    let long = "h".repeat(250);
    let c = connect_with_header(&format!("{base}/mcp"), Some(&t.token), None, Some(&long))
        .await
        .unwrap();
    let args = serde_json::json!({"content": "never stored"});
    let e = c
        .call_tool(
            CallToolRequestParams::new("remember")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap_err()
        .to_string();
    c.cancel().await.ok();
    assert!(e.contains("the X-Alice-Project header is too long"), "{e}");

    let mut slugs: Vec<String> = stored(&t.home).into_iter().map(|r| r.0).collect();
    slugs.sort();
    assert_eq!(
        slugs,
        [
            "alpha",
            "alpha",
            "global",
            "header-app",
            "queried",
            "queried",
            "root-repo"
        ]
    );
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

/// A hint can never come from the wire: a `project_hint` in a JSON body is ignored, so the
/// memory lands in `global` and no project of that name exists.
#[tokio::test(flavor = "multi_thread")]
async fn the_http_api_ignores_a_project_hint_in_the_body() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/remember", s.url("ann")))
        .bearer_auth(&t.token)
        .json(&serde_json::json!({
            "content": "A note whose body tries to name its project",
            "project_hint": "sneaky"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let rows = stored(&t.home);
    assert_eq!(rows, vec![("global".to_string(), None)]);
}

/// The team store has no route behind the emulator; a tenant's settings show neither its URL
/// nor its token, and no `team.*` key can be set.
#[tokio::test(flavor = "multi_thread")]
async fn the_team_store_is_unrouted_and_hidden_from_the_settings() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let team = s.team.as_ref().unwrap();
    let http = reqwest::Client::new();
    for path in ["/u/team/api/v1/ping", "/u/team/api/v1/health"] {
        let r = http
            .get(format!("{}{path}", s.base))
            .bearer_auth(&team.token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{path}");
    }
    // Both personal daemons linked to it: the link is verified and healthy.
    for name in ["ann", "bob"] {
        let t = s.tenant(name);
        let link = t.svc.team().expect("the link is built");
        link.ensure_verified().await.unwrap();
        assert!(t.svc.health().await.unwrap().team_warning.is_none());
        assert_eq!(t.svc.tenant_name(), Some(name));
        assert!(!format!("{link:?}").contains(&team.token));
    }
    let t = s.tenant("ann");
    let body = http
        .get(format!("{}/api/v1/settings", s.url("ann")))
        .bearer_auth(&t.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!body.contains(&team.token), "{body}");
    assert!(!body.contains(&team.port.to_string()), "{body}");
    assert!(!body.contains("team.token"), "{body}");
    let health = http
        .get(format!("{}/api/v1/health", s.url("ann")))
        .bearer_auth(&t.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!health.contains(&team.token), "{health}");
    for key in ["team.url", "team.token", "team.enabled"] {
        let r = http
            .patch(format!("{}/api/v1/settings", s.url("ann")))
            .bearer_auth(&t.token)
            .json(&serde_json::json!({ key: "x" }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_client_error(), "{key}: {}", r.status());
    }
}

/// Only the team store keeps a project hint that comes in an HTTP body.
#[tokio::test(flavor = "multi_thread")]
async fn a_team_store_keeps_the_project_hint_of_a_body_and_a_tenant_clears_it() {
    let s = Server::start_with_team(&["ann"]).await;
    let team = s.team.as_ref().unwrap();
    let http = reqwest::Client::new();
    let body = serde_json::json!({
        "content": "A note naming its project in the body",
        "project_hint": "shared-proj"
    });
    let r = http
        .post(format!("http://127.0.0.1:{}/api/v1/remember", team.port))
        .bearer_auth(&team.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let slugs: Vec<String> = stored(&team.home).into_iter().map(|(s, _)| s).collect();
    assert_eq!(slugs, vec!["shared-proj".to_string()]);
    let t = s.tenant("ann");
    let r = http
        .post(format!("{}/api/v1/remember", s.url("ann")))
        .bearer_auth(&t.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let slugs: Vec<String> = stored(&t.home).into_iter().map(|(s, _)| s).collect();
    assert_eq!(slugs, vec!["global".to_string()]);
}

/// The outcome of a tool call that may fail either as an MCP error or as an error result:
/// `Err(message)` for a failure, `Ok(text)` otherwise.
async fn try_tool(c: &Conn, name: &'static str, args: serde_json::Value) -> Result<String, String> {
    let a = args.as_object().unwrap().clone();
    match c
        .call_tool(CallToolRequestParams::new(name).with_arguments(a))
        .await
    {
        Ok(r) if r.is_error == Some(true) => Err(text(&r)),
        Ok(r) => Ok(text(&r)),
        Err(e) => Err(e.to_string()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_query_over_the_limits_is_refused_and_nothing_is_stored() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let base = s.url("ann");
    let long = "q".repeat(101);
    let c = connect(&format!("{base}/mcp?project={long}"), Some(&t.token), None)
        .await
        .unwrap();
    for name in ["remember", "recall", "context"] {
        let args = match name {
            "remember" => serde_json::json!({"content": "too long a project name"}),
            "recall" => serde_json::json!({"query": "anything at all"}),
            _ => serde_json::json!({}),
        };
        let e = try_tool(&c, name, args).await.unwrap_err();
        assert!(e.contains("100") || e.contains("200"), "{name}: {e}");
        assert!(!e.contains(&long), "{name} echoed the value: {e}");
    }
    c.cancel().await.ok();
    assert!(stored(&t.home).is_empty());
    // Exactly at the limit works.
    let ok = "r".repeat(100);
    let c = connect(&format!("{base}/mcp?project={ok}"), Some(&t.token), None)
        .await
        .unwrap();
    let out = try_tool(
        &c,
        "remember",
        serde_json::json!({"content": "a name at the limit"}),
    )
    .await
    .unwrap();
    assert!(out.contains(&ok), "{out}");
    c.cancel().await.ok();
    assert_eq!(stored(&t.home), vec![(ok, None)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_root_basename_over_the_limits_is_dropped_not_an_error() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let root = format!("/srv/clientside/work/{}", "d".repeat(150));
    let c = connect(
        &format!("{}/mcp", s.url("ann")),
        Some(&t.token),
        Some(Path::new(&root)),
    )
    .await
    .unwrap();
    let out = try_tool(
        &c,
        "remember",
        serde_json::json!({"content": "a root with a very long name"}),
    )
    .await
    .unwrap();
    assert!(out.contains("global"), "{out}");
    c.cancel().await.ok();
    assert_eq!(stored(&t.home), vec![("global".to_string(), None)]);
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
                // An accept test must not depend on the host's free space.
                free_space: Some(Arc::new(|_| Some(u64::MAX / 4))),
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

// ---------------------------------------------------------------------------------------
// Shared writes and the rules for changing shared memories (spec §2.10, R34).
// ---------------------------------------------------------------------------------------

use alice_core::api::{GetRequest, RememberRequest, RememberStatus, UndoRequest, UpdateRequest};
use alice_core::model::Author;
use alice_core::store::memories::{MemoryUpdate, UpdateAction};

const SHARED_REFUSAL: &str = "only the author or the admin can change a shared memory";

fn shared(content: &str, agent: Option<&str>) -> RememberRequest {
    RememberRequest {
        content: content.into(),
        shared: true,
        author: Author {
            agent: agent.map(Into::into),
            model: Some("m1".into()),
            session: Some("s1".into()),
        },
        ..Default::default()
    }
}

fn edit(id: &str, body: &str, agent: &str) -> UpdateRequest {
    UpdateRequest {
        id: id.into(),
        update: MemoryUpdate {
            action: UpdateAction::Edit,
            body: Some(body.into()),
            ..Default::default()
        },
        author: Author {
            agent: Some(agent.into()),
            ..Default::default()
        },
    }
}

/// The author of a memory as the team store holds it.
async fn team_author(s: &Server, team_id: &str) -> Author {
    let id = team_id.strip_prefix("team:").unwrap();
    let got = s
        .team
        .as_ref()
        .unwrap()
        .svc
        .get(GetRequest {
            ids: vec![id.into()],
            include_provenance: false,
        })
        .await
        .unwrap();
    got.memories[0].memory.author.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_remember_lands_in_the_team_store_with_its_tenant_as_author_and_not_in_the_personal_one()
 {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let ann = s.tenant("ann");
    let mut req = shared("The release train leaves on Fridays", Some("cursor"));
    req.project_hint = Some("shared-proj".into());
    let r = ann.svc.remember_or_spool(req).await.unwrap();
    assert_eq!(r.status, RememberStatus::New);
    assert!(r.shared);
    let id = r.id.clone().unwrap();
    assert!(id.starts_with("team:"), "{id}");
    assert_eq!(r.project, "shared-proj");
    let a = team_author(&s, &id).await;
    assert_eq!(a.agent.as_deref(), Some("ann/cursor"));
    assert_eq!(a.session, None);
    assert_eq!(
        stored(&s.team.as_ref().unwrap().home),
        vec![("shared-proj".to_string(), None)]
    );
    assert!(stored(&ann.home).is_empty(), "no personal copy");
    assert!(stored(&s.tenant("bob").home).is_empty());
    // A client that names no agent is the tenant alone.
    let r = ann
        .svc
        .remember(shared("A second shared note", None))
        .await
        .unwrap();
    assert_eq!(
        team_author(&s, &r.id.unwrap()).await.agent.as_deref(),
        Some("ann")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_sent_author_on_a_shared_write_is_replaced() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let r = s
        .tenant("ann")
        .svc
        .remember(shared("Claims to be bob", Some("bob")))
        .await
        .unwrap();
    let a = team_author(&s, &r.id.unwrap()).await;
    // Whatever the client says, the tenant part is the caller's own.
    assert_eq!(a.agent.as_deref(), Some("ann/bob"));
    assert_eq!(a.model.as_deref(), Some("m1"));
    assert_eq!(a.session, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_remember_without_a_team_link_is_a_clear_invalid() {
    let s = Server::start(&["solo"]).await;
    let e = s
        .tenant("solo")
        .svc
        .remember_or_spool(shared("Nowhere to go", Some("cursor")))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, alice_core::error::AliceError::Invalid(m)
            if m == "no team memory is configured on this server; ask the admin"),
        "{e:?}"
    );
    assert!(stored(&s.tenant("solo").home).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_remember_with_the_team_store_down_fails_and_spools_nothing() {
    let s = Server::start_with_team(&["ann"]).await;
    let ann = s.tenant("ann");
    ann.svc.team().unwrap().ensure_verified().await.unwrap();
    s.stop_team().await;
    let e = ann
        .svc
        .remember_or_spool(shared("Team is away", Some("cursor")))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("the team memory is unreachable"), "{e}");
    assert!(e.contains("nothing was saved"), "{e}");
    assert!(e.contains("save it personally without shared"), "{e}");
    let team = s.team.as_ref().unwrap();
    assert!(!e.contains(&team.token), "{e}");
    assert!(!e.contains(&team.port.to_string()), "{e}");
    let spooled = std::fs::read_dir(ann.home.spool_dir())
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(spooled, 0, "a shared write is never spooled");
    assert!(stored(&ann.home).is_empty(), "and never saved personally");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_author_updates_a_shared_memory_and_anyone_reads_it() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let (ann, bob) = (&s.tenant("ann").svc, &s.tenant("bob").svc);
    let id = ann
        .remember(shared("Original shared text", Some("cursor")))
        .await
        .unwrap()
        .id
        .unwrap();
    // Anyone reads it, with the team prefix, its source and its author's tenant.
    let got = bob
        .get(GetRequest {
            ids: vec![id.clone()],
            include_provenance: false,
        })
        .await
        .unwrap();
    assert!(got.missing.is_empty(), "{:?}", got.missing);
    assert_eq!(got.memories[0].memory.id, id);
    assert_eq!(got.memories[0].source.as_deref(), Some("team"));
    assert_eq!(got.memories[0].author_name.as_deref(), Some("ann"));
    // Bob cannot change it.
    let e = bob
        .update(edit(&id, "Bob rewrites it", "cursor"))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, alice_core::error::AliceError::Invalid(m) if m == SHARED_REFUSAL),
        "{e:?}"
    );
    // Ann can.
    let out = ann
        .update(edit(&id, "Ann's corrected text", "cursor"))
        .await
        .unwrap();
    assert_eq!(out.memory.id, id);
    assert_eq!(out.memory.body, "Ann's corrected text");
    assert!(out.event_id.as_deref().unwrap().starts_with("team:"));
    let team_id = id.strip_prefix("team:").unwrap().to_string();
    let stored_body = s
        .team
        .as_ref()
        .unwrap()
        .svc
        .get(GetRequest {
            ids: vec![team_id],
            include_provenance: false,
        })
        .await
        .unwrap()
        .memories[0]
        .memory
        .body
        .clone();
    assert_eq!(stored_body, "Ann's corrected text");
}

/// `update --action move` of a `team:` id is an update like any other: only its author may, and
/// the target project is one of the team store's; the memory never enters a personal store.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_memory_moves_only_within_the_team_store_and_only_by_its_author() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let (ann, bob) = (&s.tenant("ann").svc, &s.tenant("bob").svc);
    let mut req = shared("The shared deploy checklist", Some("cursor"));
    req.project_hint = Some("shared-proj".into());
    let id = ann.remember(req).await.unwrap().id.unwrap();
    let move_to = |agent: &str| UpdateRequest {
        id: id.clone(),
        update: MemoryUpdate {
            action: UpdateAction::Move,
            project: Some("Team Ops".into()),
            ..Default::default()
        },
        author: Author {
            agent: Some(agent.into()),
            ..Default::default()
        },
    };
    let e = bob.update(move_to("cursor")).await.unwrap_err();
    assert!(
        matches!(&e, alice_core::error::AliceError::Invalid(m) if m == SHARED_REFUSAL),
        "{e:?}"
    );
    let team_home = &s.team.as_ref().unwrap().home;
    assert_eq!(stored(team_home), vec![("shared-proj".to_string(), None)]);
    let out = ann.update(move_to("cursor")).await.unwrap();
    assert_eq!(out.memory.id, id);
    assert_eq!(out.memory.project, "team-ops");
    assert!(out.event_id.as_deref().unwrap().starts_with("team:"));
    assert_eq!(stored(team_home), vec![("team-ops".to_string(), None)]);
    assert!(stored(&s.tenant("ann").home).is_empty());
    assert!(stored(&s.tenant("bob").home).is_empty());
}

/// Final review I-1: superseding marks the older memory, so it is the older memory's author's to do.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_author_supersedes_a_shared_memory() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let (ann, bob) = (&s.tenant("ann").svc, &s.tenant("bob").svc);
    let id = ann
        .remember(shared("The deploy window is on Friday", Some("cursor")))
        .await
        .unwrap()
        .id
        .unwrap();
    let status = |s: &Server, team_id: &str| {
        let id = team_id.strip_prefix("team:").unwrap().to_string();
        let svc = s.team.as_ref().unwrap().svc.clone();
        async move {
            svc.get(GetRequest {
                ids: vec![id],
                include_provenance: false,
            })
            .await
            .unwrap()
            .memories[0]
                .memory
                .status
        }
    };
    let mut by_bob = shared("The deploy window is on Monday", Some("cursor"));
    by_bob.supersedes = vec![id.clone()];
    let e = bob.remember(by_bob).await.unwrap_err();
    assert!(
        matches!(&e, alice_core::error::AliceError::Invalid(m) if m == SHARED_REFUSAL),
        "{e:?}"
    );
    assert_ne!(status(&s, &id).await, alice_core::model::Status::Superseded);
    let mut by_ann = shared("The deploy window is on Monday", Some("cursor"));
    by_ann.supersedes = vec![id.clone()];
    ann.remember(by_ann).await.unwrap();
    assert_eq!(status(&s, &id).await, alice_core::model::Status::Superseded);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_author_undoes_a_shared_change() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let (ann, bob) = (&s.tenant("ann").svc, &s.tenant("bob").svc);
    let id = ann
        .remember(shared("Shared and then edited", Some("cursor")))
        .await
        .unwrap()
        .id
        .unwrap();
    let event = ann
        .update(edit(&id, "An edit worth undoing", "cursor"))
        .await
        .unwrap()
        .event_id
        .unwrap();
    assert!(event.starts_with("team:"), "{event}");
    let undo = |e: &str| UndoRequest {
        event_id: e.into(),
        author: Author {
            agent: Some("cursor".into()),
            ..Default::default()
        },
    };
    let e = bob.undo(undo(&event)).await.unwrap_err();
    assert!(
        matches!(&e, alice_core::error::AliceError::Invalid(m) if m == SHARED_REFUSAL),
        "{e:?}"
    );
    let r = ann.undo(undo(&event)).await.unwrap();
    assert!(r.undone, "{}", r.message);
    let body = ann
        .get(GetRequest {
            ids: vec![id],
            include_provenance: false,
        })
        .await
        .unwrap()
        .memories[0]
        .memory
        .body
        .clone();
    assert_eq!(body, "Shared and then edited");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_lists_remember_with_a_shared_flag_and_saves_to_the_team_memory() {
    let s = Server::start_with_team(&["ann"]).await;
    let t = s.tenant("ann");
    let url = format!("{}/mcp", s.url("ann"));
    let c = connect(&url, Some(&t.token), None).await.unwrap();
    let tools = c.list_all_tools().await.unwrap();
    let remember = tools.iter().find(|t| t.name == "remember").unwrap();
    let props = remember.input_schema.get("properties").unwrap();
    let flag = props.get("shared").expect("remember exposes `shared`");
    assert_eq!(flag["type"], "boolean", "{flag}");
    let d = flag["description"].as_str().unwrap();
    assert!(d.contains("shared"), "{d}");
    assert!(d.contains("every team member"), "{d}");
    let reply = try_tool(
        &c,
        "remember",
        serde_json::json!({"content": "Central note over MCP", "shared": true}),
    )
    .await
    .unwrap();
    assert!(reply.contains("Saved to the team memory"), "{reply}");
    assert!(stored(&t.home).is_empty());
    assert_eq!(stored(&s.team.as_ref().unwrap().home).len(), 1);
    let id = reply
        .split_whitespace()
        .find(|w| w.starts_with("team:"))
        .unwrap()
        .to_string();
    assert_eq!(
        team_author(&s, &id).await.agent.as_deref(),
        Some("ann/team-agent")
    );
}

// ---------------------------------------------------------------------------- the read merge

use alice_core::api::{ContextRequest, OutputFormat, RecallMode, RecallRequest, RecallResponse};
use alice_core::judge::jev::{HttpReply, JevTransport, TransportError};
use alice_core::model::Kind;
use std::sync::Arc;

const TEAM_LATE_NOTE: &str = "the team memory did not answer in time; this answer is personal only";

fn recall_in(query: &str, project: Option<&str>, hint: Option<&str>) -> RecallRequest {
    RecallRequest {
        query: query.into(),
        project: project.map(Into::into),
        project_hint: hint.map(Into::into),
        mode: Some(RecallMode::Fast),
        format: OutputFormat::Json,
        ..Default::default()
    }
}

/// A shared memory written by `tenant` into the team project `project`.
async fn share(s: &Server, tenant: &str, project: &str, title: &str, content: &str) -> String {
    let mut req = shared(content, Some("cursor"));
    req.title = Some(title.into());
    req.project_hint = Some(project.into());
    req.kind = Some(Kind::Decision);
    let r = s.tenant(tenant).svc.remember(req).await.unwrap();
    r.id.unwrap()
}

/// A personal memory of `tenant` in `project`.
async fn own(s: &Server, tenant: &str, project: &str, title: &str, content: &str) {
    let req = RememberRequest {
        content: content.into(),
        title: Some(title.into()),
        kind: Some(Kind::Decision),
        project_hint: Some(project.into()),
        ..Default::default()
    };
    s.tenant(tenant).svc.remember(req).await.unwrap();
}

fn ids(r: &RecallResponse) -> Vec<String> {
    r.results.iter().map(|h| h.title.clone()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recall_returns_the_team_memory_labelled_with_its_author() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let id = share(
        &s,
        "ann",
        "wiki",
        "Quokka rollout checklist",
        "The quokka rollout checklist lives in the wiki",
    )
    .await;
    let bob = &s.tenant("bob").svc;
    let r = bob
        .recall(recall_in("quokka rollout", None, None))
        .await
        .unwrap();
    let hit = r
        .results
        .iter()
        .find(|h| h.title == "Quokka rollout checklist")
        .unwrap_or_else(|| panic!("no team hit: {r:?}"));
    assert_eq!(hit.id, id);
    assert!(hit.id.starts_with("team:"));
    assert_eq!(hit.source.as_deref(), Some("team"));
    assert_eq!(hit.author.as_deref(), Some("ann"));
    assert_eq!(
        hit.cloud_ok, None,
        "the team store's mark stays out of the answer"
    );
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    // The text form says whose it is.
    let mut text_req = recall_in("quokka rollout", None, None);
    text_req.format = OutputFormat::Text;
    let t = bob.recall(text_req).await.unwrap().text.unwrap();
    assert!(t.contains("(team, by ann)"), "{t}");
    // The author's own personal answer has none of this.
    assert!(
        !ann_has_team_label(&s).await,
        "a personal tenant without a team memory hit"
    );
}

async fn ann_has_team_label(s: &Server) -> bool {
    let r = s
        .tenant("ann")
        .svc
        .recall(recall_in("zzzz nothing matches this", None, None))
        .await
        .unwrap();
    r.results.iter().any(|h| h.source.is_some())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_personal_hit_equal_to_a_team_hit_appears_once() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    let body = "The quokka rollout checklist lives in the wiki";
    share(&s, "ann", "wiki", "Quokka rollout checklist", body).await;
    own(&s, "bob", "wiki", "Quokka rollout checklist", body).await;
    let r = s
        .tenant("bob")
        .svc
        .recall(recall_in("quokka rollout", None, Some("wiki")))
        .await
        .unwrap();
    let same: Vec<_> = r
        .results
        .iter()
        .filter(|h| h.title == "Quokka rollout checklist")
        .collect();
    assert_eq!(same.len(), 1, "{:?}", ids(&r));
    assert!(!same[0].id.starts_with("team:"), "the personal one stays");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_team_memory_leaves_the_personal_answer_with_a_note() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    share(&s, "ann", "wiki", "Quokka team note", "quokka team note").await;
    own(&s, "bob", "wiki", "Quokka own note", "quokka own note").await;
    s.delay_team_recall(3_000);
    let t0 = std::time::Instant::now();
    let r = s
        .tenant("bob")
        .svc
        .recall(recall_in("quokka", None, Some("wiki")))
        .await
        .unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(1_500),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(r.notes, vec![TEAM_LATE_NOTE.to_string()]);
    assert_eq!(ids(&r), vec!["Quokka own note".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_team_store_leaves_the_personal_answer_with_a_note() {
    let s = Server::start_with_team(&["bob"]).await;
    own(&s, "bob", "wiki", "Quokka own note", "quokka own note").await;
    s.stop_team().await;
    let t0 = std::time::Instant::now();
    let r = s
        .tenant("bob")
        .svc
        .recall(recall_in("quokka", None, Some("wiki")))
        .await
        .unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(1_500),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(r.notes, vec![TEAM_LATE_NOTE.to_string()]);
    assert_eq!(ids(&r), vec!["Quokka own note".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_memory_shows_in_its_own_project_and_not_in_another() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    share(&s, "ann", "x", "Quokka in x", "quokka lives in project x").await;
    own(&s, "bob", "x", "Own note in x", "own note in project x").await;
    own(&s, "bob", "y", "Own note in y", "own note in project y").await;
    let bob = &s.tenant("bob").svc;
    let in_x = bob
        .recall(recall_in("quokka", Some("x"), None))
        .await
        .unwrap();
    assert_eq!(
        ids(&in_x),
        vec!["Quokka in x".to_string()],
        "{:?}",
        in_x.notes
    );
    let in_y = bob
        .recall(recall_in("quokka", Some("y"), None))
        .await
        .unwrap();
    assert!(ids(&in_y).is_empty(), "{:?}", ids(&in_y));
    assert!(
        in_y.notes.is_empty(),
        "no note for a project the team lacks: {:?}",
        in_y.notes
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_context_carries_the_team_memory_or_says_it_is_missing() {
    let s = Server::start_with_team(&["ann", "bob"]).await;
    share(
        &s,
        "ann",
        "x",
        "Quokka decision",
        "we decided about the quokka",
    )
    .await;
    let bob = &s.tenant("bob").svc;
    let ctx = |hint: &str| ContextRequest {
        project_hint: Some(hint.into()),
        ..Default::default()
    };
    let c = bob.context(ctx("x")).await.unwrap();
    assert!(c.text.contains("## Team memory"), "{}", c.text);
    assert!(c.text.contains("Quokka decision"), "{}", c.text);
    s.stop_team().await;
    let c = bob.context(ctx("x")).await.unwrap();
    assert!(c.text.contains(TEAM_LATE_NOTE), "{}", c.text);
}

// ------------------------------------------------------------------- precise mode, team hits

const JEV_KEY: &str = concat!("ts_", "live_server_mode_0001");

/// Grades a result that contains `keyword` as answering, the rest as unrelated; records the
/// state of every grading request.
struct Grader {
    keyword: &'static str,
    states: parking_lot::Mutex<Vec<String>>,
}

impl JevTransport for Grader {
    fn post(
        &self,
        _endpoint: &str,
        _key: &str,
        body: &[u8],
        _timeout: Duration,
    ) -> Result<HttpReply, TransportError> {
        let req: serde_json::Value = serde_json::from_slice(body).unwrap();
        let state = req["state"].as_str().unwrap();
        let mut answers = serde_json::Map::new();
        let mut grading = false;
        for (qid, q) in req["questions"].as_object().unwrap() {
            let options: Vec<String> = q["criteria"].as_object().unwrap().keys().cloned().collect();
            let (choice, other) = if let Some(k) = qid.strip_prefix("r_") {
                grading = true;
                let block = state
                    .split(&format!("Result {k} ("))
                    .nth(1)
                    .unwrap_or("")
                    .split("\nResult ")
                    .next()
                    .unwrap_or("");
                if block.contains(self.keyword) {
                    ("answers", "unrelated")
                } else {
                    ("unrelated", "answers")
                }
            } else {
                ("unrelated", "duplicate")
            };
            let probs: serde_json::Map<String, serde_json::Value> = options
                .iter()
                .map(|o| {
                    let p = if o == choice {
                        0.9
                    } else if o == other {
                        0.05
                    } else {
                        0.05 / (options.len().max(3) - 2) as f64
                    };
                    (o.clone(), serde_json::json!(p))
                })
                .collect();
            answers.insert(
                qid.clone(),
                serde_json::json!({"type": "choice", "choice": choice, "confidence": 0.9, "probabilities": probs}),
            );
        }
        if grading {
            self.states.lock().push(state.to_string());
        }
        let resp = serde_json::json!({"model": "jev-1.13.0", "answers": answers,
            "usage": {"input_tokens": 800, "output_tokens": 0}});
        Ok(HttpReply {
            status: 200,
            body: serde_json::to_vec(&resp).unwrap(),
            retry_after: None,
        })
    }
}

fn patch(key: &str, value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(key.into(), value);
    m
}

/// A server whose tenant `bob` has three personal deploy notes in project `p` and a model judge
/// (a scripted grader that likes `keyword`).
async fn precise_server(keyword: &'static str) -> (Server, Arc<Grader>) {
    let grader = Arc::new(Grader {
        keyword,
        states: Default::default(),
    });
    let s = Server::start_with_team_and_judge(&["ann", "bob"], grader.clone()).await;
    for n in ["staging", "freeze", "canary"] {
        own(
            &s,
            "bob",
            "p",
            &format!("Deploy {n}"),
            &format!("deploy notes: the {n} deploy runs make deploy"),
        )
        .await;
    }
    (s, grader)
}

async fn enable_judge(s: &Server) {
    let bob = &s.tenant("bob").svc;
    bob.patch_settings(patch("judge.provider", serde_json::json!("jev")))
        .unwrap();
    bob.set_secret(alice_daemon::secrets::SecretName::JevApiKey, JEV_KEY.into())
        .unwrap();
}

fn precise(query: &str) -> RecallRequest {
    RecallRequest {
        mode: Some(RecallMode::Precise),
        ..recall_in(query, None, Some("p"))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cloud_ok_team_hit_is_graded_with_the_personal_hits_and_can_rank_first() {
    let (s, grader) = precise_server("ZEBRA").await;
    share(
        &s,
        "ann",
        "teamproj",
        "Deploy zebra",
        "deploy notes: the ZEBRA cluster deploy uses blue green",
    )
    .await;
    enable_judge(&s).await;
    let r = s
        .tenant("bob")
        .svc
        .recall(precise("deploy notes"))
        .await
        .unwrap();
    let states = grader.states.lock().join("\n");
    assert!(states.contains("ZEBRA"), "the team hit is graded: {states}");
    assert!(r.reranked, "{:?}", r.notes);
    assert_eq!(r.results[0].title, "Deploy zebra", "{:?}", ids(&r));
    assert_eq!(r.results[0].source.as_deref(), Some("team"));
    assert_eq!(r.results.len(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_hit_of_a_cloud_denied_project_never_reaches_the_grader_and_follows_the_graded() {
    let (s, grader) = precise_server("staging").await;
    s.team
        .as_ref()
        .unwrap()
        .svc
        .patch_settings(patch(
            "privacy.cloud_denied_projects",
            serde_json::json!(["secretproj"]),
        ))
        .unwrap();
    share(
        &s,
        "ann",
        "secretproj",
        "Deploy kappa",
        "deploy notes: the KAPPA cluster deploy is secret",
    )
    .await;
    enable_judge(&s).await;
    let r = s
        .tenant("bob")
        .svc
        .recall(precise("deploy notes"))
        .await
        .unwrap();
    let states = grader.states.lock().join("\n");
    assert!(!states.is_empty(), "the personal hits are graded");
    assert!(
        !states.contains("KAPPA"),
        "never sent to the grader: {states}"
    );
    assert!(r.reranked, "{:?}", r.notes);
    let titles = ids(&r);
    assert_eq!(titles.len(), 4, "{titles:?}");
    assert_eq!(
        titles[3], "Deploy kappa",
        "after the graded hits: {titles:?}"
    );
    assert_eq!(r.results[0].title, "Deploy staging", "{titles:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_team_store_marks_cloud_ok_on_its_hits() {
    let (s, _grader) = precise_server("ZEBRA").await;
    let team = &s.team.as_ref().unwrap().svc;
    team.patch_settings(patch(
        "privacy.cloud_denied_projects",
        serde_json::json!(["secretproj"]),
    ))
    .unwrap();
    share(&s, "ann", "teamproj", "Deploy open", "deploy notes: open").await;
    share(&s, "ann", "secretproj", "Deploy shut", "deploy notes: shut").await;
    let r = team
        .recall(recall_in("deploy notes", None, None))
        .await
        .unwrap();
    let mark = |t: &str| r.results.iter().find(|h| h.title == t).unwrap().cloud_ok;
    assert_eq!(mark("Deploy open"), Some(true));
    assert_eq!(mark("Deploy shut"), Some(false));
    // A personal tenant's own hits carry no mark.
    let r = s
        .tenant("bob")
        .svc
        .recall(recall_in("deploy notes", None, Some("p")))
        .await
        .unwrap();
    assert!(r.results.iter().all(|h| h.cloud_ok.is_none()));
    assert!(!r.results.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_hit_of_a_project_the_person_denies_never_reaches_the_grader_and_follows_the_graded()
{
    let (s, grader) = precise_server("staging").await;
    share(
        &s,
        "ann",
        "teamproj",
        "Deploy kappa",
        "deploy notes: the KAPPA cluster deploy is open to the team store",
    )
    .await;
    // The team store allows `teamproj`; the person does not.
    s.tenant("bob")
        .svc
        .patch_settings(patch(
            "privacy.cloud_denied_projects",
            serde_json::json!(["teamproj"]),
        ))
        .unwrap();
    enable_judge(&s).await;
    let r = s
        .tenant("bob")
        .svc
        .recall(precise("deploy notes"))
        .await
        .unwrap();
    let states = grader.states.lock().join("\n");
    assert!(!states.is_empty(), "the personal hits are graded");
    assert!(
        !states.contains("KAPPA"),
        "never sent to the grader: {states}"
    );
    assert!(r.reranked, "{:?}", r.notes);
    let titles = ids(&r);
    assert_eq!(titles.len(), 4, "{titles:?}");
    assert_eq!(
        titles[3], "Deploy kappa",
        "after the graded hits: {titles:?}"
    );
    assert!(r.results.iter().all(|h| h.cloud_ok.is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_hit_both_policies_allow_is_graded_and_no_precise_hit_carries_a_mark() {
    let (s, grader) = precise_server("ZEBRA").await;
    share(
        &s,
        "ann",
        "teamproj",
        "Deploy zebra",
        "deploy notes: the ZEBRA cluster deploy uses blue green",
    )
    .await;
    // The person denies another project; `teamproj` stays allowed on both sides.
    s.tenant("bob")
        .svc
        .patch_settings(patch(
            "privacy.cloud_denied_projects",
            serde_json::json!(["elsewhere"]),
        ))
        .unwrap();
    enable_judge(&s).await;
    let r = s
        .tenant("bob")
        .svc
        .recall(precise("deploy notes"))
        .await
        .unwrap();
    let states = grader.states.lock().join("\n");
    assert!(states.contains("ZEBRA"), "graded: {states}");
    assert_eq!(r.results[0].title, "Deploy zebra", "{:?}", ids(&r));
    assert!(r.results.iter().all(|h| h.cloud_ok.is_none()), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_whose_own_project_the_person_denies_sends_nothing_to_the_grader() {
    let (s, grader) = precise_server("ZEBRA").await;
    share(
        &s,
        "ann",
        "teamproj",
        "Deploy zebra",
        "deploy notes: the ZEBRA cluster deploy uses blue green",
    )
    .await;
    s.tenant("bob")
        .svc
        .patch_settings(patch(
            "privacy.cloud_denied_projects",
            serde_json::json!(["p"]),
        ))
        .unwrap();
    enable_judge(&s).await;
    let r = s
        .tenant("bob")
        .svc
        .recall(precise("deploy notes"))
        .await
        .unwrap();
    assert!(
        grader.states.lock().is_empty(),
        "nothing reaches the grader: {:?}",
        grader.states.lock()
    );
    assert!(r.results.iter().all(|h| h.cloud_ok.is_none()));
    assert!(!r.results.is_empty());
}
