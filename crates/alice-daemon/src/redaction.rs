//! Compiled redactors: one for the global privacy settings and one per project root whose
//! `.alice.toml` adds rules. Compiling the detectors costs milliseconds, so it is never done
//! per request; the effective [`PrivacySettings`] decide, so a change made through the live
//! settings API, a `config.toml` edit or a project file rebuilds a redactor on the next write.
//!
//! The project rules come from [`SettingsHandle::project_privacy`], i.e. from the one
//! [`alice_core::settings::ProjectSettingsCache`] of the daemon: an invalid project file fails
//! closed, and a loosening waits for three polls. Nothing here reads `.alice.toml` itself.
//!
//! Storage follows `privacy.redact`; egress never does (owner's decision 2026-10-17). A person's
//! store, a server tenant's included (where the admin pins the switch), keeps secrets as written
//! when `privacy.redact` is off, and [`Redactors::for_root`] then lets them through. Text that
//! leaves for a cloud service takes [`Redactors::for_root_always_on`] whatever the switch says,
//! and whatever the allowlist says of a built-in detector's match (re-review N3: an allowlist is
//! an exception for storage, never a way out for a key), and the shared team store redacts every
//! write ([`Redactors::for_team_store`]): everyone reads it.

use crate::config::SettingsHandle;
use alice_core::redact::Redactor;
use alice_core::settings::{PrivacySettings, ProjectTree};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// More project roots than this cached at once: the cache is dropped and refilled (a daemon
/// serves a handful of repositories; this only bounds a pathological caller).
const MAX_ROOTS: usize = 256;

type Entry = (PrivacySettings, Arc<Redactor>);

enum Source {
    /// The daemon: the live settings and the project files.
    Live(Arc<SettingsHandle>),
    /// A fixed set of rules, no project files (a writer built without settings, tests).
    Fixed(PrivacySettings),
}

pub struct Redactors {
    source: Source,
    /// The team store: `privacy.redact` is on for every write, whatever the settings say.
    forced: bool,
    /// Indexed by `always_on` (see [`Redactors::for_root_always_on`]).
    global: Mutex<[Option<Entry>; 2]>,
    projects: Mutex<HashMap<(PathBuf, bool), Entry>>,
}

impl Redactors {
    /// Redactors over the daemon's live settings.
    pub fn new(settings: Arc<SettingsHandle>) -> Arc<Self> {
        Arc::new(Self::with(Source::Live(settings)))
    }

    /// The team store's redactors: the live settings with `privacy.redact` forced on for every
    /// write, since every tenant reads what it holds.
    pub fn for_team_store(settings: Arc<SettingsHandle>) -> Arc<Self> {
        Arc::new(Self {
            forced: true,
            ..Self::with(Source::Live(settings))
        })
    }

    /// Redactors for fixed privacy settings, without project files.
    pub fn fixed(privacy: PrivacySettings) -> Arc<Self> {
        Arc::new(Self::with(Source::Fixed(privacy)))
    }

    fn with(source: Source) -> Self {
        Self {
            source,
            forced: false,
            global: Mutex::new([None, None]),
            projects: Mutex::new(HashMap::new()),
        }
    }

    /// The privacy rules in force for writes to a project in the working tree `tree` (global
    /// ones for `None`).
    pub fn privacy_for(&self, tree: Option<&ProjectTree>) -> PrivacySettings {
        match &self.source {
            Source::Live(s) => s.project_privacy(tree),
            Source::Fixed(p) => p.clone(),
        }
    }

    /// The redactor for storing text of a project in the working tree `tree` (`None`: no
    /// project): it follows `privacy.redact`, so a store that keeps secrets as written gets one
    /// that lets them through (the team store's never does). Never fails and never falls back to
    /// "no redaction": if the effective settings do not compile (a pattern that settings
    /// validation should have refused), the built-in detectors run with the configured switch
    /// and mode, and a warning names no pattern.
    pub fn for_root(&self, tree: Option<&ProjectTree>) -> Arc<Redactor> {
        self.cached(tree, false)
    }

