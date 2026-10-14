//! Jev for the daemon: HTTP through `BlockingHttp`, cache and costs through the reader pool and
//! the writer, the cloud policy over the live settings, and choosing the judge from settings.
//!
//! The one decision whether text of a project may leave the machine is
//! [`SettingsHandle::cloud_allowed`], asked here once per call through [`DaemonJevPolicy`];
//! nothing in this module checks `privacy.cloud_denied_projects`, a project's `.alice.toml` or
//! `config_degraded` on its own.

use crate::config::SettingsHandle;
use crate::redaction::Redactors;
use crate::secrets::{self, EnvLookup, SecretName, SecretStore};
use crate::service::SecretEnv;
use crate::writer::{WriteOp, WriterHandle};
use alice_core::judge::Judge;
use alice_core::judge::heuristic::HeuristicJudge;
use alice_core::judge::jev::{
    HttpReply, JevCall, JevConfig, JevJudge, JevPolicy, JevStore, JevTransport, TransportError,
};
use alice_core::redact::Redactor;
use alice_core::settings::{JudgeProvider, Settings};
use alice_core::store::projects;
use alice_core::store::{ReaderPool, judge as judge_store};
use alice_embed::net::{BlockingHttp, HttpError};
use parking_lot::{Mutex, RwLock};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The HTTP client's identity: the program and its version, nothing about the person or the
/// machine.
const USER_AGENT: &str = concat!("alice/", env!("CARGO_PKG_VERSION"));

pub struct ReqwestTransport {
    http: BlockingHttp,
}

impl ReqwestTransport {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            http: BlockingHttp::new(USER_AGENT)?,
        })
    }
}

