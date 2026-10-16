//! `alice hook <event>` (SP2 spec §4.1): what an agent (Claude Code, Codex) runs on
//! SessionStart, Stop, SubagentStop, PreCompact and SessionEnd. It tells the local daemon that a
//! session's transcript changed (the daemon reads the file later, from its cursor), prints the
//! briefing when a Claude Code session starts, and when the daemon is down spools the notice and
//! starts one in the background.
//!
//! It runs at every turn, so it is fast (p95 under 100 ms from process start to exit), and it
//! never fails the agent: it always exits 0, writes nothing to stderr and prints nothing but the
//! SessionStart briefing. Problems go to `logs/hook.log`, one line each, never with the input's
//! text.

use crate::cmd::HookArgs;
use alice_core::api::{BriefingQuery, CaptureNotify};
use alice_core::error::AliceError;
use alice_daemon::client::{DaemonClient, start_detached};
use alice_daemon::paths::AliceHome;
use alice_daemon::spool::{self, SpoolEntry};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// The whole hook, from its start: a briefing that has not arrived by then is not printed.
pub const HOOK_DEADLINE: Duration = Duration::from_millis(90);
/// How long the daemon may take to answer the ping that proves it holds the token.
pub const HOOK_PING: Duration = Duration::from_millis(30);
/// How long the daemon may take to accept the notice.
pub const HOOK_REQUEST: Duration = Duration::from_millis(40);
/// Stdin past this is not read: a hook's input is a few hundred bytes.
const MAX_INPUT: usize = 1 << 20;
/// `hook.log` becomes `hook.log.1` past this size.
const LOG_MAX: u64 = 1_000_000;
/// A logged problem is cut to this many characters.
const LINE_MAX: usize = 400;

/// The agents' `hook_event_name`s and the events of the command line.
const EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start"),
    ("Stop", "stop"),
    ("SubagentStop", "subagent-stop"),
    ("PreCompact", "pre-compact"),
    ("SessionEnd", "session-end"),
];
const AGENTS: [&str; 2] = ["claude-code", "codex"];

/// `alice hook …` as `main` dispatches it, before anything else is set up. Arguments clap cannot
/// read are logged and nothing else is done; `--help` prints the help.
pub fn main() -> ExitCode {
    use clap::Parser;
    use clap::error::ErrorKind;
    match crate::Cli::try_parse() {
        Ok(crate::Cli {
            command: crate::Command::Hook(args),
            ..
        }) => run(args),
        // `main` sends only `alice hook …` here.
        Ok(_) => ExitCode::SUCCESS,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            let _ = e.print();
            ExitCode::SUCCESS
        }
        Err(e) => {
            Log::new(&AliceHome::from_env(), "", "").line(&format!(
                "ignored: bad arguments ({}); the hook command is `alice hook <event> --agent claude-code|codex`",
                e.kind()
            ));
            ExitCode::SUCCESS
        }
    }
}

/// The hook. Always `ExitCode::SUCCESS`, whatever the input, the daemon or the disk do.
pub fn run(args: HookArgs) -> ExitCode {
    let started = Instant::now();
    let home = AliceHome::from_env();
    let log = Log::new(&home, &args.agent, &args.event);
    // A bug must not fail the agent's turn or reach its screen either: it is logged. Only a
    // fixed message is kept, never one formatted with data.
    let on_panic = log.clone();
    std::panic::set_hook(Box::new(move |info| {
        let what = info.payload().downcast_ref::<&str>().copied();
        let place = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        on_panic.line(&format!(
            "the hook stopped on a bug{place}: {}",
            what.unwrap_or("(message not kept)")
        ));
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hook(&home, &args, &log, started)
    }));
    ExitCode::SUCCESS
}

fn hook(home: &AliceHome, args: &HookArgs, log: &Log, started: Instant) {
    let n = match read_input(started + HOOK_DEADLINE).and_then(|input| notice(&input, args)) {
        Ok(n) => n,
        Err(why) => return log.line(&why),
    };
    let taken = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(deliver(home, &n, started, log)),
        Err(e) => {
            log.line(&format!(
                "cannot start the hook's runtime, so the notice is spooled: {e}"
            ));
            false
        }
    };
    if !taken {
        spool_and_start(home, n, log);
    }
}

