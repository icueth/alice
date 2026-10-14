//! The daemon's live settings: `config.toml` + environment, re-read when the file changes
//! (polled), changed through `patch` (which edits the file and keeps its comments). An invalid
//! file, patch or variable never replaces valid settings; the errors are kept for
//! `alice config list`, `/api/v1/settings` and `alice doctor`.
//!
//! # Fail closed
//!
//! Settings that protect privacy are never loosened by a file that may be half written or
//! broken:
//!
//! - a change that **loosens** a protective key (see
//!   [`alice_core::settings::loosened_protections`]) or empties or removes a non-empty
//!   `config.toml` applies only when the same content is read on three polls in a row
//!   ([`alice_core::settings::LooseningGate`]); a change that tightens, or a `patch`, applies at
//!   once;
//! - a `patch` refuses to build a file from a read that is mid-write or not applied yet;
//! - a leaf of `config.toml` that is invalid costs only itself ([`alice_core::settings::resolve_salvaging`]),
//!   and while no valid file has ever loaded in this process [`SettingsHandle::config_degraded`]
//!   is `true`. **Cloud consumers (the Jev judge, HTTP embedders) must treat that as "no cloud"**:
//!   a typo in `privacy.cloud_denied_projects` itself would otherwise mean an empty denylist.
//!
//! Only `ALICE_*` variables are looked at, and only the values of setting variables are kept:
//! a secret in `ALICE_EMBEDDING_API_KEY` (or one put in a misnamed variable) is never held,
//! logged or shown here.

use crate::paths::AliceHome;
use alice_core::settings::{
    self, LooseningGate, ProjectSettingsCache, Resolved, SettingError, Settings, Source,
    loosened_protections,
};
use parking_lot::{Mutex, RwLock};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the daemon polls `config.toml`; a look at a project's `.alice.toml` counts as a
/// poll at most this often.
pub const SETTINGS_POLL: Duration = Duration::from_secs(2);

/// How long a changed `config.toml` must hold still before it is read as the new settings. An
/// editor that truncates the file and then writes it is not caught half-way: an empty file is
/// valid and would mean every default (an empty `cloud_denied_projects` among them) for a
/// moment.
const SETTLE: Duration = Duration::from_millis(30);

pub use alice_core::api::PatchOutcome;

fn err(key: &str, message: String) -> SettingError {
    SettingError {
        key: key.into(),
        message,
    }
}

/// The environment as the settings see it: `ALICE_*` names, with values only for setting
/// variables that are valid, alone and together. A variable that is not a setting keeps its
/// name only (enough for the warning about it), so its value (perhaps a secret) is never held.
struct EnvLayer {
    vars: Vec<(String, String)>,
    /// Setting variables set aside because their values are invalid.
    errors: Vec<SettingError>,
}

impl EnvLayer {
    fn new(env: &[(String, String)]) -> Self {
        let mut vars = Vec::new();
        let mut errors = Vec::new();
        for (k, v) in env.iter().filter(|(k, _)| k.starts_with("ALICE_")) {
            if settings::key_for_env_var(k).is_none() {
                vars.push((k.clone(), String::new()));
                continue;
            }
            match settings::resolve(None, &[(k.clone(), v.clone())]) {
                Ok(_) => vars.push((k.clone(), v.clone())),
                Err(e) => errors.extend(e),
            }
        }
        // Valid one by one, invalid together (a review threshold above the auto threshold): set
        // aside the variables the errors name, and look again. An unrelated variable keeps
        // working.
        while let Err(e) = settings::resolve(None, &vars) {
            let named: Vec<&str> = e.iter().map(|x| x.key.as_str()).collect();
            let is_setting = |k: &str| settings::key_for_env_var(k).is_some();
            let mut aside: Vec<String> = vars
                .iter()
                .filter(|(k, _)| {
                    settings::key_for_env_var(k).is_some_and(|key| named.contains(&key))
                })
                .map(|(k, _)| k.clone())
                .collect();
            if aside.is_empty() {
                // The errors do not point at a variable: none of them can be trusted.
                aside = vars
                    .iter()
                    .filter(|(k, _)| is_setting(k))
                    .map(|(k, _)| k.clone())
                    .collect();
            }
            let names = aside.join(", ");
            errors.extend(e.into_iter().map(|x| SettingError {
                message: format!(
                    "{} (from the environment; {names} {} ignored until this is fixed)",
                    x.message,
                    if aside.len() == 1 { "is" } else { "are" }
                ),
                key: x.key,
            }));
            vars.retain(|(k, _)| !aside.contains(k));
        }
        Self { vars, errors }
    }

    fn names(&self) -> Vec<&str> {
        self.vars.iter().map(|(k, _)| k.as_str()).collect()
    }
}

/// The process's `ALICE_*` variables; values only for setting variables.
fn process_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            if !k.starts_with("ALICE_") {
                return None;
            }
            let v = match settings::key_for_env_var(&k) {
                Some(_) => v.to_string_lossy().into_owned(),
                None => String::new(),
            };
            Some((k, v))
        })
        .collect()
}

/// `config.toml`'s text; `None` when there is none. Not text, or unreadable: an error.
fn read_config(path: &Path) -> Result<Option<String>, SettingError> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| {
            err(
                "config.toml",
                format!("{} is not UTF-8 text; save it as UTF-8", path.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(
            "config.toml",
            format!("cannot read {}: {e}", path.display()),
        )),
    }
}

type Read = Result<Option<String>, SettingError>;

fn fingerprint(read: &Read) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match read {
        Ok(None) => 0u8.hash(&mut h),
        Ok(Some(text)) => (1u8, text).hash(&mut h),
        Err(e) => (2u8, &e.message).hash(&mut h),
    }
    h.finish()
}

/// Whether a read holds any settings text (a blank file is as good as none).
fn has_text(read: &Read) -> bool {
    matches!(read, Ok(Some(t)) if !t.trim().is_empty())
}

/// What the handle last acted on: the file's fingerprint and whether it had text.
#[derive(Clone, Copy)]
struct Seen {
    fingerprint: u64,
    had_text: bool,
}

impl Seen {
    fn of(read: &Read) -> Self {
        Self {
            fingerprint: fingerprint(read),
            had_text: has_text(read),
        }
    }
}

/// Two reads `settle` apart; `None` unless they agree (the file is still being written).
fn settled<T: PartialEq>(mut read: impl FnMut() -> T, settle: Duration) -> Option<T> {
    let first = read();
    std::thread::sleep(settle);
    (read() == first).then_some(first)
}

fn default_resolved() -> Resolved {
    settings::resolve(None, &[]).expect("defaults are valid")
}

