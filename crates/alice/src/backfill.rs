//! `alice capture backfill <claude|codex>` (SP2 spec §4.4, Task 18b): capture the transcripts
//! already on disk, such as the sessions from before the hooks were installed.
//!
//! The command only finds the files and tells the local daemon about each one, with a
//! `backfill` notice like the one a hook sends. The daemon's capture worker reads every file
//! later, from its source's cursor, closing turns by the file's age as its sweep does. Turn
//! dedupe and the MemPalace rule (spec §9.2) keep what is stored or imported already from being
//! stored twice. A source that a backfill registers (a subagent's too) starts
//! `skipped_backfill`, so months of sessions do not become a burst of memories or spend;
//! `--distill` marks the sessions due once the daemon has registered them (`alice capture
//! distill`'s route). A backfilled session that goes on live (a resumed session's hooks) is
//! distilled from there on, never its backfilled history.

use alice_core::api::{CaptureDistillRequest, CaptureNotify};
use alice_core::error::AliceError;
use alice_daemon::client::DaemonClient;
use anyhow::{Context as _, Result, bail};
use clap::{Args, ValueEnum};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Args)]
pub struct BackfillArgs {
    /// Whose transcripts: `claude` (Claude Code: `projects/` under $CLAUDE_CONFIG_DIR or
    /// ~/.claude, with each session's `subagents/`) or `codex` (`sessions/` under $CODEX_HOME
    /// or ~/.codex; compressed `.zst` rollouts are skipped).
    pub agent: BackfillAgent,
    /// Only files modified at or after this date (YYYY-MM-DD, from local midnight, or an RFC
    /// 3339 time).
    #[arg(long)]
    pub since: Option<String>,
    /// Only sessions whose working directory belongs to this project: the slug a hook's
    /// session there gets (the repository's or the directory's name).
    #[arg(long)]
    pub project: Option<String>,
    /// Distil the backfilled sessions too: each is marked due once the daemon has registered
    /// it (only while distillation is on). Without it they are captured but not distilled
    /// (`alice capture distill <session>` distils one later).
    #[arg(long)]
    pub distill: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackfillAgent {
    Claude,
    Codex,
}

impl BackfillAgent {
    /// The agent as the daemon names it.
    fn name(self) -> &'static str {
        match self {
            BackfillAgent::Claude => "claude-code",
            BackfillAgent::Codex => "codex",
        }
    }

    /// The capture switch of this agent (`capture.<key>`).
    fn switch(self) -> &'static str {
        match self {
            BackfillAgent::Claude => "claude_code",
            BackfillAgent::Codex => "codex",
        }
    }
}

/// How much of a transcript's start is read for its working directory (and a Codex rollout's
/// session id). A line longer than this is not read.
const HEAD_BYTES: u64 = 2 << 20;
/// How deep `$CODEX_HOME/sessions` is searched (`YYYY/MM/DD/rollout-*.jsonl` is 4 levels).
const CODEX_DEPTH: usize = 6;
/// How long the daemon may take to accept a notice.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// `--distill`: how long to go on waiting for the daemon to register the remaining sessions
/// after it last registered one.
const MARK_STALL: Duration = Duration::from_secs(30);
/// `--distill`: how often the sessions not registered yet are asked for again.
const MARK_POLL: Duration = Duration::from_secs(1);

/// One transcript file to notify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Found {
    pub path: PathBuf,
    /// The session id (a subagent's: its main session's).
    pub session_id: String,
    /// A Claude Code subagent's id, for its own file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The session's working directory, from the transcript: a new source's project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The main transcript of a subagent's session (the path itself for a main file).
    #[serde(skip)]
    main: PathBuf,
    #[serde(skip)]
    modified: SystemTime,
}

/// Files found and not queued, by why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// Modified before `--since`.
    pub older: usize,
    /// Their sessions' working directories are in other projects (`--project`).
    pub other_projects: usize,
    /// Named like no session (an id the daemon would refuse).
    pub not_sessions: usize,
    /// Compressed Codex rollouts (`*.jsonl.zst`), which are not read (spec R21).
    pub compressed: usize,
}