    /// [`for_root`](Self::for_root) with the `privacy.redact` switch forced on, whatever it
    /// says, and an allowlist that exempts no built-in detector's match
    /// ([`Redactor::for_egress`]): for text that leaves for a cloud service (the Jev judge's and
    /// distillation's requests, a remote embedder's input) or for the team store, which is
    /// redacted even when the person stores secrets as written. The mode does not matter to the
    /// caller, which applies the redactor and never asks it to refuse.
    pub fn for_root_always_on(&self, tree: Option<&ProjectTree>) -> Arc<Redactor> {
        self.cached(tree, true)
    }

    fn cached(&self, tree: Option<&ProjectTree>, always_on: bool) -> Arc<Redactor> {
        let mut privacy = self.privacy_for(tree);
        if always_on || self.forced {
            privacy.redact = true;
        }
        match tree {
            Some(tree) => {
                let key = std::fs::canonicalize(&tree.root).unwrap_or_else(|_| tree.root.clone());
                let mut cache = self.projects.lock();
                if let Some((p, r)) = cache.get(&(key.clone(), always_on))
                    && *p == privacy
                {
                    return r.clone();
                }
                let r = Self::build(&privacy, always_on);
                if cache.len() >= MAX_ROOTS {
                    cache.clear();
                }
                cache.insert((key, always_on), (privacy, r.clone()));
                r
            }
            None => {
                let mut g = self.global.lock();
                if let Some((p, r)) = g[always_on as usize].as_ref()
                    && *p == privacy
                {
                    return r.clone();
                }
                let r = Self::build(&privacy, always_on);
                g[always_on as usize] = Some((privacy, r.clone()));
                r
            }
        }
    }