/// Stdin to its end, at most [`MAX_INPUT`] bytes, by `deadline`; `Err` says why there is no input
/// to use. A terminal is not read (a person who types `alice hook stop` gets the prompt back at
/// once), and a pipe that is never closed is left behind at the deadline.
fn read_input(deadline: Instant) -> Result<Vec<u8>, String> {
    if std::io::stdin().is_terminal() {
        return Err("ignored: stdin is a terminal; an agent sends the hook's JSON there".into());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = std::io::stdin()
            .lock()
            .take(MAX_INPUT as u64 + 1)
            .read_to_end(&mut buf);
        let _ = tx.send(read.map(|_| buf));
    });
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(buf)) if buf.len() > MAX_INPUT => {
            Err("ignored: stdin is over 1 MiB; a hook's input is a few hundred bytes".into())
        }
        Ok(Ok(buf)) => Ok(buf),
        Ok(Err(e)) => Err(format!("ignored: cannot read stdin: {e}")),
        Err(_) => Err("ignored: stdin was not closed within the hook's 90 ms".into()),
    }
}

/// The command line's event for an agent's `hook_event_name`.
fn event_named(name: &str) -> Option<&'static str> {
    EVENTS.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
}

/// The notice for the daemon from the hook's input (SP2 spec §2.1, §2.2), in which only string
/// fields count. `Err` says why the input is ignored, without repeating it.
fn notice(input: &[u8], args: &HookArgs) -> Result<CaptureNotify, String> {
    if input.iter().all(u8::is_ascii_whitespace) {
        return Err("ignored: stdin is empty; an agent sends the hook's JSON there".into());
    }
    let v: serde_json::Value = serde_json::from_slice(input).map_err(|e| {
        format!(
            "ignored: stdin holds no hook input (invalid JSON at line {}, column {})",
            e.line(),
            e.column()
        )
    })?;
    let Some(fields) = v.as_object() else {
        return Err("ignored: stdin holds no hook input (a JSON value that is no object)".into());
    };
    let text = |key: &str| {
        fields
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    // The input's event wins over the argument when it names one of ours.
    let event = text("hook_event_name")
        .and_then(|name| event_named(&name))
        .or_else(|| EVENTS.iter().map(|(_, e)| *e).find(|e| *e == args.event))
        .ok_or_else(|| {
            format!(
                "ignored: unknown event \"{}\"; use session-start, stop, subagent-stop, pre-compact or session-end",
                clean(&args.event, 40)
            )
        })?;
    if !AGENTS.contains(&args.agent.as_str()) {
        return Err(format!(
            "ignored: unknown agent \"{}\"; use --agent claude-code or --agent codex",
            clean(&args.agent, 40)
        ));
    }
    let Some(session_id) = text("session_id") else {
        return Err("ignored: the input has no session_id".into());
    };
    alice_daemon::capture::check_ids(&session_id, None).map_err(|e| format!("ignored: {e}"))?;
    let transcript_path = match text("transcript_path") {
        Some(p) => p,
        // Codex may give none: the daemon finds the rollout by the session id.
        None if args.agent == "codex" => String::new(),
        None => return Err("ignored: the input has no transcript_path".into()),
    };
    Ok(CaptureNotify {
        agent: args.agent.clone(),
        event: event.into(),
        session_id,
        transcript_path,
        cwd: text("cwd"),
        agent_id: text("agent_id"),
        agent_transcript_path: text("agent_transcript_path"),
        source: text("source"),
        reason: text("reason"),
    })
}

/// Whether the notice is a new Claude Code session's start, which gets the briefing: after
/// `resume` and `fork` the conversation already holds one, and Codex gets it through the MCP
/// instructions.
fn wants_briefing(n: &CaptureNotify) -> bool {
    n.agent == "claude-code"
        && n.event == "session-start"
        && matches!(
            n.source.as_deref(),
            None | Some("startup" | "clear" | "compact")
        )
}

/// Hands the notice to the daemon and, at a new session's start, prints the briefing. `false`
/// when the daemon did not take it (none answers, or not in time): the caller spools it. A
/// refused notice counts as taken: the drain would only be refused again.
async fn deliver(home: &AliceHome, n: &CaptureNotify, started: Instant, log: &Log) -> bool {
    let Some(daemon) = DaemonClient::for_hook(home, HOOK_PING).await else {
        return false;
    };
    match daemon.capture_notify(n, HOOK_REQUEST).await {
        Ok(()) => {}
        Err(AliceError::Invalid(why)) => {
            log.line(&format!("the daemon refused the notice: {why}"));
            return true;
        }
        Err(e) => {
            log.line(&format!(
                "the daemon did not take the notice, so it is spooled: {e}"
            ));
            return false;
        }
    }
    if wants_briefing(n) {
        print_briefing(&daemon, n, started, log).await;
    }
    true
}

/// Prints the session's briefing as Claude Code's SessionStart output, if the daemon gives one
/// within what is left of [`HOOK_DEADLINE`]; without it the MCP instructions make the agent ask
/// for `context` itself.
async fn print_briefing(daemon: &DaemonClient, n: &CaptureNotify, started: Instant, log: &Log) {
    let left = HOOK_DEADLINE.saturating_sub(started.elapsed());
    if left.is_zero() {
        return log.line("no briefing: the hook's 90 ms were used up before it was asked for");
    }
    let q = BriefingQuery {
        source: n.source.clone(),
        cwd: n.cwd.clone(),
        ..BriefingQuery::default()
    };
    let text = match daemon.capture_briefing(&q, left).await {
        Ok(Some(text)) if !text.trim().is_empty() => text,
        Ok(_) => return,
        Err(e) => return log.line(&format!("no briefing within the hook's 90 ms: {e}")),
    };
    let out = serde_json::json!({
        "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": text}
    });
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = writeln!(stdout, "{out}").and_then(|()| stdout.flush()) {
        log.line(&format!("cannot print the briefing: {e}"));
    }
}