/// What a backfill would notify: found and filtered, before anything is sent.
#[derive(Debug)]
pub struct Plan {
    agent: BackfillAgent,
    root: PathBuf,
    files: Vec<Found>,
    skipped: Skipped,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The report of a backfill that sends nothing.
    pub fn into_report(self, distill: bool) -> BackfillReport {
        BackfillReport {
            agent: self.agent.name(),
            root: self.root,
            queued: Vec::new(),
            skipped: self.skipped,
            refused: Vec::new(),
            distill,
            due: Vec::new(),
            not_due: Vec::new(),
            distill_off: false,
        }
    }
}

/// A notice the daemon refused, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refused {
    pub path: PathBuf,
    pub why: String,
}

/// What `alice capture backfill` did.
#[derive(Debug, Serialize)]
pub struct BackfillReport {
    pub agent: &'static str,
    /// Where the transcripts were looked for.
    pub root: PathBuf,
    /// The files the daemon took a notice for, in the order sent.
    pub queued: Vec<Found>,
    pub skipped: Skipped,
    pub refused: Vec<Refused>,
    /// `--distill` was given.
    pub distill: bool,
    /// `--distill`: the capture sources marked due.
    pub due: Vec<String>,
    /// `--distill`: the sessions the daemon had not registered when the command stopped
    /// waiting (nothing complete in them yet, or still in the queue).
    pub not_due: Vec<String>,
    /// `--distill` with `distill.provider = off`: nothing was marked due, so the sessions stay
    /// `skipped_backfill` (marked now, they would be distilled all at once when it is turned on).
    pub distill_off: bool,
}

impl BackfillReport {
    pub fn text(&self) -> String {
        let mut t = String::new();
        for f in &self.queued {
            t.push_str(&format!("queued {}\n", f.path.display()));
        }
        for r in &self.refused {
            t.push_str(&format!("refused {}: {}\n", r.path.display(), r.why));
        }
        for uri in &self.due {
            t.push_str(&format!("due for distillation: {uri}\n"));
        }
        for s in &self.not_due {
            t.push_str(&format!(
                "not captured yet, so not marked due: {s} (run `alice capture distill {s}` once `alice capture status` lists it)\n"
            ));
        }
        let s = &self.skipped;
        let mut why = Vec::new();
        for (n, what) in [
            (s.older, "older than --since"),
            (s.other_projects, "in other projects"),
            (s.not_sessions, "not named like a session"),
            (s.compressed, "compressed (.zst) rollouts"),
        ] {
            if n > 0 {
                why.push(format!("{n} {what}"));
            }
        }
        let skipped = if why.is_empty() {
            String::new()
        } else {
            format!("; skipped: {}", why.join(", "))
        };
        if self.queued.is_empty() {
            t.push_str(&format!(
                "no {} transcripts to backfill under {}{skipped}\n",
                self.agent,
                self.root.display()
            ));
            return t;
        }
        t.push_str(&format!(
            "{} file{} queued{skipped}. The daemon captures their complete turns in the background (`alice capture status`).\n",
            self.queued.len(),
            if self.queued.len() == 1 { "" } else { "s" },
        ));
        if !self.distill {
            t.push_str(
                "Backfilled sessions are not distilled: `alice capture distill <session>` distils one, or backfill again with --distill.\n",
            );
        } else if self.distill_off {
            t.push_str(
                "Distillation is off (distill.provider = off), so no session was marked due; they stay skipped_backfill. Turn it on with `alice config set distill.provider jev`, then backfill again with --distill (or `alice capture distill <session>`).\n",
            );
        }
        t
    }
}

/// Finds the transcripts and applies the filters. Reads files only; the daemon is not asked.
pub fn plan(a: &BackfillArgs) -> Result<Plan> {
    let since = match a.since.as_deref() {
        Some(s) => Some(since_ms(s)?),
        None => None,
    };
    let want = match a.project.as_deref().map(alice_core::normalize::slugify) {
        Some(p) if p.is_empty() => {
            bail!("--project names no project; use a slug from `alice projects`")
        }
        other => other,
    };
    let root = absolute(agent_root(a.agent)?)?;
    Ok(plan_in(a.agent, root, since, want.as_deref()))
}