impl JevTransport for ReqwestTransport {
    fn post(
        &self,
        endpoint: &str,
        key: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpReply, TransportError> {
        match self
            .http
            .post_json(endpoint, Some(key), body.to_vec(), timeout)
        {
            Ok(r) => {
                let retry_after = r
                    .headers
                    .iter()
                    .find(|(k, _)| k == "retry-after")
                    .and_then(|(_, v)| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                Ok(HttpReply {
                    status: r.status,
                    body: r.body,
                    retry_after,
                })
            }
            Err(HttpError::Timeout) => Err(TransportError::Timeout),
            Err(HttpError::Connect(e) | HttpError::Other(e)) => Err(TransportError::Connect(e)),
        }
    }
}

pub struct DaemonJevStore {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
}

impl JevStore for DaemonJevStore {
    fn cached(&self, hash: &str) -> Option<String> {
        self.readers
            .with(|c| judge_store::cache_get(c, hash))
            .ok()
            .flatten()
    }

    fn spent(&self, day: &str) -> Result<f64, String> {
        // No fallback to 0: an unreadable spend must stop calls, not allow a full new budget.
        self.readers
            .with(|c| judge_store::spent(c, day, "jev"))
            .map_err(|e| e.to_string())
    }

    fn record(&self, call: &JevCall) {
        self.writer.send_nowait(WriteOp::JudgeCall(call.clone()));
    }
}

/// The cloud policy of the daemon: the live settings decide, the stored project root tells
/// which `.alice.toml` applies, and the redactors are the ones writes use (switched on).
pub struct DaemonJevPolicy {
    settings: Arc<SettingsHandle>,
    readers: Arc<ReaderPool>,
    redactors: Arc<Redactors>,
}

impl DaemonJevPolicy {
    pub fn new(
        settings: Arc<SettingsHandle>,
        readers: Arc<ReaderPool>,
        redactors: Arc<Redactors>,
    ) -> Self {
        Self {
            settings,
            readers,
            redactors,
        }
    }

    /// The stored project `slug_or_alias` names: its canonical slug, working-tree root and every
    /// name it is known by (slug, each alias, display name, and the name as asked). `None` for
    /// a project that is not stored or cannot be read.
    fn lookup(&self, slug_or_alias: &str) -> Option<Identity> {
        self.readers
            .with(|c| {
                let Some(p) = projects::resolve_slug(c, slug_or_alias)? else {
                    return Ok(None);
                };
                let root = projects::root_of(c, &p.id)?;
                let mut names = vec![
                    slug_or_alias.trim().to_string(),
                    p.slug.clone(),
                    p.name.clone(),
                ];
                let mut stmt =
                    c.prepare("SELECT alias FROM project_aliases WHERE project_id = ?1")?;
                let aliases = stmt.query_map([&p.id], |r| r.get::<_, String>(0))?;
                for a in aliases {
                    names.push(a?);
                }
                Ok(Some(Identity {
                    slug: p.slug,
                    root,
                    names,
                }))
            })
            .ok()
            .flatten()
    }
}

/// A stored project as the policy sees it.
struct Identity {
    slug: String,
    root: Option<String>,
    names: Vec<String>,
}

impl JevPolicy for DaemonJevPolicy {
    /// Fails closed: an empty slug, a project the store does not know, a store that cannot be
    /// read, and everything `SettingsHandle::cloud_allowed` refuses all answer `false`. That
    /// one decision is asked about every name the project has (canonical slug, aliases, display
    /// name, the name as given), so a denylist entry written as any of them denies it.
    fn cloud_allowed(&self, project: &str) -> bool {
        if project.trim().is_empty() {
            return false;
        }
        let Some(id) = self.lookup(project) else {
            return false;
        };
        let root = id.root.as_deref().map(Path::new);
        id.names
            .iter()
            .filter(|n| !n.trim().is_empty())
            .all(|n| self.settings.cloud_allowed(n, root))
            && self.settings.cloud_allowed(&id.slug, root)
    }

    fn redactor(&self, project: &str) -> Arc<Redactor> {
        let root = self.lookup(project).and_then(|id| id.root);
        self.redactors
            .for_root_always_on(root.as_deref().map(Path::new))
    }
}

fn heuristic() -> Arc<dyn Judge> {
    Arc::new(HeuristicJudge::default())
}

/// The judge `settings` ask for, and a warning when it had to fall back to the heuristic.
/// `transport` is asked for only when Jev is wanted and has a key.
pub fn build_judge(
    settings: &Settings,
    secrets: &dyn SecretStore,
    env: EnvLookup<'_>,
    readers: Arc<ReaderPool>,
    writer: WriterHandle,
    policy: Arc<dyn JevPolicy>,
    transport: &dyn Fn() -> Result<Arc<dyn JevTransport>, String>,
) -> (Arc<dyn Judge>, Option<String>) {
    if settings.judge.provider == JudgeProvider::Heuristic {
        return (heuristic(), None);
    }
    let key = match secrets::resolve_with(secrets, SecretName::JevApiKey, env) {
        Ok(Some((k, _))) => k,
        Ok(None) => {
            return (
                heuristic(),
                Some("judge.provider is jev but no TypeSafe key is set; using the heuristic judge. Set one with `alice config set-secret jev_api_key`".into()),
            );
        }
        Err(e) => {
            return (
                heuristic(),
                Some(format!(
                    "judge.provider is jev but the TypeSafe key cannot be read ({e}); using the heuristic judge"
                )),
            );
        }
    };
    let transport = match transport() {
        Ok(t) => t,
        Err(e) => {
            return (
                heuristic(),
                Some(format!(
                    "cannot start the HTTP client for Jev ({e}); using the heuristic judge"
                )),
            );
        }
    };
    let cfg = JevConfig::from_settings(&settings.judge, key);
    (
        Arc::new(JevJudge::new(
            cfg,
            transport,
            Arc::new(DaemonJevStore { readers, writer }),
            policy,
        )),
        None,
    )
}

/// The judge in force and the reason it is not the one asked for. Rebuilt when `judge.*` or
/// `privacy.*` settings change or the Jev key is set or deleted; settings and secrets are read
/// at each rebuild, never kept.
pub struct JudgeHub {
    slot: RwLock<(Arc<dyn Judge>, Option<String>)>,
    /// One rebuild at a time; the later one reads the newest settings.
    building: Mutex<()>,
    settings: Arc<SettingsHandle>,
    secrets: Arc<dyn SecretStore>,
    secret_env: SecretEnv,
    readers: Arc<ReaderPool>,
    writer: WriterHandle,
    policy: Arc<dyn JevPolicy>,
    /// Tests inject a scripted transport; otherwise one HTTP client is made on first need and
    /// kept across rebuilds.
    injected: Option<Arc<dyn JevTransport>>,
    transport: Mutex<Option<Arc<dyn JevTransport>>>,
}

impl JudgeHub {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        settings: Arc<SettingsHandle>,
        secrets: Arc<dyn SecretStore>,
        secret_env: SecretEnv,
        readers: Arc<ReaderPool>,
        writer: WriterHandle,
        redactors: Arc<Redactors>,
        injected: Option<Arc<dyn JevTransport>>,
    ) -> Arc<Self> {
        let policy: Arc<dyn JevPolicy> = Arc::new(DaemonJevPolicy::new(
            settings.clone(),
            readers.clone(),
            redactors,
        ));
        let hub = Arc::new(Self {
            slot: RwLock::new((heuristic(), None)),
            building: Mutex::new(()),
            settings,
            secrets,
            secret_env,
            readers,
            writer,
            policy,
            injected,
            transport: Mutex::new(None),
        });
        hub.rebuild();
        hub
    }

    pub fn current(&self) -> Arc<dyn Judge> {
        self.slot.read().0.clone()
    }

    /// Why the judge is not the one the settings ask for, else what the judge itself reports
    /// (Jev paused for lack of a readable spend or a used-up budget).
    pub fn warning(&self) -> Option<String> {
        let (judge, build) = {
            let slot = self.slot.read();
            (slot.0.clone(), slot.1.clone())
        };
        build.or_else(|| judge.warning())
    }

    /// Whether the judge in force is a fallback for the one asked for.
    pub fn fell_back(&self) -> bool {
        self.slot.read().1.is_some()
    }

    fn transport(&self) -> Result<Arc<dyn JevTransport>, String> {
        if let Some(t) = &self.injected {
            return Ok(t.clone());
        }
        let mut slot = self.transport.lock();
        if let Some(t) = slot.as_ref() {
            return Ok(t.clone());
        }
        let t: Arc<dyn JevTransport> =
            Arc::new(ReqwestTransport::new().map_err(|e| e.to_string())?);
        *slot = Some(t.clone());
        Ok(t)
    }

    /// Builds the judge from the settings and secrets as they are now.
    pub fn rebuild(&self) {
        let _one_at_a_time = self.building.lock();
        let settings = self.settings.get();
        let built = build_judge(
            &settings,
            self.secrets.as_ref(),
            self.secret_env.as_ref(),
            self.readers.clone(),
            self.writer.clone(),
            self.policy.clone(),
            &|| self.transport(),
        );
        *self.slot.write() = built;
    }

    /// Settings keys changed: rebuild when one of them can change the judge.
    pub fn on_change(&self, changed: &[String]) {
        if changed
            .iter()
            .any(|k| k.starts_with("judge.") || k.starts_with("privacy."))
        {
            self.rebuild();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AliceHome;
    use alice_core::judge::jev::scenarios;
    use alice_core::judge::{Choice, JudgeError};
    use alice_core::project::ProjectRef;
    use alice_core::store::{open_writer, projects as store_projects};
    use std::collections::VecDeque;

    /// Answers every request with a yes and records the bodies. Never touches the network.
    #[derive(Default)]
    struct Recording {
        bodies: Mutex<Vec<serde_json::Value>>,
        script: Mutex<VecDeque<u16>>,
    }

    impl JevTransport for Recording {
        fn post(
            &self,
            _e: &str,
            key: &str,
            body: &[u8],
            _t: Duration,
        ) -> Result<HttpReply, TransportError> {
            assert_eq!(key, TEST_KEY);
            self.bodies
                .lock()
                .push(serde_json::from_slice(body).unwrap());
            let status = self.script.lock().pop_front().unwrap_or(200);
            let ok = serde_json::json!({"answers": {"q": {"choice": "yes", "probabilities": {"yes": 0.9, "no": 0.1}}},
                "usage": {"input_tokens": 50, "output_tokens": 1}});
            Ok(HttpReply {
                status,
                body: serde_json::to_vec(&ok).unwrap(),
                retry_after: None,
            })
        }
    }

    const TEST_KEY: &str = concat!("ts_", "live_unit_test_key_0001");

    struct Fixture {
        _dir: tempfile::TempDir,
        home: AliceHome,
        readers: Arc<ReaderPool>,
        settings: Arc<SettingsHandle>,
        transport: Arc<Recording>,
        judge: alice_core::judge::jev::JevJudge,
        repo: tempfile::TempDir,
    }

    fn yes_no() -> Vec<Choice> {
        vec![Choice::new("yes", "it is"), Choice::new("no", "it is not")]
    }

    /// A store with projects `client` (rooted at a temp repository) and `plain`, a settings
    /// handle over `config`, and a Jev judge over the daemon's policy and a recording transport.
    fn fixture(config: &str, project_file: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = AliceHome::new(dir.path().to_path_buf());
        home.ensure_dirs().unwrap();
        std::fs::write(home.config_file(), config).unwrap();
        let repo = tempfile::tempdir().unwrap();
        if let Some(text) = project_file {
            std::fs::write(repo.path().join(".alice.toml"), text).unwrap();
        }
        {
            let conn = open_writer(&home.db()).unwrap();
            let mk = |key: &str, slug: &str, root: Option<&Path>| ProjectRef {
                key: key.into(),
                slug: slug.into(),
                name: slug.into(),
                root: root.map(Path::to_path_buf),
                remote: None,
            };
            store_projects::upsert_project(&conn, &mk("k/client", "client", Some(repo.path())))
                .unwrap();
            store_projects::upsert_project(&conn, &mk("k/plain", "plain", None)).unwrap();
        }
        let readers = Arc::new(ReaderPool::new(home.db(), 2));
        let settings = SettingsHandle::load_with_env(&home, Vec::new());
        let redactors = Redactors::new(settings.clone());
        let policy = Arc::new(DaemonJevPolicy::new(
            settings.clone(),
            readers.clone(),
            redactors,
        ));
        let transport = Arc::new(Recording::default());
        let store = Arc::new(alice_core::judge::jev::MemoryJevStore::default());
        let cfg = JevConfig {
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            model: "jev-1.13.0".into(),
            api_key: alice_core::settings::SecretValue::new(TEST_KEY),
            rate_per_sec: 10.0,
            timeout: Duration::from_secs(5),
            daily_budget_usd: 1.0,
            max_retries: 0,
        };
        let judge = alice_core::judge::jev::JevJudge::new(cfg, transport.clone(), store, policy)
            .with_sleeper(Arc::new(|_| {}));
        Fixture {
            _dir: dir,
            home,
            readers,
            settings,
            transport,
            judge,
            repo,
        }
    }

    #[test]
    fn a_project_denied_only_by_its_alice_toml_never_reaches_the_transport() {
        let f = fixture("", Some("[privacy]\ncloud_denied = true\n"));
        let e = f
            .judge
            .classify_in(&["client"], "text", &yes_no())
            .unwrap_err();
        assert_eq!(e, JudgeError::Denied);
        assert!(f.transport.bodies.lock().is_empty());
        assert_eq!(f.judge.stats().denied, 1);
        // The other project, and the unscoped text, still go.
        f.judge.classify_in(&["plain"], "text", &yes_no()).unwrap();
        f.judge.classify("other text", &yes_no()).unwrap();
        assert_eq!(f.transport.bodies.lock().len(), 2);
        // A call that covers both is denied as a whole.
        let (mut new, mut existing) = scenarios::compare_case();
        new.project = "plain".into();
        existing[0].project = "client".into();
        assert_eq!(f.judge.compare(&new, &existing), Err(JudgeError::Denied));
        assert_eq!(f.transport.bodies.lock().len(), 2);
        let _ = &f.repo;
    }

    #[test]
    fn a_broken_project_file_and_the_global_denylist_deny_too() {
        let f = fixture("", Some("[privacy\ncloud_denied = tru"));
        assert_eq!(
            f.judge.classify_in(&["client"], "text", &yes_no()),
            Err(JudgeError::Denied),
            "a broken project file fails closed"
        );
        let f = fixture("[privacy]\ncloud_denied_projects = [\"plain\"]\n", None);
        assert_eq!(
            f.judge.classify_in(&["plain"], "text", &yes_no()),
            Err(JudgeError::Denied)
        );
        f.judge.classify_in(&["client"], "text", &yes_no()).unwrap();
        assert_eq!(f.transport.bodies.lock().len(), 1);
    }

    #[test]
    fn unknown_and_empty_projects_fail_closed_and_aliases_cannot_get_around_the_list() {
        let f = fixture("[privacy]\ncloud_denied_projects = [\"plain\"]\n", None);
        for p in ["", "  ", "no-such-project"] {
            assert_eq!(
                f.judge.classify_in(&[p], "text", &yes_no()),
                Err(JudgeError::Denied),
                "{p:?}"
            );
        }
        {
            let conn = open_writer(&f.home.db()).unwrap();
            let plain = store_projects::resolve_slug(&conn, "plain")
                .unwrap()
                .unwrap();
            store_projects::add_alias(&conn, "nickname", &plain.id).unwrap();
        }
        assert_eq!(
            f.judge.classify_in(&["nickname"], "text", &yes_no()),
            Err(JudgeError::Denied),
            "an alias of a denied project is denied"
        );
        assert!(f.transport.bodies.lock().is_empty());
        let _ = &f.readers;
    }

    #[test]
    fn a_denylist_entry_that_is_an_alias_a_display_name_or_another_case_still_denies() {
        for entry in ["nickname", "Plain Display", "PLAIN", "Plain"] {
            let f = fixture(
                &format!("[privacy]\ncloud_denied_projects = [\"{entry}\"]\n"),
                None,
            );
            {
                let conn = open_writer(&f.home.db()).unwrap();
                let plain = store_projects::resolve_slug(&conn, "plain")
                    .unwrap()
                    .unwrap();
                store_projects::add_alias(&conn, "nickname", &plain.id).unwrap();
                conn.execute(
                    "UPDATE projects SET name = 'Plain Display' WHERE slug = 'plain'",
                    [],
                )
                .unwrap();
            }
            assert_eq!(
                f.judge.classify_in(&["plain"], "text", &yes_no()),
                Err(JudgeError::Denied),
                "entry {entry:?}"
            );
            // Asked by its alias as well.
            assert_eq!(
                f.judge.classify_in(&["nickname"], "text", &yes_no()),
                Err(JudgeError::Denied),
                "entry {entry:?} by alias"
            );
            f.judge.classify_in(&["client"], "text", &yes_no()).unwrap();
            assert_eq!(f.transport.bodies.lock().len(), 1);
        }
        let f = fixture("[privacy]\ncloud_denied_projects = [\"Client X\"]\n", None);
        {
            let conn = open_writer(&f.home.db()).unwrap();
            store_projects::upsert_project(
                &conn,
                &ProjectRef {
                    key: "k/cx".into(),
                    slug: "client-x".into(),
                    name: "Client X Ltd".into(),
                    root: None,
                    remote: None,
                },
            )
            .unwrap();
        }
        assert_eq!(
            f.judge.classify_in(&["client-x"], "text", &yes_no()),
            Err(JudgeError::Denied),
            "`Client X` denies the slug client-x"
        );
    }

    #[test]
    fn a_degraded_configuration_sends_nothing() {
        // The file does not parse: no valid configuration has loaded.
        let f = fixture("[privacy\nredact = ", None);
        assert!(f.settings.config_degraded());
        for p in ["plain", "client", "global"] {
            assert_eq!(
                f.judge.classify_in(&[p], "text", &yes_no()),
                Err(JudgeError::Denied),
                "{p}"
            );
        }
        assert_eq!(f.judge.classify("text", &yes_no()), Err(JudgeError::Denied));
        assert!(f.transport.bodies.lock().is_empty());
    }

    #[test]
    fn the_denylist_is_read_per_call_so_a_live_change_applies_at_once() {
        let f = fixture("", None);
        f.judge.classify_in(&["plain"], "one", &yes_no()).unwrap();
        let mut change = serde_json::Map::new();
        change.insert(
            "privacy.cloud_denied_projects".into(),
            serde_json::json!(["plain"]),
        );
        f.settings.patch(&change).unwrap();
        assert_eq!(
            f.judge.classify_in(&["plain"], "two", &yes_no()),
            Err(JudgeError::Denied)
        );
        assert_eq!(
            f.judge.classify_in(&["plain"], "one", &yes_no()),
            Err(JudgeError::Denied),
            "not even from the cache"
        );
        assert_eq!(f.transport.bodies.lock().len(), 1);
    }

    #[test]
    fn a_project_custom_pattern_is_redacted_in_the_request_body() {
        let f = fixture(
            "",
            Some(
                "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\n",
            ),
        );
        f.judge
            .classify_in(&["client"], "see TCK-98765 and more", &yes_no())
            .unwrap();
        f.judge
            .classify_in(&["plain"], "see TCK-98765 and more", &yes_no())
            .unwrap();
        let bodies = f.transport.bodies.lock();
        let (a, b) = (bodies[0].to_string(), bodies[1].to_string());
        assert!(
            a.contains("[REDACTED:custom:ticket]") && !a.contains("TCK-98765"),
            "{a}"
        );
        assert!(b.contains("TCK-98765"), "no project rule for `plain`: {b}");
    }

    #[test]
    fn requests_are_redacted_even_when_redaction_is_off_for_storage() {
        let f = fixture("[privacy]\nredact = false\n", None);
        f.judge
            .classify_in(
                &["plain"],
                concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here"),
                &yes_no(),
            )
            .unwrap();
        let sent = f.transport.bodies.lock()[0].to_string();
        assert!(
            !sent.contains("ghp_") && sent.contains("[REDACTED:github_token]"),
            "{sent}"
        );
    }
}
