//! Health checks for `alice doctor` (spec §5.7): what is wrong and what to do next.
//!
//! Two halves. [`local_checks`] needs no daemon (and works when none can start): the settings
//! files, file permissions, the model on disk, whether a daemon and a service unit exist.
//! [`daemon_checks`] is what only the daemon can see (it alone opens the database): the
//! database, the jobs, the backups, the embedding and judge workers, the settings it holds.
//!
//! A check never carries a secret: keys are reported as set or not, files as counts and
//! modes, and provider errors are the fixed texts the workers already store.

use crate::paths::AliceHome;
use crate::service::Service;
use alice_core::api::{Health, JobStatus, SettingsView};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The daemon's memory budget with the real local model at 1M chunks (spec §4.6), in MB.
const MEMORY_BUDGET_MB: u64 = 800;
/// Open review items from which doctor suggests clearing them.
const REVIEWS_WARN: u64 = 50;
/// Free space below which doctor fails the disk check (Alice goes read-only near this), bytes.
const DISK_FAIL_BYTES: f64 = 0.1e9;
/// Free space below which doctor warns, bytes.
const DISK_WARN_BYTES: f64 = 1.0e9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    /// What to do about it.
    pub fix: Option<String>,
}

impl Check {
    /// One line for people: `OK   name: detail`, `WARN name: detail — fix`, `FAIL name: detail — fix`.
    ///
    /// ```
    /// use alice_daemon::doctor::{Check, CheckStatus};
    /// let c = Check { name: "disk".into(), status: CheckStatus::Warn, detail: "0.50 GB free".into(), fix: Some("free some space".into()) };
    /// assert_eq!(c.line(), "WARN disk: 0.50 GB free — free some space");
    /// let ok = Check { name: "config".into(), status: CheckStatus::Ok, detail: "valid".into(), fix: None };
    /// assert_eq!(ok.line(), "OK   config: valid");
    /// ```
    pub fn line(&self) -> String {
        let tag = match self.status {
            CheckStatus::Ok => "OK  ",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        let fix = self
            .fix
            .as_deref()
            .map(|f| format!(" — {f}"))
            .unwrap_or_default();
        format!("{tag} {}: {}{fix}", self.name, self.detail)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

impl DoctorReport {
    /// Whether any check failed (`alice doctor` then exits non-zero).
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Fail)
    }
}

fn check(name: &str, status: CheckStatus, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        name: name.into(),
        status,
        detail: detail.into(),
        fix: fix.map(String::from),
    }
}

/// The daemon's own memory: physical footprint on macOS, resident set on Linux; `None` where
/// that cannot be read (Windows).
pub fn self_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `proc_pid_rusage` fills the zeroed struct for our own pid and writes nothing else.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as libc::c_int,
                libc::RUSAGE_INFO_V4,
                (&mut info as *mut libc::rusage_info_v4).cast(),
            )
        };
        (rc == 0).then_some(info.ri_phys_footprint)
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: `sysconf` has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        u64::try_from(page).ok().map(|p| pages * p)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Free bytes on the volume that holds `path`; `None` where that is not known (Windows).
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    crate::backup::free_space(path)
}

#[cfg(unix)]
fn mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

/// Checks that need no daemon (and must work when it cannot start). `daemon` is the health of
/// the daemon that answers, if one does.
pub fn local_checks(home: &AliceHome, daemon: Option<&Health>) -> Vec<Check> {
    let fake_embedder = std::env::var("ALICE_EMBEDDER").is_ok_and(|v| v == "fake");
    let mut out = local_checks_with(home, daemon, fake_embedder);
    let now = chrono::Utc::now();
    match daemon {
        // A server tenant's hooks are on its person's machine: only the capture line, which the
        // uploads feed, is checked (without the quiet-week rule, which needs the hooks).
        Some(h) if h.tenant => out.extend(capture_check(h, false, now)),
        _ => out.extend(capture_checks(daemon, &|k| std::env::var_os(k), now)),
    }
    out
}