/// [`plan`] under `root`: files modified at `since` (ms since the epoch) or later, of sessions
/// in the project `want` (a slug) when one is given.
fn plan_in(agent: BackfillAgent, root: PathBuf, since: Option<i64>, want: Option<&str>) -> Plan {
    let mut skipped = Skipped::default();
    let found = match agent {
        BackfillAgent::Claude => claude_files(&root, &mut skipped),
        BackfillAgent::Codex => codex_files(&root, &mut skipped),
    };
    // A project is detected once per working directory (it may run git).
    let mut slugs: HashMap<Option<String>, String> = HashMap::new();
    let mut files = Vec::new();
    for mut f in found {
        if since.is_some_and(|since| unix_ms(f.modified) < since) {
            skipped.older += 1;
            continue;
        }
        let head = head_facts(&f.path, agent);
        if let Some(id) = head.session {
            f.session_id = id;
        }
        if !alice_core::capture::valid_id(&f.session_id) {
            skipped.not_sessions += 1;
            continue;
        }
        f.cwd = head.cwd;
        if let Some(want) = want {
            // What the daemon files the session under: the directory's project, or `global`
            // when the transcript names none.
            let slug = slugs.entry(f.cwd.clone()).or_insert_with(|| match &f.cwd {
                Some(cwd) => alice_core::project::detect_project(Path::new(cwd)).slug,
                None => "global".into(),
            });
            if slug != want {
                skipped.other_projects += 1;
                continue;
            }
        }
        files.push(f);
    }
    // The oldest first, every main file before the subagents: a continued session's copy of
    // its parent's turns is met after the parent, and a subagent finds its parent's source.
    files.sort_by(|x, y| {
        (x.agent_id.is_some(), x.modified, &x.path).cmp(&(
            y.agent_id.is_some(),
            y.modified,
            &y.path,
        ))
    });
    Plan {
        agent,
        root,
        files,
        skipped,
    }
}

/// Sends one `backfill` notice per planned file and, with `distill` (and distillation on),
/// marks the sessions due as the daemon registers them. Refuses to start while the agent's
/// capture is switched off: the daemon would record every turn as not captured, for good.
pub async fn send(daemon: &DaemonClient, plan: Plan, distill: bool) -> Result<BackfillReport> {
    let view = daemon.settings().await?;
    let switch = |path: &[&str]| {
        path.iter()
            .try_fold(&view.settings, |v, k| v.get(k))
            .and_then(serde_json::Value::as_bool)
    };
    if switch(&["capture", "enabled"]) == Some(false) {
        bail!(
            "capture is off (capture.enabled = false): a backfill now would record every turn as not captured, for good. Turn it on with `alice config set capture.enabled true`, then backfill"
        );
    }
    let agent_switch = plan.agent.switch();
    if switch(&["capture", agent_switch]) == Some(false) {
        bail!(
            "capture of {} is off (capture.{agent_switch} = false): a backfill now would record every turn as not captured, for good. Turn it on with `alice config set capture.{agent_switch} true`, then backfill",
            plan.agent.name()
        );
    }
    let distill_off = distill
        && view.settings["distill"]["provider"]
            .as_str()
            .is_some_and(|p| p == "off");
    let Plan {
        agent,
        root,
        files,
        skipped,
    } = plan;
    let mut report = Plan {
        agent,
        root,
        files: Vec::new(),
        skipped,
    }
    .into_report(distill);
    report.distill_off = distill_off;
    for f in files {
        match daemon
            .capture_notify(&notice(agent, &f), NOTIFY_TIMEOUT)
            .await
        {
            Ok(()) => report.queued.push(f),
            Err(AliceError::Invalid(why)) => report.refused.push(Refused { path: f.path, why }),
            Err(e) => {
                return Err(anyhow::Error::from(e)).with_context(|| {
                    format!(
                        "the daemon did not take the notice for {} ({} sent before it)",
                        f.path.display(),
                        report.queued.len()
                    )
                });
            }
        }
    }
    // With distillation off, a session marked due is set back to idle at once, and the quiet
    // trigger would then distil the whole backfill in a burst once it is turned on (review M1):
    // nothing is marked, and the report says to run it again.
    if distill && !distill_off {
        mark_due(daemon, &mut report).await?;
    }
    Ok(report)
}