    fn build(privacy: &PrivacySettings, egress: bool) -> Arc<Redactor> {
        let r = Redactor::new(privacy).unwrap_or_else(|e| {
            // The error names the setting, never the pattern.
            tracing::warn!(error = %e, "privacy settings do not compile; using the built-in detectors only");
            Redactor::fallback(privacy)
        });
        Arc::new(if egress { r.for_egress() } else { r })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AliceHome;
    use alice_core::settings::{CustomPattern, RedactMode};

    fn handle(config: &str) -> (tempfile::TempDir, Arc<SettingsHandle>) {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(home.config_file(), config).unwrap();
        let s = SettingsHandle::load_with_env(&home, Vec::new());
        (d, s)
    }

    #[test]
    fn a_redactor_is_compiled_once_per_setting_and_rebuilt_when_the_setting_changes() {
        let (_d, s) = handle("");
        let r = Redactors::new(s.clone());
        let a = r.for_root(None);
        assert!(Arc::ptr_eq(&a, &r.for_root(None)), "reused");
        let mut change = serde_json::Map::new();
        change.insert("privacy.redact_mode".into(), serde_json::json!("block"));
        s.patch(&change).unwrap();
        let b = r.for_root(None);
        assert!(
            !Arc::ptr_eq(&a, &b) && b.blocks(),
            "rebuilt for the new mode"
        );
    }

    #[test]
    fn a_project_file_adds_rules_and_a_broken_one_fails_closed() {
        let (_d, s) = handle("");
        let r = Redactors::new(s);
        let repo = tempfile::tempdir().unwrap();
        let file = repo.path().join(".alice.toml");
        assert!(
            !r.for_root(Some(&ProjectTree::alone(repo.path()))).blocks(),
            "no file, no rules"
        );
        std::fs::write(
            &file,
            "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]{4,}\" }]\n",
        )
        .unwrap();
        let red = r.for_root(Some(&ProjectTree::alone(repo.path())));
        assert_eq!(red.apply("see TCK-1234").found, ["custom:ticket"]);
        // A typo in the file must not drop the project's rules: blocked, last patterns kept.
        std::fs::write(&file, "[privacy\nblock_secrets = tru").unwrap();
        let red = r.for_root(Some(&ProjectTree::alone(repo.path())));
        assert!(red.blocks(), "an invalid project file blocks secrets");
        assert!(red.check("content", "see TCK-1234").is_err());
    }

    #[test]
    fn the_always_on_redactor_redacts_even_when_redaction_is_off() {
        let r = Redactors::fixed(PrivacySettings {
            redact: false,
            ..Default::default()
        });
        let text = concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here");
        assert_eq!(r.for_root(None).apply(text).text, text, "off for storage");
        let on = r.for_root_always_on(None);
        assert_eq!(on.apply(text).found, ["github_token"]);
        assert!(Arc::ptr_eq(&on, &r.for_root_always_on(None)), "reused");
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(
            r.for_root_always_on(Some(&ProjectTree::alone(repo.path())))
                .apply(text)
                .found,
            ["github_token"]
        );
        assert!(
            r.for_root(Some(&ProjectTree::alone(repo.path())))
                .apply(text)
                .found
                .is_empty()
        );
    }

    /// Re-review N3: an allowlist (even one that allows everything) exempts nothing a built-in
    /// detector finds in text that leaves for the cloud; it still exempts a custom pattern's
    /// match there, and storage follows it as before.
    #[test]
    fn egress_ignores_the_allowlist_for_the_built_in_detectors() {
        let r = Redactors::fixed(PrivacySettings {
            allowlist: vec!["(?s).*".into()],
            custom_patterns: vec![CustomPattern {
                name: "ticket".into(),
                regex: "TCK-[0-9]{4,}".into(),
            }],
            ..Default::default()
        });
        let text = concat!(
            "token gh",
            "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 for TCK-1234"
        );
        assert_eq!(r.for_root(None).apply(text).text, text, "storage: allowed");
        let out = r.for_root_always_on(None).apply(text);
        assert_eq!(out.found, ["github_token"]);
        assert!(
            out.text.contains("TCK-1234"),
            "a custom pattern's exception holds"
        );
        let repo = tempfile::tempdir().unwrap();
        let tree = ProjectTree::alone(repo.path());
        assert_eq!(
            r.for_root_always_on(Some(&tree)).apply(text).found,
            ["github_token"]
        );
    }

    #[test]
    fn settings_that_do_not_compile_still_redact() {
        let r = Redactors::fixed(PrivacySettings {
            custom_patterns: vec![CustomPattern {
                name: "bad".into(),
                regex: "(".into(),
            }],
            redact_mode: RedactMode::Block,
            ..Default::default()
        });
        let red = r.for_root(None);
        assert!(red.check("content", "password = hunter2").is_err());
    }

    /// Owner's decision 2026-10-17: a tenant whose admin pinned `ALICE_PRIVACY_REDACT=false`
    /// stores secrets as written, like a local home that turned redaction off; what leaves for
    /// the cloud is redacted all the same, and the team store redacts every write.
    #[test]
    fn a_tenant_follows_the_admins_pin_and_the_team_store_always_redacts() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let env = vec![("ALICE_PRIVACY_REDACT".to_string(), "false".to_string())];
        let s = SettingsHandle::load_with(&home, env.clone(), true);
        assert!(s.is_tenant());
        let text = concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here");
        let repo = tempfile::tempdir().unwrap();
        let tree = ProjectTree::alone(repo.path());

        let kept = Redactors::new(s.clone());
        assert_eq!(
            kept.for_root(None).apply(text).text,
            text,
            "stored as written"
        );
        assert_eq!(kept.for_root(Some(&tree)).apply(text).text, text);
        assert_eq!(
            kept.for_root_always_on(None).apply(text).found,
            ["github_token"],
            "egress is redacted"
        );

        let team = Redactors::for_team_store(s);
        assert_eq!(team.for_root(None).apply(text).found, ["github_token"]);
        assert_eq!(
            team.for_root(Some(&tree)).apply(text).found,
            ["github_token"]
        );

        // Without the pin a tenant redacts, as the defaults say.
        let d2 = tempfile::tempdir().unwrap();
        let plain =
            SettingsHandle::load_with(&AliceHome::new(d2.path().to_path_buf()), Vec::new(), true);
        assert_eq!(
            Redactors::new(plain).for_root(None).apply(text).found,
            ["github_token"]
        );
    }
}
