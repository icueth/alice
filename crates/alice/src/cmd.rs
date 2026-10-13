//! Subcommands: their arguments and what each one does.

use crate::Command;
use alice_core::api::*;
use alice_core::error::AliceError;
use alice_core::model::{Author, Kind};
use alice_core::store::kg::{AssertFact, FactAction, FactsQuery};
use alice_core::store::memories::{MemoryUpdate, UpdateAction};
use alice_daemon::client::{DaemonClient, Launcher, Unavailable, remember_or_spool};
use alice_daemon::daemon::{DaemonOptions, run};
use alice_daemon::paths::AliceHome;
use alice_daemon::startup::{self, lock_free, lock_pid, pid_alive};
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long `daemon stop` waits for the daemon to finish saving and let go of its lock (and,
/// before that, for a daemon that is still starting to answer).
const STOP_WAIT: Duration = Duration::from_secs(30);
const STOP_POLL: Duration = Duration::from_millis(100);
/// How long `daemon start` first waits for the daemon it starts before it looks at who holds
/// the lock and what that process is doing.
const START_FIRST_WAIT: Duration = Duration::from_secs(2);
/// How long `daemon start` waits for a lock holder that announces nothing new (a download in
/// progress announces itself every second).
const START_IDLE_WAIT: Duration = Duration::from_secs(60);
const START_POLL: Duration = Duration::from_millis(200);
/// Daemons `daemon start` starts at most, when one lets go of the lock without serving.
const START_ATTEMPTS: usize = 3;
/// Lines `daemon logs` prints.
const LOG_LINES: usize = 100;
/// Only the end of the daemon log is read: it is not rotated and may grow large.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Args)]
pub struct RecallArgs {
    /// Keywords or a question.
    #[arg(required = true)]
    pub query: Vec<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long = "kind")]
    pub kinds: Vec<String>,
    #[arg(long = "topic")]
    pub topics: Vec<String>,
    /// Also search raw session transcripts.
    #[arg(long)]
    pub raw: bool,
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub max_tokens: Option<usize>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub before: Option<String>,
}

#[derive(Args)]
pub struct RememberArgs {
    /// Memory text, or "-" to read stdin.
    pub text: Vec<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long = "topic")]
    pub topics: Vec<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub importance: Option<u8>,
    #[arg(long)]
    pub supersedes: Vec<String>,
    /// Author shown on the memory (default: $ALICE_AGENT or "cli").
    #[arg(long, env = "ALICE_AGENT", default_value = "cli")]
    pub agent: String,
}

#[derive(Args)]
pub struct ContextArgs {
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub max_tokens: Option<usize>,
}

#[derive(Args)]
pub struct GetArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub provenance: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ActionArg {
    Edit,
    Supersede,
    Archive,
    Forget,
    Restore,
}

#[derive(Args)]
pub struct UpdateArgs {
    pub id: String,
    #[arg(long, value_enum)]
    pub action: ActionArg,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub importance: Option<u8>,
    /// For supersede: the id of the replacing memory.
    #[arg(long)]
    pub by: Option<String>,
}

#[derive(Args)]
pub struct FactsArgs {
    pub entity: String,
    #[arg(long)]
    pub predicate: Option<String>,
    #[arg(long)]
    pub as_of: Option<String>,
    #[arg(long)]
    pub timeline: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum FactActionArg {
    Add,
    Supersede,
    End,
}

#[derive(Args)]
pub struct FactArgs {
    #[arg(value_enum)]
    pub action: FactActionArg,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// When it became (or, for end, stopped being) true.
    #[arg(long)]
    pub from: Option<String>,
}

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// Run in the foreground.
    Run,
    /// Start in the background if not running.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Show whether it runs.
    Status,
    /// Print the last lines of the daemon log.
    Logs,
}

#[derive(Subcommand)]
pub enum ModelCmd {
    /// Download the embedding model (~136 MB) for offline use.
    Fetch,
}

#[derive(Args)]
pub struct BenchArgs {
    #[arg(long, default_value_t = 2_000)]
    pub memories: usize,
    #[arg(long, default_value_t = 100_000)]
    pub chunks: usize,
    /// Use the real embedding model instead of the fake one.
    #[arg(long)]
    pub real: bool,
    /// Write results as JSON to this file.
    #[arg(long)]
    pub out: Option<std::path::PathBuf>,
    /// Calls measured per operation.
    #[arg(long, default_value_t = 200)]
    pub samples: usize,
    /// Keep the bench data in this directory (instead of a temporary one) and reuse it when it
    /// already holds the requested data. Never point it at a real ALICE_HOME.
    #[arg(long)]
    pub dir: Option<std::path::PathBuf>,
}

fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn launcher() -> Launcher {
    std::env::current_exe()
        .map(Launcher::Exe)
        .unwrap_or(Launcher::None)
}

fn cwd() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn parse_kind(k: &str) -> Result<Kind> {
    Kind::parse(k).with_context(|| {
        format!(
            "unknown kind {k:?}; use one of: {}",
            Kind::ALL
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn kind(s: &Option<String>) -> Result<Option<Kind>> {
    s.as_deref().map(parse_kind).transpose()
}

/// Stdout was closed by its reader (`alice recall … | head`): the command ends quietly.
#[derive(Debug)]
pub struct StdoutClosed;

impl std::fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout was closed")
    }
}

impl std::error::Error for StdoutClosed {}

/// Write `text` to stdout; a closed stdout is `StdoutClosed`, not a panic (as `print!` would).
pub(crate) fn out(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(StdoutClosed.into()),
        r => Ok(r?),
    }
}

fn print<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce() -> String) -> Result<()> {
    if json {
        out(&format!("{}\n", serde_json::to_string_pretty(value)?))
    } else {
        out(&text())
    }
}

async fn client(home: &AliceHome) -> Result<DaemonClient> {
    Ok(DaemonClient::connect(home, &launcher(), Duration::from_secs(10)).await?)
}

pub fn dispatch(command: Command, json: bool) -> Result<()> {
    let home = AliceHome::from_env();
    match command {
        Command::Daemon(DaemonCmd::Run) => run(
            home,
            DaemonOptions {
                port: 0,
                fake_embedder: std::env::var("ALICE_EMBEDDER").is_ok_and(|v| v == "fake"),
            },
        ),
        Command::Mcp => rt()?.block_on(alice_daemon::mcp::run_stdio(home, launcher())),
        Command::Model(ModelCmd::Fetch) => rt()?.block_on(async {
            alice_embed::fetch_model(&home.models_dir(), &|name, done, total| {
                eprint!(
                    "\r{name}: {:>5.1}%",
                    done as f64 * 100.0 / total.max(1) as f64
                );
            })
            .await?;
            eprintln!(
                "\nmodel ready in {}",
                alice_embed::model_dir(&home.models_dir()).display()
            );
            Ok(())
        }),
        Command::Bench(args) => crate::bench::bench(home, args, json),
        other => rt()?.block_on(client_command(home, other, json)),
    }
}

