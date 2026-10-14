//! `alice`: long-term memory for AI coding agents.

mod bench;
mod cmd;

use clap::{Parser, Subcommand};
use std::io::IsTerminal;

#[derive(Parser)]
#[command(
    name = "alice",
    version,
    about = "Long-term memory for AI coding agents"
)]
struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search memories.
    Recall(cmd::RecallArgs),
    /// Save a memory (text argument, or "-" to read stdin).
    Remember(cmd::RememberArgs),
    /// Briefing for the current project.
    Context(cmd::ContextArgs),
    /// Show memories by id.
    Get(cmd::GetArgs),
    /// Edit, supersede, archive, forget or restore a memory.
    Update(cmd::UpdateArgs),
    /// Facts about an entity.
    Facts(cmd::FactsArgs),
    /// Add, supersede or end a fact.
    Fact(cmd::FactArgs),
    /// Recent changes (all, or to one memory or fact): who changed what, with event ids for undo.
    History(cmd::HistoryArgs),
    /// Reverse a change by its event id (from `alice history`).
    Undo {
        event_id: String,
        /// Who undoes it (default: $ALICE_AGENT or "cli", as for `remember`).
        #[arg(long, env = "ALICE_AGENT", default_value = "cli")]
        agent: String,
    },
    /// Decisions Alice left to you: duplicates, updates, contradictions.
    Review {
        #[command(subcommand)]
        cmd: Option<cmd::ReviewCmd>,
    },
    /// Counts, review backlog and judge costs.
    Stats,
    /// Backups of the database (`now` makes one; `list` shows them).
    #[command(subcommand)]
    Backup(cmd::BackupCmd),
    /// Maintenance jobs: when they last ran, or run one now.
    #[command(subcommand)]
    Maintenance(cmd::MaintenanceCmd),
    /// Replace the memory database with a backup (default: the newest). The current database is
    /// kept in backups/; the daemon restarts and rebuilds its index. Memories purged after the
    /// backup come back, and text stored under older redaction rules is not scanned again.
    Restore {
        /// A backup file name from `alice backup list`, or a path.
        backup: Option<String>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// List projects.
    Projects,
    /// Daemon status and counts.
    Status,
    /// Check the installation, the daemon and the data, with what to do about each problem.
    Doctor,
    /// Manage the API token.
    #[command(subcommand)]
    Token(cmd::TokenCmd),
    /// Run the daemon at login (launchd, systemd --user, or a scheduled task).
    #[command(subcommand)]
    Service(cmd::ServiceCmd),
    /// Manage the background daemon.
    #[command(subcommand)]
    Daemon(cmd::DaemonCmd),
    /// Run the MCP server on stdio (configure agents to launch `alice mcp`).
    Mcp,
    /// Read and change settings (config.toml, applied live) and secrets.
    #[command(subcommand)]
    Config(cmd::ConfigCmd),
    /// Embedding model management.
    #[command(subcommand)]
    Model(cmd::ModelCmd),
    /// Measure latency and throughput on synthetic data.
    Bench(cmd::BenchArgs),
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    alice_daemon::install_tls();
    let is_server = matches!(
        cli.command,
        Command::Daemon(cmd::DaemonCmd::Run) | Command::Mcp
    );
    // Logs go to stderr: on stdio MCP, stdout is the protocol channel. The daemon's stderr is
    // its log file, so colours only when a person watches.
    let filter = tracing_subscriber::EnvFilter::try_from_env("ALICE_LOG").unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if is_server { "info" } else { "warn" })
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    // SIGPIPE stays ignored (Rust's default): with the default action, a write to a daemon
    // that died mid-request would kill a command before it could spool. A closed stdout
    // (`alice recall … | head`) comes back as `StdoutClosed` instead and ends quietly.
    match cmd::dispatch(cli.command, cli.json) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) if e.chain().any(|c| c.is::<cmd::StdoutClosed>()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("alice: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