/// The notice a hook would send for `f`, as a backfill.
fn notice(agent: BackfillAgent, f: &Found) -> CaptureNotify {
    let subagent = f.agent_id.is_some();
    CaptureNotify {
        agent: agent.name().into(),
        event: "backfill".into(),
        session_id: f.session_id.clone(),
        transcript_path: f.main.to_string_lossy().into_owned(),
        cwd: f.cwd.clone(),
        agent_id: f.agent_id.clone(),
        agent_transcript_path: subagent.then(|| f.path.to_string_lossy().into_owned()),
        source: None,
        reason: None,
    }
}

/// `--distill`: marks every queued session due once the daemon has registered it, asking again
/// for the rest every [`MARK_POLL`] until none is left or none was registered for
/// [`MARK_STALL`].
async fn mark_due(daemon: &DaemonClient, report: &mut BackfillReport) -> Result<()> {
    let mut seen = HashSet::new();
    let mut pending: Vec<String> = report
        .queued
        .iter()
        .map(|f| f.session_id.clone())
        .filter(|s| seen.insert(s.clone()))
        .collect();
    let mut progress = Instant::now();
    loop {
        let mut left = Vec::new();
        for session in pending {
            let asked = daemon
                .capture_distill(&CaptureDistillRequest {
                    session_id: Some(session.clone()),
                    source_id: None,
                })
                .await;
            match asked {
                Ok(r) => {
                    report.due.extend(r.due);
                    progress = Instant::now();
                }
                Err(AliceError::NotFound(_)) => left.push(session),
                Err(e) => {
                    return Err(anyhow::Error::from(e))
                        .with_context(|| format!("cannot mark the session {session} due"));
                }
            }
        }
        pending = left;
        if pending.is_empty() || progress.elapsed() >= MARK_STALL {
            break;
        }
        tokio::time::sleep(MARK_POLL).await;
    }
    report.not_due = pending;
    Ok(())
}

/// `$CLAUDE_CONFIG_DIR` or `~/.claude`; `$CODEX_HOME` or `~/.codex`.
fn agent_root(agent: BackfillAgent) -> Result<PathBuf> {
    let (var, dir) = match agent {
        BackfillAgent::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        BackfillAgent::Codex => ("CODEX_HOME", ".codex"),
    };
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    std::env::home_dir()
        .map(|h| h.join(dir))
        .with_context(|| format!("cannot find the home directory; set HOME or {var}"))
}

/// `p`, absolute (the daemon takes absolute paths only).
fn absolute(p: PathBuf) -> Result<PathBuf> {
    if p.is_absolute() {
        return Ok(p);
    }
    Ok(std::env::current_dir()
        .context("cannot read the current directory")?
        .join(p))
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok())
}

fn read_dir(p: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(p)
        .map(|d| d.flatten().collect())
        .unwrap_or_default()
}