/// The daemon did not take the notice: queue it in the spool (one fsynced file), which the daemon
/// drains when it runs, and start one in the background unless `ALICE_NO_AUTOSTART` forbids it.
/// Nothing waits for the daemon to serve.
fn spool_and_start(home: &AliceHome, n: CaptureNotify, log: &Log) {
    let dir = home.spool_dir();
    if let Err(e) = spool::append(&dir, &SpoolEntry::CaptureNotify(n)) {
        log.line(&format!(
            "cannot queue the notice in {}: {e}",
            dir.display()
        ));
    }
    if !autostart() {
        return;
    }
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = start_detached(home, &exe) {
                log.line(&format!(
                    "cannot start the daemon with {}: {e}",
                    exe.display()
                ));
            }
        }
        Err(e) => log.line(&format!(
            "cannot find this program to start the daemon: {e}"
        )),
    }
}

/// Whether a daemon may be started: `cmd::launcher`'s rule (`ALICE_NO_AUTOSTART` set to anything
/// but empty or `0` forbids it).
fn autostart() -> bool {
    !std::env::var_os("ALICE_NO_AUTOSTART").is_some_and(|v| !v.is_empty() && v != "0")
}

/// `logs/hook.log`: one line per problem, `<time> <agent> <event>: <what>`.
#[derive(Clone)]
struct Log {
    path: PathBuf,
    who: String,
}

impl Log {
    /// `agent` and `event` as the command line gave them (`-` when it gave none).
    fn new(home: &AliceHome, agent: &str, event: &str) -> Log {
        let word = |s: &str| {
            if s.is_empty() {
                "-".to_string()
            } else {
                clean(s, 40)
            }
        };
        Log {
            path: home.logs_dir().join("hook.log"),
            who: format!("{} {}", word(agent), word(event)),
        }
    }

