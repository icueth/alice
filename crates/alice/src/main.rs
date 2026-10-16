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

/// Libraries under the MCP and HTTP layers that log whole requests at debug (an MCP call's
/// arguments, before any redaction): kept at info unless `ALICE_LOG` names them.
const QUIET_TARGETS: [&str; 4] = ["rmcp", "hyper", "h2", "reqwest"];

/// The log filter for `ALICE_LOG` (`spec`), or the default when it is unset or invalid. A plain
/// `debug` or `trace` does not reach [`QUIET_TARGETS`] (final review A-m4); `rmcp=debug` does.
fn log_filter(spec: Option<&str>, is_server: bool) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    let Some(f) = spec.and_then(|s| EnvFilter::try_new(s).ok()) else {
        return EnvFilter::new(if is_server { "info" } else { "warn" });
    };
    let spec = spec.unwrap_or_default().to_ascii_lowercase();
    if !(spec.contains("debug") || spec.contains("trace")) {
        return f;
    }
    let named = |t: &str| spec.split(',').any(|d| d.trim().starts_with(t));
    QUIET_TARGETS
        .into_iter()
        .filter(|t| !named(t))
        .fold(f, |f, t| {
            f.add_directive(format!("{t}=info").parse().expect("a valid directive"))
        })
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
    let filter = log_filter(std::env::var("ALICE_LOG").ok().as_deref(), is_server);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Final review A-m4: `ALICE_LOG=debug` never writes an MCP call's arguments (rmcp logs the
    /// request at debug); naming the target still turns it on.
    #[test]
    fn a_plain_debug_log_keeps_the_request_logging_libraries_at_info() {
        let f = log_filter(Some("debug"), true).to_string();
        for t in QUIET_TARGETS {
            assert!(f.contains(&format!("{t}=info")), "{f}");
        }
        let f = log_filter(Some("debug,rmcp=debug"), true).to_string();
        assert!(f.contains("rmcp=debug") && !f.contains("rmcp=info"), "{f}");
        assert!(
            !log_filter(Some("alice_daemon=debug,warn"), true)
                .to_string()
                .contains("alice_daemon=info")
        );
        assert_eq!(log_filter(Some("warn"), true).to_string(), "warn");
        assert_eq!(log_filter(None, true).to_string(), "info");
        assert_eq!(log_filter(Some("not a [filter"), false).to_string(), "warn");
    }
}