/// The file layer over the environment. A leaf of the file that is invalid costs only itself
/// (the privacy leaves that are valid always apply); the errors say what was left out. A file
/// that cannot be read or does not parse leaves the defaults plus the (already checked)
/// environment.
fn resolve_layers(read: &Read, env: &EnvLayer) -> (Resolved, Vec<SettingError>) {
    match read {
        Ok(text) => settings::resolve_salvaging(text.as_deref(), &env.vars),
        Err(e) => (
            settings::resolve(None, &env.vars).unwrap_or_else(|_| default_resolved()),
            vec![e.clone()],
        ),
    }
}

fn log_problems(errors: &[SettingError], warnings: &[String]) {
    for e in errors {
        tracing::warn!(key = %e.key, message = %e.message, "invalid setting; the last valid value (or the default) stays in effect");
    }
    for w in warnings {
        tracing::warn!("{w}");
    }
}

/// Settings as a process outside the running daemon sees them.
#[derive(Debug, Clone)]
pub struct Effective {
    pub resolved: Resolved,
    /// Problems in `config.toml` or in setting variables. A bad leaf of the file is left out
    /// (the rest applies); an invalid variable is left out and the rest still applies.
    pub errors: Vec<SettingError>,
    /// `config.toml` could not be read, did not parse, or had leaves that were left out. A
    /// leaf left out may be a privacy one (a typo inside `cloud_denied_projects` itself), so
    /// a cloud consumer (Jev, an HTTP embedder) must treat `true` as "no cloud".
    pub degraded: bool,
}

/// `config.toml` + the process's `ALICE_*` variables, resolved over the defaults. Never fails.
/// The one rule for code that cannot ask the daemon: spool redaction (Task 7), stdio MCP
/// (Task 11), restore (Task 15) and doctor (Task 17). The daemon loads the same way.
pub fn load_effective(home: &AliceHome) -> Effective {
    effective(&home.config_file(), &process_env())
}

fn effective(file: &Path, env: &[(String, String)]) -> Effective {
    let env = EnvLayer::new(env);
    let (resolved, mut errors) = resolve_layers(&read_config(file), &env);
    let degraded = !errors.is_empty();
    errors.extend(env.errors.iter().cloned());
    log_problems(&errors, &resolved.warnings);
    Effective {
        resolved,
        errors,
        degraded,
    }
}

pub(crate) fn changed_keys(a: &Settings, b: &Settings) -> Vec<String> {
    let a = settings::leaves(&serde_json::to_value(a).expect("settings serialise"));
    let b: std::collections::BTreeMap<_, _> =
        settings::leaves(&serde_json::to_value(b).expect("settings serialise"))
            .into_iter()
            .collect();
    a.into_iter()
        .filter(|(k, v)| b.get(k) != Some(v))
        .map(|(k, _)| k)
        .collect()
}

/// The settings-change loop: `on_change(changed keys, protective keys that got weaker)` for
/// every change that took effect.
/// After missed notifications (`Lagged`) it diffs the current settings against the last ones it
/// saw: it never stops on a lag, only when the channel closes.
pub(crate) fn follow_settings(
    mut rx: tokio::sync::broadcast::Receiver<Vec<String>>,
    start: Arc<Settings>,
    current: impl Fn() -> Arc<Settings>,
    mut on_change: impl FnMut(Vec<String>, Vec<String>),
) {
    use tokio::sync::broadcast::error::RecvError;
    let mut last = start;
    loop {
        let changed = match rx.blocking_recv() {
            Ok(changed) => changed,
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(
                    missed,
                    "settings notifications lagged; re-reading the current settings"
                );
                changed_keys(&last, &current())
            }
            Err(RecvError::Closed) => break,
        };
        let now = current();
        let loosened = loosened_protections(&last, &now);
        last = now;
        if !changed.is_empty() {
            on_change(changed, loosened);
        }
    }
}

/// Where a write of `path` goes: through a symlink to its target (a config file kept in a
/// dotfiles repository stays linked).
fn write_target(path: &Path) -> PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

/// Written to a new file next to `target` (with `target`'s permissions), synced, then renamed
/// over it: a reader sees the old file or the new one, never a half-written one.
fn write_atomically(target: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let tmp = target.with_file_name(format!(".{name}.{}.tmp", alice_core::ids::new_id()));
    let perms = std::fs::metadata(target).ok().map(|m| m.permissions());
    let done = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create_new(&tmp)?;
        f.write_all(text.as_bytes())?;
        if let Some(p) = perms {
            f.set_permissions(p)?;
        }
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, target)
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// One consistent view: the settings and where they came from.
struct Snapshot {
    settings: Arc<Settings>,
    resolved: Arc<Resolved>,
}

impl Snapshot {
    fn new(r: Resolved) -> Self {
        Self {
            settings: Arc::new(r.settings.clone()),
            resolved: Arc::new(r),
        }
    }
}

pub struct SettingsHandle {
    file: PathBuf,
    env: EnvLayer,
    snapshot: RwLock<Snapshot>,
    /// Problems with `config.toml` (cleared by the next good read or patch).
    file_errors: RwLock<Vec<SettingError>>,
    /// The `config.toml` last read or written. A loosening that is waiting is not recorded
    /// here, so it is looked at again on every poll.
    seen: Mutex<Seen>,
    /// Holds back loosenings until the same content is read on three polls in a row.
    gate: Mutex<LooseningGate>,
    /// A `config.toml` without errors (or none) has loaded in this process.
    ever_valid: AtomicBool,
    /// Per-repository `.alice.toml`, failing closed.
    projects: ProjectSettingsCache,
    /// Serialises reloads and patches.
    write: Mutex<()>,
    changes: tokio::sync::broadcast::Sender<Vec<String>>,
}

/// Names and problems only (the variables' values are not shown).
impl std::fmt::Debug for SettingsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsHandle")
            .field("file", &self.file)
            .field("env", &self.env.names())
            .field("errors", &self.errors())
            .field("config_degraded", &self.config_degraded())
            .finish_non_exhaustive()
    }
}

impl SettingsHandle {
    /// The settings for `home` with the process's `ALICE_*` variables. Never fails.
    pub fn load(home: &AliceHome) -> Arc<Self> {
        Self::load_with_env(home, process_env())
    }

