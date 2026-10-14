//! Subcommands: their arguments and what each one does.

use crate::Command;
use alice_core::api::*;
use alice_core::error::AliceError;
use alice_core::model::{Author, Kind};
use alice_core::store::kg::{AssertFact, FactAction, FactsQuery};
use alice_core::store::memories::{MemoryUpdate, UpdateAction};
use alice_core::store::reviews::{ReviewItem, ReviewType};
use alice_daemon::client::{DaemonClient, Launcher, Unavailable, remember_or_spool};
use alice_daemon::daemon::{DaemonOptions, run};
use alice_daemon::doctor::{self, Check, CheckStatus, DoctorReport};
use alice_daemon::paths::AliceHome;
use alice_daemon::service_install::{self as svcinst, Platform, Runner, SystemRunner};
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
    /// Let the judge re-rank the top results (slower; uses Jev when it is configured).
    #[arg(long)]
    pub precise: bool,
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
    /// Who makes the change, in the history (default: $ALICE_AGENT or "cli").
    #[arg(long, env = "ALICE_AGENT", default_value = "cli")]
    pub agent: String,
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
    /// Who asserts it, in the history (default: $ALICE_AGENT or "cli").
    #[arg(long, env = "ALICE_AGENT", default_value = "cli")]
    pub agent: String,
}

#[derive(Args)]
pub struct HistoryArgs {
    /// A memory or relation id; omit for all recent changes.
    pub target: Option<String>,
    /// Only changes by this actor: user, an agent (codex or agent:codex), judge:<provider>,
    /// sync, import or system.
    #[arg(long)]
    pub actor: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Subcommand)]
pub enum ReviewCmd {
    /// Open items (or all with --all). The default for `alice review`.
    List {
        /// Only this type: supersede, contradiction, merge, predicate, project, sync_conflict
        /// or undo.
        #[arg(long = "type")]
        review_type: Option<String>,
        /// Resolved items too.
        #[arg(long)]
        all: bool,
    },
    /// One item in full.
    Show { id: String },
    /// Apply one of the item's choices (see `alice review show <id>`).
    Resolve { id: String, choice: String },
}

#[derive(Subcommand)]
pub enum BackupCmd {
    /// Copy the database into the backup directory now.
    Now,
    /// The backups there, newest first.
    List,
}

#[derive(Subcommand)]
pub enum MaintenanceCmd {
    /// When each job last ran.
    Status,
    /// Run one job now, to its end: backup, quick_check, index_check, fts_optimize,
    /// raw_retention, trash_purge, judge_retry, merge_suggestions or events_prune.
    Run { job: String },
}

#[derive(Subcommand)]
pub enum TokenCmd {
    /// Replace the API token; a running daemon switches at once and refuses the old one.
    Rotate {
        /// Print the new token (by default it is only written to the `token` file).
        #[arg(long)]
        show: bool,
    },
}

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Run the daemon at login: a launchd agent (macOS), a systemd user unit (Linux) or a
    /// scheduled task (Windows).
    Install {
        /// Print the files and commands; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Write the files under this directory instead of your home and run no command (for tests).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// Stop running the daemon at login and remove the unit.
    Uninstall {
        /// Print the commands; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Remove the files under this directory instead of your home and run no command.
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// Whether the unit is installed.
    Status,
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
pub enum ConfigCmd {
    /// Every setting with its value and where it comes from, then problems and secrets.
    List,
    /// One setting's value (or a whole group, such as `judge`).
    Get { key: String },
    /// Change a setting (written to config.toml, applied at once unless it says otherwise).
    /// Values are TOML (`0.7`, `true`, `["a", "b"]`) or plain text (`heuristic`, `90d`).
    Set { key: String, value: String },
    /// Remove a setting from config.toml (back to its default).
    Unset { key: String },
    /// Open config.toml in $EDITOR; problems are shown afterwards.
    Edit,
    /// Store a secret read from stdin (never from the command line).
    SetSecret { name: String },
    /// Remove a stored secret.
    DeleteSecret { name: String },
    /// Check that a provider answers (judge or embedding).
    Test { provider: String },
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
    /// With --real: write the chunk and memory vectors with a fast fake embedder that reports
    /// the real model's id and dimension, so a 1M-chunk index exists in minutes; the daemon run
    /// then loads the real model, embeds new memories with it and serves the bursts. Measures
    /// the daemon's memory at scale; in-process latency figures then use fake query vectors.
    #[arg(long, requires = "real")]
    pub prefill_vectors: bool,
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

#[derive(Subcommand)]
pub enum ImportCmd {
    /// Import a MemPalace palace. Reads the palace through a read-only snapshot; MemPalace does
    /// not need to be installed or stopped. Run it again any time: it imports only what is new.
    /// Add --dry-run to see what would happen.
    Mempalace(ImportMempalaceArgs),
    /// Progress of the running import, or the outcome of the last one.
    Status,
    /// Copy a palace through the audited snapshot path (no daemon).
    #[command(hide = true)]
    Snapshot(ImportSnapshotArgs),
}

#[derive(Args)]
pub struct ImportMempalaceArgs {
    /// The palace folder (default: $MEMPALACE_PALACE_PATH, $MEMPAL_PALACE_PATH, or the
    /// palace_path in ~/.mempalace/config.json).
    #[arg(long)]
    pub palace: Option<std::path::PathBuf>,
    /// Report what would be imported and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Only drawers filed at or after this date (YYYY-MM-DD or RFC 3339).
    #[arg(long)]
    pub since: Option<String>,
    /// Map a MemPalace wing to an Alice project, as wing=project (repeatable).
    #[arg(long, value_name = "WING=PROJECT", value_parser = parse_map_wing)]
    pub map_wing: Vec<(String, String)>,
    /// The collection that holds the drawers (default mempalace_drawers).
    #[arg(long)]
    pub collection: Option<String>,
    /// Let the configured remote embedding provider receive every chunk of your MemPalace
    /// history (without it, an import refuses when the provider is not the local model).
    #[arg(long)]
    pub allow_remote_embedding: bool,
}

#[derive(Args)]
pub struct ImportSnapshotArgs {
    #[arg(long)]
    pub palace: std::path::PathBuf,
    #[arg(long)]
    pub out: std::path::PathBuf,
}

/// `wing=project`, split at the first `=`.
fn parse_map_wing(s: &str) -> std::result::Result<(String, String), String> {
    match s.split_once('=') {
        Some((w, p)) if !w.trim().is_empty() && !p.trim().is_empty() => {
            Ok((w.trim().to_string(), p.trim().to_string()))
        }
        _ => Err(format!(
            "\"{s}\" is not wing=project (for example wing_project_alice=alice)"
        )),
    }
}

/// The palace folder, absolute: `--palace`, else `MEMPALACE_PALACE_PATH`, else
/// `MEMPAL_PALACE_PATH`, else `palace_path` in `<home>/.mempalace/config.json` (read only).
/// The daemon never does this: it only reads the folder it is given.
fn resolve_palace(
    arg: Option<std::path::PathBuf>,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
    cwd: &Path,
) -> Result<std::path::PathBuf> {
    let not_found = || anyhow::anyhow!("no palace found; pass --palace <folder>");
    let chosen = if let Some(a) = arg {
        a
    } else if let Some(v) = ["MEMPALACE_PALACE_PATH", "MEMPAL_PALACE_PATH"]
        .iter()
        .filter_map(|k| env(k))
        .find(|v| !v.trim().is_empty())
    {
        std::path::PathBuf::from(v)
    } else {
        let home = home.ok_or_else(not_found)?;
        let cfg = home.join(".mempalace").join("config.json");
        let raw = match std::fs::read_to_string(&cfg) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found()),
            Err(e) => bail!("cannot read {}: {e}; pass --palace <folder>", cfg.display()),
        };
        let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid JSON ({e}); pass --palace <folder>",
                cfg.display()
            )
        })?;
        let path = v
            .get("palace_path")
            .and_then(|p| p.as_str())
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(not_found)?;
        match path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if path == "~" => home.to_path_buf(),
            None => std::path::PathBuf::from(path),
        }
    };
    Ok(if chosen.is_absolute() {
        chosen
    } else {
        cwd.join(chosen)
    })
}