fn name_of(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// Claude Code's transcripts: `<root>/projects/<folder>/<session>.jsonl`, and each session's
/// subagents, `<root>/projects/<folder>/<session>/subagents/agent-<id>.jsonl`.
fn claude_files(root: &Path, skipped: &mut Skipped) -> Vec<Found> {
    let mut out = Vec::new();
    for folder in read_dir(&root.join("projects")) {
        let folder = folder.path();
        if !folder.is_dir() {
            continue;
        }
        for entry in read_dir(&folder) {
            let path = entry.path();
            if path.is_dir() {
                let session = name_of(&path).to_owned();
                let main = folder.join(format!("{session}.jsonl"));
                for sub in read_dir(&path.join("subagents")) {
                    let sub = sub.path();
                    let Some(stem) = name_of(&sub).strip_suffix(".jsonl") else {
                        continue;
                    };
                    let Some(at) = modified(&sub) else { continue };
                    let agent = stem.strip_prefix("agent-").unwrap_or(stem).to_owned();
                    if !alice_core::capture::valid_id(&session)
                        || !alice_core::capture::valid_id(&agent)
                    {
                        skipped.not_sessions += 1;
                        continue;
                    }
                    out.push(Found {
                        path: sub.clone(),
                        session_id: session.clone(),
                        agent_id: Some(agent),
                        cwd: None,
                        main: main.clone(),
                        modified: at,
                    });
                }
                continue;
            }
            let Some(stem) = name_of(&path).strip_suffix(".jsonl") else {
                continue;
            };
            let Some(at) = modified(&path) else { continue };
            if !alice_core::capture::valid_id(stem) {
                skipped.not_sessions += 1;
                continue;
            }
            out.push(Found {
                session_id: stem.to_owned(),
                agent_id: None,
                cwd: None,
                main: path.clone(),
                path,
                modified: at,
            });
        }
    }
    out
}

/// Codex's rollouts: `<root>/sessions/**/rollout-<time>-<session id>.jsonl`. A compressed one
/// (`.jsonl.zst`) is counted, not queued. The session id is the name's (the session meta's,
/// when [`plan`] reads it).
fn codex_files(root: &Path, skipped: &mut Skipped) -> Vec<Found> {
    let mut files = Vec::new();
    walk(&root.join("sessions"), 0, &mut files);
    let mut out = Vec::new();
    for path in files {
        let name = name_of(&path);
        if !name.starts_with("rollout-") {
            continue;
        }
        if name.ends_with(".jsonl.zst") {
            skipped.compressed += 1;
            continue;
        }
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some(at) = modified(&path) else { continue };
        out.push(Found {
            session_id: rollout_session(stem).to_owned(),
            agent_id: None,
            cwd: None,
            main: path.clone(),
            path,
            modified: at,
        });
    }
    out
}

/// The session id in a rollout's name, `rollout-YYYY-MM-DDTHH-MM-SS-<id>`: what follows the
/// time (empty when the name has none).
fn rollout_session(stem: &str) -> &str {
    let rest = stem.strip_prefix("rollout-").unwrap_or(stem);
    // `YYYY-MM-DDTHH-MM-SS-` is 20 characters.
    rest.get(20..).unwrap_or("")
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > CODEX_DEPTH {
        return;
    }
    for entry in read_dir(dir) {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&entry.path(), depth + 1, out);
        } else {
            out.push(entry.path());
        }
    }
}

/// What the start of a transcript says.
#[derive(Debug, Default, PartialEq, Eq)]
struct Head {
    cwd: Option<String>,
    /// A Codex rollout's session id (its `session_meta`).
    session: Option<String>,
}

/// The working directory (and a rollout's session id) from the first [`HEAD_BYTES`] of a
/// transcript: a Claude Code record's `cwd`; a Codex `session_meta`'s `id` and `cwd`, or a
/// `turn_context`'s `cwd`. Lines that do not parse are passed over. Read a line at a time,
/// stopping at the first answer (usually the first line or two, review M3).
fn head_facts(path: &Path, agent: BackfillAgent) -> Head {
    let Ok(file) = std::fs::File::open(path) else {
        return Head::default();
    };
    let mut reader = std::io::BufReader::new(file).take(HEAD_BYTES);
    let text = |v: &serde_json::Value| {
        v.as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    };
    let mut head = Head::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        // Only complete lines: the last one may be cut at the limit (or the file's end).
        match reader.read_until(b'\n', &mut line) {
            Ok(_) if line.last() == Some(&b'\n') => {}
            _ => return head,
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        match agent {
            BackfillAgent::Claude => {
                if let Some(cwd) = text(&v["cwd"]) {
                    head.cwd = Some(cwd);
                    return head;
                }
            }
            BackfillAgent::Codex => {
                let payload = &v["payload"];
                match v["type"].as_str() {
                    Some("session_meta") => {
                        head.session = head
                            .session
                            .or_else(|| text(&payload["id"]))
                            .or_else(|| text(&payload["session_id"]));
                        head.cwd = head.cwd.or_else(|| text(&payload["cwd"]));
                    }
                    Some("turn_context") => head.cwd = head.cwd.or_else(|| text(&payload["cwd"])),
                    _ => {}
                }
                if head.cwd.is_some() && head.session.is_some() {
                    return head;
                }
            }
        }
    }
}