async fn client_command(home: AliceHome, command: Command, json: bool) -> Result<()> {
    match command {
        Command::Recall(a) => {
            let kinds = a
                .kinds
                .iter()
                .map(|k| parse_kind(k))
                .collect::<Result<Vec<_>>>()?;
            let req = RecallRequest {
                query: a.query.join(" "),
                project: a.project,
                topics: a.topics,
                kinds,
                since: a.since,
                before: a.before,
                include_raw: a.raw,
                limit: a.limit,
                max_tokens: a.max_tokens,
                format: if json {
                    OutputFormat::Json
                } else {
                    OutputFormat::Text
                },
                cwd: cwd(),
                ..Default::default()
            };
            let r = client(&home).await?.recall(&req).await?;
            print(json, &r, || r.text.clone().unwrap_or_default())
        }
        Command::Remember(a) => {
            let content = if a.text.len() == 1 && a.text[0] == "-" {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .context("cannot read the memory from stdin")?;
                s
            } else {
                a.text.join(" ")
            };
            if content.trim().is_empty() {
                bail!("nothing to remember: pass text or \"-\" to read stdin");
            }
            let req = RememberRequest {
                content,
                kind: kind(&a.kind)?,
                title: a.title,
                project: a.project,
                topics: a.topics,
                importance: a.importance,
                supersedes: a.supersedes,
                cwd: cwd(),
                author: Author {
                    // `ALICE_AGENT=` (set but empty) means the default.
                    agent: Some(match a.agent.trim() {
                        "" => "cli".to_string(),
                        agent => agent.to_string(),
                    }),
                    ..Default::default()
                },
                ..Default::default()
            };
            let r = remember_or_spool(&home, &launcher(), req).await?;
            print(json, &r, || match (&r.status, &r.id) {
                // Why it was queued and when it is saved (the daemon is starting, could not
                // start, or the database is read-only).
                (RememberStatus::Spooled, _) => format!(
                    "Queued: {}.\n",
                    r.reason
                        .as_deref()
                        .unwrap_or("the memory is saved when the daemon is available")
                ),
                (RememberStatus::Duplicate, Some(id)) => {
                    format!("Already remembered: {id} ({})\n", r.project)
                }
                (_, Some(id)) => format!("Saved {id} ({})\n", r.project),
                _ => "Saved.\n".into(),
            })
        }
        Command::Context(a) => {
            let r = client(&home)
                .await?
                .context(&ContextRequest {
                    project: a.project,
                    max_tokens: a.max_tokens,
                    cwd: cwd(),
                })
                .await?;
            print(json, &r, || r.text.clone())
        }
        Command::Get(a) => {
            let r = client(&home)
                .await?
                .get(&GetRequest {
                    ids: a.ids,
                    include_provenance: a.provenance,
                })
                .await?;
            print(json, &r, || {
                let mut s = String::new();
                for m in &r.memories {
                    let m = &m.memory;
                    s.push_str(&format!(
                        "{} · {} · {} · {} · {}\n{}\n{}\n\n",
                        m.id,
                        m.kind.as_str(),
                        m.project,
                        m.status.as_str(),
                        m.created_at,
                        m.title,
                        m.body
                    ));
                }
                for id in &r.missing {
                    s.push_str(&format!("not found: {id}\n"));
                }
                s
            })
        }
        Command::Update(a) => {
            let action = match a.action {
                ActionArg::Edit => UpdateAction::Edit,
                ActionArg::Supersede => UpdateAction::Supersede,
                ActionArg::Archive => UpdateAction::Archive,
                ActionArg::Forget => UpdateAction::Forget,
                ActionArg::Restore => UpdateAction::Restore,
            };
            let update = MemoryUpdate {
                action,
                title: a.title,
                body: a.body,
                kind: kind(&a.kind)?,
                topics: None,
                importance: a.importance,
                superseded_by: a.by,
            };
            let r = client(&home)
                .await?
                .update(&UpdateRequest { id: a.id, update })
                .await?;
            print(json, &r, || {
                format!("{} is now {}\n", r.memory.id, r.memory.status.as_str())
            })
        }
        Command::Facts(a) => {
            let r = client(&home)
                .await?
                .facts(&FactsQuery {
                    entity: a.entity,
                    predicate: a.predicate,
                    as_of: a.as_of,
                    timeline: a.timeline,
                })
                .await?;
            print(json, &r, || r.text.clone())
        }
        Command::Fact(a) => {
            let action = match a.action {
                FactActionArg::Add => FactAction::Add,
                FactActionArg::Supersede => FactAction::Supersede,
                FactActionArg::End => FactAction::End,
            };
            let r = client(&home)
                .await?
                .assert_fact(&AssertFact {
                    subject: a.subject,
                    predicate: a.predicate,
                    object: a.object,
                    valid_from: a.from,
                    action,
                    ..Default::default()
                })
                .await?;
            print(json, &r, || {
                format!(
                    "ok ({}; ended {})\n",
                    r.relation_id.clone().unwrap_or_else(|| "-".into()),
                    r.ended
                )
            })
        }
        Command::Projects => {
            let r = client(&home).await?.projects().await?;
            print(json, &r, || {
                r.iter()
                    .map(|p| format!("{}\t{}\n", p.slug, p.name))
                    .collect()
            })
        }
        Command::Status | Command::Daemon(DaemonCmd::Status) => status(&home, json).await,
        Command::Daemon(DaemonCmd::Start) => start(&home, json).await,
        Command::Daemon(DaemonCmd::Stop) => stop(&home, json).await,
        Command::Daemon(DaemonCmd::Logs) => {
            let path = home.logs_dir().join("daemon.log");
            let lines = log_tail(&path, LOG_LINES)
                .with_context(|| format!("cannot read the daemon log {}", path.display()))?;
            out(&lines.iter().map(|l| format!("{l}\n")).collect::<String>())
        }
        Command::Daemon(DaemonCmd::Run) | Command::Mcp | Command::Model(_) | Command::Bench(_) => {
            unreachable!("handled in dispatch")
        }
    }
}