    /// The same with the environment passed in (tests).
    pub fn load_with_env(home: &AliceHome, env: Vec<(String, String)>) -> Arc<Self> {
        let file = home.config_file();
        let env = EnvLayer::new(&env);
        let read = read_config(&file);
        let (resolved, file_errors) = resolve_layers(&read, &env);
        log_problems(
            &file_errors
                .iter()
                .chain(&env.errors)
                .cloned()
                .collect::<Vec<_>>(),
            &resolved.warnings,
        );
        let (changes, _) = tokio::sync::broadcast::channel(16);
        Arc::new(Self {
            seen: Mutex::new(Seen::of(&read)),
            gate: Mutex::new(LooseningGate::default()),
            ever_valid: AtomicBool::new(file_errors.is_empty()),
            projects: ProjectSettingsCache::new(SETTINGS_POLL),
            file,
            env,
            snapshot: RwLock::new(Snapshot::new(resolved)),
            file_errors: RwLock::new(file_errors),
            write: Mutex::new(()),
            changes,
        })
    }

    /// The settings in effect (cheap: a shared pointer).
    pub fn get(&self) -> Arc<Settings> {
        self.snapshot.read().settings.clone()
    }

    /// The settings with each key's source and the ignored variables.
    pub fn resolved(&self) -> Arc<Resolved> {
        self.snapshot.read().resolved.clone()
    }

    /// Why `config.toml` or a setting variable is not in effect; empty when all is well.
    pub fn errors(&self) -> Vec<SettingError> {
        self.file_errors
            .read()
            .iter()
            .chain(&self.env.errors)
            .cloned()
            .collect()
    }

    /// `true` while no valid `config.toml` has loaded in this process: it did not parse, could
    /// not be read, or had leaves left out (see [`alice_core::settings::resolve_salvaging`]),
    /// and nothing better has been loaded since. A left-out leaf may be a privacy one (a typo
    /// inside `privacy.cloud_denied_projects` itself leaves the denylist empty), so **cloud
    /// consumers must treat `true` as "no cloud"**: the Jev judge (Task 9) sends nothing and
    /// HTTP embedders (Task 16) are not used. `/api/v1/settings`, health and `alice doctor`
    /// show it next to [`errors`](Self::errors).
    ///
    /// Once a valid file has loaded it stays `false`: a later invalid edit is not applied at
    /// all, so the last valid settings (denylist included) remain in force.
    pub fn config_degraded(&self) -> bool {
        !self.ever_valid.load(Ordering::SeqCst)
    }

    /// May content of `project_slug` go to a cloud provider (the Jev judge, an HTTP embedder)?
    /// `project_root` is the repository's root when the project has one.
    ///
    /// **Tasks 9 and 16 must use only this helper** to decide, once per call that would leave
    /// the machine; no consumer checks `privacy.cloud_denied_projects`, `.alice.toml` or
    /// [`config_degraded`](Self::config_degraded) itself. It answers `false` (fail closed) when
    /// the configuration is degraded, the slug is empty, the global list names the project, the
    /// project's `.alice.toml` sets `cloud_denied`, or that file is broken, unreadable or blank
    /// on its first look. A project file that would loosen waits for three spaced looks
    /// ([`ProjectSettingsCache`]).
    pub fn cloud_allowed(&self, project_slug: &str, project_root: Option<&Path>) -> bool {
        self.cloud_allowed_with(project_slug, project_root, &self.projects)
    }

    /// [`cloud_allowed`](Self::cloud_allowed) over a project cache of the caller's (tests with
    /// a clock of their own). Not public: a caller with a cache of its own would have a gate of
    /// its own, and a repository would be looked at twice.
    pub(crate) fn cloud_allowed_with(
        &self,
        project_slug: &str,
        project_root: Option<&Path>,
        projects: &ProjectSettingsCache,
    ) -> bool {
        if self.config_degraded() || project_slug.trim().is_empty() {
            return false;
        }
        let project = project_root.map(|root| projects.get(root).settings);
        let effective = project.unwrap_or_default().apply_to(&self.get().privacy);
        !effective.is_cloud_denied(project_slug)
    }

    /// The privacy rules for writes to a project: the global ones with the project's
    /// `.alice.toml` on top (`root`: its working-tree root; `None`: the global ones alone).
    ///
    /// Read from the one [`ProjectSettingsCache`] this handle owns, so an invalid or unreadable
    /// project file fails closed (`block_secrets` on, the last good custom patterns kept) and a
    /// loosening applies only after three spaced polls, exactly as for
    /// [`cloud_allowed`](Self::cloud_allowed). The daemon never reads `.alice.toml` any other way.
    pub fn project_privacy(&self, root: Option<&Path>) -> alice_core::settings::PrivacySettings {
        let global = self.get().privacy.clone();
        match root {
            Some(root) => self.projects.get(root).settings.apply_to(&global).privacy,
            None => global,
        }
    }

    /// Whether the privacy rules for `root` are in a fail-closed state right now rather than
    /// the ones a person configured: `config.toml` is degraded, or the project's `.alice.toml`
    /// is invalid or unreadable, or blank on its first look, or a loosening of it (or of `config.toml`) is
    /// waiting for its polls. A write refused under such rules may be accepted a moment later.
    pub fn project_fail_closed(&self, root: Option<&Path>) -> bool {
        self.config_degraded()
            || self.loosening_waiting()
            || root.is_some_and(|r| {
                let l = self.projects.get(r);
                l.error.is_some() || l.loosening_pending
            })
    }

    /// A loosening in `config.toml` (a protection switched off, say) was read and is waiting to
    /// be read the same on three polls in a row before it applies; the settings view says so.
    pub fn loosening_waiting(&self) -> bool {
        self.gate.lock().is_pending()
    }

    /// Ignored `ALICE_*` variables (reported in health and `alice config list`).
    pub fn warnings(&self) -> Vec<String> {
        self.snapshot.read().resolved.warnings.clone()
    }