/// `--since` as milliseconds since the Unix epoch: a date is local midnight, as for
/// `alice import mempalace --since`.
fn since_ms(s: &str) -> Result<i64> {
    alice_core::import::plan::parse_time(s, alice_core::import::local_offset())
        .and_then(|utc| utc_ms(&utc))
        .with_context(|| {
            format!(
                "--since \"{}\" is not a date; use YYYY-MM-DD (2026-04-16) or an RFC 3339 time (2026-04-16T09:00:00+07:00)",
                s.trim()
            )
        })
}

/// Milliseconds since the Unix epoch of a UTC time written `YYYY-MM-DDTHH:MM:SS[.mmm]Z`.
fn utc_ms(t: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let ms = if t.as_bytes().get(19) == Some(&b'.') {
        num(20..23)?
    } else {
        0
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from the civil date (H. Hinnant's algorithm), with March as the year's first month.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + ms)
}

fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, lines: &[serde_json::Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(path, text).unwrap();
    }

    fn age(path: &Path, days: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(days * 24 * 3600))
            .unwrap();
    }

    fn names(found: &[Found]) -> Vec<(String, Option<String>)> {
        found
            .iter()
            .map(|f| (f.session_id.clone(), f.agent_id.clone()))
            .collect()
    }

    #[test]
    fn claude_files_are_sessions_and_their_subagents() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects").join("-w-app");
        let rec = serde_json::json!({"type": "user", "cwd": "/w/app"});
        write(&p.join("s1.jsonl"), std::slice::from_ref(&rec));
        write(
            &p.join("s1/subagents/agent-a7.jsonl"),
            std::slice::from_ref(&rec),
        );
        write(&p.join("s1/subagents/agent-a7.meta.json"), &[]);
        write(
            &p.join("s1/tool-results/x.jsonl"),
            std::slice::from_ref(&rec),
        );
        write(&p.join("bad name.jsonl"), std::slice::from_ref(&rec));
        write(&p.join("notes.txt"), &[]);
        write(
            &d.path().join("projects/stray.jsonl"),
            std::slice::from_ref(&rec),
        );
        let mut skipped = Skipped::default();
        let mut found = claude_files(d.path(), &mut skipped);
        found.sort_by(|a, b| (a.agent_id.is_some(), &a.path).cmp(&(b.agent_id.is_some(), &b.path)));
        assert_eq!(
            names(&found),
            [
                ("s1".to_string(), None),
                ("s1".to_string(), Some("a7".to_string()))
            ]
        );
        assert_eq!(
            found[1].main,
            p.join("s1.jsonl"),
            "a subagent names its main file"
        );
        assert_eq!(skipped.not_sessions, 1, "{skipped:?}");
        // No Claude Code configuration at all: nothing, and no error.
        assert!(claude_files(&d.path().join("none"), &mut skipped).is_empty());
    }

    #[test]
    fn codex_rollouts_are_found_at_any_depth_and_compressed_ones_counted() {
        let d = tempfile::tempdir().unwrap();
        let day = d.path().join("sessions/2026/10/03");
        let id = "3c1efcf6-e7db-4061-6708-ee9e0a7a7516";
        write(
            &day.join(format!("rollout-2026-10-03T01-57-34-{id}.jsonl")),
            &[serde_json::json!({"type": "session_meta", "payload": {"id": id, "cwd": "/w/x"}})],
        );
        write(&day.join("rollout-2026-10-02T01-00-00-old.jsonl.zst"), &[]);
        write(&day.join("history.jsonl"), &[]);
        let mut skipped = Skipped::default();
        let found = codex_files(d.path(), &mut skipped);
        assert_eq!(names(&found), [(id.to_string(), None)]);
        assert_eq!(skipped.compressed, 1);
        assert_eq!(rollout_session("rollout-2026-10-03T01-57-34-abc"), "abc");
        assert_eq!(rollout_session("rollout-short"), "");
    }

    #[test]
    fn the_head_gives_the_working_directory_and_a_rollouts_session() {
        let d = tempfile::tempdir().unwrap();
        let claude = d.path().join("c.jsonl");
        std::fs::write(
            &claude,
            concat!(
                "{\"type\":\"summary\",\"summary\":\"x\"}\n",
                "not json\n",
                "{\"type\":\"user\",\"cwd\":\"\"}\n",
                "{\"type\":\"user\",\"cwd\":\"/w/งาน x\"}\n",
                "{\"type\":\"user\",\"cwd\":\"/w/later\"}\n",
            ),
        )
        .unwrap();
        assert_eq!(
            head_facts(&claude, BackfillAgent::Claude),
            Head {
                cwd: Some("/w/งาน x".into()),
                session: None
            }
        );
        let codex = d.path().join("r.jsonl");
        write(
            &codex,
            &[
                serde_json::json!({"type": "session_meta", "payload": {"session_id": "c-1"}}),
                serde_json::json!({"type": "turn_context", "payload": {"cwd": "/w/y"}}),
            ],
        );
        assert_eq!(
            head_facts(&codex, BackfillAgent::Codex),
            Head {
                cwd: Some("/w/y".into()),
                session: Some("c-1".into())
            }
        );
        // A line cut at the limit is not read; a missing file says nothing.
        let long = d.path().join("long.jsonl");
        let mut big = br#"{"cwd":"/w/z","text":""#.to_vec();
        big.extend(std::iter::repeat_n(b'x', HEAD_BYTES as usize));
        big.extend(b"\"}\n");
        std::fs::write(&long, big).unwrap();
        assert_eq!(head_facts(&long, BackfillAgent::Claude), Head::default());
        assert_eq!(
            head_facts(&d.path().join("missing.jsonl"), BackfillAgent::Claude),
            Head::default()
        );
    }

    #[test]
    fn plan_filters_by_date_and_project_and_puts_subagents_last() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("claude");
        let work = d.path().join("work");
        let other = d.path().join("other");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let rec = |cwd: &Path| serde_json::json!({"type": "user", "cwd": cwd});
        let p = root.join("projects").join("-w");
        for (name, cwd, days) in [
            ("old.jsonl", &work, 30),
            ("s2.jsonl", &work, 2),
            ("s1.jsonl", &work, 3),
            ("s1/subagents/agent-k1.jsonl", &work, 3),
            ("o1.jsonl", &other, 1),
        ] {
            write(&p.join(name), &[rec(cwd)]);
            age(&p.join(name), days);
        }
        let ten_days_ago = unix_ms(SystemTime::now() - Duration::from_secs(10 * 24 * 3600));
        let all = plan_in(
            BackfillAgent::Claude,
            root.clone(),
            Some(ten_days_ago),
            None,
        );
        assert_eq!(
            names(&all.files),
            [
                ("s1".to_string(), None),
                ("s2".to_string(), None),
                ("o1".to_string(), None),
                ("s1".to_string(), Some("k1".to_string())),
            ],
            "the oldest first, subagents last"
        );
        assert_eq!(all.skipped.older, 1);
        assert_eq!(all.files[0].cwd.as_deref(), work.to_str());
        let work_only = plan_in(BackfillAgent::Claude, root.clone(), None, Some("work"));
        assert_eq!(
            names(&work_only.files),
            [
                ("old".to_string(), None),
                ("s1".to_string(), None),
                ("s2".to_string(), None),
                ("s1".to_string(), Some("k1".to_string())),
            ]
        );
        assert_eq!(work_only.skipped.other_projects, 1);
        // A transcript that names no working directory is the global project's.
        write(&p.join("g1.jsonl"), &[serde_json::json!({"type": "user"})]);
        let global = plan_in(BackfillAgent::Claude, root, None, Some("global"));
        assert_eq!(names(&global.files), [("g1".to_string(), None)]);
    }

    #[test]
    fn since_reads_dates_and_times_and_refuses_the_rest() {
        assert_eq!(utc_ms("1970-01-01T00:00:00.000Z"), Some(0));
        // `date -u -j -f %Y-%m-%dT%H:%M:%S 2026-10-17T05:06:07 +%s` = 1792213567.
        assert_eq!(utc_ms("2026-10-17T05:06:07.089Z"), Some(1_792_213_567_089));
        assert_eq!(utc_ms("2000-02-29T00:00:00Z"), Some(951_782_400_000));
        assert_eq!(utc_ms("1969-12-31T23:59:59.000Z"), Some(-1000));
        assert_eq!(utc_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(
            since_ms("2026-10-17T12:06:07+07:00").unwrap(),
            1_792_213_567_000
        );
        assert!(since_ms("2026-10-17").is_ok());
        let e = since_ms("last week").unwrap_err().to_string();
        assert!(e.contains("is not a date"), "{e}");
    }

    #[test]
    fn the_report_lists_what_was_queued_and_what_to_do_next() {
        let f = |path: &str, agent: Option<&str>| Found {
            path: PathBuf::from(path),
            session_id: "s1".into(),
            agent_id: agent.map(String::from),
            cwd: None,
            main: PathBuf::from("/c/s1.jsonl"),
            modified: SystemTime::UNIX_EPOCH,
        };
        let mut r = BackfillReport {
            agent: "claude-code",
            root: PathBuf::from("/c"),
            queued: vec![
                f("/c/s1.jsonl", None),
                f("/c/s1/subagents/agent-k.jsonl", Some("k")),
            ],
            skipped: Skipped {
                older: 2,
                ..Skipped::default()
            },
            refused: Vec::new(),
            distill: false,
            due: Vec::new(),
            not_due: Vec::new(),
            distill_off: false,
        };
        let t = r.text();
        assert!(
            t.starts_with("queued /c/s1.jsonl\nqueued /c/s1/subagents/agent-k.jsonl\n"),
            "{t}"
        );
        assert!(
            t.contains("2 files queued; skipped: 2 older than --since."),
            "{t}"
        );
        assert!(t.contains("are not distilled"), "{t}");
        r.distill = true;
        r.distill_off = true;
        r.due = vec!["claude-code:session:s1".into()];
        r.not_due = vec!["s9".into()];
        let t = r.text();
        assert!(
            t.contains("due for distillation: claude-code:session:s1\n"),
            "{t}"
        );
        assert!(t.contains("alice capture distill s9"), "{t}");
        assert!(t.contains("Distillation is off"), "{t}");
        r.queued.clear();
        assert!(
            r.text()
                .contains("no claude-code transcripts to backfill under /c; skipped: 2 older")
        );
    }

    #[test]
    fn a_notice_is_what_a_hook_sends_as_a_backfill() {
        let main = Found {
            path: PathBuf::from("/c/p/s1.jsonl"),
            session_id: "s1".into(),
            agent_id: None,
            cwd: Some("/w".into()),
            main: PathBuf::from("/c/p/s1.jsonl"),
            modified: SystemTime::UNIX_EPOCH,
        };
        let n = notice(BackfillAgent::Claude, &main);
        assert_eq!(
            (n.agent.as_str(), n.event.as_str(), n.session_id.as_str()),
            ("claude-code", "backfill", "s1")
        );
        assert_eq!(
            (n.transcript_path.as_str(), n.cwd.as_deref()),
            ("/c/p/s1.jsonl", Some("/w"))
        );
        assert_eq!((n.agent_id, n.agent_transcript_path), (None, None));
        let sub = Found {
            path: PathBuf::from("/c/p/s1/subagents/agent-k.jsonl"),
            agent_id: Some("k".into()),
            ..main.clone()
        };
        let n = notice(BackfillAgent::Claude, &sub);
        assert_eq!(n.transcript_path, "/c/p/s1.jsonl");
        assert_eq!(n.agent_id.as_deref(), Some("k"));
        assert_eq!(
            n.agent_transcript_path.as_deref(),
            Some("/c/p/s1/subagents/agent-k.jsonl")
        );
        assert_eq!(notice(BackfillAgent::Codex, &main).agent, "codex");
    }
}