/// The last `n` lines of the log at `path` (invalid UTF-8 replaced); none if there is no log yet.
fn log_tail(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let start = f.metadata()?.len().saturating_sub(LOG_TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // Read from the middle of a line.
        lines.remove(0);
    }
    Ok(lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect())
}

/// Never starts a daemon. Without one, says whether one is starting (and what it does) or why
/// the last start failed.
async fn status(home: &AliceHome, json: bool) -> Result<()> {
    let Some(c) = DaemonClient::try_existing(home).await else {
        let starting = startup::lock_holder(home)
            .map(|pid| (pid, startup::read_progress(home).map(|p| p.describe())));
        let failed = startup::read_error(home).filter(|_| starting.is_none());
        let value = serde_json::json!({
            "running": false,
            "starting": starting.as_ref().map(|(pid, doing)| serde_json::json!({"pid": pid, "doing": doing})),
            "start_error": failed.as_ref().map(|e| &e.error),
        });
        return print(json, &value, || match (&starting, &failed) {
            (Some((pid, Some(doing))), _) => format!("starting (pid {pid}): {doing}\n"),
            (Some((pid, None)), _) => format!("starting (pid {pid})\n"),
            (None, Some(e)) => format!(
                "not running; the last start failed ({}): {}\n",
                e.at, e.error
            ),
            (None, None) => "not running\n".into(),
        });
    };
    let h = c.health().await?;
    print(json, &h, || {
        format!(
            "running (pid {}, v{})\nmemories {} · chunks {} · projects {} · embedding backlog {}{}{}\n",
            h.pid,
            h.version,
            h.memories,
            h.chunks,
            h.projects,
            h.embed_backlog,
            if h.index_ready {
                ""
            } else {
                " · index rebuilding"
            },
            h.read_only
                .as_deref()
                .map(|r| format!("\nREAD-ONLY: {r}"))
                .unwrap_or_default()
        )
    })
}

/// Gone once its process has exited or its lock is free (the daemon saves everything, then
/// releases the lock, then exits).
async fn wait_gone(home: &AliceHome, pid: Option<u32>) -> bool {
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        if pid.is_some_and(|p| !pid_alive(p)) || lock_free(home) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

fn stopped(json: bool, pid: Option<u32>) -> Result<()> {
    print(
        json,
        &serde_json::json!({"stopped": true, "pid": pid}),
        || "stopped\n".into(),
    )
}

/// Start the daemon if it does not run, and wait until it serves. While a live process holds
/// the lock (a daemon that is starting: downloading the model, say), keep waiting and say what
/// it does; give up only when it announces nothing new for a minute. A failed start is
/// reported with its reason.
async fn start(home: &AliceHome, json: bool) -> Result<()> {
    let launcher = launcher();
    // What was last printed (step, file, percent in steps of 5) and the last announcement seen.
    let mut said = None;
    let mut seen = 0;
    let mut attempts = 0;
    let c = 'started: loop {
        attempts += 1;
        match DaemonClient::try_connect(home, &launcher, START_FIRST_WAIT).await {
            Ok(c) => break c,
            Err(Unavailable::Starting(_)) => {}
            Err(u) => return Err(AliceError::from(u).into()),
        }
        let mut idle_since = Instant::now();
        loop {
            if let Some(c) = DaemonClient::try_existing(home).await {
                break 'started c;
            }
            if let Some(e) = startup::read_error(home) {
                bail!("{}", Unavailable::StartFailed(e.error));
            }
            let Some(pid) = startup::lock_holder(home) else {
                // The holder let go without serving or saying why: start one again.
                break;
            };
            if let Some(p) = startup::read_progress(home) {
                if p.unix_ms != seen {
                    seen = p.unix_ms;
                    idle_since = Instant::now();
                }
                let pct = p.done.zip(p.total).map(|(d, t)| d * 100 / t.max(1) / 5);
                let key = Some((p.step.clone(), p.file.clone(), pct));
                if key != said {
                    eprintln!("alice: the daemon (pid {pid}) is {}", p.describe());
                    said = key;
                }
            }
            if idle_since.elapsed() >= START_IDLE_WAIT {
                bail!(
                    "the daemon (pid {pid}) holds {} but has not answered for {} s; see \"alice daemon logs\", or end it with \"kill {pid}\" and retry",
                    home.lock_file().display(),
                    START_IDLE_WAIT.as_secs()
                );
            }
            tokio::time::sleep(START_POLL).await;
        }
        if attempts >= START_ATTEMPTS {
            bail!(
                "the daemon exited {attempts} times without serving or saying why; see \"alice daemon logs\""
            );
        }
    };
    let h = c.health().await?;
    print(json, &h, || format!("running (pid {})\n", h.pid))
}