fn user_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
}

const IMPORT_POLL: Duration = Duration::from_millis(500);

async fn import_mempalace(home: &AliceHome, a: ImportMempalaceArgs, json: bool) -> Result<()> {
    use alice_core::import::{ImportRequest, ImportState};
    use std::io::IsTerminal;
    // Everything the CLI can refuse by itself happens before a daemon is started.
    let cwd = std::env::current_dir().context("cannot read the working folder")?;
    let palace = resolve_palace(
        a.palace,
        &|k| std::env::var(k).ok(),
        user_home().as_deref(),
        &cwd,
    )?;
    let mut map_wing = std::collections::BTreeMap::new();
    for (wing, project) in a.map_wing {
        if let Some(old) = map_wing.insert(wing.clone(), project.clone()) {
            bail!(
                "--map-wing names the wing {wing} twice ({old} and {project}); give each wing one project"
            );
        }
    }
    // Registered before the daemon starts or the job begins, so a Ctrl-C at any point ends
    // only this command.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("cannot listen for Ctrl-C")?;
    let req = ImportRequest {
        palace: Some(palace.to_string_lossy().into_owned()),
        dry_run: a.dry_run,
        since: a.since,
        map_wing,
        collection: a.collection,
        allow_remote_embedding: a.allow_remote_embedding,
    };
    let c = client(home).await?;
    let first = c.start_import(&req).await?;
    let tty = std::io::stderr().is_terminal();
    let mut last_phase = String::new();
    let mut drew = false;
    let mut status = first;
    // Ctrl-C ends the wait, never the job: the daemon keeps importing and the next run resumes.
    const CONTINUES: &str = "the import continues in the background (`alice import status` shows it, and the next `alice import mempalace` resumes)";
    let progress = |s: &alice_core::import::ImportStatus| {
        if s.total == 0 {
            format!("{}\u{2026}", s.phase)
        } else {
            format!("{}: {} of {}", s.phase, s.done, s.total)
        }
    };
    loop {
        if status.state != ImportState::Running {
            break;
        }
        if tty {
            eprint!("\r\x1b[2K{}", progress(&status));
            drew = true;
        } else if status.phase != last_phase {
            eprintln!(
                "{}: {} of {} drawers",
                status.phase, status.done, status.total
            );
        }
        last_phase.clone_from(&status.phase);
        tokio::select! {
            _ = sigint.recv() => {
                if drew { eprintln!(); }
                bail!("stopped waiting; {CONTINUES}");
            }
            () = tokio::time::sleep(IMPORT_POLL) => {}
        }
        status = match c.import_status().await {
            Ok(s) => s,
            Err(e) => {
                if drew {
                    eprintln!();
                }
                bail!("lost contact with the daemon ({e}); {CONTINUES}")
            }
        };
    }
    if drew {
        eprint!("\r\x1b[2K");
    }
    match &status.report {
        Some(r) => print(json, r, || r.render_text())?,
        None => print(json, &status, String::new)?,
    }
    import_verdict(&status)
}

/// Exit 0 only for a finished import without errors; otherwise the one sentence for stderr.
fn import_verdict(status: &alice_core::import::ImportStatus) -> Result<()> {
    use alice_core::import::ImportState;
    match status.state {
        ImportState::Done => match status.report.as_ref().map(|r| &r.errors) {
            Some(errors) if !errors.is_empty() => bail!(
                "the import finished with {} error(s); the first: {}",
                errors.len(),
                errors[0]
            ),
            _ => Ok(()),
        },
        ImportState::Failed => bail!(
            "the import failed: {}",
            status.error.as_deref().unwrap_or("no reason was given")
        ),
        ImportState::Idle => {
            bail!("no import is running and none has run since the daemon started")
        }
        ImportState::Running => bail!("the import is still running"),
    }
}

async fn import_status(home: &AliceHome, json: bool) -> Result<()> {
    use alice_core::import::ImportState;
    let s = client(home).await?.import_status().await?;
    match (&s.report, s.state) {
        (Some(r), ImportState::Done) if !json => out(&r.render_text()),
        _ => print(json, &s, || match s.state {
            ImportState::Idle => "No import has run since the daemon started.\n".into(),
            ImportState::Running => format!(
                "Import running: {}, {} of {} drawers.\n",
                s.phase, s.done, s.total
            ),
            ImportState::Failed => format!(
                "The last import failed: {}\n",
                s.error.as_deref().unwrap_or("no reason was given")
            ),
            ImportState::Done => "The last import finished.\n".into(),
        }),
    }
}

/// The audited copy of a palace (no daemon): prints the notes and the files written.
fn import_snapshot(a: ImportSnapshotArgs, json: bool) -> Result<()> {
    use alice_core::import::palace::{Backend, snapshot};
    let cwd = std::env::current_dir().context("cannot read the working folder")?;
    let abs = |p: std::path::PathBuf| if p.is_absolute() { p } else { cwd.join(p) };
    let (palace, dest) = (abs(a.palace), abs(a.out));
    let mp_home = user_home()
        .map(|h| h.join(".mempalace"))
        .filter(|h| h.is_dir());
    let snap = snapshot(&palace, &dest, mp_home.as_deref())?;
    let main = snap.dir.join(match snap.backend {
        Backend::SqliteExact => "sqlite_exact.sqlite3",
        Backend::Chroma => "chroma.sqlite3",
    });
    let mut files = vec![main];
    files.extend(snap.kg.clone());
    files.extend(snap.tunnels.clone());
    let value = serde_json::json!({
        "dir": snap.dir,
        "notes": snap.notes,
        "files": files,
    });
    print(json, &value, || {
        let mut t = String::new();
        for n in &snap.notes {
            t.push_str(&format!("{n}\n"));
        }
        for f in &files {
            t.push_str(&format!("wrote {}\n", f.display()));
        }
        t
    })
}

fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// How a command starts the daemon when none runs: this program, unless `ALICE_NO_AUTOSTART`
/// is set (to anything but `0` or empty), in which case writes are queued in the spool.
fn launcher() -> Launcher {
    let no_autostart =
        std::env::var_os("ALICE_NO_AUTOSTART").is_some_and(|v| !v.is_empty() && v != "0");
    if no_autostart {
        return Launcher::None;
    }
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

/// The author of a CLI write: `--agent` / `$ALICE_AGENT`, where set but empty means the
/// default `cli` (which history records as the user).
fn author(agent: &str) -> Author {
    Author {
        agent: Some(match agent.trim() {
            "" => "cli".to_string(),
            a => a.to_string(),
        }),
        ..Default::default()
    }
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

/// The text of a recall answer; a precise one that the judge re-ordered says so.
fn recall_text(r: &RecallResponse) -> String {
    let mut text = r.text.clone().unwrap_or_default();
    if r.reranked {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("(re-ranked by the judge)\n");
    }
    text
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
                settings_env: None,
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
        // No daemon: the snapshot is a plain read-only copy.
        Command::Import(ImportCmd::Snapshot(a)) => import_snapshot(a, json),
        // Not through `client_command`: that would start a daemon, and `restore` must have none.
        Command::Restore { backup, yes } => rt()?.block_on(restore(&home, backup, yes, json)),
        // Neither starts a daemon: `doctor` reports on what is there, `token rotate` and
        // `service` work with none.
        Command::Doctor => rt()?.block_on(doctor_command(&home, json)),
        Command::Token(TokenCmd::Rotate { show }) => {
            rt()?.block_on(token_rotate(&home, show, json))
        }
        Command::Service(c) => service_command(&home, c, json),
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
                mode: if a.precise {
                    RecallMode::Precise
                } else {
                    RecallMode::Fast
                },
                limit: a.limit,
                max_tokens: a.max_tokens,
                format: if json {
                    OutputFormat::Json
                } else {
                    OutputFormat::Text
                },
                cwd: cwd(),
            };
            let r = client(&home).await?.recall(&req).await?;
            print(json, &r, || recall_text(&r))
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
                author: author(&a.agent),
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
                for d in &r.redirects {
                    s.push_str(&format!(
                        "{} was merged into {} (shown above)\n",
                        d.from, d.to
                    ));
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
                .update(&UpdateRequest {
                    id: a.id,
                    update,
                    author: author(&a.agent),
                })
                .await?;
            print(json, &r, || {
                let undo = r
                    .event_id
                    .as_deref()
                    .map(|e| format!(" (event {e}; undo with `alice undo {e}`)"))
                    .unwrap_or_default();
                format!(
                    "{} is now {}{undo}\n",
                    r.memory.id,
                    r.memory.status.as_str()
                )
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
                    author: author(&a.agent),
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
        Command::History(a) => {
            let r = client(&home)
                .await?
                .events(&EventsRequest {
                    target_id: a.target,
                    actor: a.actor,
                    before: None,
                    limit: Some(a.limit),
                })
                .await?;
            print(json, &r, || {
                r.iter()
                    .map(|e| {
                        format!(
                            "{}  {}  {:<16} {:<18} {} {}\n",
                            e.id,
                            &e.ts[..19.min(e.ts.len())],
                            e.actor,
                            e.action,
                            e.target_type,
                            e.target_id
                        )
                    })
                    .collect()
            })
        }
        Command::Undo { event_id, agent } => {
            let r = client(&home)
                .await?
                .undo(&UndoRequest {
                    event_id,
                    author: author(&agent),
                })
                .await?;
            print(json, &r, || format!("{}\n", r.message))
        }
        Command::Review { cmd } => review(&home, cmd, json).await,
        Command::Stats => {
            let r = client(&home).await?.stats().await?;
            print(json, &r, || stats_text(&r))
        }
        Command::Backup(c) => backup(&home, c, json).await,
        Command::Maintenance(c) => maintenance(&home, c, json).await,
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
        Command::Config(c) => config(&home, c, json).await,
        Command::Import(ImportCmd::Mempalace(a)) => import_mempalace(&home, a, json).await,
        Command::Import(ImportCmd::Status) => import_status(&home, json).await,
        Command::Daemon(DaemonCmd::Run)
        | Command::Mcp
        | Command::Model(_)
        | Command::Bench(_)
        | Command::Import(ImportCmd::Snapshot(_))
        | Command::Doctor
        | Command::Token(_)
        | Command::Service(_)
        | Command::Restore { .. } => {
            unreachable!("handled in dispatch")
        }
    }
}

fn review_type_arg(t: &Option<String>) -> Result<Option<ReviewType>> {
    match t.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(None),
        Some(t) => ReviewType::parse(t).map(Some).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown review type \"{t}\"; use one of: {}",
                ReviewType::ALL
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
    }
}

/// One line about what the item asks, for kinds whose subjects are not memories.
fn review_detail(i: &ReviewItem) -> Option<String> {
    let s = |k: &str| i.payload.get(k).and_then(|v| v.as_str());
    match i.review_type {
        ReviewType::Predicate => Some(format!(
            "predicate \"{}\" may mean \"{}\"",
            s("predicate")?,
            s("canonical")?
        )),
        ReviewType::Project => Some(format!("move to project {}", s("to")?)),
        ReviewType::Undo => Some(format!("undo blocked: {}", s("reason")?)),
        _ => None,
    }
}

fn review_block(i: &ReviewItem) -> String {
    let p = i
        .confidence
        .map(|p| format!("{p:.2}"))
        .unwrap_or_else(|| "-".into());
    let mut s = format!("{}  {}  p={p}\n", i.id, i.review_type.as_str());
    if let Some(d) = review_detail(i) {
        s.push_str(&format!("  {d}\n"));
    }
    for m in &i.subjects {
        s.push_str(&format!("  - {} [{}] ({})\n", m.title, m.id, m.status));
    }
    match (&i.resolution, &i.resolved_by) {
        (Some(r), by) => s.push_str(&format!(
            "  resolved: {r} by {}\n",
            by.as_deref().unwrap_or("?")
        )),
        _ => s.push_str(&format!(
            "  choices: {}\n",
            i.choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        )),
    }
    s
}

fn review_full(i: &ReviewItem) -> String {
    let mut s = review_block(i);
    s.push_str(&format!(
        "  status: {}  created: {}\n",
        match i.status {
            alice_core::store::reviews::ReviewStatus::Open => "open",
            alice_core::store::reviews::ReviewStatus::Resolved => "resolved",
        },
        i.created_at
    ));
    for c in &i.choices {
        s.push_str(&format!("  {} - {}\n", c.id, c.description));
    }
    s.push_str(&format!(
        "  payload: {}\n",
        serde_json::to_string(&i.payload).unwrap_or_default()
    ));
    s
}

async fn review(home: &AliceHome, c: Option<ReviewCmd>, json: bool) -> Result<()> {
    match c.unwrap_or(ReviewCmd::List {
        review_type: None,
        all: false,
    }) {
        ReviewCmd::List { review_type, all } => {
            let review_type = review_type_arg(&review_type)?;
            let client = client(home).await?;
            let mut items = client
                .reviews(&ReviewQuery {
                    review_type,
                    ..Default::default()
                })
                .await?;
            if all {
                items.extend(
                    client
                        .reviews(&ReviewQuery {
                            status: Some(alice_core::store::reviews::ReviewStatus::Resolved),
                            review_type,
                            ..Default::default()
                        })
                        .await?,
                );
            }
            print(json, &items, || {
                if items.is_empty() {
                    "Nothing waits for a decision.\n".to_string()
                } else {
                    items
                        .iter()
                        .map(review_block)
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        }
        ReviewCmd::Show { id } => {
            let r = client(home).await?.review(&id).await?;
            print(json, &r, || review_full(&r))
        }
        ReviewCmd::Resolve { id, choice } => {
            let r = client(home)
                .await?
                .resolve_review(&ResolveRequest {
                    id,
                    choice,
                    author: author("cli"),
                })
                .await?;
            print(json, &r, || format!("{}\n", r.message))
        }
    }
}

fn backup_line(b: &alice_daemon::backup::BackupInfo) -> String {
    format!(
        "{}  {}  {} bytes  {}\n",
        b.created_at.format("%Y-%m-%d %H:%M:%S"),
        b.kind.as_str(),
        b.bytes,
        b.path.display()
    )
}

async fn backup(home: &AliceHome, c: BackupCmd, json: bool) -> Result<()> {
    match c {
        BackupCmd::Now => {
            let b = client(home).await?.backup_now().await?;
            print(json, &b, || format!("Backed up to {}\n", b.path.display()))
        }
        BackupCmd::List => {
            let list = client(home).await?.backups().await?;
            print(json, &list, || {
                if list.is_empty() {
                    "No backups yet; \"alice backup now\" makes one.\n".to_string()
                } else {
                    list.iter().map(backup_line).collect()
                }
            })
        }
    }
}

async fn maintenance(home: &AliceHome, c: MaintenanceCmd, json: bool) -> Result<()> {
    use alice_daemon::maintenance::Job;
    match c {
        MaintenanceCmd::Status => {
            let jobs = client(home).await?.maintenance_status().await?;
            print(json, &jobs, || {
                jobs.iter()
                    .map(|j| {
                        let mut line = format!(
                            "{:<18} every {:<8} last run {}",
                            j.job,
                            every_text(j.every_secs),
                            j.last_run.as_deref().unwrap_or("never")
                        );
                        if let Some(e) = &j.last_error {
                            line.push_str(&format!("  last error: {e}"));
                        }
                        line.push('\n');
                        line
                    })
                    .collect()
            })
        }
        MaintenanceCmd::Run { job } => {
            // Checked here too, so a typo is answered before a daemon is started for it.
            let job = Job::parse_or_err(&job)?;
            let r = client(home).await?.run_job(job).await?;
            print(json, &r, || {
                let outcome = match r.outcome {
                    alice_daemon::maintenance::JobOutcome::Done => "done",
                    alice_daemon::maintenance::JobOutcome::Paused => "paused",
                    alice_daemon::maintenance::JobOutcome::Skipped => "skipped",
                    alice_daemon::maintenance::JobOutcome::Failed => "failed",
                };
                format!("{outcome}: {} ({} ms)\n", r.detail, r.took_ms)
            })?;
            if r.outcome == alice_daemon::maintenance::JobOutcome::Failed {
                bail!("{} failed: {}", r.job.name(), r.detail);
            }
            Ok(())
        }
    }
}

fn every_text(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s => format!("{s}s"),
    }
}

fn stats_text(s: &Stats) -> String {
    let map = |m: &std::collections::BTreeMap<String, u64>| {
        if m.is_empty() {
            "none".to_string()
        } else {
            m.iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut t = format!(
        "memories by status: {}\nmemories by kind: {}\nraw chunks: {}\n",
        map(&s.memories_by_status),
        map(&s.memories_by_kind),
        s.chunks
    );
    if !s.top_projects.is_empty() {
        t.push_str(&format!(
            "top projects: {}\n",
            s.top_projects
                .iter()
                .take(5)
                .map(|(p, n)| format!("{p} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    t.push_str(&format!(
        "reviews waiting: {}\njudge: {} (waiting {}, to retry {})\nembedding backlog: {}\njudge cost: ${:.4} today, ${:.4} in 7 days\n",
        s.reviews_open,
        s.judge_provider,
        s.judge_backlog,
        s.judge_retry,
        s.embed_backlog,
        s.cost_today_usd,
        s.cost_7d_usd
    ));
    if let Some(w) = &s.judge_warning {
        t.push_str(&format!("judge warning: {w}\n"));
    }
    t
}

/// How long `config edit` waits, at most, for the daemon to show what an edit did (the daemon
/// looks at the file every 2 s), and how often it asks.
const EDIT_WAIT: Duration = Duration::from_secs(5);
const EDIT_POLL: Duration = Duration::from_millis(200);

/// The text of `value` for a person: text without quotes, anything else as JSON.
fn plain(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What is wrong or worth knowing about the settings, one line each.
fn settings_notes(v: &SettingsView) -> String {
    let mut out = String::new();
    if v.config_degraded {
        out.push_str("WARNING no valid config.toml has loaded since the daemon started; cloud providers stay off until one does\n");
    }
    for e in &v.errors {
        out.push_str(&format!(
            "PROBLEM {}: {} (the previous settings stay in effect)\n",
            e.key, e.message
        ));
    }
    for w in &v.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    for n in &v.notes {
        out.push_str(&format!("note: {n}\n"));
    }
    out
}

async fn config(home: &AliceHome, c: ConfigCmd, json: bool) -> Result<()> {
    use alice_core::settings::{Source, leaves, parse_cli_value};
    let client = client(home).await?;
    match c {
        ConfigCmd::List => {
            let v = client.settings().await?;
            print(json, &v, || {
                let mut out = String::new();
                for (key, value) in leaves(&v.settings) {
                    let src = match v.sources.get(&key) {
                        Some(Source::File) => " (config.toml)".to_string(),
                        Some(Source::Env { var }) => format!(" (${var})"),
                        _ => String::new(),
                    };
                    let unused = if v.keys.iter().any(|i| i.key == key && i.not_used_yet) {
                        " (not used yet)"
                    } else {
                        ""
                    };
                    out.push_str(&format!("{key} = {value}{src}{unused}\n"));
                }
                out.push_str(&settings_notes(&v));
                for k in v
                    .restart_pending
                    .iter()
                    .filter(|k| !v.keys.iter().any(|i| &i.key == *k && i.not_used_yet))
                {
                    out.push_str(&format!(
                        "RESTART {k} changes after `alice daemon stop` and the next start\n"
                    ));
                }
                for (name, s) in &v.secrets {
                    out.push_str(&format!(
                        "secret {name}: {}\n",
                        if s.set {
                            format!(
                                "set {} ({})",
                                s.hint.clone().unwrap_or_default(),
                                s.source.clone().unwrap_or_default()
                            )
                        } else {
                            "not set".into()
                        }
                    ));
                }
                out
            })
        }
        ConfigCmd::Get { key } => {
            let v = client.settings().await?;
            let value = key
                .split('.')
                .try_fold(&v.settings, |v, p| v.get(p))
                .cloned()
                .with_context(|| format!("no setting {key}; see `alice config list`"))?;
            print(json, &value, || match &value {
                serde_json::Value::Object(_) => {
                    format!(
                        "{}\n",
                        serde_json::to_string_pretty(&value).unwrap_or_default()
                    )
                }
                other => format!("{}\n", plain(other)),
            })
        }
        ConfigCmd::Set { key, value } => patch(&client, key, parse_cli_value(&value), json).await,
        ConfigCmd::Unset { key } => patch(&client, key, serde_json::Value::Null, json).await,
        ConfigCmd::Edit => {
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .context(
                    "set $EDITOR (or $VISUAL) to edit config.toml, or use `alice config set`",
                )?;
            home.ensure_dirs()?;
            let before = client.settings().await?;
            let text_before = std::fs::read(home.config_file()).ok();
            // "code -w" is a program and its arguments.
            let mut words = editor.split_whitespace();
            let program = words.next().context("$EDITOR is empty")?;
            let status = std::process::Command::new(program)
                .args(words)
                .arg(home.config_file())
                .status()
                .with_context(|| format!("cannot run the editor {program}"))?;
            anyhow::ensure!(status.success(), "the editor exited with {status}");
            // Wait for the daemon to show what the edit did (it looks at the file every 2 s),
            // not for a fixed time; an edit that changes nothing it shows (a comment) is given
            // EDIT_WAIT and then reported as it is.
            let edited = std::fs::read(home.config_file()).ok() != text_before;
            let deadline = Instant::now() + EDIT_WAIT;
            let v = loop {
                let v = client.settings().await?;
                let moved = v.settings != before.settings
                    || v.errors != before.errors
                    || v.notes != before.notes;
                if !edited || moved || Instant::now() >= deadline {
                    break v;
                }
                tokio::time::sleep(EDIT_POLL).await;
            };
            eprint!("{}", settings_notes(&v));
            Ok(())
        }
        ConfigCmd::SetSecret { name } => {
            // Before anything is read: a wrong name must not cost a pasted secret. The name is
            // not echoed in the error (it may be the secret itself, typed in the wrong place).
            anyhow::ensure!(
                alice_daemon::secrets::SecretName::parse(&name).is_some(),
                "unknown secret name; use one of: {}",
                alice_daemon::secrets::SecretName::ALL
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let value = read_secret(&format!("Paste {name} and press Enter: "))?;
            let s = client.set_secret(&name, &value).await?;
            print(json, &s, || {
                format!("{name} set ({})\n", s.hint.clone().unwrap_or_default())
            })
        }
        ConfigCmd::DeleteSecret { name } => {
            let had = client.delete_secret(&name).await?;
            print(json, &serde_json::json!({ "deleted": had }), || {
                if had {
                    format!("{name} deleted\n")
                } else {
                    format!("{name} was not set\n")
                }
            })
        }
        ConfigCmd::Test { provider } => {
            let r = client.test_provider(&provider).await?;
            print(json, &r, || {
                format!(
                    "{} {} ({} ms)\n",
                    if r.ok { "OK" } else { "FAILED" },
                    r.message,
                    r.latency_ms
                )
            })?;
            anyhow::ensure!(r.ok, "the {provider} provider test failed");
            Ok(())
        }
    }
}

async fn patch(
    client: &DaemonClient,
    key: String,
    value: serde_json::Value,
    json: bool,
) -> Result<()> {
    let mut m = serde_json::Map::new();
    m.insert(key.clone(), value);
    let r = client.patch_settings(&m).await?;
    print(json, &r, || {
        let mut out = if r.changed.is_empty() {
            format!("{key}: no change\n")
        } else {
            format!("{key}: applied\n")
        };
        let needs_restart = r
            .restart_required
            .iter()
            .any(|k| !r.view.keys.iter().any(|i| &i.key == k && i.not_used_yet));
        if needs_restart {
            out.push_str("takes effect after `alice daemon stop` and the next start\n");
        }
        if !r.overridden_by_env.is_empty() {
            out.push_str(&format!(
                "note: ${} overrides it\n",
                alice_core::settings::env_var(&key)
            ));
        }
        for n in r
            .notes
            .iter()
            .chain(r.view.notes.iter().filter(|n| !r.notes.contains(n)))
        {
            out.push_str(&format!("note: {n}\n"));
        }
        out
    })
}

/// One line from stdin; on a Unix terminal the input is not echoed, and the terminal is put back
/// however the prompt ends: after the line, on an error, and when the process is killed
/// (SIGINT, SIGTERM, SIGHUP) in the middle of it.
fn read_secret(prompt: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal};
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal();
    if tty {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
    }
    #[cfg(unix)]
    let echo_off = if tty { echo::EchoOff::new() } else { None };
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    #[cfg(unix)]
    if echo_off.is_some() {
        drop(echo_off);
        eprintln!();
    }
    read.context("cannot read the secret from stdin")?;
    let v = line.trim().to_string();
    anyhow::ensure!(!v.is_empty(), "no secret was given; paste it on stdin");
    Ok(v)
}

/// Terminal echo off for the length of a prompt, restored by `Drop` and by a signal handler.
#[cfg(unix)]
mod echo {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The attributes to put back, readable by the signal handler: written once before
    /// `SAVED` is set, never while it is set.
    static mut OLD: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();
    static SAVED: AtomicBool = AtomicBool::new(false);
    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// Put the saved attributes back (async-signal-safe: `tcsetattr` and an atomic read).
    fn put_back() {
        if SAVED.load(Ordering::Acquire) {
            // SAFETY: `OLD` was written before `SAVED` was set and is not written while it is set;
            // `tcsetattr` is async-signal-safe.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, (&raw const OLD).cast()) };
        }
    }

    extern "C" fn on_signal(sig: libc::c_int) {
        put_back();
        // SAFETY: back to the default action (`SA_RESETHAND` did that already; this is for a
        // handler entered twice), then the signal again: the process ends as it would have.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    pub struct EchoOff {
        previous: Vec<(libc::c_int, libc::sigaction)>,
    }

    impl EchoOff {
        /// Turn echo off on stdin; `None` when it is not a terminal that can be changed.
        pub fn new() -> Option<EchoOff> {
            // SAFETY: tcgetattr on our own stdin fills a termios; `OLD` is written only while
            // `SAVED` is false, and only by the one prompt that runs at a time.
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) != 0 {
                    return None;
                }
                (&raw mut OLD).write(std::mem::MaybeUninit::new(t));
                SAVED.store(true, Ordering::Release);
                // The handlers go in before echo goes off: a signal in between finds echo on.
                let mut previous = Vec::new();
                for sig in SIGNALS {
                    let mut act: libc::sigaction = std::mem::zeroed();
                    act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                    act.sa_flags = libc::SA_RESETHAND;
                    libc::sigemptyset(&mut act.sa_mask);
                    let mut old: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, &act, &mut old) == 0 {
                        previous.push((sig, old));
                    }
                }
                t.c_lflag &= !libc::ECHO;
                libc::tcsetattr(0, libc::TCSANOW, &t);
                Some(EchoOff { previous })
            }
        }
    }

    impl Drop for EchoOff {
        fn drop(&mut self) {
            put_back();
            SAVED.store(false, Ordering::Release);
            for (sig, old) in &self.previous {
                // SAFETY: the actions saved when the handlers were installed.
                unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
            }
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

/// Never starts a daemon. Without one, says whether one is starting (and what it does), runs but
/// does not answer, or why the last start failed.
async fn status(home: &AliceHome, json: bool) -> Result<()> {
    let Some(c) = DaemonClient::try_existing(home).await else {
        let holder = startup::lock_holder(home);
        let progress = holder.and_then(|_| startup::read_progress(home));
        // A holder that announces nothing and has been running longer than a start takes is not
        // starting: it runs and does not answer.
        let silent = holder.filter(|&pid| {
            progress.is_none()
                && startup::holder_age(home, pid).is_some_and(|age| age >= START_IDLE_WAIT)
        });
        let starting = holder
            .filter(|_| silent.is_none())
            .map(|pid| (pid, progress.as_ref().map(|p| p.describe())));
        let failed = startup::read_error(home).filter(|_| holder.is_none());
        let value = serde_json::json!({
            "running": false,
            "starting": starting.as_ref().map(|(pid, doing)| serde_json::json!({"pid": pid, "doing": doing})),
            "not_answering": silent,
            "start_error": failed.as_ref().map(|e| &e.error),
        });
        return print(json, &value, || match (&starting, silent, &failed) {
            (_, Some(pid), _) => format!(
                "running (pid {pid}, not answering); see \"alice daemon logs\", or end it with \"kill {pid}\" and start again\n"
            ),
            (Some((pid, Some(doing))), _, _) => format!("starting (pid {pid}): {doing}\n"),
            (Some((pid, None)), _, _) => format!("starting (pid {pid})\n"),
            (None, _, Some(e)) => format!(
                "not running; the last start failed ({}): {}\n",
                e.at, e.error
            ),
            (None, _, None) => "not running\n".into(),
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
        ) + &health_notes(&h)
    })
}

/// Settings problems in a health reading, one line each, after the status.
fn health_notes(h: &Health) -> String {
    let mut out = String::new();
    if h.config_degraded {
        out.push_str(
            "WARNING: no valid config.toml has loaded; cloud providers stay off until one does\n",
        );
    }
    for e in &h.settings_errors {
        out.push_str(&format!(
            "PROBLEM {}: {} (see `alice config list`)\n",
            e.key, e.message
        ));
    }
    if !h.embedding_model.is_empty() {
        out.push_str(&format!("embedding model: {}", h.embedding_model));
        if let Some(r) = &h.reembed {
            out.push_str(&format!(
                " (re-embedding into {}: {} of {})",
                r.model, r.done, r.total
            ));
        }
        if h.embed_skipped_cloud > 0 {
            out.push_str(&format!(
                " · {} rows held back by the cloud policy (searched by words only)",
                h.embed_skipped_cloud
            ));
        }
        out.push('\n');
    }
    if let Some(e) = &h.embedding_error {
        out.push_str(&format!("WARNING: {e}\n"));
    }
    if !h.judge_provider.is_empty() {
        out.push_str(&format!(
            "judge: {} (backlog {})\n",
            h.judge_provider, h.judge_backlog
        ));
    }
    if let Some(w) = &h.judge_warning {
        out.push_str(&format!("WARNING: {w}\n"));
    }
    if h.reviews_open > 0 {
        out.push_str(&format!(
            "reviews open: {} (`alice review`)\n",
            h.reviews_open
        ));
    }
    if h.spool_pending > 0 {
        out.push_str(&format!("spooled writes waiting: {}\n", h.spool_pending));
    }
    if let Some(t) = &h.last_backup {
        out.push_str(&format!("last backup: {t}\n"));
    }
    if let Some(b) = h.memory_bytes {
        out.push_str(&format!("memory in use: {} MB\n", b / 1_000_000));
    }
    if let Some(w) = &h.maintenance_warning {
        out.push_str(&format!("WARNING: {w}\n"));
    }
    for w in &h.settings_warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
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
    // A `daemon.error` older than this call belongs to an earlier daemon.
    let since = startup::unix_ms();
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
            let holder = startup::lock_holder(home);
            // A live process holds the lock: it is starting, slowly, and an older failure is not
            // its. Without a holder, the last failure is the answer.
            if let Some(e) = startup::read_error(home)
                && (e.unix_ms >= since || holder.is_none())
            {
                bail!("{}", Unavailable::StartFailed(e.error));
            }
            let Some(pid) = holder else {
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

/// What stopping found; `stop` prints it, `restore` only acts on it.
pub(crate) enum StopOutcome {
    /// The daemon that ran (pid if known) has saved and let go of its lock.
    Stopped(Option<u32>),
    /// Asked to stop, still shutting down when the wait ended.
    Stopping(Option<u32>),
    NotRunning,
}

/// Ask the daemon to stop and wait until it has saved and let go of its lock, so that a
/// command run right after starts a fresh daemon on the saved data. Prints nothing.
pub(crate) async fn stop_daemon(home: &AliceHome) -> Result<StopOutcome> {
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
                return Ok(StopOutcome::Stopped(pid));
            }
            requested?;
            return Ok(StopOutcome::Stopping(pid));
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
            _ if waited_for.is_some() => return Ok(StopOutcome::Stopped(waited_for)),
            _ => return Ok(StopOutcome::NotRunning),
        }
    }
}

async fn stop(home: &AliceHome, json: bool) -> Result<()> {
    match stop_daemon(home).await? {
        StopOutcome::Stopped(pid) => stopped(json, pid),
        StopOutcome::Stopping(pid) => print(
            json,
            &serde_json::json!({"stopped": false, "stopping": true, "pid": pid}),
            || match pid {
                Some(p) => format!("stop requested; still shutting down (pid {p})\n"),
                None => "stop requested; still shutting down\n".into(),
            },
        ),
        StopOutcome::NotRunning => print(
            json,
            &serde_json::json!({"stopped": false, "running": false}),
            || "not running\n".into(),
        ),
    }
}

/// Stop the daemon and swap the database, again if a write started a daemon in the gap.
async fn swap_stopping_the_daemon(
    home: &AliceHome,
    dir: &Path,
    verified: &alice_daemon::restore::VerifiedBackup,
) -> Result<alice_daemon::restore::RestoreReport> {
    use alice_daemon::restore as r;
    for attempt in 0..RESTORE_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // Quiet: `restore` prints one document, its report, never stop's output too. Waits until
        // the daemon has let go of its lock (writes spool meanwhile).
        if let StopOutcome::Stopping(pid) = stop_daemon(home).await? {
            bail!(
                "the daemon{} is still shutting down (an import or a long job may be ending); nothing was changed, retry `alice restore` in a moment",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            );
        }
        let (h, d, v) = (home.clone(), dir.to_path_buf(), verified.clone());
        match tokio::task::spawn_blocking(move || r::swap_now(&h, &d, &v)).await? {
            Ok(rep) => return Ok(rep),
            // Something else holds the daemon lock (a daemon started in the gap): stop it again.
            Err(e) if e.to_string().contains(r::DAEMON_RUNNING) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!(
        "something keeps holding the daemon lock during the restore; nothing was changed, retry `alice restore`"
    )
}

/// How many times `restore` stops a daemon that an agent's write started in the gap before the swap.
const RESTORE_ATTEMPTS: usize = 5;

/// Replace the database by a backup (spec §5.6). Verifies first and changes nothing when the
/// backup is damaged or from a newer Alice. The daemon is stopped for the swap only; writes
/// agents make meanwhile wait in the spool and are applied when the daemon is back.
async fn restore(home: &AliceHome, backup: Option<String>, yes: bool, json: bool) -> Result<()> {
    use alice_daemon::restore as r;
    use std::io::{BufRead, IsTerminal};
    let settings = alice_daemon::config::load_effective(home).resolved.settings;
    let dir = alice_daemon::backup::backup_dir(home, &settings.backup);
    let path = r::find_backup(home, &dir, backup.as_deref())?;
    let verified = r::verify(&path)?;
    // Everything that can be refused is refused before the daemon is touched.
    r::check_room(home, &dir, &path)?;
    if !yes {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "restoring replaces the memory database; pass --yes to confirm"
        );
        eprint!(
            "Restore {} ({} memories, {} chunks)? Memories written after that backup leave the active database; they stay in a before-restore copy in {} (kept at least {} days), and `alice restore <that copy>` undoes this. {} [y/N] ",
            path.display(),
            verified.memories,
            verified.chunks,
            dir.display(),
            r::KEEP_DAYS,
            r::CAVEATS
        );
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        anyhow::ensure!(
            matches!(answer.trim(), "y" | "Y" | "yes"),
            "restore cancelled"
        );
    }
    // One restore at a time; while it is held, clients spool writes at once.
    let guard = r::RestoreGuard::acquire(home)?;
    // An installed service unit would start a daemon in the middle of the swap (launchd and
    // systemd restart what exits): stop it first, and start it again afterwards.
    let suspended = suspend_service()?;
    let swapped = swap_stopping_the_daemon(home, &dir, &verified).await;
    drop(guard);
    let resumed = suspended.map(Suspended::resume);
    let mut report = swapped?;
    if let Some(Err(e)) = resumed {
        report.warnings.push(format!(
            "the service unit could not be started again ({e}); start it with `alice service install`"
        ));
    }
    // Starts the daemon on the restored database (it migrates an older one, rebuilds the index
    // and drains the spool).
    match client(home).await {
        Ok(c) => {
            for _ in 0..600 {
                if c.health().await.map(|h| h.index_ready).unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Err(e) => report.warnings.push(format!(
            "restored, but the daemon did not answer yet ({e}); \"alice daemon start\" waits for it"
        )),
    }
    print(json, &report, || {
        let mut t = format!(
            "Restored {}: {} memories, {} chunks.\n",
            report.restored_from, report.memories, report.chunks
        );
        if let (Some(saved), Some(undo)) = (&report.saved_current_as, &report.undo) {
            t.push_str(&format!(
                "Memories written after that backup are no longer in the active database. They stay in {} (kept at least {} days).\nTo undo: {undo} --yes\n",
                dir.join(saved).display(),
                r::KEEP_DAYS
            ));
        }
        for w in &report.warnings {
            t.push_str(&format!("warning: {w}\n"));
        }
        t.push_str(report.caveats);
        t.push('\n');
        t
    })
}

// ---------------------------------------------------------------------------- doctor

/// `alice doctor`: the checks that need no daemon, then the daemon's own when one answers. Never
/// starts a daemon. Exits non-zero when any check fails.
async fn doctor_command(home: &AliceHome, json: bool) -> Result<()> {
    let daemon = DaemonClient::try_existing(home).await;
    let health = match &daemon {
        Some(c) => c.health().await.ok(),
        None => None,
    };
    let mut checks = doctor::local_checks(home, health.as_ref());
    if let Some(c) = &daemon {
        match c.doctor().await {
            Ok(r) => checks.extend(r.checks),
            Err(e) => checks.push(Check {
                name: "daemon report".into(),
                status: CheckStatus::Warn,
                detail: format!("the daemon could not report its checks: {e}"),
                fix: Some(
                    "run `alice daemon stop`; the next command starts the current version".into(),
                ),
            }),
        }
    }
    let report = DoctorReport { checks };
    print(json, &report, || {
        report
            .checks
            .iter()
            .map(|c| format!("{}\n", c.line()))
            .collect()
    })?;
    let failed = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .count();
    anyhow::ensure!(
        failed == 0,
        "{failed} check{} failed; the FAIL lines say what to do",
        if failed == 1 { "" } else { "s" }
    );
    Ok(())
}

// ---------------------------------------------------------------------------- token

/// `alice token rotate`: a running daemon writes the new token and switches at once; with none,
/// the file is replaced while this process holds the daemon lock (so no daemon starts on the
/// old token meanwhile).
async fn token_rotate(home: &AliceHome, show: bool, json: bool) -> Result<()> {
    if let Some(c) = DaemonClient::try_existing(home).await {
        c.rotate_token().await?;
    } else {
        home.ensure_dirs()?;
        let Some(_lock) = alice_daemon::daemon::try_lock_home(home)? else {
            bail!(
                "a daemon holds {} but does not answer (it is starting or stopping); nothing was changed. Retry in a moment, or see \"alice daemon status\"",
                home.lock_file().display()
            );
        };
        alice_daemon::discovery::rotate_token(home).with_context(|| {
            format!(
                "cannot write the new token to {}; the old one stays in force. Check the permissions of {}",
                home.token_file().display(),
                home.root().display()
            )
        })?;
    }
    let token = show
        .then(|| alice_daemon::discovery::read_token(home))
        .flatten();
    let value = match &token {
        Some(t) => serde_json::json!({ "rotated": true, "token": t }),
        None => serde_json::json!({ "rotated": true }),
    };
    print(json, &value, || {
        let mut t =
            "Token rotated. Clients that cached the old token reconnect on their next call.\n"
                .to_string();
        if let Some(token) = &token {
            t.push_str(&format!("{token}\n"));
        }
        t
    })
}

// ---------------------------------------------------------------------------- service

/// How a service command treats the service manager.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ServiceMode {
    /// The user's home: commands run for real.
    Real,
    /// `--target-dir`: files go under that directory and no command runs.
    TargetDir,
    /// `ALICE_SERVICE_ROOT` (for tests): files go under that directory and the commands are
    /// recorded in `service-commands.log` there instead of run.
    Sandbox,
}

/// Where service files go, and whether commands may run.
struct ServiceRoot {
    path: std::path::PathBuf,
    mode: ServiceMode,
}

fn service_root(target_dir: Option<std::path::PathBuf>) -> Result<ServiceRoot> {
    if let Some(path) = target_dir {
        return Ok(ServiceRoot {
            path,
            mode: ServiceMode::TargetDir,
        });
    }
    if let Some(path) = std::env::var_os("ALICE_SERVICE_ROOT").filter(|v| !v.is_empty()) {
        return Ok(ServiceRoot {
            path: path.into(),
            mode: ServiceMode::Sandbox,
        });
    }
    Ok(ServiceRoot {
        path: svcinst::user_home().context("cannot find your home directory")?,
        mode: ServiceMode::Real,
    })
}

fn user_id() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// A sandbox "runs" a command by appending it to `service-commands.log` in the sandbox, so a test
/// can see what would have run without ever calling `launchctl`, `systemctl` or `schtasks`.
struct LogRunner(std::path::PathBuf);

impl Runner for LogRunner {
    fn run(&self, cmd: &[String]) -> alice_core::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)?;
        writeln!(f, "{}", cmd.join(" "))?;
        Ok(())
    }
}

/// The runner for a root, or `None` where no command may run (`--target-dir`).
fn runner_for(root: &ServiceRoot) -> Option<Box<dyn Runner>> {
    match root.mode {
        ServiceMode::Real => Some(Box::new(SystemRunner)),
        ServiceMode::Sandbox => Some(Box::new(LogRunner(root.path.join("service-commands.log")))),
        ServiceMode::TargetDir => None,
    }
}

/// A service unit stopped for a restore, to be started again afterwards.
struct Suspended {
    platform: Platform,
    root: ServiceRoot,
    runner: Box<dyn Runner>,
}

impl Suspended {
    fn resume(self) -> alice_core::Result<()> {
        svcinst::resume(
            self.platform,
            &self.root.path,
            user_id(),
            self.runner.as_ref(),
        )
    }
}

/// Stop the installed service unit's daemon. `None`: there is no unit (nothing to restart).
/// A unit that cannot be stopped refuses the restore, before anything is changed.
fn suspend_service() -> Result<Option<Suspended>> {
    let Some(platform) = Platform::current() else {
        return Ok(None);
    };
    let Ok(root) = service_root(None) else {
        return Ok(None);
    };
    let Some(runner) = runner_for(&root) else {
        return Ok(None);
    };
    match svcinst::suspend(platform, &root.path, user_id(), runner.as_ref()) {
        Ok(true) => Ok(Some(Suspended {
            platform,
            root,
            runner,
        })),
        Ok(false) => Ok(None),
        Err(e) => bail!(
            "the installed service could not be stopped ({e}); nothing was changed. It would start a daemon in the middle of the restore: stop it yourself (`alice service uninstall`), retry, and `alice service install` again afterwards"
        ),
    }
}

fn service_command(home: &AliceHome, c: ServiceCmd, json: bool) -> Result<()> {
    let platform =
        Platform::current().context("alice service supports macOS, Linux and Windows")?;
    match c {
        ServiceCmd::Install {
            dry_run,
            target_dir,
        } => {
            let root = service_root(target_dir)?;
            // The program that is running now, and the home this command uses (absolute: the
            // service runs from another directory).
            let exe = std::env::current_exe().context("cannot find this program's path")?;
            #[cfg(unix)]
            let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
            let home = AliceHome::new(
                std::path::absolute(home.root()).unwrap_or_else(|_| home.root().to_path_buf()),
            );
            let plan = svcinst::plan_install(platform, &root.path, &exe, &home, user_id());
            if dry_run {
                return print(
                    json,
                    &serde_json::json!({ "dry_run": true, "plan": plan }),
                    || plan_text(&plan, "Would write", "Would run"),
                );
            }
            let runner = runner_for(&root);
            // A unit that is already loaded is unloaded first (launchd refuses to load it twice).
            if let Some(r) = &runner
                && svcinst::installed(platform, &root.path).is_some()
            {
                svcinst::unload(platform, &root.path, user_id(), r.as_ref())?;
            }
            // `--target-dir` writes the files and runs nothing.
            svcinst::apply(&plan, runner.as_deref())?;
            let ran = runner.is_some();
            print(
                json,
                &serde_json::json!({ "installed": true, "files": plan.files.iter().map(|(p, _)| p).collect::<Vec<_>>(), "commands_run": ran }),
                || {
                    let mut t = String::new();
                    for (p, _) in &plan.files {
                        t.push_str(&format!("Wrote {}\n", p.display()));
                    }
                    for cmd in &plan.commands {
                        t.push_str(&format!(
                            "{}: {}\n",
                            if ran { "Ran" } else { "not run (--target-dir)" },
                            cmd.join(" ")
                        ));
                    }
                    for n in &plan.notes {
                        t.push_str(&format!("{n}\n"));
                    }
                    t
                },
            )
        }
        ServiceCmd::Uninstall {
            dry_run,
            target_dir,
        } => {
            let root = service_root(target_dir)?;
            let plan = svcinst::plan_uninstall(platform, &root.path, user_id());
            if dry_run {
                let unit = svcinst::installed(platform, &root.path);
                return print(
                    json,
                    &serde_json::json!({ "dry_run": true, "plan": plan, "would_remove": unit }),
                    || {
                        let mut t = plan_text(&plan, "Would write", "Would run");
                        if let Some(u) = &unit {
                            t.push_str(&format!("Would remove {}\n", u.display()));
                        }
                        t
                    },
                );
            }
            let runner = runner_for(&root);
            svcinst::uninstall(platform, &root.path, user_id(), runner.as_deref())?;
            print(json, &serde_json::json!({ "installed": false }), || {
                "Service removed.\n".into()
            })
        }
        ServiceCmd::Status => {
            let root = service_root(None)?;
            let unit = svcinst::installed(platform, &root.path);
            // Windows keeps the task in the Task Scheduler, not in a file.
            let task =
                (platform == Platform::Windows && root.mode == ServiceMode::Real).then(|| {
                    SystemRunner
                        .run(&[
                            "schtasks".into(),
                            "/Query".into(),
                            "/TN".into(),
                            svcinst::TASK.into(),
                        ])
                        .is_ok()
                });
            let installed = unit.is_some() || task == Some(true);
            print(
                json,
                &serde_json::json!({ "installed": installed, "path": unit }),
                || match (&unit, task) {
                    (Some(p), _) => format!("installed ({})\n", p.display()),
                    (None, Some(true)) => format!("installed (scheduled task {})\n", svcinst::TASK),
                    _ => "not installed\n".into(),
                },
            )
        }
    }
}

fn plan_text(plan: &svcinst::InstallPlan, write: &str, run: &str) -> String {
    let mut t = String::new();
    for (p, content) in &plan.files {
        t.push_str(&format!("{write} {}:\n{content}\n", p.display()));
    }
    for cmd in &plan.commands {
        t.push_str(&format!("{run}: {}\n", cmd.join(" ")));
    }
    for n in &plan.notes {
        t.push_str(&format!("note: {n}\n"));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn the_palace_comes_from_the_argument_then_two_variables_then_the_config() {
        let home = tempfile::tempdir().unwrap();
        let cwd = Path::new("/work");
        let arg = Some(std::path::PathBuf::from("rel/palace"));
        assert_eq!(
            resolve_palace(
                arg,
                &lookup(&[("MEMPALACE_PALACE_PATH", "/env")]),
                Some(home.path()),
                cwd
            )
            .unwrap(),
            Path::new("/work/rel/palace")
        );
        let both = [
            ("MEMPALACE_PALACE_PATH", "/a"),
            ("MEMPAL_PALACE_PATH", "/b"),
        ];
        assert_eq!(
            resolve_palace(None, &lookup(&both), Some(home.path()), cwd).unwrap(),
            Path::new("/a")
        );
        assert_eq!(
            resolve_palace(
                None,
                &lookup(&[("MEMPAL_PALACE_PATH", "/b")]),
                Some(home.path()),
                cwd
            )
            .unwrap(),
            Path::new("/b")
        );
        // An empty variable counts as unset.
        let e = resolve_palace(
            None,
            &lookup(&[("MEMPALACE_PALACE_PATH", "")]),
            Some(home.path()),
            cwd,
        );
        assert!(e.unwrap_err().to_string().contains("--palace"));
        std::fs::create_dir_all(home.path().join(".mempalace")).unwrap();
        std::fs::write(
            home.path().join(".mempalace/config.json"),
            r#"{"palace_path": "~/pal"}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_palace(None, &lookup(&[]), Some(home.path()), cwd).unwrap(),
            home.path().join("pal")
        );
    }

    #[test]
    fn a_broken_config_says_so_and_a_missing_one_asks_for_the_flag() {
        let home = tempfile::tempdir().unwrap();
        let cwd = Path::new("/work");
        let e = resolve_palace(None, &lookup(&[]), Some(home.path()), cwd)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no palace found") && e.contains("--palace"),
            "{e}"
        );
        std::fs::create_dir_all(home.path().join(".mempalace")).unwrap();
        std::fs::write(home.path().join(".mempalace/config.json"), "{ nope").unwrap();
        let e = resolve_palace(None, &lookup(&[]), Some(home.path()), cwd)
            .unwrap_err()
            .to_string();
        assert!(e.contains("config.json") && e.contains("--palace"), "{e}");
        std::fs::write(
            home.path().join(".mempalace/config.json"),
            r#"{"other": 1}"#,
        )
        .unwrap();
        let e = resolve_palace(None, &lookup(&[]), Some(home.path()), cwd)
            .unwrap_err()
            .to_string();
        assert!(e.contains("no palace found"), "{e}");
    }

    #[test]
    fn map_wing_splits_on_the_first_equals_sign() {
        assert_eq!(parse_map_wing("w=a=b").unwrap(), ("w".into(), "a=b".into()));
        assert!(parse_map_wing("nope").is_err());
        assert!(parse_map_wing("=x").is_err());
        assert!(parse_map_wing("x=").is_err());
    }

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

    #[test]
    fn a_reranked_recall_says_so_and_a_plain_one_does_not() {
        let mut r = RecallResponse {
            text: Some("1. one\n2. two\n".into()),
            ..Default::default()
        };
        assert_eq!(recall_text(&r), "1. one\n2. two\n");
        r.reranked = true;
        assert_eq!(
            recall_text(&r),
            "1. one\n2. two\n(re-ranked by the judge)\n"
        );
        r.text = Some("no newline".into());
        assert_eq!(recall_text(&r), "no newline\n(re-ranked by the judge)\n");
    }

    /// `alice status` names the embedding model, the progress of a re-embed, the rows the cloud
    /// policy holds back and the reason the model asked for is not in force.
    #[test]
    fn status_notes_name_the_embedding_model_its_re_embed_and_its_problems() {
        let mut h: Health = serde_json::from_value(serde_json::json!({
            "service": "alice", "instance": "i", "version": "0", "pid": 1, "memories": 0,
            "chunks": 0, "projects": 0, "embed_backlog": 0, "index_ready": true
        }))
        .unwrap();
        assert!(
            !health_notes(&h).contains("embedding model"),
            "old daemons say nothing"
        );
        h.embedding_model = "multilingual-e5-small-q8".into();
        assert_eq!(
            health_notes(&h),
            "embedding model: multilingual-e5-small-q8\n"
        );
        h.reembed = Some(alice_core::api::ReembedStatus {
            model: "openai:m".into(),
            done: 40,
            total: 100,
        });
        h.embed_skipped_cloud = 7;
        h.embedding_error = Some("openai:m is unreachable".into());
        let notes = health_notes(&h);
        assert!(
            notes.contains("(re-embedding into openai:m: 40 of 100)")
                && notes.contains("7 rows held back by the cloud policy")
                && notes.contains("WARNING: openai:m is unreachable"),
            "{notes}"
        );
    }
}