/// `local_checks` with the test-embedder switch passed in (tests set it either way).
pub(crate) fn local_checks_with(
    home: &AliceHome,
    daemon: Option<&Health>,
    fake_embedder: bool,
) -> Vec<Check> {
    use CheckStatus::*;
    let mut out = Vec::new();
    // The one rule for "config.toml + ALICE_* + defaults": what a daemon would start with.
    let eff = crate::config::load_effective(home);
    if eff.errors.is_empty() {
        out.push(check(
            "config",
            Ok,
            "config.toml and ALICE_* variables are valid",
            None,
        ));
    } else {
        let detail = eff
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.key, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        out.push(check(
            "config",
            Fail,
            detail,
            Some(
                "fix the keys named (alice config edit); the daemon keeps the last valid settings meanwhile",
            ),
        ));
    }
    if !eff.resolved.warnings.is_empty() {
        out.push(check(
            "environment",
            Warn,
            eff.resolved.warnings.join("; "),
            Some("unset or rename the variables named; `alice config list` shows the valid ones"),
        ));
    }
    let settings = eff.resolved.settings;
    #[cfg(unix)]
    {
        for (name, path, want) in [
            ("home permissions", home.root().to_path_buf(), 0o700),
            ("token permissions", home.token_file(), 0o600),
            ("secrets permissions", home.secrets_file(), 0o600),
        ] {
            match mode(&path) {
                None => {}
                Some(m) if m & !want == 0 => out.push(check(
                    name,
                    Ok,
                    format!("{} is {:o}", path.display(), m),
                    None,
                )),
                Some(m) => out.push(check(
                    name,
                    Fail,
                    format!(
                        "{} is {:o}; only you should be able to read it",
                        path.display(),
                        m
                    ),
                    Some(&format!("chmod {:o} {}", want, path.display())),
                )),
            }
        }
    }
    if settings.embedding.provider == alice_core::settings::EmbeddingProvider::Local {
        if fake_embedder {
            out.push(check(
                "embedding model",
                Warn,
                "ALICE_EMBEDDER=fake: a test embedder is in use and the local model is not checked",
                Some("unset ALICE_EMBEDDER to use the real model"),
            ));
        } else if alice_embed::is_model_present(&home.models_dir()) {
            out.push(check(
                "embedding model",
                Ok,
                "multilingual-e5-small-q8 is downloaded",
                None,
            ));
        } else {
            out.push(check(
                "embedding model",
                Fail,
                "the local embedding model is missing or incomplete",
                Some("run `alice model fetch`"),
            ));
        }
    }
    match daemon {
        Some(h) => {
            out.push(check(
                "daemon",
                Ok,
                format!("running (pid {}, v{})", h.pid, h.version),
                None,
            ));
            if h.version != env!("CARGO_PKG_VERSION") {
                out.push(check(
                    "version",
                    Warn,
                    format!(
                        "the daemon is v{}, this CLI is v{}",
                        h.version,
                        env!("CARGO_PKG_VERSION")
                    ),
                    Some("run `alice daemon stop`; the next command starts the new version"),
                ));
            }
        }
        None => out.push(not_running(home)),
    }
    if daemon.is_some_and(|h| h.tenant) {
        // A server tenant's daemon is the server's own unit: a user unit would only compete with it.
        out.push(check(
            "service",
            CheckStatus::Ok,
            "run by systemd on this server (alice@<name>; the admin's alice-server manages it)",
            None,
        ));
    } else if let Some(root) = dirs::home_dir() {
        out.extend(service_check(home, &root));
    }
    out
}

/// Whether a service unit is installed in `root` (the user's home directory) and for which data
/// directory. On Windows the task lives in the Task Scheduler, not in a file this can look at:
/// `alice service status` asks `schtasks`.
pub(crate) fn service_check(home: &AliceHome, root: &Path) -> Option<Check> {
    use crate::service_install as si;
    let p = si::Platform::current().filter(|p| *p != si::Platform::Windows)?;
    Some(match si::installed(p, root) {
        Some(f) if si::unit_serves(p, root, home.root()) => check(
            "service",
            CheckStatus::Ok,
            format!("installed ({})", f.display()),
            None,
        ),
        Some(f) => {
            let other = si::unit_home(p, root)
                .map(|h| h.display().to_string())
                .unwrap_or_else(|| "another data directory".into());
            check(
                "service",
                CheckStatus::Ok,
                format!(
                    "not installed for this data directory ({}); the unit {} serves {other}",
                    home.root().display(),
                    f.display()
                ),
                Some("`alice service install` keeps this one running (it replaces that unit)"),
            )
        }
        None => check(
            "service",
            CheckStatus::Ok,
            "not installed (optional: the daemon starts on first use)",
            Some("`alice service install` keeps it running"),
        ),
    })
}

/// Days without a capture after which doctor warns while capture hooks are installed.
const CAPTURE_QUIET_DAYS: i64 = 7;