/// Ask the daemon to stop and wait until it has saved and let go of its lock, so that a
/// command run right after starts a fresh daemon on the saved data.
async fn stop(home: &AliceHome, json: bool) -> Result<()> {
    let deadline = Instant::now() + STOP_WAIT;
    let mut waited_for = None;
    loop {
        if let Some(c) = DaemonClient::try_existing(home).await {
            let pid = c
                .health()
                .await
                .ok()
                .map(|h| h.pid)
                .or_else(|| lock_pid(home));
            let requested = c.shutdown().await;
            // A daemon already on its way out may drop the request: what counts is that it goes.
            if wait_gone(home, pid).await {
                return stopped(json, pid);
            }
            requested?;
            return print(
                json,
                &serde_json::json!({"stopped": false, "stopping": true, "pid": pid}),
                || match pid {
                    Some(p) => format!("stop requested; still shutting down (pid {p})\n"),
                    None => "stop requested; still shutting down\n".into(),
                },
            );
        }
        // Nothing answers. A daemon that is starting (no daemon.json yet) or stopping (it
        // withdrew daemon.json) still holds the lock: wait for it to serve or to finish.
        let holder = lock_pid(home).filter(|&p| pid_alive(p));
        match holder {
            Some(pid) if !lock_free(home) => {
                if Instant::now() >= deadline {
                    bail!(
                        "the daemon (pid {pid}) holds {} but does not answer; it may still be starting (the first start downloads the embedding model). Retry \"alice daemon stop\" in a moment, or end it with \"kill {pid}\"",
                        home.lock_file().display()
                    );
                }
                waited_for = Some(pid);
                tokio::time::sleep(STOP_POLL).await;
            }
            _ if waited_for.is_some() => return stopped(json, waited_for),
            _ => {
                return print(
                    json,
                    &serde_json::json!({"stopped": false, "running": false}),
                    || "not running\n".into(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_reads_only_the_end_and_never_a_partial_line() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("daemon.log");
        assert!(log_tail(&path, 3).unwrap().is_empty(), "no log yet");

        // Many short lines: only the last 100 come back, read from the end of the file.
        let mut log = String::new();
        let mut i = 0;
        while (log.len() as u64) < 2 * LOG_TAIL_BYTES {
            log.push_str(&format!("line {i} {}\n", "x".repeat(i % 50)));
            i += 1;
        }
        log.push_str("last line \u{e44}\u{e17}\u{e22} ");
        let mut bytes = log.into_bytes();
        bytes.push(0xff);
        std::fs::write(&path, &bytes).unwrap();
        let tail = log_tail(&path, 100).unwrap();
        assert_eq!(tail.len(), 100);
        assert_eq!(tail[99], "last line \u{e44}\u{e17}\u{e22} \u{fffd}");
        assert!(
            tail[98].starts_with(&format!("line {} ", i - 1)),
            "{}",
            tail[98]
        );

        // Long lines: the window holds fewer than 100, and the one it starts inside is dropped.
        let long = |i: usize| format!("line {i} {}", "y".repeat(10_000 + i));
        let all: Vec<String> = (0..60).map(long).collect();
        std::fs::write(&path, all.join("\n") + "\n").unwrap();
        let tail = log_tail(&path, 100).unwrap();
        assert!((20..60).contains(&tail.len()), "{}", tail.len());
        // Whole lines only: a cut-off first line would not equal its original.
        assert_eq!(tail, all[all.len() - tail.len()..]);

        std::fs::write(&path, "a\nb\nc\n").unwrap();
        assert_eq!(log_tail(&path, 100).unwrap(), ["a", "b", "c"]);
        assert_eq!(log_tail(&path, 2).unwrap(), ["b", "c"]);
    }
}