    /// Changed keys, after every change that took effect.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Vec<String>> {
        self.changes.subscribe()
    }

    fn swap(&self, r: Resolved, errors: Vec<SettingError>) -> Vec<String> {
        let changed = changed_keys(&self.snapshot.read().settings, &r.settings);
        *self.snapshot.write() = Snapshot::new(r);
        *self.file_errors.write() = errors;
        if !changed.is_empty() {
            let _ = self.changes.send(changed.clone());
        }
        changed
    }

    /// Re-read `config.toml` if it changed on disk (by content, not by time). `Some(changed
    /// keys)` when new settings took effect; `None` when nothing changed, the file is still
    /// being written, it is invalid (the last good settings stay, and `errors` says why), or it
    /// loosens a protection and has not yet been read the same on three polls in a row.
    pub fn reload_if_changed(&self) -> Option<Vec<String>> {
        let _w = self.write.lock();
        if fingerprint(&read_config(&self.file)) == self.seen.lock().fingerprint {
            self.gate.lock().reset();
            return None;
        }
        // Still being written: the next poll looks again.
        let read = settled(|| read_config(&self.file), SETTLE)?;
        self.observe(read, true)
    }

    /// Turn one settled read of `config.toml` into settings, or not. `counts_as_poll` is false
    /// for a `patch` looking at the file: it must not move a waiting loosening along.
    fn observe(&self, read: Read, counts_as_poll: bool) -> Option<Vec<String>> {
        let seen = *self.seen.lock();
        let now = Seen::of(&read);
        if now.fingerprint == seen.fingerprint {
            self.gate.lock().reset();
            return None;
        }
        let (candidate, errors) = resolve_layers(&read, &self.env);
        let ever_valid = self.ever_valid.load(Ordering::SeqCst);
        if !errors.is_empty() && ever_valid {
            // An invalid edit is not applied, not even its valid parts: the last good settings
            // stay exactly as they were. The same error is not logged on every poll.
            for e in &errors {
                tracing::warn!(key = %e.key, message = %e.message, "config.toml is invalid; keeping the previous settings");
            }
            *self.file_errors.write() = errors;
            *self.seen.lock() = now;
            self.gate.lock().reset();
            return None;
        }
        // An emptied or removed file counts as loosening even when it holds nothing protective:
        // it is the shape of a half-written save.
        let loosens = !loosened_protections(&self.get(), &candidate.settings).is_empty()
            || (seen.had_text && !now.had_text);
        if loosens && !counts_as_poll {
            return None;
        }
        if !self.gate.lock().admit(now.fingerprint, loosens) {
            tracing::warn!(
                "config.toml loosens a protection; applying it once it reads the same on 3 polls in a row"
            );
            return None;
        }
        *self.seen.lock() = now;
        if errors.is_empty() {
            self.ever_valid.store(true, Ordering::SeqCst);
        } else {
            for e in &errors {
                tracing::warn!(key = %e.key, message = %e.message, "config.toml has invalid settings; they are left out and the rest applies");
            }
        }
        Some(self.swap(candidate, errors))
    }

    /// Apply dotted-key changes (`null` removes a key from config.toml). All or nothing: on an
    /// error the file and the settings stay as they were. Refused while `config.toml` is being
    /// written or holds a change that has not been applied yet (a truncated file, or a
    /// loosening waiting for its three polls): a file built from such a read would drop
    /// whatever the truncated part held.
    pub fn patch(
        &self,
        changes: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PatchOutcome, Vec<SettingError>> {
        if changes.is_empty() {
            return Ok(PatchOutcome::default());
        }
        let _w = self.write.lock();
        let busy = || {
            vec![err(
                "config.toml",
                "changed on disk and that change is not applied yet (a program may still be writing it, or it removes protections and is waiting to hold still); wait a few seconds, then try again".into(),
            )]
        };
        let read = settled(|| read_config(&self.file), SETTLE).ok_or_else(busy)?;
        if let Err(e) = &read {
            return Err(vec![e.clone()]);
        }
        if fingerprint(&read) != self.seen.lock().fingerprint {
            // Changed since the last poll: the normal rules decide whether it is in force.
            self.observe(read.clone(), false);
            if fingerprint(&read) != self.seen.lock().fingerprint {
                return Err(busy());
            }
        }
        let mut text = read.map_err(|e| vec![e])?.unwrap_or_default();
        for (k, v) in changes {
            text = settings::edit_toml(&text, k, v).map_err(|e| vec![e])?;
        }
        let r = settings::resolve(Some(&text), &self.env.vars)?;
        let target = write_target(&self.file);
        if let Err(e) = write_atomically(&target, &text) {
            return Err(vec![err(
                "config.toml",
                format!("cannot write {}: {e}", target.display()),
            )]);
        }
        *self.seen.lock() = Seen::of(&Ok(Some(text)));
        self.gate.lock().reset();
        self.ever_valid.store(true, Ordering::SeqCst);
        let overridden_by_env = changes
            .keys()
            .filter(|k| matches!(r.sources.get(k.as_str()), Some(Source::Env { .. })))
            .cloned()
            .collect();
        let changed = self.swap(r, Vec::new());
        let restart_required = changes
            .keys()
            .filter(|k| settings::RESTART_KEYS.contains(&k.as_str()) && changed.contains(*k))
            .cloned()
            .collect();
        Ok(PatchOutcome {
            changed,
            restart_required,
            overridden_by_env,
        })
    }

    /// Poll `config.toml` every `every` (the first look is one interval after the start) until
    /// `stop` is set.
    pub fn spawn_watcher(
        self: &Arc<Self>,
        every: Duration,
        stop: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("alice-config".into())
            .spawn(move || {
                // Wait first: the file was read when the handle loaded.
                while !stop.load(Ordering::Relaxed) {
                    let until = std::time::Instant::now() + every;
                    while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(every.min(Duration::from_millis(50)));
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(changed) = me.reload_if_changed() {
                        tracing::info!(?changed, "settings reloaded from config.toml");
                    }
                }
            })
            .expect("spawn config watcher")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::settings::RedactMode;

    fn home() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        h.ensure_dirs().unwrap();
        (d, h)
    }

    fn one(key: &str, value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(key.into(), value);
        m
    }

    #[test]
    fn an_invalid_file_at_start_means_defaults_and_a_visible_error() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 2\n").unwrap();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert_eq!(s.get().judge.review_threshold, 0.5);
        assert!(s.errors().iter().any(|e| e.key == "judge.review_threshold"));
    }

    #[test]
    fn load_effective_reads_the_file_and_the_environment_like_the_daemon() {
        let (_d, h) = home();
        std::fs::write(
            h.config_file(),
            "[privacy]\ncloud_denied_projects = [\"p\"]\n",
        )
        .unwrap();
        let e = effective(
            &h.config_file(),
            &[
                ("ALICE_JUDGE_REVIEW_THRESHOLD".into(), "0.7".into()),
                ("ALICE_NOPE".into(), "1".into()),
            ],
        );
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        assert_eq!(e.resolved.settings.privacy.cloud_denied_projects, vec!["p"]);
        assert_eq!(e.resolved.settings.judge.review_threshold, 0.7);
        assert_eq!(e.resolved.warnings.len(), 1);
        std::fs::write(h.config_file(), "[judge\n").unwrap();
        let e = effective(
            &h.config_file(),
            &[("ALICE_JUDGE_REVIEW_THRESHOLD".into(), "0.7".into())],
        );
        assert_eq!(e.errors[0].key, "config.toml");
        assert_eq!(
            e.resolved.settings.judge.review_threshold, 0.7,
            "a broken file keeps the valid environment"
        );
        let s = SettingsHandle::load_with_env(&h, vec![("ALICE_NOPE".into(), "1".into())]);
        assert_eq!(
            s.warnings().len(),
            1,
            "the handle reports ignored variables too"
        );
    }

    #[test]
    fn patch_validates_writes_the_file_and_notifies() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "# keep me\n").unwrap();
        let s = SettingsHandle::load_with_env(
            &h,
            vec![("ALICE_SEARCH_RELEVANCE_FLOOR".into(), "0.8".into())],
        );
        let mut rx = s.subscribe();
        let mut m = serde_json::Map::new();
        m.insert("judge.review_threshold".into(), serde_json::json!(0.6));
        m.insert("search.relevance_floor".into(), serde_json::json!(0.9));
        m.insert("server.port".into(), serde_json::json!(4545));
        let out = s.patch(&m).unwrap();
        assert_eq!(s.get().judge.review_threshold, 0.6);
        assert_eq!(
            s.get().search.relevance_floor,
            0.8,
            "the environment still wins"
        );
        assert_eq!(
            out.overridden_by_env,
            vec!["search.relevance_floor".to_string()]
        );
        assert_eq!(out.restart_required, vec!["server.port".to_string()]);
        let text = std::fs::read_to_string(h.config_file()).unwrap();
        assert!(
            text.contains("# keep me") && text.contains("review_threshold = 0.6"),
            "{text}"
        );
        assert!(
            rx.try_recv()
                .unwrap()
                .contains(&"judge.review_threshold".to_string())
        );
        assert!(
            s.reload_if_changed().is_none(),
            "its own write is not a change on disk"
        );

        let errs = s
            .patch(&one("judge.auto_apply_threshold", serde_json::json!(0.1)))
            .unwrap_err();
        assert_eq!(errs[0].key, "judge.review_threshold");
        assert_eq!(
            std::fs::read_to_string(h.config_file()).unwrap(),
            text,
            "file untouched"
        );
        assert!(
            s.patch(&one("storage.data_dir", serde_json::json!("/tmp")))
                .is_err()
        );
        assert!(rx.try_recv().is_err(), "a refused patch notifies nobody");
    }

    #[test]
    fn an_empty_patch_writes_nothing() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        let out = s.patch(&serde_json::Map::new()).unwrap();
        assert!(out.changed.is_empty() && out.restart_required.is_empty());
        assert!(!h.config_file().exists());
    }

    #[test]
    fn file_edits_apply_and_a_half_written_file_keeps_the_last_good_settings() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 0.6\n").unwrap();
        assert_eq!(
            s.reload_if_changed().unwrap(),
            vec!["judge.review_threshold".to_string()]
        );
        assert_eq!(s.get().judge.review_threshold, 0.6);
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 0.").unwrap();
        assert!(s.reload_if_changed().is_none());
        assert_eq!(
            s.get().judge.review_threshold,
            0.6,
            "last good settings stay"
        );
        assert!(s.errors().iter().any(|e| e.key == "config.toml"));
        // Fixed: the error goes away.
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 0.6\n").unwrap();
        assert_eq!(s.reload_if_changed(), Some(vec![]));
        assert!(s.errors().is_empty(), "{:?}", s.errors());
    }

    /// Same length, same modification time (coarse clocks, fast editors): still a change.
    #[test]
    fn an_edit_that_keeps_the_size_and_the_time_is_still_seen() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 0.6\n").unwrap();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        let before = std::fs::metadata(h.config_file())
            .unwrap()
            .modified()
            .unwrap();
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 0.7\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(h.config_file())
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert_eq!(
            s.reload_if_changed(),
            Some(vec!["judge.review_threshold".to_string()])
        );
    }

    #[test]
    fn a_read_counts_only_when_the_file_holds_still() {
        let reads = |seq: &[&str]| {
            let mut it = seq
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .into_iter();
            settled(move || it.next(), Duration::ZERO)
        };
        assert_eq!(
            reads(&["", "[judge]\nprovider = \"jev\"\n"]),
            None,
            "an editor truncated the file and is writing it"
        );
        assert_eq!(reads(&["a", "a"]), Some(Some("a".to_string())));
    }

    #[test]
    fn a_bad_variable_keeps_the_file_and_later_edits_still_apply() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 0.6\n").unwrap();
        let vars = vec![
            (
                "ALICE_SEARCH_RELEVANCE_FLOOR".to_string(),
                "very".to_string(),
            ),
            ("ALICE_JUDGE_TIMEOUT_MS".to_string(), "2000".to_string()),
        ];
        let s = SettingsHandle::load_with_env(&h, vars.clone());
        assert_eq!(
            s.get().judge.review_threshold,
            0.6,
            "the file still applies"
        );
        assert_eq!(
            s.get().judge.timeout_ms,
            2000,
            "and so do the valid variables"
        );
        assert_eq!(s.get().search.relevance_floor, 0.82);
        let errs = s.errors();
        assert!(
            errs.iter().any(|e| e.key == "search.relevance_floor"
                && e.message.contains("ALICE_SEARCH_RELEVANCE_FLOOR")),
            "{errs:?}"
        );
        // Edits still apply, and the variable's error stays in view.
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 0.7\n").unwrap();
        assert!(s.reload_if_changed().is_some());
        assert_eq!(s.get().judge.review_threshold, 0.7);
        s.patch(&one("judge.auto_apply_threshold", serde_json::json!(0.9)))
            .unwrap();
        assert_eq!(s.get().judge.auto_apply_threshold, 0.9);
        assert!(s.errors().iter().any(|e| e.key == "search.relevance_floor"));
        // The rule outside the daemon is the same.
        let e = effective(&h.config_file(), &vars);
        assert_eq!(e.resolved.settings.judge.review_threshold, 0.7);
        assert_eq!(e.resolved.settings.judge.timeout_ms, 2000);
        assert_eq!(e.errors.len(), 1, "{:?}", e.errors);
    }

    #[test]
    fn variables_that_conflict_with_each_other_are_set_aside_together() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "[mcp]\nadmin_tools = true\n").unwrap();
        // Each is valid alone; together the review threshold is above the auto threshold.
        let e = effective(
            &h.config_file(),
            &[
                ("ALICE_JUDGE_AUTO_APPLY_THRESHOLD".into(), "0.6".into()),
                ("ALICE_JUDGE_REVIEW_THRESHOLD".into(), "0.7".into()),
            ],
        );
        assert!(
            e.resolved.settings.mcp.admin_tools,
            "the file still applies"
        );
        assert_eq!(e.resolved.settings.judge.review_threshold, 0.5);
        assert!(
            e.errors.iter().any(|x| x.key == "judge.review_threshold"),
            "{:?}",
            e.errors
        );
    }

    #[test]
    fn secret_variables_are_neither_kept_nor_shown() {
        let key = concat!("sk-", "proj-SECRETSECRET1234");
        let (_d, h) = home();
        let vars = vec![
            ("ALICE_EMBEDDING_API_KEY".to_string(), key.to_string()),
            ("ALICE_JUDGE_JEV_API_KEY".to_string(), key.to_string()),
            ("TYPESAFE_API_KEY".to_string(), key.to_string()),
            (
                "ALICE_JUDGE_REVIEW_THRESHOLD".to_string(),
                "0.6".to_string(),
            ),
        ];
        let layer = EnvLayer::new(&vars);
        assert!(
            layer
                .vars
                .iter()
                .all(|(k, v)| !v.contains("SECRET") && k.starts_with("ALICE_")),
            "only setting values are kept: {:?}",
            layer.vars.iter().map(|(k, _)| k).collect::<Vec<_>>()
        );
        let s = SettingsHandle::load_with_env(&h, vars);
        assert_eq!(s.get().judge.review_threshold, 0.6);
        assert_eq!(s.warnings().len(), 1, "the misplaced key is named");
        let shown = format!(
            "{s:?} {:?} {:?} {:?}",
            s.resolved(),
            s.errors(),
            s.warnings()
        );
        assert!(!shown.contains("SECRETSECRET"), "{shown}");
    }

    #[test]
    fn an_unreadable_config_is_an_error_not_the_defaults() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), [b'[', 0xff, 0xfe, b']']).unwrap();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(
            s.errors()
                .iter()
                .any(|e| e.key == "config.toml" && e.message.contains("UTF-8")),
            "{:?}",
            s.errors()
        );
        assert!(
            s.patch(&one("mcp.admin_tools", serde_json::json!(true)))
                .is_err(),
            "a file that cannot be read is not overwritten"
        );
        std::fs::remove_file(h.config_file()).unwrap();
        std::fs::create_dir(h.config_file()).unwrap();
        let e = effective(&h.config_file(), &[]);
        assert_eq!(e.errors[0].key, "config.toml", "{:?}", e.errors);
    }

    #[cfg(unix)]
    #[test]
    fn patch_writes_through_a_symlink_and_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (d, h) = home();
        let real = d.path().join("dotfiles-alice.toml");
        std::fs::write(&real, "# linked\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, h.config_file()).unwrap();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        s.patch(&one("mcp.admin_tools", serde_json::json!(true)))
            .unwrap();
        assert!(
            std::fs::symlink_metadata(h.config_file())
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link stays a link"
        );
        let text = std::fs::read_to_string(&real).unwrap();
        assert!(
            text.contains("# linked") && text.contains("admin_tools = true"),
            "{text}"
        );
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert!(s.reload_if_changed().is_none());
        assert!(s.get().mcp.admin_tools);
    }

    #[test]
    fn the_watcher_picks_up_an_edit() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        let stop = Arc::new(AtomicBool::new(false));
        let t = s.spawn_watcher(Duration::from_millis(50), stop.clone());
        std::fs::write(h.config_file(), "[mcp]\nadmin_tools = true\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !s.get().mcp.admin_tools {
            assert!(
                std::time::Instant::now() < deadline,
                "watcher did not reload"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
    }

    // ------------------------------------------------------------ fix round 1

    fn write(h: &AliceHome, text: &str) {
        std::fs::write(h.config_file(), text).unwrap();
    }

    const GUARDED: &str = "[privacy]\ncloud_denied_projects = [\"client-x\", \"client-y\"]\nredact_mode = \"block\"\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }]\n";

    fn denylist(s: &SettingsHandle) -> Vec<String> {
        s.get().privacy.cloud_denied_projects.clone()
    }

    /// R1: an editor that truncates and then writes shows an empty file for a moment. The
    /// polls are the injected trigger: no clock, no sleeping.
    #[test]
    fn a_half_written_config_never_switches_protections_off_for_a_moment() {
        let (_d, h) = home();
        write(&h, GUARDED);
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        // Truncated: one poll sees the empty file, the denylist stays.
        write(&h, "");
        assert!(s.reload_if_changed().is_none());
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        // The writer finishes (the same content as before): nothing to apply, and the count of
        // empty polls is gone.
        write(&h, GUARDED);
        assert!(s.reload_if_changed().is_none());
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        // Emptied again for two polls: still held back.
        write(&h, "");
        assert!(s.reload_if_changed().is_none());
        assert!(s.reload_if_changed().is_none());
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        // The third poll in a row: a deliberate emptying is applied.
        let changed = s.reload_if_changed().expect("applied on the third poll");
        assert!(changed.contains(&"privacy.cloud_denied_projects".to_string()));
        assert!(denylist(&s).is_empty());
        assert_eq!(s.get().privacy.redact_mode, RedactMode::Redact);
    }

    #[test]
    fn every_loosening_waits_for_three_polls_and_a_tightening_applies_at_once() {
        const TWO_LEFT: &str = "[privacy]\ncloud_denied_projects = [\"client-x\"]\nredact_mode = \"block\"\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }]\n";
        const NO_PATTERN: &str = "[privacy]\ncloud_denied_projects = [\"client-x\", \"client-y\"]\nredact_mode = \"block\"\n";
        const THREE: &str = "[privacy]\ncloud_denied_projects = [\"client-x\", \"client-y\", \"client-z\"]\nredact_mode = \"block\"\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }]\n";
        // (before, after, a key the after-file changes)
        let loosenings = [
            ("", "[privacy]\nredact = false\n", "privacy.redact"),
            (
                "[privacy]\nredact_mode = \"block\"\n",
                "[privacy]\nredact_mode = \"redact\"\n",
                "privacy.redact_mode",
            ),
            (GUARDED, TWO_LEFT, "privacy.cloud_denied_projects"),
            (GUARDED, NO_PATTERN, "privacy.custom_patterns"),
            (
                "[privacy]\nallowlist = [\"^a$\"]\n",
                "[privacy]\nallowlist = [\"^a$\", \"^b$\"]\n",
                "privacy.allowlist",
            ),
            (
                "[judge]\ndaily_budget_usd = 1.0\n",
                "[judge]\ndaily_budget_usd = 5.0\n",
                "judge.daily_budget_usd",
            ),
            ("", "[judge]\nprovider = \"jev\"\n", "judge.provider"),
        ];
        for (before, after, key) in loosenings {
            let (_d, h) = home();
            write(&h, before);
            let s = SettingsHandle::load_with_env(&h, vec![]);
            write(&h, after);
            assert!(s.reload_if_changed().is_none(), "{key}: poll 1");
            assert!(s.reload_if_changed().is_none(), "{key}: poll 2");
            let changed = s
                .reload_if_changed()
                .unwrap_or_else(|| panic!("{key}: poll 3"));
            assert!(changed.contains(&key.to_string()), "{key}: {changed:?}");
        }
        let tightenings = [
            (GUARDED, THREE, "privacy.cloud_denied_projects"),
            (
                "",
                "[privacy]\nredact_mode = \"block\"\n",
                "privacy.redact_mode",
            ),
            (
                "[judge]\ndaily_budget_usd = 5.0\n",
                "[judge]\ndaily_budget_usd = 1.0\n",
                "judge.daily_budget_usd",
            ),
        ];
        for (before, after, key) in tightenings {
            let (_d, h) = home();
            write(&h, before);
            let s = SettingsHandle::load_with_env(&h, vec![]);
            write(&h, after);
            let changed = s
                .reload_if_changed()
                .unwrap_or_else(|| panic!("{key}: at once"));
            assert!(changed.contains(&key.to_string()), "{key}: {changed:?}");
        }
    }

    #[test]
    fn an_emptied_or_missing_config_waits_even_when_nothing_protective_is_in_it() {
        type Gone = fn(&AliceHome);
        let gone: [Gone; 2] = [
            |h| std::fs::remove_file(h.config_file()).unwrap(),
            |h| write(h, "  \n"),
        ];
        for gone in gone {
            let (_d, h) = home();
            write(&h, "[judge]\nreview_threshold = 0.6\n");
            let s = SettingsHandle::load_with_env(&h, vec![]);
            gone(&h);
            assert!(s.reload_if_changed().is_none());
            assert!(s.reload_if_changed().is_none());
            assert_eq!(s.get().judge.review_threshold, 0.6, "held back");
            assert!(s.reload_if_changed().is_some(), "third poll");
            assert_eq!(s.get().judge.review_threshold, 0.5);
        }
    }

    #[test]
    fn a_patch_during_a_truncated_write_is_refused() {
        let (_d, h) = home();
        write(&h, GUARDED);
        let s = SettingsHandle::load_with_env(&h, vec![]);
        write(&h, ""); // an editor truncated the file and has not written it yet
        let errs = s
            .patch(&one("mcp.admin_tools", serde_json::json!(true)))
            .unwrap_err();
        assert_eq!(errs[0].key, "config.toml");
        assert!(errs[0].message.contains("try again"), "{errs:?}");
        assert_eq!(
            std::fs::read_to_string(h.config_file()).unwrap(),
            "",
            "no file was built from the truncated read"
        );
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        assert!(!s.get().mcp.admin_tools);
        // The patch attempts did not count as polls: the emptying still needs three of its own.
        assert!(
            s.patch(&one("mcp.admin_tools", serde_json::json!(true)))
                .is_err()
        );
        assert!(s.reload_if_changed().is_none());
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        // The writer finishes; the patch goes through on top of the real content.
        write(&h, GUARDED);
        s.patch(&one("mcp.admin_tools", serde_json::json!(true)))
            .unwrap();
        let text = std::fs::read_to_string(h.config_file()).unwrap();
        assert!(
            text.contains("client-y") && text.contains("admin_tools = true"),
            "{text}"
        );
    }

    /// R3: one conflicting pair of variables must not take an unrelated variable with it.
    #[test]
    fn conflicting_variables_set_aside_only_the_ones_the_error_names() {
        let layer = EnvLayer::new(&[
            ("ALICE_JUDGE_AUTO_APPLY_THRESHOLD".into(), "0.6".into()),
            ("ALICE_JUDGE_REVIEW_THRESHOLD".into(), "0.7".into()),
            ("ALICE_JUDGE_TIMEOUT_MS".into(), "2000".into()),
        ]);
        let kept: Vec<_> = layer.names();
        assert!(kept.contains(&"ALICE_JUDGE_TIMEOUT_MS"), "{kept:?}");
        assert!(
            kept.contains(&"ALICE_JUDGE_AUTO_APPLY_THRESHOLD"),
            "{kept:?}"
        );
        assert!(!kept.contains(&"ALICE_JUDGE_REVIEW_THRESHOLD"), "{kept:?}");
        assert!(
            layer
                .errors
                .iter()
                .any(|e| e.key == "judge.review_threshold"
                    && e.message.contains("ALICE_JUDGE_REVIEW_THRESHOLD")),
            "{:?}",
            layer.errors
        );
    }

    /// R2: a typo in an unrelated key.
    #[test]
    fn a_typo_in_an_unrelated_key_keeps_the_privacy_settings() {
        let (_d, h) = home();
        write(
            &h,
            &format!("{GUARDED}\n[judge]\nreview_treshold = 0.7\nprovider = \"jev\"\n"),
        );
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        assert_eq!(s.get().privacy.custom_patterns.len(), 1);
        assert_eq!(s.get().privacy.redact_mode, RedactMode::Block);
        assert!(s.errors().iter().any(|e| e.key == "judge.review_treshold"));
        let e = effective(&h.config_file(), &[]);
        assert_eq!(
            e.resolved.settings.privacy.cloud_denied_projects,
            vec!["client-x", "client-y"]
        );
    }

    /// R2: the flag cloud consumers fail closed on.
    #[test]
    fn the_degraded_flag_says_no_valid_config_has_loaded() {
        let (_d, h) = home();
        // No file at all is a valid (default) configuration.
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(!s.config_degraded());
        // A typo makes the file degraded: the denylist itself may be what was left out.
        write(&h, "[privacy]\ncloud_denied_projects = \"client-x\"\n");
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(s.config_degraded());
        assert!(denylist(&s).is_empty(), "the bad leaf is left out");
        assert!(effective(&h.config_file(), &[]).degraded);
        assert!(format!("{s:?}").contains("config_degraded: true"));
        // Still bad, edited: still degraded, and the valid privacy leaves of the new text apply.
        write(
            &h,
            "[privacy]\ncloud_denied_projects = \"client-x\"\nredact_mode = \"block\"\n",
        );
        assert!(s.reload_if_changed().is_some());
        assert!(s.config_degraded());
        assert_eq!(s.get().privacy.redact_mode, RedactMode::Block);
        // A valid file ends it, for good.
        write(&h, GUARDED);
        assert!(s.reload_if_changed().is_some());
        assert!(!s.config_degraded());
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        write(&h, "[privacy\n");
        assert!(s.reload_if_changed().is_none());
        assert!(
            !s.config_degraded(),
            "the last valid settings stay in force"
        );
        assert_eq!(denylist(&s), vec!["client-x", "client-y"]);
        // A file nobody can read is degraded too.
        std::fs::remove_file(h.config_file()).unwrap();
        std::fs::create_dir(h.config_file()).unwrap();
        assert!(SettingsHandle::load_with_env(&h, vec![]).config_degraded());
        assert!(effective(&h.config_file(), &[]).degraded);
        assert!(!effective(&home().1.config_file(), &[]).degraded);
    }

    /// m6: only keys whose value changed need a restart.
    #[test]
    fn restart_required_lists_only_keys_whose_value_changed() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        let out = s.patch(&one("server.port", serde_json::json!(0))).unwrap();
        assert!(out.restart_required.is_empty(), "{out:?}");
        let out = s
            .patch(&one("server.port", serde_json::json!(4545)))
            .unwrap();
        assert_eq!(out.restart_required, vec!["server.port".to_string()]);
        let out = s
            .patch(&one("server.port", serde_json::json!(4545)))
            .unwrap();
        assert!(out.restart_required.is_empty(), "{out:?}");
    }

    // ------------------------------------------------------------ fix round 2

    /// N4: the one answer Tasks 9 and 16 use.
    #[test]
    fn cloud_allowed_combines_degraded_the_global_list_and_the_project_file() {
        let (_d, h) = home();
        let repo = tempfile::tempdir().unwrap();
        write(&h, "[privacy]\ncloud_denied_projects = [\"listed\"]\n");
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(s.cloud_allowed("open", None));
        assert!(!s.cloud_allowed("listed", None), "the global list");
        assert!(!s.cloud_allowed("", None), "no project, no cloud");
        // The project's own file.
        assert!(s.cloud_allowed("open", Some(repo.path())), "no .alice.toml");
        let alice_toml = repo.path().join(".alice.toml");
        std::fs::write(&alice_toml, "[privacy]\ncloud_denied = true\n").unwrap();
        assert!(!s.cloud_allowed("open", Some(repo.path())));
        // A project file that is broken, or blank on the first look: closed.
        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join(".alice.toml"), "[privacy\n").unwrap();
        assert!(!s.cloud_allowed("open", Some(broken.path())));
        let blank = tempfile::tempdir().unwrap();
        std::fs::write(blank.path().join(".alice.toml"), "").unwrap();
        assert!(!s.cloud_allowed("open", Some(blank.path())));
        // A degraded configuration allows nothing, whatever the project.
        write(&h, "[privacy]\ncloud_denied_projects = \"listed\"\n");
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(s.config_degraded());
        assert!(!s.cloud_allowed("open", None));
        assert!(!s.cloud_allowed("open", Some(tempfile::tempdir().unwrap().path())));
    }

    #[test]
    fn cloud_allowed_does_not_open_for_an_emptied_project_file_after_rapid_calls() {
        let (_d, h) = home();
        let repo = tempfile::tempdir().unwrap();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        let alice_toml = repo.path().join(".alice.toml");
        std::fs::write(&alice_toml, "[privacy]\ncloud_denied = true\n").unwrap();
        assert!(!s.cloud_allowed("p", Some(repo.path())));
        std::fs::write(&alice_toml, "").unwrap(); // truncated, the writer is not done
        for _ in 0..50 {
            assert!(!s.cloud_allowed("p", Some(repo.path())));
        }
    }

    // ------------------------------------------------------------ Task 6

    /// A slug with stray whitespace is still the project the denylist names.
    #[test]
    fn cloud_allowed_trims_the_slug() {
        let (_d, h) = home();
        write(&h, "[privacy]\ncloud_denied_projects = [\"client-x\"]\n");
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(!s.cloud_allowed("client-x", None));
        assert!(!s.cloud_allowed(" client-x\n", None));
        assert!(s.cloud_allowed("client-y", None));
    }

    /// The settings loop re-reads after a lag and ends only when the channel closes.
    #[test]
    fn the_settings_loop_rereads_after_a_lag_and_ends_only_on_close() {
        let (tx, rx) = tokio::sync::broadcast::channel::<Vec<String>>(1);
        let mut now = Settings::default();
        now.judge.review_threshold = 0.6;
        let now = Arc::new(now);
        for k in ["a", "b", "c"] {
            tx.send(vec![k.to_string()]).unwrap(); // capacity 1: the loop starts lagged
        }
        drop(tx);
        let mut seen = Vec::new();
        follow_settings(
            rx,
            Arc::new(Settings::default()),
            || now.clone(),
            |c, _| seen.push(c),
        );
        assert_eq!(
            seen,
            vec![
                vec!["judge.review_threshold".to_string()],
                vec!["c".to_string()]
            ]
        );
    }

    /// A loosening in config.toml waits for three polls; until then the handle says so.
    #[test]
    fn a_waiting_loosening_is_visible_until_it_applies() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(!s.loosening_waiting());
        write(&h, "[privacy]\nredact = false\n");
        assert!(s.reload_if_changed().is_none());
        assert!(s.loosening_waiting());
        assert!(s.reload_if_changed().is_none());
        assert!(s.loosening_waiting());
        assert!(s.get().privacy.redact, "still on");
        assert!(s.reload_if_changed().is_some(), "the third poll applies it");
        assert!(!s.loosening_waiting());
        assert!(!s.get().privacy.redact);
        // A second loosening that goes back to what is in force before its third poll is gone.
        write(&h, "[privacy]\nredact = true\n");
        assert!(
            s.reload_if_changed().is_some(),
            "a tightening applies at once"
        );
        write(&h, "[privacy]\nredact = false\n");
        assert!(s.reload_if_changed().is_none() && s.loosening_waiting());
        write(&h, "[privacy]\nredact = true\n");
        assert!(
            s.reload_if_changed().is_none(),
            "back to what is in force: nothing to apply"
        );
        assert!(!s.loosening_waiting());
    }

    /// A loosening of `config.toml` that is still waiting for its polls counts as a fail-closed
    /// state: a write refused meanwhile may be accepted once it applies.
    #[test]
    fn a_waiting_config_loosening_is_a_fail_closed_state() {
        let (_d, h) = home();
        write(&h, "[privacy]\nredact_mode = \"block\"\n");
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert!(!s.project_fail_closed(None));
        write(&h, "");
        assert!(s.reload_if_changed().is_none());
        assert!(s.loosening_waiting());
        assert!(s.project_fail_closed(None), "pending loosening");
    }
}