/// The capture checks (SP2 spec §4.3, §10): whether Claude Code's and Codex's hooks, in the
/// files `env` locates, run an `alice` that exists; and the capture line of the daemon's health.
pub(crate) fn capture_checks(
    daemon: Option<&Health>,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Check> {
    use crate::agent_hooks::{HookAgent, HookTarget, status};
    // Only files that hold Alice's entries: one that is missing or unreadable is the agent's
    // business, not a capture problem.
    let found: Vec<_> = [HookAgent::Claude, HookAgent::Codex]
        .into_iter()
        .filter_map(|a| {
            let s = status(&HookTarget::locate(a, None, env).ok()?).ok()?;
            (!s.events.is_empty()).then_some((a, s))
        })
        .collect();
    let broken: Vec<_> = found
        .iter()
        .filter(|(_, s)| !s.missing.is_empty())
        .collect();
    let mut out = vec![if !broken.is_empty() {
        let detail = broken
            .iter()
            .map(|(a, s)| {
                format!(
                    "{}'s hooks in {} run {}, which does not exist",
                    a.label(),
                    s.file.display(),
                    s.missing.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let fix = broken
            .iter()
            .map(|(a, _)| format!("`alice hooks install {}`", a.name()))
            .collect::<Vec<_>>()
            .join(" and ");
        check(
            "capture hooks",
            CheckStatus::Warn,
            format!("{detail}, so those sessions are not captured"),
            Some(&format!("run {fix}")),
        )
    } else if found.is_empty() {
        check(
            "capture hooks",
            CheckStatus::Ok,
            "not installed (optional: sessions are then not captured automatically)",
            Some("`alice hooks install claude` (or `codex`) captures sessions as they run"),
        )
    } else {
        let agents: Vec<&str> = found.iter().map(|(a, _)| a.label()).collect();
        check(
            "capture hooks",
            CheckStatus::Ok,
            format!("installed for {}", agents.join(" and ")),
            None,
        )
    }];
    out.extend(daemon.and_then(|h| capture_check(h, !found.is_empty(), now)));
    out
}

/// The capture line of the daemon's health; `None` while capture never ran.
pub(crate) fn capture_check(
    h: &Health,
    hooks_installed: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Check> {
    let c = h.capture.as_deref()?;
    let mut problems = Vec::new();
    let mut fixes = Vec::new();
    let quiet = c
        .last_capture_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| {
            now - t.with_timezone(&chrono::Utc) > chrono::Duration::days(CAPTURE_QUIET_DAYS)
        });
    if hooks_installed && quiet {
        problems.push(format!(
            "nothing was captured for {CAPTURE_QUIET_DAYS} days (last at {})",
            c.last_capture_at.as_deref().unwrap_or_default()
        ));
        fixes.push("check `alice hooks status claude` (or `codex`) and `alice daemon logs`");
    }
    if c.unrecognised_lines_7d > 0 {
        problems.push(format!(
            "{} transcript lines of the last 7 days had a shape Alice does not know and were skipped; the agent's transcript format may have changed",
            c.unrecognised_lines_7d
        ));
        fixes.push("update Alice");
    }
    if c.rollouts_not_found > 0 {
        problems.push(format!(
            "{} Codex sessions gave no transcript path and no rollout of theirs was found in CODEX_HOME/sessions since the daemon started",
            c.rollouts_not_found
        ));
        fixes.push(
            "if Codex keeps its sessions elsewhere, start the daemon with the same CODEX_HOME",
        );
    }
    // Mentioned, not warned about: there is nothing to do (Alice reads only uncompressed ones).
    let compressed = (c.rollouts_compressed > 0).then(|| {
        format!(
            "{} Codex rollouts are compressed (.jsonl.zst) and were skipped since the daemon started",
            c.rollouts_compressed
        )
    });
    Some(if problems.is_empty() {
        let last = c
            .last_capture_at
            .as_deref()
            .map(|t| format!("; last at {t}"))
            .unwrap_or_default();
        let detail = format!(
            "{} sessions and {} chunks captured in the last 24 hours{last}",
            c.sessions_24h, c.chunks_24h
        );
        check(
            "capture",
            CheckStatus::Ok,
            [Some(detail), compressed]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; "),
            None,
        )
    } else {
        problems.extend(compressed);
        check(
            "capture",
            CheckStatus::Warn,
            problems.join("; "),
            Some(&fixes.join("; ")),
        )
    })
}

/// The `daemon` check when none answers: whether one is starting, why the last start failed,
/// or the last lines of its log.
fn not_running(home: &AliceHome) -> Check {
    use CheckStatus::*;
    if let Some(pid) = crate::startup::lock_holder(home) {
        return check(
            "daemon",
            Warn,
            format!("pid {pid} holds the daemon lock but does not answer"),
            Some(
                "it may still be starting (the first start downloads the model); see `alice daemon status`, then `alice daemon logs`",
            ),
        );
    }
    if let Some(e) = crate::startup::read_error(home) {
        return check(
            "daemon",
            Warn,
            format!("not running; the last start failed ({}): {}", e.at, e.error),
            Some("fix what it says, then `alice daemon start`"),
        );
    }
    let tail = log_tail(&home.logs_dir().join("daemon.log"), LOG_TAIL_BYTES);
    let last: Vec<&str> = tail.lines().rev().take(3).collect();
    let detail = if last.is_empty() {
        "not running".to_string()
    } else {
        format!(
            "not running; last log lines: {}",
            last.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )
    };
    check(
        "daemon",
        Warn,
        detail,
        Some("start it with `alice daemon start`; if it exits at once, read `alice daemon logs`"),
    )
}

/// How much of the end of `daemon.log` doctor reads (the log is never rotated).
const LOG_TAIL_BYTES: u64 = 16 * 1024;

/// The last `max` bytes of the file as text (lossy; a cut first line is dropped); empty when it
/// cannot be read.
fn log_tail(path: &Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.take(max).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text
    }
}

/// Time at the start of a `maint.<job>.last_error` value (`<stamp> <detail>`).
fn stamp_of(value: &str) -> &str {
    value.split_whitespace().next().unwrap_or_default()
}

/// The checks a health reading answers by itself.
pub fn health_checks(h: &Health) -> Vec<Check> {
    use CheckStatus::{Fail, Warn};
    let mut out = Vec::new();
    match &h.read_only {
        Some(r) => out.push(check(
            "database",
            Fail,
            format!("read-only: {r}"),
            Some(
                "free disk space or restore a backup (`alice restore`), then `alice daemon stop` and start again; writes are spooled meanwhile",
            ),
        )),
        None => out.push(check(
            "database",
            CheckStatus::Ok,
            format!("{} memories, {} chunks", h.memories, h.chunks),
            None,
        )),
    }
    out.push(match (&h.maintenance_warning, &h.last_backup) {
        (Some(w), _) => check(
            "backups",
            Warn,
            w.clone(),
            Some("run `alice backup now`; check `alice maintenance status`"),
        ),
        (None, Some(t)) => check(
            "backups",
            CheckStatus::Ok,
            format!("newest backup at {t}"),
            None,
        ),
        (None, None) => check(
            "backups",
            Warn,
            "no backup yet",
            Some("run `alice backup now` (backups also run daily when the daemon is idle)"),
        ),
    });
    out.push(if h.index_ready {
        check("vector index", CheckStatus::Ok, "ready", None)
    } else {
        check(
            "vector index",
            Warn,
            "being rebuilt; search uses words only meanwhile",
            Some("nothing to do; it is ready again within minutes (longer on a large database)"),
        )
    });
    if let Some(e) = &h.embedding_error {
        out.push(check(
            "embedding provider",
            Fail,
            e.clone(),
            Some("fix [embedding] settings or the key, then `alice config test embedding`"),
        ));
    }
    if let Some(r) = &h.reembed {
        out.push(check(
            "re-embed",
            Warn,
            format!(
                "embedding everything again into {}: {} of {} done; {} answers searches meanwhile",
                r.model, r.done, r.total, h.embedding_model
            ),
            Some("nothing to do; it finishes by itself and the switch is atomic"),
        ));
    }
    if h.embed_skipped_cloud > 0 {
        out.push(check(
            "embedding policy",
            Warn,
            format!(
                "{} rows are not embedded because the cloud policy keeps their project at home; they are found by words only",
                h.embed_skipped_cloud
            ),
            Some(
                "intended for privacy.cloud_denied_projects; embed locally (embedding.provider = \"local\") to search them by meaning",
            ),
        ));
    }
    out.push(match &h.judge_warning {
        Some(w) => check(
            "judge",
            Warn,
            w.clone(),
            Some("`alice config set-secret jev_api_key`, or set judge.provider = \"heuristic\""),
        ),
        None => check(
            "judge",
            CheckStatus::Ok,
            format!("{} (backlog {})", h.judge_provider, h.judge_backlog),
            None,
        ),
    });
    if h.reviews_open > REVIEWS_WARN {
        out.push(check(
            "reviews",
            Warn,
            format!("{} items wait for a decision", h.reviews_open),
            Some("`alice review`"),
        ));
    }
    if h.spool_pending > 0 {
        out.push(check(
            "spool",
            Warn,
            format!("{} writes wait in the spool", h.spool_pending),
            Some("they are applied within seconds; if the number stays, read `alice daemon logs`"),
        ));
    }
    if let Some(b) = h.memory_bytes {
        let mb = b / 1_000_000;
        out.push(if mb > MEMORY_BUDGET_MB {
            check(
                "memory",
                Warn,
                format!("{mb} MB in use (budget {MEMORY_BUDGET_MB} MB)"),
                Some("set embedding.idle_unload so the model unloads when idle"),
            )
        } else {
            check("memory", CheckStatus::Ok, format!("{mb} MB in use"), None)
        });
    }
    out
}

/// A job whose last run failed (a later success clears it): its error, and for the disk guard
/// what to do about the space.
pub fn job_checks(jobs: &[JobStatus]) -> Vec<Check> {
    jobs.iter()
        .filter_map(|j| {
            let e = j.last_error.as_ref()?;
            if j.last_ok.as_deref() >= Some(stamp_of(e)) {
                return None;
            }
            let fix = if e.contains("disk space") {
                "free space on the volume that holds the data or the backups (or point backup.dir at a larger one); the job retries by itself".to_string()
            } else {
                format!("run `alice maintenance run {}` to retry", j.job)
            };
            Some(check(
                &format!("job {}", j.job),
                CheckStatus::Warn,
                e.clone(),
                Some(&fix),
            ))
        })
        .collect()
}

/// What the settings the daemon holds say: invalid edits, a config that never loaded, a change
/// waiting to hold still, ignored variables, settings that wait for a restart.
pub fn settings_checks(view: &SettingsView, loosening_waiting: bool) -> Vec<Check> {
    use CheckStatus::{Fail, Warn};
    let mut out = Vec::new();
    if !view.errors.is_empty() {
        out.push(check(
            "settings",
            Fail,
            format!(
                "the daemon keeps the previous settings: {}",
                view.errors
                    .iter()
                    .map(|e| format!("{}: {}", e.key, e.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Some("`alice config edit`"),
        ));
    }
    if view.config_degraded {
        out.push(check(
            "config loaded",
            Warn,
            "no valid config.toml has loaded since the daemon started; cloud providers stay off until one does",
            Some("fix config.toml (`alice config edit`); the daemon notices the fix by itself"),
        ));
    }
    if loosening_waiting {
        out.push(check(
            "settings change",
            Warn,
            "config.toml loosens a protection (redaction, a cloud denylist or a budget); it applies once it reads the same on several polls in a row",
            Some("wait a few seconds, or apply it deliberately with `alice config set`"),
        ));
    }
    if !view.warnings.is_empty() {
        out.push(check(
            "daemon environment",
            Warn,
            view.warnings.join("; "),
            Some("unset or rename the variables named; `alice config list` shows the valid ones"),
        ));
    }
    if !view.restart_pending.is_empty() {
        out.push(check(
            "restart",
            Warn,
            format!(
                "changed settings wait for a restart: {}",
                view.restart_pending.join(", ")
            ),
            Some("`alice daemon stop`; the next command starts it again"),
        ));
    }
    out
}

/// The `distill` line (SP2 spec §8.4): `off`; OK with how many memories were distilled today;
/// or a warning (the judge is not Jev, no key, the breaker is open, today's share is used up)
/// with what to do.
///
/// ```
/// use alice_daemon::distill::DistillView;
/// use alice_daemon::doctor::{CheckStatus, distill_check};
/// let ready = DistillView { state: "ready", warning: None, fix: None };
/// assert_eq!(distill_check(&ready, 3).line(), "OK   distill: jev, 3 distilled today");
/// let off = DistillView { state: "off", warning: None, fix: None };
/// assert_eq!(distill_check(&off, 0).line(), "OK   distill: off");
/// let key = DistillView { state: "no_key", warning: Some("no Jev key is set".into()), fix: Some("`alice config set-secret jev_api_key`".into()) };
/// assert_eq!(distill_check(&key, 0).status, CheckStatus::Warn);
/// ```
pub fn distill_check(v: &crate::distill::DistillView, distilled_today: u64) -> Check {
    match v.state {
        "off" => check("distill", CheckStatus::Ok, "off", None),
        "ready" => check(
            "distill",
            CheckStatus::Ok,
            format!("jev, {distilled_today} distilled today"),
            None,
        ),
        other => check(
            "distill",
            CheckStatus::Warn,
            v.warning.clone().unwrap_or_else(|| other.replace('_', " ")),
            v.fix.as_deref(),
        ),
    }
}

/// Checks only the daemon can make (it alone opens the database).
pub async fn daemon_checks(svc: &Service) -> alice_core::Result<Vec<Check>> {
    use CheckStatus::{Fail, Warn};
    let h = svc.health().await?;
    // Without the secrets' status (a keychain may wait for a prompt): doctor counts, never reads keys.
    let view = svc.settings_view_without_secrets();
    let settings = svc.settings();
    let mut out = health_checks(&h);
    out.extend(job_checks(&svc.maintenance_status().await?));
    let failed = svc
        .count(
            "SELECT (SELECT count(*) FROM memories WHERE embed_state <> 'done' AND embed_state = 'failed')
                  + (SELECT count(*) FROM chunks WHERE embed_state <> 'done' AND embed_state = 'failed')",
        )
        .await?;
    if failed > 0 {
        out.push(check(
            "embeddings",
            Warn,
            format!("{failed} texts have no vector and are found by words only"),
            Some("check `alice config test embedding`"),
        ));
    }
    let stats = svc.stats().await?;
    // `cost_today_usd` is the judge's whole day: its own calls and distillation's.
    if settings.judge.provider == alice_core::settings::JudgeProvider::Jev
        && settings.judge.daily_budget_usd > 0.0
        && stats.cost_today_usd >= 0.8 * settings.judge.daily_budget_usd
    {
        let distill = if stats.distill_cost_today_usd > 0.0 {
            format!(" (distillation ${:.4})", stats.distill_cost_today_usd)
        } else {
            String::new()
        };
        out.push(check(
            "judge budget",
            Warn,
            format!(
                "${:.4} of ${:.2} used today{distill}",
                stats.cost_today_usd, settings.judge.daily_budget_usd
            ),
            Some(
                "raise judge.daily_budget_usd if you expect more; at the limit the heuristic decides and Jev retries tomorrow",
            ),
        ));
    }
    // SP2 §8.4: where distillation stands.
    out.push(distill_check(
        &svc.distill_status().await?,
        stats.distilled_today,
    ));
    out.extend(settings_checks(
        &view,
        svc.settings_handle().loosening_waiting(),
    ));
    if let Some(free) = free_disk_bytes(svc.home().root()) {
        let free = free as f64;
        let gb = free / 1e9;
        out.push(if free < DISK_FAIL_BYTES {
            check(
                "disk",
                Fail,
                format!("{gb:.2} GB free"),
                Some("free space; Alice goes read-only when the disk is full"),
            )
        } else if free < DISK_WARN_BYTES {
            check(
                "disk",
                Warn,
                format!("{gb:.2} GB free"),
                Some("free some space"),
            )
        } else {
            check("disk", CheckStatus::Ok, format!("{gb:.1} GB free"), None)
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_checks_flag_loose_permissions_a_bad_config_and_a_missing_model() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().join("home"));
        home.ensure_dirs().unwrap();
        std::fs::write(home.token_file(), "t").unwrap();
        std::fs::write(home.config_file(), "[judge]\nreview_threshold = 2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home.token_file(), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let checks = local_checks_with(&home, None, false);
        // Every check has its own name (a report line is told apart by it).
        let mut names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), checks.len(), "{checks:?}");
        let get = |n: &str| {
            checks
                .iter()
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("no check {n}: {checks:?}"))
        };
        assert_eq!(get("config").status, CheckStatus::Fail);
        assert!(get("config").detail.contains("judge.review_threshold"));
        #[cfg(unix)]
        assert_eq!(get("token permissions").status, CheckStatus::Fail);
        assert_eq!(
            get("embedding model").status,
            CheckStatus::Fail,
            "the local model is not downloaded here"
        );
        assert!(
            get("embedding model")
                .fix
                .as_deref()
                .unwrap()
                .contains("alice model fetch")
        );
        assert_eq!(get("daemon").status, CheckStatus::Warn);
        // With the test embedder (ALICE_EMBEDDER=fake) a missing model is only worth a warning.
        let fake = local_checks_with(&home, None, true);
        let model = fake.iter().find(|c| c.name == "embedding model").unwrap();
        assert_eq!(model.status, CheckStatus::Warn, "{model:?}");
    }

    #[test]
    fn a_failed_start_and_a_starting_daemon_are_named_in_the_daemon_check() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().join("home"));
        home.ensure_dirs().unwrap();
        crate::startup::write_error(&home, "alice.db is not a usable database; move it aside")
            .unwrap();
        let c = not_running(&home);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.contains("not a usable database"), "{c:?}");
        assert!(c.fix.as_deref().unwrap().contains("alice daemon start"));
    }

    /// Doctor reads the end of a large log, not the whole file.
    #[test]
    fn the_log_tail_is_bounded_and_starts_on_a_whole_line() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("daemon.log");
        assert_eq!(log_tail(&path, 100), "", "no log");
        let log: String = (0..5_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &log).unwrap();
        let tail = log_tail(&path, 200);
        assert!(
            tail.len() <= 200 && tail.ends_with("line 4999\n"),
            "{tail:?}"
        );
        assert!(tail.starts_with("line "), "a cut line is dropped: {tail:?}");
        std::fs::write(&path, "short\n").unwrap();
        assert_eq!(log_tail(&path, 100), "short\n");
    }

    /// The service check says whose unit it found.
    /// On a server tenant the daemon is the server's systemd unit: doctor says so and never
    /// suggests `alice service install` there.
    #[test]
    fn a_tenant_daemon_is_run_by_the_server_s_unit() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().join("home"));
        home.ensure_dirs().unwrap();
        let mut h = health();
        h.tenant = true;
        let checks = local_checks(&home, Some(&h));
        let s = find(&checks, "service");
        assert_eq!(s.status, CheckStatus::Ok);
        assert!(
            s.detail.starts_with("run by systemd on this server"),
            "{s:?}"
        );
        assert!(s.fix.is_none(), "{s:?}");
        assert!(
            !checks
                .iter()
                .any(|c| c.line().contains("alice service install")),
            "{checks:?}"
        );
        // The flag is only sent when set: a local daemon's health reads as before.
        assert!(!serde_json::to_string(&health()).unwrap().contains("tenant"));
        // Task 8 review: the capture line, which the uploads feed, is kept; the hooks check,
        // which would read this server's agent files, is not made, nor is the quiet-week rule
        // that needs it.
        h.capture = Some(Box::new(alice_core::api::CaptureHealth {
            last_capture_at: Some("2026-01-01T00:00:00.000Z".into()),
            unrecognised_lines_7d: 4,
            ..Default::default()
        }));
        let checks = local_checks(&home, Some(&h));
        let c = find(&checks, "capture");
        assert_eq!(c.status, CheckStatus::Warn, "{c:?}");
        assert!(
            c.detail.contains("4 transcript lines") && !c.detail.contains("nothing was captured"),
            "{c:?}"
        );
        assert!(
            !checks.iter().any(|c| c.name == "capture hooks"),
            "{checks:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_service_check_tells_a_unit_for_this_home_from_one_for_another() {
        use crate::service_install as si;
        let d = tempfile::tempdir().unwrap();
        let (mine, theirs) = (d.path().join("mine"), d.path().join("theirs"));
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();
        let (mine, theirs) = (AliceHome::new(mine), AliceHome::new(theirs));
        let none = service_check(&mine, d.path()).unwrap();
        assert!(none.detail.starts_with("not installed"), "{none:?}");
        let p = si::Platform::current().unwrap();
        si::apply(
            &si::plan_install(p, d.path(), Path::new("/x/alice"), &theirs, 1),
            None,
        )
        .unwrap();
        let other = service_check(&mine, d.path()).unwrap();
        assert!(
            other
                .detail
                .contains("not installed for this data directory")
                && other.detail.contains(&theirs.root().display().to_string()),
            "{other:?}"
        );
        let same = service_check(&theirs, d.path()).unwrap();
        assert!(same.detail.starts_with("installed ("), "{same:?}");
    }

    #[test]
    fn a_check_line_names_the_status_the_detail_and_the_fix() {
        let c = check(
            "disk",
            CheckStatus::Fail,
            "0.05 GB free",
            Some("free space"),
        );
        assert_eq!(c.line(), "FAIL disk: 0.05 GB free — free space");
        let ok = check("config", CheckStatus::Ok, "valid", None);
        assert_eq!(ok.line(), "OK   config: valid");
        assert!(
            !DoctorReport {
                checks: vec![ok.clone()]
            }
            .failed()
        );
        assert!(
            DoctorReport {
                checks: vec![ok, c]
            }
            .failed()
        );
    }

    fn health() -> Health {
        serde_json::from_value(serde_json::json!({
            "service": "alice", "instance": "i", "version": "0", "pid": 1, "memories": 3,
            "chunks": 5, "projects": 1, "embed_backlog": 0, "index_ready": true
        }))
        .unwrap()
    }

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}: {checks:?}"))
    }

    /// Every problem the workers surface shows up as a check with a next step: the read-only
    /// database, an overdue backup, a re-embed, rows the cloud policy holds back, the provider's
    /// error, the judge's warning, open reviews, the spool and the memory budget.
    #[test]
    fn a_health_reading_with_problems_gives_a_check_and_a_next_step_for_each() {
        let healthy = health_checks(&health());
        assert!(
            healthy.iter().all(|c| c.status != CheckStatus::Fail),
            "{healthy:?}"
        );
        assert_eq!(find(&healthy, "database").status, CheckStatus::Ok);
        assert_eq!(
            find(&healthy, "backups").status,
            CheckStatus::Warn,
            "none yet"
        );

        let mut h = health();
        h.read_only = Some("the disk is full".into());
        h.maintenance_warning =
            Some("the newest backup is 3 days old; run \"alice backup now\"".into());
        h.index_ready = false;
        h.embedding_model = "multilingual-e5-small-q8".into();
        h.reembed = Some(alice_core::api::ReembedStatus {
            model: "openai:m".into(),
            done: 40,
            total: 100,
        });
        h.embedding_error = Some("openai:m is unreachable".into());
        h.embed_skipped_cloud = 7;
        h.judge_warning = Some("no TypeSafe key; run set-secret jev_api_key".into());
        h.reviews_open = 51;
        h.spool_pending = 2;
        h.memory_bytes = Some(900_000_000);
        let checks = health_checks(&h);
        let db = find(&checks, "database");
        assert_eq!(db.status, CheckStatus::Fail);
        assert!(db.fix.as_deref().unwrap().contains("alice restore"));
        assert_eq!(find(&checks, "backups").status, CheckStatus::Warn);
        assert_eq!(find(&checks, "vector index").status, CheckStatus::Warn);
        let provider = find(&checks, "embedding provider");
        assert_eq!(provider.status, CheckStatus::Fail);
        assert!(
            provider
                .fix
                .as_deref()
                .unwrap()
                .contains("config test embedding")
        );
        let reembed = find(&checks, "re-embed");
        assert!(reembed.detail.contains("40 of 100") && reembed.detail.contains("openai:m"));
        assert!(find(&checks, "embedding policy").detail.contains("7 rows"));
        let judge = find(&checks, "judge");
        assert_eq!(judge.status, CheckStatus::Warn);
        assert!(
            judge
                .fix
                .as_deref()
                .unwrap()
                .contains("set-secret jev_api_key")
        );
        assert!(find(&checks, "reviews").detail.contains("51"));
        assert!(find(&checks, "spool").detail.contains('2'));
        assert!(find(&checks, "memory").fix.is_some());
        // Every check that is not fine says what to do.
        assert!(
            checks
                .iter()
                .filter(|c| c.status != CheckStatus::Ok)
                .all(|c| c.fix.is_some()),
            "{checks:?}"
        );
    }

    #[test]
    fn a_job_that_failed_since_it_last_worked_is_reported_and_a_later_success_clears_it() {
        let job = |last_ok: Option<&str>, err: Option<&str>| JobStatus {
            job: "backup".into(),
            every_secs: 86_400,
            last_run: None,
            last_ok: last_ok.map(String::from),
            last_error: err.map(String::from),
        };
        let failing = job(
            Some("2026-10-01T00:00:00.000Z"),
            Some("2026-10-02T00:00:00.000Z disk space: 10 MB free, 2 GB needed"),
        );
        let checks = job_checks(&[failing]);
        let c = find(&checks, "job backup");
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.fix.as_deref().unwrap().contains("free space"), "{c:?}");
        let other = job(None, Some("2026-10-02T00:00:00.000Z the copy failed"));
        let c = &job_checks(&[other])[0];
        assert!(
            c.fix
                .as_deref()
                .unwrap()
                .contains("alice maintenance run backup")
        );
        let recovered = job(
            Some("2026-10-03T00:00:00.000Z"),
            Some("2026-10-02T00:00:00.000Z disk space: 10 MB free"),
        );
        assert!(job_checks(&[recovered]).is_empty());
        assert!(job_checks(&[job(None, None)]).is_empty());
    }

    #[test]
    fn the_settings_the_daemon_holds_show_errors_degradation_waiting_changes_and_restarts() {
        let mut view: SettingsView = serde_json::from_value(serde_json::json!({
            "settings": {}, "sources": {}, "keys": [], "errors": [], "warnings": [],
            "secrets": {}, "restart_pending": []
        }))
        .unwrap();
        assert!(settings_checks(&view, false).is_empty());
        view.errors = vec![alice_core::settings::SettingError {
            key: "judge.review_threshold".into(),
            message: "must be below auto_apply_threshold".into(),
        }];
        view.config_degraded = true;
        view.warnings = vec!["ALICE_JUDGE_PROVIDR is not a setting".into()];
        view.restart_pending = vec!["server.port".into()];
        let checks = settings_checks(&view, true);
        let settings = find(&checks, "settings");
        assert_eq!(settings.status, CheckStatus::Fail);
        assert!(settings.detail.contains("judge.review_threshold"));
        assert!(settings.detail.contains("keeps the previous settings"));
        assert_eq!(find(&checks, "config loaded").status, CheckStatus::Warn);
        assert!(find(&checks, "settings change").fix.is_some());
        assert!(
            find(&checks, "daemon environment")
                .detail
                .contains("PROVIDR")
        );
        assert!(find(&checks, "restart").detail.contains("server.port"));
    }

    #[test]
    fn memory_and_disk_probes_answer_on_this_platform() {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(self_memory_bytes().unwrap() > 0);
            assert!(free_disk_bytes(Path::new(".")).unwrap() > 0);
        }
    }

    /// SP2 Task 8: hooks that run an `alice` which is gone (a moved binary) are a warning that
    /// names the agent, the program and the command that repairs them.
    #[test]
    fn doctor_warns_when_hooks_point_at_a_missing_binary() {
        use crate::agent_hooks::{self as ah, HookAgent, HookTarget};
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        let env = {
            let root = root.clone();
            move |k: &str| -> Option<std::ffi::OsString> {
                Some(match k {
                    "HOME" => root.join("home").into(),
                    "CLAUDE_CONFIG_DIR" => root.join("home/.claude").into(),
                    "CODEX_HOME" => root.join("home/.codex").into(),
                    "ALICE_HOME" => root.join("alice").into(),
                    _ => return None,
                })
            }
        };
        let now = chrono::Utc::now();
        let hooks = || find(&capture_checks(None, &env, now), "capture hooks").clone();
        let c = hooks();
        assert_eq!(c.status, CheckStatus::Ok, "nothing installed: {c:?}");
        assert!(
            c.fix
                .as_deref()
                .unwrap()
                .contains("alice hooks install claude"),
            "{c:?}"
        );
        let target = |agent, alice: std::path::PathBuf| HookTarget {
            alice,
            ..HookTarget::locate(agent, None, &env).unwrap()
        };
        let gone = root.join("gone/alice");
        ah::install(&target(HookAgent::Claude, gone.clone()), false).unwrap();
        let c = hooks();
        assert_eq!(c.status, CheckStatus::Warn, "{c:?}");
        assert!(
            c.detail.contains("Claude Code") && c.detail.contains(&gone.display().to_string()),
            "{c:?}"
        );
        assert_eq!(c.fix.as_deref(), Some("run `alice hooks install claude`"));
        ah::install(&target(HookAgent::Codex, gone.clone()), false).unwrap();
        let fix = hooks().fix.unwrap();
        assert!(
            fix.contains("alice hooks install claude") && fix.contains("alice hooks install codex"),
            "{fix}"
        );
        // Installed again from an `alice` that exists: fine.
        let alice = root.join("bin/alice");
        std::fs::create_dir_all(alice.parent().unwrap()).unwrap();
        std::fs::write(&alice, "").unwrap();
        ah::install(&target(HookAgent::Claude, alice.clone()), false).unwrap();
        ah::install(&target(HookAgent::Codex, alice), false).unwrap();
        let c = hooks();
        assert_eq!(c.status, CheckStatus::Ok, "{c:?}");
        assert!(
            c.detail.contains("Claude Code") && c.detail.contains("Codex"),
            "{c:?}"
        );
        // A settings file that is not JSON is the agent's to report, not a hooks warning.
        std::fs::write(root.join("home/.claude/settings.json"), "{,").unwrap();
        assert_eq!(hooks().status, CheckStatus::Ok);
    }

    /// SP2 Task 8: the capture line of the health warns when installed hooks captured nothing
    /// for a week, when transcript lines were not recognised, and names the Codex rollouts that
    /// were not found or were compressed.
    #[test]
    fn doctor_capture_line_warns_on_a_quiet_week_and_unrecognised_lines() {
        use alice_core::api::CaptureHealth;
        let now = chrono::Utc::now();
        let ago = |days| {
            (now - chrono::Duration::days(days))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let mut h = health();
        assert_eq!(capture_check(&h, true, now), None, "capture never ran");
        h.capture = Some(Box::new(CaptureHealth {
            last_capture_at: Some(ago(8)),
            open_sessions: 1,
            ..Default::default()
        }));
        let c = capture_check(&h, true, now).unwrap();
        assert_eq!(c.status, CheckStatus::Warn, "{c:?}");
        assert!(
            c.detail.contains("nothing was captured for 7 days"),
            "{c:?}"
        );
        assert!(c.fix.is_some(), "{c:?}");
        // With no hooks installed, a quiet week is what to expect.
        assert_eq!(
            capture_check(&h, false, now).unwrap().status,
            CheckStatus::Ok
        );
        let cap = h.capture.as_mut().unwrap();
        cap.last_capture_at = Some(ago(2));
        cap.sessions_24h = 4;
        cap.chunks_24h = 30;
        let c = capture_check(&h, true, now).unwrap();
        assert_eq!(c.status, CheckStatus::Ok, "{c:?}");
        assert!(
            c.detail.contains("4 sessions") && c.detail.contains("30 chunks"),
            "{c:?}"
        );
        h.capture.as_mut().unwrap().unrecognised_lines_7d = 12;
        let c = capture_check(&h, true, now).unwrap();
        assert_eq!(c.status, CheckStatus::Warn, "{c:?}");
        assert!(
            c.detail.contains("12")
                && c.detail
                    .contains("the agent's transcript format may have changed"),
            "{c:?}"
        );
        // Compressed Codex rollouts are mentioned, but leave nothing to do: no warning.
        let cap = h.capture.as_mut().unwrap();
        cap.unrecognised_lines_7d = 0;
        cap.rollouts_compressed = 2;
        let c = capture_check(&h, true, now).unwrap();
        assert_eq!(c.status, CheckStatus::Ok, "{c:?}");
        assert!(
            c.detail.contains("4 sessions") && c.detail.contains("2 Codex rollouts"),
            "{c:?}"
        );
        assert!(c.fix.is_none(), "{c:?}");
        // Rollouts not found are a warning, and the line still names the compressed ones.
        h.capture.as_mut().unwrap().rollouts_not_found = 3;
        let c = capture_check(&h, true, now).unwrap();
        assert_eq!(c.status, CheckStatus::Warn, "{c:?}");
        assert!(
            c.detail.contains("3 Codex sessions") && c.detail.contains("2 Codex rollouts"),
            "{c:?}"
        );
        assert!(c.fix.as_deref().unwrap().contains("CODEX_HOME"), "{c:?}");
        // The counts are sent only when set: a daemon's health reads as before.
        let json = serde_json::to_string(&CaptureHealth::default()).unwrap();
        assert!(!json.contains("rollouts"), "{json}");
    }
}
