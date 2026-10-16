//! Compiled redactors: one for the global privacy settings and one per project root whose
//! `.alice.toml` adds rules. Compiling the detectors costs milliseconds, so it is never done
//! per request; the effective [`PrivacySettings`] decide, so a change made through the live
//! settings API, a `config.toml` edit or a project file rebuilds a redactor on the next write.
//!
//! The project rules come from [`SettingsHandle::project_privacy`], i.e. from the one
//! [`alice_core::settings::ProjectSettingsCache`] of the daemon: an invalid project file fails
//! closed, and a loosening waits for three polls. Nothing here reads `.alice.toml` itself.

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
    /// Indexed by `always_on` (see [`Redactors::for_root_always_on`]).
    global: Mutex<[Option<Entry>; 2]>,
    projects: Mutex<HashMap<(PathBuf, bool), Entry>>,
}

impl Redactors {
    /// Redactors over the daemon's live settings.
    pub fn new(settings: Arc<SettingsHandle>) -> Arc<Self> {
        Arc::new(Self::with(Source::Live(settings)))
    }

    /// Redactors for fixed privacy settings, without project files.
    pub fn fixed(privacy: PrivacySettings) -> Arc<Self> {
        Arc::new(Self::with(Source::Fixed(privacy)))
    }

    fn with(source: Source) -> Self {
        Self {
            source,
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

    /// The redactor for a project in the working tree `tree` (`None`: no project). Never fails and never
    /// falls back to "no redaction": if the effective settings do not compile (a pattern that
    /// settings validation should have refused), the built-in detectors run with the configured
    /// switch and mode, and a warning names no pattern.
    pub fn for_root(&self, tree: Option<&ProjectTree>) -> Arc<Redactor> {
        self.cached(tree, false)
    }

    /// [`for_root`](Self::for_root) with the `privacy.redact` switch forced on, whatever it
    /// says: for text that leaves the machine (the Jev judge's requests), which is redacted
    /// even when the person stores raw text. The mode does not matter to the caller, which
    /// applies the redactor and never asks it to refuse.
    pub fn for_root_always_on(&self, tree: Option<&ProjectTree>) -> Arc<Redactor> {
        self.cached(tree, true)
    }

    fn cached(&self, tree: Option<&ProjectTree>, always_on: bool) -> Arc<Redactor> {
        let mut privacy = self.privacy_for(tree);
        // A server tenant always redacts: whatever the settings snapshot says.
        let tenant = matches!(&self.source, Source::Live(s) if s.is_tenant());
        if always_on || tenant {
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
                let r = Self::build(&privacy);
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
                let r = Self::build(&privacy);
                g[always_on as usize] = Some((privacy, r.clone()));
                r
            }
        }
    }

    fn build(privacy: &PrivacySettings) -> Arc<Redactor> {
        Arc::new(Redactor::new(privacy).unwrap_or_else(|e| {
            // The error names the setting, never the pattern.
            tracing::warn!(error = %e, "privacy settings do not compile; using the built-in detectors only");
            Redactor::fallback(privacy)
        }))
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

    #[test]
    fn a_tenant_redacts_even_when_the_settings_say_redaction_is_off() {
        // The environment is the one layer a tenant's rules do not replace with defaults, so a
        // variable can leave `privacy.redact` off in the snapshot; the redactors still redact.
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let env = vec![("ALICE_PRIVACY_REDACT".to_string(), "false".to_string())];
        let s = SettingsHandle::load_with(&home, env.clone(), true);
        assert!(s.is_tenant());
        // Since M2's A-I4 the refused variable makes the snapshot fail closed (redaction on);
        // whatever it says, the redactors of a tenant redact.
        let text = concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here");
        let r = Redactors::new(s);
        assert_eq!(r.for_root(None).apply(text).found, ["github_token"]);
        assert_eq!(
            r.for_root_always_on(None).apply(text).found,
            ["github_token"]
        );
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(
            r.for_root(Some(&ProjectTree::alone(repo.path())))
                .apply(text)
                .found,
            ["github_token"]
        );
        // A local daemon with the same variable stores the text as it is.
        let local = Redactors::new(SettingsHandle::load_with(&home, env, false));
        assert!(local.for_root(None).apply(text).found.is_empty());
    }
}
