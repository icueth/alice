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
    /// List projects.
    Projects,
    /// Daemon status and counts.
    Status,
    /// Manage the background daemon.
    #[command(subcommand)]
    Daemon(cmd::DaemonCmd),
    /// Run the MCP server on stdio (configure agents to launch `alice mcp`).
    Mcp,
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
    #[cfg(unix)]
    if !is_server {
        // `alice recall … | head` must not panic on a closed pipe.
        // SAFETY: restoring the default disposition before any output is written.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match cmd::dispatch(cli.command, cli.json) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("alice: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