    /// Appends one line, moving a full log to `hook.log.1` first. A log that cannot be written is
    /// given up: the hook never writes to stderr.
    fn line(&self, what: &str) {
        let Some(dir) = self.path.parent() else {
            return;
        };
        let _ = std::fs::create_dir_all(dir);
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= LOG_MAX) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        let line = format!(
            "{} {}: {}\n",
            alice_core::ids::now_rfc3339(),
            self.who,
            clean(what, LINE_MAX)
        );
        let mut open = std::fs::OpenOptions::new();
        open.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        if let Ok(mut f) = open.open(&self.path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// `s` on one line (control characters become spaces), cut to `max` characters.
fn clean(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(event: &str, agent: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: agent.into(),
        }
    }

    fn input(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    fn claude(event_name: &str) -> Vec<u8> {
        input(
            json!({"session_id": "s1", "transcript_path": "/t/s1.jsonl", "cwd": "/w",
            "hook_event_name": event_name}),
        )
    }

    #[test]
    fn the_inputs_event_name_wins_over_the_argument_when_it_is_one_of_ours() {
        for (name, event) in EVENTS {
            let n = notice(&claude(name), &args("stop", "claude-code")).unwrap();
            assert_eq!(n.event, event, "{name}");
        }
        // An event of another kind, or none: the argument.
        let n = notice(
            &claude("UserPromptSubmit"),
            &args("pre-compact", "claude-code"),
        )
        .unwrap();
        assert_eq!(n.event, "pre-compact");
        let none = input(json!({"session_id": "s1", "transcript_path": "/t/s1.jsonl"}));
        let n = notice(&none, &args("session-end", "claude-code")).unwrap();
        assert_eq!(n.event, "session-end");
        // The argument takes the command line's names only.
        let e = notice(&none, &args("Stop", "claude-code")).unwrap_err();
        assert!(e.contains("unknown event \"Stop\""), "{e}");
    }

    #[test]
    fn every_field_reaches_the_notice_and_only_strings_count() {
        let n = notice(
            &input(
                json!({"session_id": "s1", "transcript_path": "/t/a b/s1.jsonl",
                "cwd": "/w/งาน \"x\" y", "hook_event_name": "SubagentStop", "agent_id": "a7",
                "agent_transcript_path": "/t/a7.jsonl", "source": "startup", "reason": "exit",
                "stop_hook_active": true}),
            ),
            &args("stop", "claude-code"),
        )
        .unwrap();
        assert_eq!(
            n,
            CaptureNotify {
                agent: "claude-code".into(),
                event: "subagent-stop".into(),
                session_id: "s1".into(),
                transcript_path: "/t/a b/s1.jsonl".into(),
                cwd: Some("/w/งาน \"x\" y".into()),
                agent_id: Some("a7".into()),
                agent_transcript_path: Some("/t/a7.jsonl".into()),
                source: Some("startup".into()),
                reason: Some("exit".into()),
            }
        );
        let n = notice(
            &input(
                json!({"session_id": "s1", "transcript_path": "/t/s1.jsonl", "cwd": 5,
                "source": ["startup"], "reason": null}),
            ),
            &args("stop", "claude-code"),
        )
        .unwrap();
        assert_eq!((n.cwd, n.source, n.reason), (None, None, None));
    }

    #[test]
    fn an_input_without_a_session_or_a_path_is_ignored_except_codex_without_a_path() {
        let ignored = |v: serde_json::Value, agent: &str| {
            notice(&input(v), &args("stop", agent)).unwrap_err()
        };
        assert!(
            ignored(json!({"transcript_path": "/t/s1.jsonl"}), "claude-code")
                .contains("session_id")
        );
        assert!(
            ignored(
                json!({"session_id": 7, "transcript_path": "/t/s1.jsonl"}),
                "claude-code"
            )
            .contains("session_id")
        );
        assert!(ignored(json!({"session_id": "s1"}), "claude-code").contains("transcript_path"));
        assert!(
            ignored(
                json!({"session_id": "s1", "transcript_path": null}),
                "claude-code"
            )
            .contains("transcript_path")
        );
        assert!(
            ignored(
                json!({"session_id": "../s1", "transcript_path": "/t/s1.jsonl"}),
                "claude-code"
            )
            .contains("session id")
        );
        assert!(
            ignored(
                json!({"session_id": "s1", "transcript_path": "/t/s1.jsonl"}),
                "aider"
            )
            .contains("unknown agent")
        );
        for path in [json!(null), json!("")] {
            let n = notice(
                &input(json!({"session_id": "c1", "transcript_path": path,
                    "hook_event_name": "Stop"})),
                &args("stop", "codex"),
            )
            .unwrap();
            assert_eq!(
                (n.agent.as_str(), n.transcript_path.as_str()),
                ("codex", "")
            );
        }
    }

    #[test]
    fn a_bad_input_is_described_without_repeating_it() {
        for bad in [
            &b"not json at all, secret-word"[..],
            b"",
            b"  \n",
            b"[\"secret-word\"]",
            b"\"secret-word\"",
            b"{\"session_id\": \"secret-word",
        ] {
            let why = notice(bad, &args("stop", "claude-code")).unwrap_err();
            assert!(why.starts_with("ignored: "), "{why}");
            assert!(
                !why.contains("secret-word") && !why.contains("not json"),
                "{why}"
            );
        }
    }

    #[test]
    fn only_a_new_claude_code_session_gets_the_briefing() {
        let start = |agent: &str, source: Option<&str>| CaptureNotify {
            agent: agent.into(),
            event: "session-start".into(),
            source: source.map(String::from),
            ..CaptureNotify::default()
        };
        for source in [None, Some("startup"), Some("clear"), Some("compact")] {
            assert!(wants_briefing(&start("claude-code", source)), "{source:?}");
            assert!(!wants_briefing(&start("codex", source)), "{source:?}");
        }
        for source in ["resume", "fork", "other"] {
            assert!(
                !wants_briefing(&start("claude-code", Some(source))),
                "{source}"
            );
        }
        let stop = CaptureNotify {
            event: "stop".into(),
            ..start("claude-code", None)
        };
        assert!(!wants_briefing(&stop));
    }

    #[test]
    fn the_log_keeps_one_line_per_problem_and_rotates_past_one_megabyte() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().join("home"));
        let log = Log::new(&home, "claude-code", "stop\nforged: line");
        log.line("first\nsecond\r");
        let path = home.logs_dir().join("hook.log");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "{text:?}");
        assert!(
            text.contains(" claude-code stop forged: line: first second "),
            "{text:?}"
        );
        log.line(&"x".repeat(5_000));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().nth(1).unwrap().ends_with('…'));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, vec![b'x'; LOG_MAX as usize]).unwrap();
        log.line("after the rotation");
        let rotated = path.with_extension("log.1");
        assert_eq!(
            std::fs::metadata(&rotated).unwrap().len(),
            LOG_MAX,
            "the full log is kept as hook.log.1"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines().count() == 1 && text.contains("after the rotation"),
            "{text}"
        );
    }
}
