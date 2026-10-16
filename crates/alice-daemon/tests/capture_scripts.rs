//! The tenant capture scripts (SP2 Task 15; spec §5.3–§5.7): `setup.sh` and `hook.sh`, run by a
//! real `sh` against the in-process Caddy emulator.
//!
//! Every run has `HOME` in a temp dir, `PATH` = a temp `bin/` (a fake `claude` that appends its
//! arguments to `$HOME/claude-calls`) plus `/usr/bin:/bin`, and `TMPDIR`, `CLAUDE_CONFIG_DIR`,
//! `CODEX_HOME` and `ALICE_HOME` in the same temp dir. Each child runs in a session of its own
//! (no controlling terminal), so a script that wrongly asks on `/dev/tty` fails instead of
//! waiting. The hook's background workers are waited for (found by their argv) before anything
//! is asserted, and killed if a test fails, so no process outlives its test.
//!
//! `ALICE_TEST_SH=/bin/dash` runs every script, and every `sh` they start, under that shell (the
//! default is the system's `/bin/sh`: bash on macOS, dash on Ubuntu).
#![cfg(unix)]

mod support;

use alice_core::api::*;
use alice_core::model::Kind;
use alice_daemon::paths::AliceHome;
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use support::caddy::{ScriptAnswer, Server};

const SETUP_SH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/capture/setup.sh");
const HOOK_SH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/capture/hook.sh");
/// The command every hook of the plugin runs, as `hooks.json` must hold it. `|| true`: a
/// missing script must never fail the hook (exit 2 from Stop blocks the agent).
const HOOK_COMMAND: &str = r#"sh "$HOME/.config/alice/alice-capture.sh" hook claude-code || true"#;
const FIVE_EVENTS: [&str; 5] = [
    "SessionStart",
    "Stop",
    "SubagentStop",
    "PreCompact",
    "SessionEnd",
];
/// What `setup.sh` prints when it asks for the token (once per prompt).
const PROMPT: &str = "Alice token for ";
const FAKE_CLAUDE: &str = "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/claude-calls\"\nexit 0\n";

// ---- The environment a person's machine is made of ----

/// One finished script run.
struct Run {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

impl Run {
    fn ok(&self) -> bool {
        self.status.success()
    }
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "status {:?} after {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.status.code(),
            self.elapsed,
            self.stdout,
            self.stderr
        )
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    tmp: PathBuf,
    codex_home: PathBuf,
}

fn write_mode(path: &Path, content: &[u8], mode: u32) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// `ALICE_TEST_SH`, when set.
fn test_shell() -> Option<String> {
    std::env::var("ALICE_TEST_SH")
        .ok()
        .filter(|s| !s.is_empty())
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        // Not canonicalized: on macOS the temp dir sits behind the `/var` symlink, as a real
        // `$HOME` may sit behind one, and `git` answers with the resolved path.
        let root = dir.path().to_path_buf();
        let env = Env {
            home: root.join("home"),
            bin: root.join("bin"),
            tmp: root.join("tmp"),
            codex_home: root.join("codex-home"),
            root,
            _dir: dir,
        };
        for d in [
            &env.home,
            &env.bin,
            &env.tmp,
            &env.codex_home,
            &env.root.join("claude-config"),
            &env.root.join("alice-home"),
            &env.root.join("transcripts"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        write_mode(&env.bin.join("claude"), FAKE_CLAUDE.as_bytes(), 0o755);
        if let Some(sh) = test_shell() {
            std::os::unix::fs::symlink(sh, env.bin.join("sh")).unwrap();
        }
        env
    }

    /// The shell the scripts run under.
    fn sh(&self) -> String {
        test_shell().unwrap_or_else(|| "/bin/sh".into())
    }

    fn alice(&self) -> PathBuf {
        self.home.join(".config/alice")
    }

    fn script(&self) -> PathBuf {
        self.alice().join("alice-capture.sh")
    }

    fn marketplace(&self) -> PathBuf {
        self.alice().join("claude-marketplace")
    }

    fn claude_calls(&self) -> String {
        std::fs::read_to_string(self.home.join("claude-calls")).unwrap_or_default()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.alice().join("capture.log")).unwrap_or_default()
    }

    /// When the hook script last looked for a newer copy of itself (epoch seconds).
    fn checked_file(&self) -> PathBuf {
        self.alice().join("update-checked")
    }

    /// Makes the daily update check due: the last one was a day and a minute ago.
    fn make_update_due(&self) {
        std::fs::write(self.checked_file(), format!("{}\n", now_secs() - 86_460)).unwrap();
    }

    /// Temporary files an update left in `~/.config/alice` (none may stay).
    fn update_leftovers(&self) -> Vec<String> {
        let mut found = Vec::new();
        for dir in [self.alice(), self.marketplace().join("alice-capture/hooks")] {
            for e in std::fs::read_dir(&dir).into_iter().flatten() {
                let name = e.unwrap().file_name().to_string_lossy().into_owned();
                if name.starts_with(".alice-capture.sh") || name.contains(".tmp.") {
                    found.push(format!("{}/{name}", dir.display()));
                }
            }
        }
        found
    }

    /// A command with the person's environment and nothing of the test's.
    fn command(&self, program: &str) -> Command {
        let ceiling = format!(
            "{}:{}",
            self.root.display(),
            self.root.canonicalize().unwrap().display()
        );
        let mut c = Command::new(program);
        c.env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("TMPDIR", &self.tmp)
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env("CODEX_HOME", &self.codex_home)
            .env("ALICE_HOME", self.root.join("alice-home"))
            // `git` never looks above the temp dir, wherever the test runs.
            .env("GIT_CEILING_DIRECTORIES", ceiling)
            .current_dir(&self.home);
        c
    }

    /// Runs `c` with `stdin`, in a session of its own, within `limit`. Its stdout must close
    /// when it exits: a background job that kept it open would make Claude Code wait.
    fn run(&self, mut c: Command, stdin: &[u8], limit: Duration) -> Run {
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: `setsid` is async-signal-safe and touches no memory of the parent.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let start = Instant::now();
        let mut child = c.spawn().unwrap();
        let mut sin = child.stdin.take().unwrap();
        let data = stdin.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = sin.write_all(&data);
        });
        let reader = |mut r: Box<dyn Read + Send>| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut v = Vec::new();
                let _ = r.read_to_end(&mut v);
                let _ = tx.send(v);
            });
            rx
        };
        let out = reader(Box::new(child.stdout.take().unwrap()));
        let err = reader(Box::new(child.stderr.take().unwrap()));
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if start.elapsed() > limit {
                let _ = child.kill();
                let _ = child.wait();
                self.kill_workers();
                panic!("the script did not finish within {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let elapsed = start.elapsed();
        writer.join().unwrap();
        let grace = Duration::from_secs(2);
        let stdout = out
            .recv_timeout(grace)
            .expect("a background process kept the script's stdout open");
        let stderr = err
            .recv_timeout(grace)
            .expect("a background process kept the script's stderr open");
        Run {
            status,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            elapsed,
        }
    }

    /// `sh setup.sh <url>` with `stdin` and extra variables.
    fn setup(&self, url: &str, stdin: &str, vars: &[(&str, &str)]) -> Run {
        let mut c = self.command(&self.sh());
        c.arg(SETUP_SH).arg(url);
        for (k, v) in vars {
            c.env(k, v);
        }
        self.run(c, stdin.as_bytes(), Duration::from_secs(60))
    }

    /// The token and URL files, as the team's setup document leaves them.
    fn write_credentials(&self, url: &str, token: &str) {
        std::fs::create_dir_all(self.alice()).unwrap();
        std::fs::set_permissions(self.alice(), std::fs::Permissions::from_mode(0o700)).unwrap();
        write_mode(
            &self.alice().join("token"),
            format!("{token}\n").as_bytes(),
            0o600,
        );
        write_mode(
            &self.alice().join("url"),
            format!("{url}\n").as_bytes(),
            0o600,
        );
    }

    /// The credentials, then `setup.sh`, which must succeed.
    fn install(&self, s: &Server) {
        let url = s.url("ann");
        self.write_credentials(&url, &s.tenant("ann").token);
        let r = self.setup(&url, "", &[]);
        assert!(r.ok(), "setup failed: {r}");
    }

    /// The plugin's hook command, as Claude Code runs it, with `input` on stdin.
    fn hook_raw(&self, input: &[u8]) -> Run {
        let mut c = self.command(&self.sh());
        c.arg("-c").arg(HOOK_COMMAND);
        self.run(c, input, Duration::from_secs(30))
    }

    fn hook(&self, input: &serde_json::Value) -> Run {
        let r = self.hook_raw(&serde_json::to_vec(input).unwrap());
        assert!(r.ok(), "a hook must always exit 0: {r}");
        assert!(r.stderr.is_empty(), "a hook writes nothing to stderr: {r}");
        r
    }

    /// `sh ~/.config/alice/alice-capture.sh <args>`.
    fn capture_sh(&self, args: &[&str], vars: &[(&str, &str)]) -> Run {
        let mut c = self.command(&self.sh());
        c.arg(self.script()).args(args);
        for (k, v) in vars {
            c.env(k, v);
        }
        self.run(c, b"", Duration::from_secs(60))
    }

    /// `(pid, argv)` of every running background worker of this home. A hook's background child
    /// shows the hook's own argv (`… hook <agent>`) until it execs `nohup sh … run …`, so those
    /// count too: a heavily loaded machine may not have reached that exec when the hook returns.
    fn workers(&self) -> Vec<(i32, String)> {
        let run = format!("{} run ", self.script().display());
        let hook = format!("{} hook ", self.script().display());
        let out = Command::new("/bin/ps")
            .args(["-A", "-ww", "-o", "pid=,args="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&run) || l.contains(&hook))
            .filter_map(|l| {
                let l = l.trim_start();
                let (pid, rest) = l.split_once(' ')?;
                Some((pid.parse().ok()?, rest.to_string()))
            })
            .collect()
    }

    fn kill_workers(&self) {
        for (pid, _) in self.workers() {
            // SAFETY: a plain signal to a process this test started.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    /// Waits until every background worker has finished.
    fn wait_idle(&self) {
        let start = Instant::now();
        loop {
            let w = self.workers();
            if w.is_empty() {
                return;
            }
            if start.elapsed() > Duration::from_secs(90) {
                self.kill_workers();
                panic!("background workers still running: {w:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn state_file(&self, key: &str) -> PathBuf {
        self.alice().join("state").join(format!("{key}.offset"))
    }

    /// The offset the state file `key` holds (its first word).
    fn offset(&self, key: &str) -> Option<u64> {
        let s = std::fs::read_to_string(self.state_file(key)).ok()?;
        s.split_whitespace().next()?.parse().ok()
    }

    /// `git init` in `dir` (made first).
    fn git_repo(&self, dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let out = self
            .command("/usr/bin/git")
            .args(["init", "-q"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    /// Files under `$TMPDIR` (every script cleans its temporary directory up).
    fn tmp_entries(&self) -> Vec<String> {
        std::fs::read_dir(&self.tmp)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_workers();
    }
}

// ---- Transcripts ----

/// A Claude Code transcript, written line by line. Timestamps advance one second per line.
#[derive(Default, Clone)]
struct Transcript {
    bytes: Vec<u8>,
    clock: u64,
    /// A subagent's own file: every line is a sidechain carrying this `agentId`.
    agent_id: Option<String>,
}

impl Transcript {
    fn subagent(agent_id: &str) -> Self {
        Transcript {
            agent_id: Some(agent_id.into()),
            ..Default::default()
        }
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn line(&mut self, mut v: serde_json::Value) -> &mut Self {
        self.clock += 1;
        let c = self.clock;
        v["timestamp"] = json!(format!(
            "2026-10-16T{:02}:{:02}:{:02}.000Z",
            (c / 3600) % 24,
            (c / 60) % 60,
            c % 60
        ));
        v["isSidechain"] = json!(self.agent_id.is_some());
        if let Some(a) = &self.agent_id {
            v["agentId"] = json!(a);
        }
        self.bytes.extend(serde_json::to_vec(&v).unwrap());
        self.bytes.push(b'\n');
        self
    }

    fn prompt(&mut self, uuid: &str, text: &str) -> &mut Self {
        self.line(json!({"type": "user", "uuid": uuid,
            "message": {"role": "user", "content": text}}))
    }

    fn reply(&mut self, uuid: &str, text: &str) -> &mut Self {
        self.line(json!({"type": "assistant", "uuid": uuid,
            "message": {"id": format!("msg-{uuid}"), "type": "message", "role": "assistant",
                        "content": [{"type": "text", "text": text}], "stop_reason": "end_turn"}}))
    }

    /// Turn `i` of session `s`: prompt `<s>-p<i>` and reply `<s>-a<i>`, both naming `marker`.
    fn turn(&mut self, s: &str, i: usize, marker: &str) -> &mut Self {
        self.prompt(
            &format!("{s}-p{i}"),
            &format!("Question {i} about {marker}."),
        )
        .reply(
            &format!("{s}-a{i}"),
            &format!("Answer {i}: {marker} is settled."),
        )
    }
}

/// A session: its transcript file, its working directory, and the hook inputs Claude Code sends.
struct Session {
    id: String,
    path: PathBuf,
    cwd: PathBuf,
    tr: Transcript,
    written: usize,
}

impl Session {
    fn new(env: &Env, id: &str, cwd: &Path) -> Session {
        Self::at(
            id,
            env.root.join("transcripts").join(format!("{id}.jsonl")),
            cwd,
        )
    }

    fn at(id: &str, path: PathBuf, cwd: &Path) -> Session {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(cwd).unwrap();
        Session {
            id: id.into(),
            path,
            cwd: cwd.to_path_buf(),
            tr: Transcript::default(),
            written: 0,
        }
    }

    /// Appends what the transcript gained since the last flush to the file.
    fn flush(&mut self) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .unwrap();
        f.write_all(&self.tr.bytes[self.written..]).unwrap();
        self.written = self.tr.bytes.len();
    }

    /// Turn `i`, appended to the file.
    fn turn(&mut self, i: usize, marker: &str) {
        let id = self.id.clone();
        self.tr.turn(&id, i, marker);
        self.flush();
    }

    fn input(&self, event: &str) -> serde_json::Value {
        let mut v = json!({
            "session_id": self.id,
            "transcript_path": self.path,
            "cwd": self.cwd,
            "hook_event_name": event,
            "permission_mode": "default",
        });
        if event == "SessionStart" {
            v["source"] = json!("startup");
        }
        if event == "Stop" {
            v["stop_hook_active"] = json!(false);
        }
        v
    }

    fn key(&self) -> String {
        format!("claude-code-{}", self.id)
    }
}

fn uri(session: &str) -> String {
    format!("claude-code:session:{session}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// ---- What the tenant saw ----

/// The query of every transcript upload the emulator received for `session`, oldest first.
fn uploads(s: &Server, session: &str) -> Vec<HashMap<String, String>> {
    s.seen("ann")
        .into_iter()
        .filter(|(m, pq, _, _)| m == "POST" && pq.starts_with("/api/v1/capture/transcript?"))
        .map(|(_, pq, _, _)| {
            let q = pq.split_once('?').unwrap().1;
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>()
        })
        .filter(|q| q.get("session").map(String::as_str) == Some(session))
        .collect()
}

/// `(offset, body length)` of every transcript upload for `session`, oldest first.
fn upload_sizes(s: &Server, session: &str) -> Vec<(u64, usize)> {
    s.seen("ann")
        .into_iter()
        .filter(|(m, pq, _, _)| m == "POST" && pq.starts_with("/api/v1/capture/transcript?"))
        .filter_map(|(_, pq, _, body)| {
            let q: HashMap<String, String> =
                url::form_urlencoded::parse(pq.split_once('?').unwrap().1.as_bytes())
                    .into_owned()
                    .collect();
            (q.get("session").map(String::as_str) == Some(session))
                .then(|| (q["offset"].parse().unwrap(), body.len()))
        })
        .collect()
}

/// How many briefing requests the emulator received.
fn briefing_requests(s: &Server) -> usize {
    s.seen("ann")
        .iter()
        .filter(|(m, pq, _, _)| m == "GET" && pq.starts_with("/api/v1/capture/briefing"))
        .count()
}

fn db(home: &AliceHome) -> rusqlite::Connection {
    alice_core::store::open_reader(&home.db()).unwrap()
}

fn count(home: &AliceHome, sql: &str, p: &str) -> i64 {
    db(home).query_row(sql, [p], |r| r.get(0)).unwrap()
}

/// `(project key, project slug)` of the source `uri`.
fn source_project(home: &AliceHome, uri: &str) -> Option<(String, String)> {
    use rusqlite::OptionalExtension;
    db(home)
        .query_row(
            "SELECT p.key, p.slug FROM sources s JOIN projects p ON p.id = s.project_id
             WHERE s.uri = ?1",
            [uri],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
}

/// Turns stored for the source `uri`.
fn turns_of(home: &AliceHome, uri: &str) -> i64 {
    count(
        home,
        "SELECT count(*) FROM capture_turns t JOIN sources s ON s.id = t.source_id
         WHERE s.uri = ?1",
        uri,
    )
}

/// Chunks holding `text`.
fn chunks_holding(home: &AliceHome, text: &str) -> i64 {
    count(
        home,
        "SELECT count(*) FROM chunks WHERE instr(text, ?1) > 0",
        text,
    )
}

/// `(byte_offset, no_distill)` of the source `uri`.
fn capture_row(home: &AliceHome, uri: &str) -> Option<(u64, bool)> {
    use rusqlite::OptionalExtension;
    db(home)
        .query_row(
            "SELECT c.byte_offset, c.no_distill
             FROM capture_state c JOIN sources s ON s.id = c.source_id WHERE s.uri = ?1",
            [uri],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? != 0)),
        )
        .optional()
        .unwrap()
}

/// Every file under `dir` whose bytes hold `needle`, as paths relative to `dir`.
fn files_holding(dir: &Path, needle: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file()
                && let Ok(bytes) = std::fs::read(&p)
                && bytes.windows(needle.len()).any(|w| w == needle)
            {
                found.push(p.strip_prefix(dir).unwrap().display().to_string());
            }
        }
    }
    found
}

// ---- Setup ----

#[tokio::test(flavor = "multi_thread")]
async fn setup_installs_conf_script_and_plugin() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    let url = s.url("ann");
    let token = s.tenant("ann").token.clone();
    let r = env.setup(
        &url,
        &format!("{token}\n"),
        &[("ALICE_SETUP_TOKEN_STDIN", "1")],
    );
    assert!(r.ok(), "{r}");
    let dir = env.alice();
    assert_eq!(mode(&dir), 0o700);
    for f in ["token", "url", "capture.conf"] {
        assert_eq!(mode(&dir.join(f)), 0o600, "{f}");
    }
    assert_eq!(read(&dir.join("token")), format!("{token}\n"));
    assert_eq!(read(&dir.join("url")), format!("{url}\n"));
    assert_eq!(read(&dir.join("capture.conf")), "briefing=on\n");
    assert_eq!(mode(&env.script()), 0o700);
    assert_eq!(
        read(&env.script()),
        alice_daemon::http::capture_hook_text(),
        "the hook script is the tenant's own copy, as served"
    );
    // The install counts as the day's update check.
    let checked: u64 = read(&env.checked_file()).trim().parse().unwrap();
    assert!(now_secs().abs_diff(checked) < 60, "{checked}");

    let mp = env.marketplace();
    let json_of = |p: PathBuf| -> serde_json::Value {
        serde_json::from_str(&read(&p)).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    };
    let market = json_of(mp.join(".claude-plugin/marketplace.json"));
    assert_eq!(market["name"], "alice-local");
    assert_eq!(market["plugins"][0]["name"], "alice-capture");
    assert_eq!(market["plugins"][0]["source"], "./alice-capture");
    let plugin = json_of(mp.join("alice-capture/.claude-plugin/plugin.json"));
    assert_eq!(plugin["name"], "alice-capture");
    assert_eq!(
        plugin["version"],
        env!("CARGO_PKG_VERSION"),
        "the plugin is at the tenant's version"
    );
    let hooks = json_of(mp.join("alice-capture/hooks/hooks.json"));
    let events = hooks["hooks"].as_object().unwrap();
    let mut names: Vec<&str> = events.keys().map(String::as_str).collect();
    names.sort();
    let mut five = FIVE_EVENTS.to_vec();
    five.sort();
    assert_eq!(names, five);
    for (event, entries) in events {
        let h = &entries[0]["hooks"][0];
        assert_eq!(h["type"], "command", "{event}");
        assert_eq!(h["command"], HOOK_COMMAND, "{event}");
    }
    // One generator: the file is exactly what the installed script prints.
    let printed = env.capture_sh(&["print-hooks-json", "claude-code"], &[]);
    assert!(printed.ok(), "{printed}");
    assert_eq!(
        read(&mp.join("alice-capture/hooks/hooks.json")),
        printed.stdout
    );

    let calls = env.claude_calls();
    assert!(
        calls
            .lines()
            .any(|l| l == format!("plugin marketplace add {}", mp.display())),
        "{calls}"
    );
    assert!(
        calls
            .lines()
            .any(|l| l == "plugin install alice-capture@alice-local --scope user"),
        "{calls}"
    );
    assert!(r.stdout.contains("distillation: off"), "{r}");
    assert!(!r.stdout.contains(&token) && !r.stderr.contains(&token));
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_reuses_an_existing_token_file_without_prompting() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    let url = s.url("ann");
    env.write_credentials(&url, &s.tenant("ann").token);
    // Left looser than they should be: setup tightens the modes and keeps the bytes.
    std::fs::set_permissions(env.alice(), std::fs::Permissions::from_mode(0o755)).unwrap();
    for f in ["token", "url"] {
        std::fs::set_permissions(env.alice().join(f), std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    let before = (
        std::fs::read(env.alice().join("token")).unwrap(),
        std::fs::read(env.alice().join("url")).unwrap(),
    );
    // The one pasted line, exactly as a person runs it: the script comes from the tenant, and
    // the shell reads it from the pipe (so nothing may read the token from stdin).
    let mut c = env.command(&env.sh());
    c.arg("-c").arg(format!(
        "curl -fsSL '{url}/api/v1/capture/setup.sh' | sh -s -- '{url}'"
    ));
    let r = env.run(c, b"", Duration::from_secs(60));
    assert!(r.ok(), "{r}");
    assert!(
        !r.stderr.contains(PROMPT) && !r.stdout.contains(PROMPT),
        "{r}"
    );
    let after = (
        std::fs::read(env.alice().join("token")).unwrap(),
        std::fs::read(env.alice().join("url")).unwrap(),
    );
    assert_eq!(before, after, "both files are left byte-identical");
    assert_eq!(mode(&env.alice()), 0o700);
    assert_eq!(mode(&env.alice().join("token")), 0o600);
    assert_eq!(mode(&env.alice().join("url")), 0o600);
    assert!(env.script().exists());
    assert!(
        env.claude_calls()
            .contains("plugin install alice-capture@alice-local --scope user")
    );

    // Another address on the command line: said, and the file wins.
    let r = env.setup("https://elsewhere.example/u/ann", "", &[]);
    assert!(r.ok(), "{r}");
    assert!(r.stdout.contains("https://elsewhere.example/u/ann"), "{r}");
    assert!(r.stdout.contains("keeping"), "{r}");
    assert_eq!(std::fs::read(env.alice().join("url")).unwrap(), before.1);
    assert!(!r.stderr.contains(PROMPT), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_prompts_only_when_the_token_file_is_missing_or_refused() {
    let s = Server::start(&["ann"]).await;
    let url = s.url("ann");
    let token = s.tenant("ann").token.clone();
    let stdin = [("ALICE_SETUP_TOKEN_STDIN", "1")];

    // Missing: one prompt, and the answer goes to the file.
    let env = Env::new();
    env.write_credentials(&url, &token);
    std::fs::remove_file(env.alice().join("token")).unwrap();
    let r = env.setup(&url, &format!("{token}\n"), &stdin);
    assert!(r.ok(), "{r}");
    assert_eq!(r.stderr.matches(PROMPT).count(), 1, "{r}");
    assert_eq!(read(&env.alice().join("token")), format!("{token}\n"));
    assert_eq!(mode(&env.alice().join("token")), 0o600);
    assert!(!r.stderr.contains(&token) && !r.stdout.contains(&token));

    // Refused: said, one prompt, and the file is replaced.
    let env = Env::new();
    env.write_credentials(&url, &"0".repeat(64));
    let r = env.setup(&url, &format!("{token}\n"), &stdin);
    assert!(r.ok(), "{r}");
    assert_eq!(r.stderr.matches(PROMPT).count(), 1, "{r}");
    assert!(
        format!("{}{}", r.stdout, r.stderr).contains("refused"),
        "{r}"
    );
    assert_eq!(read(&env.alice().join("token")), format!("{token}\n"));
    let leftovers: Vec<_> = std::fs::read_dir(env.alice())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("token") && n != "token")
        .collect();
    assert!(leftovers.is_empty(), "no second copy: {leftovers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_rejects_a_wrong_token_and_writes_nothing() {
    let s = Server::start(&["ann"]).await;
    let url = s.url("ann");
    let env = Env::new();
    let r = env.setup(
        &url,
        &format!("{}\n", "f".repeat(64)),
        &[("ALICE_SETUP_TOKEN_STDIN", "1")],
    );
    assert!(!r.ok(), "{r}");
    assert_eq!(r.stderr.matches(PROMPT).count(), 1, "asked once: {r}");
    assert!(!env.alice().join("token").exists());
    assert!(!env.alice().join("url").exists());
    assert!(!env.script().exists());
    assert!(!env.marketplace().exists());
    assert!(env.claude_calls().is_empty(), "no plugin");
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());

    // Addresses the token must never travel to are refused before anything is sent.
    let before = s.seen("ann").len();
    for bad in [
        "http://example.com/u/ann",
        "http://127.0.0.1:80@example.com/u/ann",
        "http://localhost.example.com/u/ann",
        "ftp://127.0.0.1/u/ann",
        "https://exa mple.com/u/ann",
    ] {
        let env = Env::new();
        let r = env.setup(bad, "x\n", &[("ALICE_SETUP_TOKEN_STDIN", "1")]);
        assert!(!r.ok(), "{bad}: {r}");
        assert!(r.stderr.contains("https://"), "{bad}: {r}");
        assert!(!env.alice().join("url").exists(), "{bad}");
        assert!(
            !r.stderr.contains(PROMPT),
            "{bad}: refused before the prompt"
        );
    }
    assert_eq!(s.seen("ann").len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_prints_the_plugin_commands_when_claude_fails() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(
        &env.bin.join("claude"),
        b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/claude-calls\"\nexit 1\n",
        0o755,
    );
    env.install(&s);
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(
        r.stdout.contains(&format!(
            "/plugin marketplace add \"{}\"",
            env.marketplace().display()
        )),
        "{r}"
    );
    assert!(
        r.stdout
            .contains("/plugin install alice-capture@alice-local"),
        "{r}"
    );
    // No `claude` at all: the same two commands.
    std::fs::remove_file(env.bin.join("claude")).unwrap();
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(
        r.stdout
            .contains("/plugin install alice-capture@alice-local"),
        "{r}"
    );
}

/// The command every Codex hook runs, as the `hooks.json` setup writes must hold it.
const CODEX_HOOK_COMMAND: &str = r#"sh "$HOME/.config/alice/alice-capture.sh" hook codex || true"#;

/// The parsed `$CODEX_HOME/hooks.json`.
fn codex_hooks(env: &Env) -> serde_json::Value {
    serde_json::from_str(&read(&env.codex_home.join("hooks.json"))).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_handles_codex_hooks_json() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.write_credentials(&s.url("ann"), &s.tenant("ann").token);
    let hooks = env.codex_home.join("hooks.json");

    // Absent: setup writes it, with the five events, and reminds the person to trust it.
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(!r.stdout.contains("later version"), "{r}");
    assert!(r.stdout.contains("/hooks"), "the trust reminder: {r}");
    assert_eq!(mode(&hooks), 0o600);
    let written = codex_hooks(&env);
    for event in FIVE_EVENTS {
        let entry = &written["hooks"][event][0]["hooks"][0];
        assert_eq!(entry["type"], "command", "{event}");
        assert_eq!(entry["command"], CODEX_HOOK_COMMAND, "{event}");
        assert!(entry["timeout"].as_u64().unwrap() <= 10, "{event}");
    }
    assert_eq!(written["hooks"].as_object().unwrap().len(), 5);
    assert_eq!(
        written["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3,
        "SessionEnd allows at most 3 s"
    );
    let first = read(&hooks);

    // Again: the file is still setup's own, so it is updated in place, byte for byte.
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert_eq!(read(&hooks), first);
    assert!(
        r.stdout.contains("Codex: wrote"),
        "updated in place, not left as the person's: {r}"
    );
    assert!(!r.stdout.contains("already exists"), "{r}");

    // Uninstall removes a hooks.json that is still exactly the one setup wrote.
    let r = env.capture_sh(&["uninstall"], &[("ALICE_SETUP_ASSUME_YES", "1")]);
    assert!(r.ok(), "{r}");
    assert!(!hooks.exists(), "{r}");

    // Present and the person's own: left untouched, the entries are printed to add.
    let own = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo mine"}]}]}}"#;
    write_mode(&hooks, own.as_bytes(), 0o600);
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert_eq!(read(&hooks), own, "an existing hooks.json is never edited");
    assert!(r.stdout.contains("already exists"), "{r}");
    assert!(
        r.stdout
            .contains(r#"alice-capture.sh\" hook codex || true"#),
        "{r}"
    );
    assert!(r.stdout.contains("/hooks"), "{r}");
    // And uninstall keeps a file that is not the one setup wrote.
    let r = env.capture_sh(&["uninstall"], &[("ALICE_SETUP_ASSUME_YES", "1")]);
    assert!(r.ok(), "{r}");
    assert_eq!(read(&hooks), own);

    // Inline `[hooks]` tables in config.toml are the person's format for this layer: hooks.json
    // is not written beside them.
    std::fs::remove_file(&hooks).unwrap();
    std::fs::write(
        env.codex_home.join("config.toml"),
        "model = \"x\"\n\n[hooks]\n",
    )
    .unwrap();
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(!hooks.exists(), "{r}");
    assert!(
        r.stdout
            .contains(r#"alice-capture.sh\" hook codex || true"#),
        "{r}"
    );

    // No `codex` command on PATH: Codex is not mentioned and nothing is written.
    std::fs::remove_file(env.codex_home.join("config.toml")).unwrap();
    std::fs::remove_file(env.bin.join("codex")).unwrap();
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(!r.stdout.contains("Codex"), "{r}");
    assert_eq!(std::fs::read_dir(&env.codex_home).unwrap().count(), 0);
}

/// A person who edits the `hooks.json` setup wrote must never lose the edit: a re-run of setup
/// leaves the file alone (and says the Alice entries are in it), and uninstall keeps it. Each
/// guard is pinned on its own: the edited file stays while `codex-hooks.written` still exists.
#[tokio::test(flavor = "multi_thread")]
async fn an_edited_codex_hooks_json_is_kept_by_setup_and_uninstall() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.write_credentials(&s.url("ann"), &s.tenant("ann").token);
    let hooks = env.codex_home.join("hooks.json");
    let copy = env.alice().join("codex-hooks.written");

    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(r.stdout.contains("Codex: wrote"), "{r}");
    let original = read(&hooks);
    assert_eq!(read(&copy), original, "setup records what it wrote");

    // The person adds an entry of their own.
    let edited = original.replacen(
        "\"hooks\": {",
        "\"hooks\": {\n    \"PostToolUse\": [ { \"hooks\": [ { \"type\": \"command\", \"command\": \"echo mine\" } ] } ],",
        1,
    );
    assert_ne!(edited, original);
    write_mode(&hooks, edited.as_bytes(), 0o600);

    // Setup again: byte-identical, and it says why. The Alice entries are in the file, so it
    // must not invite a second copy of them.
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert_eq!(
        read(&hooks),
        edited,
        "setup never overwrites an edited file"
    );
    assert!(r.stdout.contains("already exists"), "{r}");
    assert!(!r.stdout.contains("Codex: wrote"), "{r}");
    assert!(
        !r.stdout.contains("Add these hook entries"),
        "the Alice entries are in the file already: {r}"
    );
    assert!(
        r.stdout.contains("already has the Alice capture entries"),
        "{r}"
    );
    assert_eq!(read(&copy), original, "the record is of what setup wrote");

    // Uninstall: the record still exists, the file differs from it, so it is kept.
    let r = env.capture_sh(&["uninstall"], &[("ALICE_SETUP_ASSUME_YES", "1")]);
    assert!(r.ok(), "{r}");
    assert_eq!(read(&hooks), edited, "uninstall keeps an edited file");
    assert!(r.stdout.contains("has changed"), "{r}");
}

/// The record of the written file is made before `hooks.json` is touched: a record that cannot be
/// written is not a failed `hooks.json` write, and leaves no file that later runs would take for
/// the person's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_record_that_cannot_be_written_leaves_hooks_json_unwritten() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.write_credentials(&s.url("ann"), &s.tenant("ann").token);
    let hooks = env.codex_home.join("hooks.json");
    // Where the record is staged: a directory in the way makes the write fail.
    let stage = env.alice().join("codex-hooks.written.new");
    std::fs::create_dir_all(&stage).unwrap();

    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(!hooks.exists(), "hooks.json is not written: {r}");
    assert!(
        !r.stdout
            .contains(&format!("could not write {}", hooks.display())),
        "it must not say hooks.json could not be written: {r}"
    );
    assert!(r.stdout.contains("could not write the record"), "{r}");
    assert!(
        r.stdout.contains("codex-hooks.written"),
        "it names the record: {r}"
    );
    assert!(
        r.stdout.contains("hook codex"),
        "the entries are printed: {r}"
    );
    assert!(!env.alice().join("codex-hooks.written").exists());

    // The way cleared, the next run writes both.
    std::fs::remove_dir(&stage).unwrap();
    let r = env.setup(&s.url("ann"), "", &[]);
    assert!(r.ok(), "{r}");
    assert!(r.stdout.contains("Codex: wrote"), "{r}");
    assert_eq!(read(&hooks), read(&env.alice().join("codex-hooks.written")));
}

/// Every TOML spelling of a `[hooks]` table keeps hooks.json from being written beside it
/// (spec section 2.2: one format per layer); other keys and tables do not.
#[tokio::test(flavor = "multi_thread")]
async fn setup_finds_every_toml_form_of_inline_hooks() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.write_credentials(&s.url("ann"), &s.tenant("ann").token);
    let hooks = env.codex_home.join("hooks.json");
    let copy = env.alice().join("codex-hooks.written");
    let config = env.codex_home.join("config.toml");
    let forms: [(&str, bool); 15] = [
        ("[hooks]\n", true),
        ("[ hooks ]\n", true),
        ("  [hooks.Stop]\n", true),
        ("[[hooks.Stop]]\n", true),
        ("[[ hooks.Stop ]]\n", true),
        ("[ hooks . Stop ]\n", true),
        ("[\"hooks\"]\n", true),
        ("['hooks'.Stop]\n", true),
        ("hooks.Stop = [ { hooks = [] } ]\n", true),
        ("hooks . Stop = []\n", true),
        ("\"hooks\" = { Stop = [] }\n", true),
        ("hooks = { Stop = [] }\n", true),
        (
            "[hooks_extra]\nhooks_dir = \"x\"\n# [hooks]\n[other.hooks]\n",
            false,
        ),
        ("model = \"hooks\"\n[hookstop]\n", false),
        ("", false),
    ];
    for (toml, inline) in forms {
        let _ = std::fs::remove_file(&hooks);
        let _ = std::fs::remove_file(&copy);
        std::fs::write(&config, toml).unwrap();
        let r = env.setup(&s.url("ann"), "", &[]);
        assert!(r.ok(), "{toml:?}: {r}");
        assert_eq!(!hooks.exists(), inline, "{toml:?}: {r}");
        if inline {
            assert!(r.stdout.contains("config.toml"), "{toml:?}: {r}");
        }
    }
}

/// `status` tells where the Codex hooks stand (spec section 5.4 step 9 points people to it).
#[tokio::test(flavor = "multi_thread")]
async fn status_reports_the_codex_hooks() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.install(&s);
    let hooks = env.codex_home.join("hooks.json");
    let status = |env: &Env| env.capture_sh(&["status"], &[]).stdout;
    let out = status(&env);
    assert!(out.contains("codex hooks: installed"), "{out}");
    assert!(out.contains("/hooks"), "the trust reminder: {out}");

    let mut text = read(&hooks);
    text.push('\n');
    std::fs::write(&hooks, &text).unwrap();
    let out = status(&env);
    assert!(out.contains("codex hooks: in"), "{out}");
    assert!(out.contains("changed"), "{out}");

    std::fs::write(&hooks, r#"{"hooks":{}}"#).unwrap();
    let out = status(&env);
    assert!(out.contains("codex hooks: not found"), "{out}");

    std::fs::remove_file(&hooks).unwrap();
    let out = status(&env);
    assert!(out.contains("codex hooks: not found"), "{out}");
}

/// A Codex rollout of `turns` question/answer pairs, in the shape of the sampled format.
fn rollout(sid: &str, turns: &[&str]) -> String {
    let mut out = format!(
        "{}\n",
        json!({"timestamp": "2026-10-16T01:00:00.000Z", "type": "session_meta",
               "payload": {"id": sid, "session_id": sid, "cwd": "/home/user/project-x"}})
    );
    for (i, marker) in turns.iter().enumerate() {
        let item = |role: &str, kind: &str, block: &str, text: String, second: usize| {
            format!(
                "{}\n",
                json!({"timestamp": format!("2026-10-16T01:{:02}:{:02}.000Z", i + 1, second),
                       "type": "response_item",
                       "payload": {"type": "message", "id": format!("m{i}{second}"), "role": role,
                                   "content": [{"type": block, "text": text}],
                                   "internal_chat_message_metadata_passthrough":
                                       {"content_item_kinds": [kind]}}})
            )
        };
        out += &item(
            "user",
            "user.text",
            "input_text",
            format!("Question {i} about {marker}."),
            1,
        );
        out += &item(
            "assistant",
            "unknown",
            "output_text",
            format!("Answer {i}: {marker} is settled."),
            2,
        );
    }
    out
}

impl Env {
    /// The Codex hook command, with `input` on stdin.
    fn codex_hook(&self, input: &serde_json::Value) -> Run {
        let mut c = self.command(&self.sh());
        c.arg("-c").arg(CODEX_HOOK_COMMAND);
        let r = self.run(
            c,
            &serde_json::to_vec(input).unwrap(),
            Duration::from_secs(30),
        );
        assert!(r.ok(), "a hook must always exit 0: {r}");
        assert!(r.stderr.is_empty(), "a hook writes nothing to stderr: {r}");
        r
    }

    /// A rollout under `$CODEX_HOME/sessions/<day>/`.
    fn write_rollout(&self, day: &str, sid: &str, text: &str) -> PathBuf {
        let dir = self.codex_home.join("sessions").join(day);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-10-16T01-02-03-{sid}.jsonl"));
        std::fs::write(&path, text).unwrap();
        path
    }
}

fn codex_input(sid: &str, path: serde_json::Value, cwd: &Path, event: &str) -> serde_json::Value {
    json!({"session_id": sid, "transcript_path": path, "cwd": cwd, "hook_event_name": event})
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_captures_a_codex_session_and_finds_a_null_transcript_path() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.install(&s);
    let work = env.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let codex_uri = |sid: &str| format!("codex:session:{sid}");

    // `transcript_path` is null: the script finds the rollout by its session id.
    let sid = "019f0000-aaaa-7000-8000-000000000001";
    let text = rollout(sid, &["firstquokka", "secondquokka"]);
    env.write_rollout("2026/10/16", sid, &text);
    // A rollout of another session in the same folder is not this one's.
    env.write_rollout(
        "2026/10/16",
        "019f0000-bbbb-7000-8000-000000000002",
        &rollout("other", &["otherquokka"]),
    );
    let r = env.codex_hook(&codex_input(sid, json!(null), &work, "SessionStart"));
    assert!(r.stdout.is_empty(), "no briefing is printed for Codex: {r}");
    let r = env.codex_hook(&codex_input(sid, json!(null), &work, "Stop"));
    assert!(r.stdout.is_empty(), "{r}");
    env.wait_idle();
    let key = format!("codex-{sid}");
    assert_eq!(env.offset(&key), Some(text.len() as u64), "{}", env.log());
    assert_eq!(turns_of(&t.home, &codex_uri(sid)), 2, "{}", env.log());
    for marker in ["firstquokka", "secondquokka"] {
        assert_eq!(chunks_holding(&t.home, marker), 1, "{marker}");
    }
    assert_eq!(chunks_holding(&t.home, "otherquokka"), 0);
    assert_eq!(
        turns_of(&t.home, &uri(sid)),
        0,
        "no Claude Code source is made"
    );
    let ups = uploads(&s, sid);
    assert!(!ups.is_empty(), "{ups:?}");
    assert!(ups.iter().all(|q| q["agent"] == "codex"), "{ups:?}");

    // A string path is used as it is, and the file growing is sent from the stored offset.
    let sid2 = "019f0000-cccc-7000-8000-000000000003";
    let elsewhere = env.root.join("transcripts").join("custom-rollout.jsonl");
    let mut text2 = rollout(sid2, &["thirdquokka"]);
    std::fs::write(&elsewhere, &text2).unwrap();
    env.codex_hook(&codex_input(sid2, json!(elsewhere), &work, "Stop"));
    env.wait_idle();
    assert_eq!(turns_of(&t.home, &codex_uri(sid2)), 1, "{}", env.log());
    let more = rollout(sid2, &["fourthquokka"]);
    let more = more.split_once('\n').unwrap().1; // without a second session_meta
    text2.push_str(more);
    std::fs::write(&elsewhere, &text2).unwrap();
    env.codex_hook(&codex_input(sid2, json!(elsewhere), &work, "SessionEnd"));
    env.wait_idle();
    assert_eq!(turns_of(&t.home, &codex_uri(sid2)), 2, "{}", env.log());
    assert_eq!(chunks_holding(&t.home, "thirdquokka"), 1);

    // Only the newest seven day folders are searched.
    let old = "019f0000-dddd-7000-8000-000000000004";
    env.write_rollout("2026/09/01", old, &rollout(old, &["oldquokka"]));
    for day in 10..=16 {
        let other = format!("019f0000-eeee-7000-8000-0000000000{day}");
        env.write_rollout(&format!("2026/10/{day}"), &other, &rollout(&other, &["x"]));
    }
    env.codex_hook(&codex_input(old, json!(null), &work, "Stop"));
    env.wait_idle();
    assert!(uploads(&s, old).is_empty());
    assert!(env.log().contains(old), "{}", env.log());
    // ... and the seventh newest is searched: a session whose rollout is only there is found.
    let seventh = "019f0000-9999-7000-8000-000000000007";
    env.write_rollout("2026/10/10", seventh, &rollout(seventh, &["seventhquokka"]));
    env.codex_hook(&codex_input(seventh, json!(null), &work, "Stop"));
    env.wait_idle();
    assert_eq!(chunks_holding(&t.home, "seventhquokka"), 1, "{}", env.log());

    // A session id is matched whole: a piece of another session's id finds nothing.
    for piece in ["7000", "aaaa-7000", "019f0000-aaaa"] {
        env.codex_hook(&codex_input(piece, json!(null), &work, "Stop"));
    }
    env.wait_idle();
    for piece in ["7000", "aaaa-7000", "019f0000-aaaa"] {
        assert!(uploads(&s, piece).is_empty(), "{piece}");
        assert!(env.offset(&format!("codex-{piece}")).is_none(), "{piece}");
    }

    // A compressed rollout is counted in the log and never read or sent.
    let zst = env.root.join("transcripts").join("rollout-z.jsonl.zst");
    std::fs::write(&zst, [0x28, 0xb5, 0x2f, 0xfd, 0, 1, 2]).unwrap();
    let sid3 = "019f0000-ffff-7000-8000-000000000005";
    env.codex_hook(&codex_input(sid3, json!(zst), &work, "Stop"));
    env.wait_idle();
    assert!(uploads(&s, sid3).is_empty());
    assert!(env.log().contains("zst"), "{}", env.log());
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());
}

/// A Codex state whose recorded rollout is gone is not paired with another file: the sweep
/// does not look the session up again, so the stored offset never meets a different file.
#[tokio::test(flavor = "multi_thread")]
async fn a_sweep_does_not_pair_a_vanished_codex_rollout_with_another_file() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.install(&s);
    let work = env.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let sid = "019f0000-1111-7000-8000-000000000011";
    let text = rollout(sid, &["vanishedquokka", "otherquokka"]);
    env.write_rollout("2026/10/16", sid, &text);
    std::fs::write(
        env.state_file(&format!("codex-{sid}")),
        format!(
            "50 {}/gone/rollout.jsonl\nsession {sid}\ncwd {}\n",
            env.root.display(),
            work.display()
        ),
    )
    .unwrap();
    // The state is older than the rollout, as it would be had the file been moved since.
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(env.state_file(&format!("codex-{sid}")))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let other = "019f0000-2222-7000-8000-000000000022";
    env.write_rollout("2026/10/16", other, &rollout(other, &["sweeperquokka"]));
    env.codex_hook(&codex_input(other, json!(null), &work, "SessionStart"));
    env.wait_idle();
    assert!(uploads(&s, sid).is_empty(), "{}", env.log());
    assert_eq!(env.offset(&format!("codex-{sid}")), Some(50));
}

// ---- The token ----

/// What the `ps` sampler found.
#[derive(Default)]
struct Samples {
    samples: usize,
    leaks: Vec<String>,
    saw: Vec<bool>,
}

/// Samples every process's argv (`ps -A -ww -o args=`, and on Linux also each readable
/// `/proc/<pid>/cmdline` and `environ`) every 5 ms until stopped.
fn sampler(
    token: String,
    watch: Vec<String>,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<Samples>) {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut found = Samples {
            saw: vec![false; watch.len()],
            ..Default::default()
        };
        while !flag.load(Ordering::SeqCst) {
            if let Ok(out) = Command::new("/bin/ps")
                .args(["-A", "-ww", "-o", "args="])
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                found.samples += 1;
                for line in text.lines().filter(|l| l.contains(&token)) {
                    found.leaks.push(line.replace(&token, "<TOKEN>"));
                }
                for (i, w) in watch.iter().enumerate() {
                    found.saw[i] |= text.contains(w.as_str());
                }
            }
            #[cfg(target_os = "linux")]
            if let Ok(dir) = std::fs::read_dir("/proc") {
                for e in dir.flatten() {
                    for f in ["cmdline", "environ"] {
                        if let Ok(b) = std::fs::read(e.path().join(f))
                            && b.windows(token.len()).any(|w| w == token.as_bytes())
                        {
                            found.leaks.push(format!("{} of {}", f, e.path().display()));
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        found
    });
    (stop, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_token_never_appears_in_argv_or_logs() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    let url = s.url("ann");
    let token = s.tenant("ann").token.clone();
    // A person's ~/.curlrc that traces every request would catch the Authorization header,
    // unless every curl call skips it (`curl -q`).
    let trace = env.home.join("curl-trace.txt");
    std::fs::write(
        env.home.join(".curlrc"),
        format!("trace-ascii = \"{}\"\n", trace.display()),
    )
    .unwrap();
    // Every request takes at least 250 ms, so each curl process lives long enough for the
    // sampler (one `ps` every ~5-40 ms) to see its argv.
    s.delay_requests(250);
    let watch = vec![
        format!("{} run ", env.script().display()),
        format!("{url}/api/v1/capture/status"),
        format!("{url}/api/v1/capture/transcript?"),
        format!("{url}/api/v1/capture/briefing"),
    ];
    let (stop, handle) = sampler(token.clone(), watch.clone());
    // The prompt path: the token comes in on stdin and goes to the file.
    let r = env.setup(
        &url,
        &format!("{token}\n"),
        &[("ALICE_SETUP_TOKEN_STDIN", "1")],
    );
    assert!(r.ok(), "{r}");
    let mut sess = Session::new(&env, "argv-check", &env.root.join("work"));
    sess.turn(1, "nothing secret here");
    env.hook(&sess.input("SessionStart"));
    sess.turn(2, "still nothing secret");
    env.hook(&sess.input("Stop"));
    env.hook(&sess.input("SessionEnd"));
    env.wait_idle();
    stop.store(true, Ordering::SeqCst);
    let found = handle.join().unwrap();

    assert!(found.leaks.is_empty(), "{:?}", found.leaks);
    assert!(
        found.samples >= 20,
        "the sampler ran {} times",
        found.samples
    );
    for (seen, what) in found.saw.iter().zip(&watch) {
        assert!(
            seen,
            "the sampler never saw `{what}`, so it did not cover that process"
        );
    }
    assert_eq!(turns_of(&s.tenant("ann").home, &uri("argv-check")), 2);
    assert!(!env.log().contains(&token));
    assert!(!read(&env.alice().join("capture.conf")).contains(&token));
    assert_eq!(
        files_holding(&env.home, token.as_bytes()),
        vec![".config/alice/token".to_string()],
        "the token is in the token file only"
    );
    assert!(!trace.exists(), "~/.curlrc was read");
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());
}

// ---- Capture ----

#[tokio::test(flavor = "multi_thread")]
async fn a_transcript_grown_in_steps_is_captured_once() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "grown", &env.root.join("work"));
    for (i, marker) in ["firstquokka", "secondquokka", "thirdquokka"]
        .iter()
        .enumerate()
    {
        sess.turn(i + 1, marker);
        env.hook(&sess.input("Stop"));
    }
    env.wait_idle();
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
    assert_eq!(turns_of(&t.home, &uri("grown")), 3);
    for marker in ["firstquokka", "secondquokka", "thirdquokka"] {
        assert_eq!(chunks_holding(&t.home, marker), 1, "{marker}");
    }
    assert_eq!(
        capture_row(&t.home, &uri("grown")).unwrap().0,
        sess.tr.len()
    );
    let ups = uploads(&s, "grown");
    assert!(!ups.is_empty() && ups.len() <= 3, "{ups:?}");
    for q in &ups {
        assert_eq!(q["agent"], "claude-code");
        assert_eq!(q["event"], "stop");
        assert!(!q.contains_key("project"), "the hint travels as a header");
    }
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_state_file_resyncs_with_one_409() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "lost", &env.root.join("work"));
    sess.turn(1, "before the loss");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    let first = sess.tr.len();
    assert_eq!(env.offset(&sess.key()), Some(first));
    let sent_before = uploads(&s, "lost").len();

    std::fs::remove_file(env.state_file(&sess.key())).unwrap();
    sess.turn(2, "after the loss");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    let ups = uploads(&s, "lost")[sent_before..].to_vec();
    let offsets: Vec<&str> = ups.iter().map(|q| q["offset"].as_str()).collect();
    assert_eq!(
        offsets,
        vec!["0".to_string(), first.to_string()],
        "one upload from 0 (answered 409), then one from the server's cursor"
    );
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
    assert_eq!(turns_of(&t.home, &uri("lost")), 2);
    assert_eq!(chunks_holding(&t.home, "before the loss"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_down_keeps_the_offset_and_session_start_sweeps() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut a = Session::new(&env, "down-a", &env.root.join("work"));
    a.turn(1, "while the server was down");
    a.turn(2, "still down");
    s.pause(true);
    env.hook(&a.input("Stop"));
    env.wait_idle();
    assert_eq!(uploads(&s, "down-a").len(), 1, "tried once");
    assert_eq!(env.offset(&a.key()), Some(0), "the offset is kept");
    assert!(source_project(&t.home, &uri("down-a")).is_none());
    assert!(env.log().contains("502"), "{}", env.log());

    s.pause(false);
    let mut b = Session::new(&env, "down-b", &env.root.join("work"));
    b.turn(1, "the next session");
    env.hook(&b.input("SessionStart"));
    env.wait_idle();
    assert_eq!(
        env.offset(&a.key()),
        Some(a.tr.len()),
        "the sweep finished it"
    );
    assert_eq!(turns_of(&t.home, &uri("down-a")), 2);
    let swept = uploads(&s, "down-a");
    assert_eq!(swept.last().unwrap()["event"], "sweep");
    assert_eq!(turns_of(&t.home, &uri("down-b")), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_409_beyond_the_end_of_the_file_stops_the_session() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    // The server already holds a longer copy of the session than this machine's file.
    let mut longer = Transcript::default();
    longer
        .turn("past", 1, "one")
        .turn("past", 2, "two")
        .turn("past", 3, "three");
    let r = reqwest::Client::new()
        .post(format!(
            "{}/api/v1/capture/transcript?agent=claude-code&session=past&offset=0&event=stop",
            s.url("ann")
        ))
        .bearer_auth(&t.token)
        .body(longer.bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut sess = Session::new(&env, "past", &env.root.join("work"));
    sess.turn(1, "a shorter local file");
    assert!(sess.tr.len() < longer.len());
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    let ups = uploads(&s, "past");
    assert_eq!(ups.len(), 2, "the test's own upload and one from the hook");
    let log = env.log();
    assert_eq!(
        log.matches(
            "the server's cursor is past this file's end; capture stopped for this session"
        )
        .count(),
        1,
        "{log}"
    );
    // Stopped for good: the file grows, and nothing more is sent.
    sess.turn(2, "more");
    sess.turn(3, "and more");
    sess.turn(4, "past the server's cursor now");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(uploads(&s, "past").len(), 2, "{:?}", uploads(&s, "past"));
    assert_eq!(turns_of(&t.home, &uri("past")), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_consumed_nothing_waits_for_growth() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let cwd = env.root.join("work");
    let mut main = Session::new(&env, "waits", &cwd);
    main.turn(1, "the main session");
    // A subagent too short to judge yet: the server keeps the client at offset 0.
    let sub_path = env
        .root
        .join("transcripts/waits/subagents/agent-agent9.jsonl");
    std::fs::create_dir_all(sub_path.parent().unwrap()).unwrap();
    let mut tiny = Transcript::subagent("agent9");
    tiny.prompt("waits-agent9-p", "Suggest a title.")
        .reply("waits-agent9-a", "Fine.");
    std::fs::write(&sub_path, &tiny.bytes).unwrap();
    let sub_input = || {
        let mut v = main.input("SubagentStop");
        v["agent_id"] = json!("agent9");
        v["agent_type"] = json!("general-purpose");
        v["agent_transcript_path"] = json!(sub_path);
        v
    };
    env.hook(&sub_input());
    env.wait_idle();
    let sub_ups = || {
        uploads(&s, "waits")
            .into_iter()
            .filter(|q| q.get("subagent").map(String::as_str) == Some("agent9"))
            .count()
    };
    assert_eq!(sub_ups(), 1);
    assert_eq!(env.offset("claude-code-waits-agent9"), Some(0));

    // Not sent again until the file grows: neither by its own hook nor by a sweep.
    env.hook(&sub_input());
    env.wait_idle();
    let other = Session::new(&env, "waits-other", &cwd);
    env.hook(&other.input("SessionStart"));
    env.wait_idle();
    assert_eq!(sub_ups(), 1, "{:?}", uploads(&s, "waits"));

    // It grows: sent, and captured whole.
    tiny.prompt("waits-agent9-p2", "Now explain the choice.")
        .reply(
            "waits-agent9-a2",
            &"The title names the feature and the reason it exists, nothing more. ".repeat(3),
        );
    std::fs::write(&sub_path, &tiny.bytes).unwrap();
    env.hook(&sub_input());
    env.wait_idle();
    assert_eq!(sub_ups(), 2);
    assert_eq!(
        turns_of(&t.home, "claude-code:session:waits:agent:agent9"),
        2
    );
    assert_eq!(env.offset("claude-code-waits-agent9"), Some(tiny.len()));
}

/// A refinement of the "not before it grows" rule: SessionEnd closes a trailing turn the server
/// waited on (a tool call without its result), so it is sent once even though the file did not
/// grow.
#[tokio::test(flavor = "multi_thread")]
async fn session_end_sends_once_past_a_hold() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "ends", &env.root.join("work"));
    sess.tr.prompt("ends-p1", "Run the long job.").line(
        json!({"type": "assistant", "uuid": "ends-a1",
            "message": {"id": "msg-ends-a1", "role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Bash",
                 "input": {"command": "sleep 999"}}]}}),
    );
    sess.flush();
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(uploads(&s, "ends").len(), 1);
    assert_eq!(
        env.offset(&sess.key()),
        Some(0),
        "the turn is not complete yet"
    );
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(uploads(&s, "ends").len(), 1, "held until the file grows");
    env.hook(&sess.input("SessionEnd"));
    env.wait_idle();
    let ups = uploads(&s, "ends");
    assert_eq!(ups.len(), 2, "{ups:?}");
    assert_eq!(ups[1]["event"], "session-end");
    assert_eq!(turns_of(&t.home, &uri("ends")), 1);
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
}

/// Merge fix round 1: a `SessionEnd` after a `Stop` that sent the whole file still reaches the
/// server, as one empty upload at the offset. The source ends and is marked due for distillation.
#[tokio::test(flavor = "multi_thread")]
async fn session_end_after_a_stop_that_sent_everything_still_ends_the_session() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    std::fs::write(t.home.config_file(), "[distill]\nprovider = \"jev\"\n").unwrap();
    let waited = Instant::now();
    while t.svc.settings().distill.provider != alice_core::settings::DistillProvider::Jev {
        assert!(
            waited.elapsed() < Duration::from_secs(30),
            "the tenant never read distill.provider = jev"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "endsall", &env.root.join("work"));
    sess.turn(1, "everything was sent");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
    let row = |home: &AliceHome| -> (bool, String) {
        db(home)
            .query_row(
                "SELECT c.ended, c.distill_state FROM capture_state c
                 JOIN sources s ON s.id = c.source_id WHERE s.uri = ?1",
                [uri("endsall")],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(row(&t.home), (false, "idle".to_string()));

    env.hook(&sess.input("SessionEnd"));
    env.wait_idle();
    let ups = uploads(&s, "endsall");
    assert_eq!(ups.len(), 2, "{ups:?}");
    assert_eq!(ups[1]["event"], "session-end");
    assert_eq!(ups[1]["offset"], sess.tr.len().to_string());
    assert_eq!(
        upload_sizes(&s, "endsall")[1],
        (sess.tr.len(), 0),
        "an empty body at the offset"
    );
    let (ended, state) = row(&t.home);
    assert!(ended, "the session end reached the server");
    // No Jev judge is set, so the distill worker may already have moved it on to wait for one.
    assert!(matches!(state.as_str(), "due" | "waiting_judge"), "{state}");
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
    assert_eq!(turns_of(&t.home, &uri("endsall")), 1);

    // A second SessionEnd (nothing new, already ended) sends the empty end again, harmlessly.
    env.hook(&sess.input("SessionEnd"));
    env.wait_idle();
    assert!(row(&t.home).0);
    assert_eq!(turns_of(&t.home, &uri("endsall")), 1);
}

/// `ended` of the capture state of the source `uri`.
fn source_ended(home: &AliceHome, uri: &str) -> bool {
    db(home)
        .query_row(
            "SELECT c.ended FROM capture_state c JOIN sources s ON s.id = c.source_id
             WHERE s.uri = ?1",
            [uri],
            |r| r.get(0),
        )
        .unwrap()
}

/// Task 15b (carried fix): the empty end is owed until a session end is accepted in the call,
/// not only on its first pass. A stored offset behind the server makes the SessionEnd's first
/// upload a 409 that jumps to the end of the file; the end still reaches the server, once.
#[tokio::test(flavor = "multi_thread")]
async fn session_end_after_a_409_jump_to_the_end_still_ends_the_session() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "jumpend", &env.root.join("work"));
    sess.turn(1, "sent by the stop");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    let len = sess.tr.len();
    assert_eq!(env.offset(&sess.key()), Some(len));
    assert!(!source_ended(&t.home, &uri("jumpend")));
    // The state file falls behind the server (an older copy restored, say).
    let st = env.state_file(&sess.key());
    let text = read(&st);
    let rest = text.split_once('\n').unwrap().1;
    std::fs::write(&st, format!("0 {}\n{rest}", sess.path.display())).unwrap();
    let before = uploads(&s, "jumpend").len();

    env.hook(&sess.input("SessionEnd"));
    env.wait_idle();
    assert_eq!(
        upload_sizes(&s, "jumpend")[before..],
        [(0, len as usize), (len, 0)],
        "the whole file from 0 (answered 409), then one empty end at the server's cursor: {}",
        env.log()
    );
    assert!(
        uploads(&s, "jumpend")[before..]
            .iter()
            .all(|q| q["event"] == "session-end")
    );
    assert!(source_ended(&t.home, &uri("jumpend")), "{}", env.log());
    assert_eq!(env.offset(&sess.key()), Some(len));
    assert!(
        env.log().contains("the server ended the session"),
        "{}",
        env.log()
    );
}

/// Task 15b (carried fix): the line logged after an empty session end reads the answer. It says
/// "ended" only when the answer says so; otherwise it logs what came back. A tenant whose
/// configuration is degraded waits on every upload, so it ends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_session_end_is_logged_as_the_server_answered() {
    let dir = tempfile::tempdir().unwrap();
    let home = AliceHome::new(dir.path().join("ann"));
    home.ensure_dirs().unwrap();
    std::fs::write(home.config_file(), "[capture\nenabled = tru").unwrap();
    let s = Server::start_on("ann", home).await;
    let t = s.tenant("ann");
    assert!(t.svc.settings_handle().config_degraded());
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "waitend", &env.root.join("work"));
    sess.turn(1, "held while the rules are unknown");
    let len = sess.tr.len();
    // As if a stop had sent it all: the stored offset is the end of the file.
    std::fs::write(
        env.state_file(&sess.key()),
        format!(
            "{len} {}\nsession waitend\ncwd {}\n",
            sess.path.display(),
            sess.cwd.display()
        ),
    )
    .unwrap();
    env.hook(&sess.input("SessionEnd"));
    env.wait_idle();
    assert_eq!(upload_sizes(&s, "waitend"), [(len, 0)], "{}", env.log());
    let log = env.log();
    assert!(!log.contains("ended the session"), "{log}");
    assert!(log.contains("did not end the session"), "{log}");
    assert!(log.contains(&format!("next_offset {len}")), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_line_is_skipped() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "huge", &env.root.join("work"));
    let mut big = br#"{"type":"progress","data":""#.to_vec();
    big.extend(std::iter::repeat_n(b'x', 9 << 20));
    big.extend(b"\"}\n");
    sess.tr.bytes.extend(&big);
    sess.turn(1, "after the huge line");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
    assert_eq!(turns_of(&t.home, &uri("huge")), 1);
    // 2 MiB, then 4, then 8 (the full body the server calls oversized), then past the line.
    let sizes = upload_sizes(&s, "huge");
    assert_eq!(
        sizes,
        vec![
            (0, 2 << 20),
            (0, 4 << 20),
            (0, 8 << 20),
            (big.len() as u64, sess.tr.bytes.len() - big.len()),
        ]
    );
    assert!(env.log().contains("oversized"), "{}", env.log());
}

/// The per-call cap starts at 2 MiB, so a slow link sends a backlog in several bounded calls.
#[tokio::test(flavor = "multi_thread")]
async fn a_backlog_over_two_mebibytes_drains_in_several_calls() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "backlog", &env.root.join("work"));
    let filler = "a long answer line ".repeat(1000);
    let mut i = 0;
    while sess.tr.len() < 5 << 20 {
        i += 1;
        let id = sess.id.clone();
        sess.tr
            .prompt(
                &format!("{id}-p{i}"),
                &format!("Question {i} of the backlog."),
            )
            .reply(
                &format!("{id}-a{i}"),
                &format!("backlogmarker{i}: {filler}"),
            );
    }
    sess.flush();
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    let sizes = upload_sizes(&s, "backlog");
    assert!(sizes.len() >= 3, "{sizes:?}");
    assert!(sizes.iter().all(|(_, n)| *n <= 2 << 20), "{sizes:?}");
    // Every capped body says the transcript goes on; the last one, which reaches the end, not.
    let ups = uploads(&s, "backlog");
    let (last, capped) = ups.split_last().unwrap();
    assert!(
        capped
            .iter()
            .all(|q| q.get("more").map(String::as_str) == Some("1"))
    );
    assert!(!last.contains_key("more"));
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
    assert_eq!(turns_of(&t.home, &uri("backlog")), i as i64);
    assert_eq!(chunks_holding(&t.home, &format!("backlogmarker{i}:")), 1);
}

/// One turn longer than the cap: the answer takes nothing from a capped body, so the same offset
/// is sent again with twice the cap, and the turn is captured whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_over_two_mebibytes_escalates_the_cap() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "longturn", &env.root.join("work"));
    sess.tr
        .prompt("longturn-p1", "Write the long report.")
        .reply(
            "longturn-a1",
            &format!("longturnmarker {}", "word ".repeat(600_000)),
        );
    sess.flush();
    assert!(sess.tr.len() > (2 << 20) + 4096 && sess.tr.len() < 4 << 20);
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    let sizes = upload_sizes(&s, "longturn");
    assert_eq!(
        sizes,
        vec![(0, 2 << 20), (0, sess.tr.len() as usize)],
        "2 MiB took nothing; twice the cap holds the whole turn"
    );
    assert_eq!(turns_of(&t.home, &uri("longturn")), 1);
    assert!(chunks_holding(&t.home, "longturnmarker") >= 1);
    assert_eq!(env.offset(&sess.key()), Some(sess.tr.len()));
}

/// The lock of a transcript (ages are planted by writing `at` directly):
/// - a dead holder whose `at` is older than 360 s is taken over;
/// - a live holder is never broken under 900 s, however old its `at` (one upload may take
///   5 + 300 s);
/// - a dead holder with a recent `at` is respected;
/// - a worker refreshes `at` every pass, and one that lost its lock stops, writes no state and
///   never removes the lock it no longer owns.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_lock_is_taken_over_and_a_foreign_lock_is_never_removed() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let state = env.alice().join("state");
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let me = std::process::id().to_string();
    // A pid that is surely dead: a child that has exited and been reaped.
    let dead = {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid.to_string()
    };
    let plant = |key: &str, at: u64, pid: &str| {
        let lock = state.join(format!("{key}.lock"));
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("at"), format!("{at}\n")).unwrap();
        std::fs::write(lock.join("pid"), format!("{pid}\n")).unwrap();
        lock
    };
    let cwd = env.root.join("work");

    // Dead holder, `at` older than 360 s: taken over, and nothing of it is left behind.
    let mut a = Session::new(&env, "lock-stale", &cwd);
    a.turn(1, "behind a stale lock");
    let lock_a = plant(&a.key(), now() - 400, &dead);
    env.hook(&a.input("Stop"));
    env.wait_idle();
    assert_eq!(turns_of(&t.home, &uri("lock-stale")), 1);
    assert!(!lock_a.exists());
    let leftovers: Vec<String> = std::fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".lock"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // A live holder whose `at` is 400 s old (a long upload), and a dead holder whose `at` is
    // 200 s old: both waited for, and the workers go on once they are released.
    let mut b = Session::new(&env, "lock-live", &cwd);
    b.turn(1, "behind a live lock");
    let lock_b = plant(&b.key(), now() - 400, &me);
    let mut d = Session::new(&env, "lock-recent", &cwd);
    d.turn(1, "behind a recent lock");
    let lock_d = plant(&d.key(), now() - 200, &dead);
    env.hook(&b.input("Stop"));
    env.hook(&d.input("Stop"));
    // A sweep (no wait at all) must not break them either.
    let sweeper = Session::new(&env, "lock-sweeper", &cwd);
    env.hook(&sweeper.input("SessionStart"));
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        uploads(&s, "lock-live").is_empty() && uploads(&s, "lock-recent").is_empty(),
        "nothing while they are held"
    );
    assert_eq!(
        read(&lock_b.join("pid")).trim(),
        me,
        "a live holder is never broken"
    );
    assert_eq!(
        read(&lock_d.join("pid")).trim(),
        dead,
        "a recent lock is respected"
    );
    std::fs::remove_dir_all(&lock_b).unwrap();
    std::fs::remove_dir_all(&lock_d).unwrap();
    env.wait_idle();
    assert_eq!(turns_of(&t.home, &uri("lock-live")), 1);
    assert_eq!(turns_of(&t.home, &uri("lock-recent")), 1);

    // Mid-run: `at` is refreshed every pass. Taken over while pass 2 is in flight, the worker
    // writes nothing more (not even pass 2's answer), stops, and leaves the new holder's lock
    // alone.
    let mut c = Session::new(&env, "lock-mid", &cwd);
    let filler = "a long answer line ".repeat(1000);
    let mut i = 0;
    while c.tr.len() < 5 << 20 {
        i += 1;
        let id = c.id.clone();
        c.tr.prompt(&format!("{id}-p{i}"), "Next.")
            .reply(&format!("{id}-a{i}"), &filler);
    }
    c.flush();
    s.delay_requests(1500);
    env.hook(&c.input("Stop"));
    let lock_c = state.join(format!("{}.lock", c.key()));
    let wait_for = |n: usize| {
        let start = Instant::now();
        while uploads(&s, "lock-mid").len() < n {
            assert!(start.elapsed() < Duration::from_secs(30), "no upload {n}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for(1);
    let at1: u64 = read(&lock_c.join("at")).trim().parse().unwrap();
    let owner = read(&lock_c.join("pid"));
    assert_ne!(owner.trim(), me);
    wait_for(2);
    let at2: u64 = read(&lock_c.join("at")).trim().parse().unwrap();
    assert!(
        at2 > at1,
        "`at` is refreshed at each pass: {at1} then {at2}"
    );
    let offset_at_loss = env.offset(&c.key()).unwrap();
    std::fs::remove_dir_all(&lock_c).unwrap();
    plant(&c.key(), now(), &me);
    env.wait_idle();
    s.delay_requests(0);
    assert_eq!(
        env.offset(&c.key()),
        Some(offset_at_loss),
        "a worker that lost its lock writes no state"
    );
    assert_eq!(
        uploads(&s, "lock-mid").len(),
        2,
        "it stopped once it lost the lock"
    );
    assert_eq!(
        read(&lock_c.join("pid")).trim(),
        me,
        "the new holder's lock is left alone"
    );
    assert!(env.offset(&c.key()).unwrap() < c.tr.len());
    std::fs::remove_dir_all(&lock_c).unwrap();
    env.hook(&c.input("Stop"));
    env.wait_idle();
    assert_eq!(env.offset(&c.key()), Some(c.tr.len()));
    assert_eq!(turns_of(&t.home, &uri("lock-mid")), i as i64);
}

/// A state whose transcript does not exist (yet) is kept for a week from when it last saw its
/// transcript (its `seen` line), whatever its mtime says: a session's state is written at
/// SessionStart, before Claude Code creates the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_state_for_a_transcript_not_written_yet_survives_a_sweep() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let cwd = env.root.join("work");
    let early = Session::new(&env, "not-yet", &cwd);
    assert!(!early.path.exists());
    env.hook(&early.input("SessionStart"));
    env.wait_idle();
    assert!(env.state_file(&early.key()).exists());
    // Planted: transcripts gone for eight days and for six.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let plant = |key: &str, days: u64| {
        std::fs::write(
            env.state_file(key),
            format!(
                "0 /nowhere/{key}.jsonl\nsession {key}\ncwd \nseen {}\n",
                now - days * 86_400
            ),
        )
        .unwrap();
    };
    plant("claude-code-gone-8d", 8);
    plant("claude-code-gone-6d", 6);
    // A state first written eight days ago whose transcript was captured just now, then
    // deleted: `seen` was refreshed by that capture, so it is kept.
    let mut recent = Session::new(&env, "seen-recently", &cwd);
    recent.turn(1, "captured just before its file went");
    std::fs::write(
        env.state_file(&recent.key()),
        format!(
            "0 {}\nsession seen-recently\ncwd {}\nseen {}\n",
            recent.path.display(),
            cwd.display(),
            now - 8 * 86_400
        ),
    )
    .unwrap();
    env.hook(&recent.input("Stop"));
    env.wait_idle();
    assert_eq!(env.offset(&recent.key()), Some(recent.tr.len()));
    std::fs::remove_file(&recent.path).unwrap();
    let sweeper = Session::new(&env, "not-yet-sweeper", &cwd);
    env.hook(&sweeper.input("SessionStart"));
    env.wait_idle();
    assert!(
        env.state_file(&early.key()).exists(),
        "a state written before its transcript survives a sweep"
    );
    assert!(env.state_file("claude-code-gone-6d").exists());
    assert!(
        !env.state_file("claude-code-gone-8d").exists(),
        "gone for a week: forgotten"
    );
    assert!(
        env.state_file(&recent.key()).exists(),
        "`seen` is refreshed whenever the transcript is there"
    );
    // The early session's transcript appears: captured from 0.
    let mut early = early;
    early.turn(1, "written after the state");
    env.hook(&early.input("Stop"));
    env.wait_idle();
    assert_eq!(turns_of(&s.tenant("ann").home, &uri("not-yet")), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_token_pauses_capture_for_an_hour() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    write_mode(
        &env.alice().join("token"),
        format!("{}\n", "0".repeat(64)).as_bytes(),
        0o600,
    );
    let mut sess = Session::new(&env, "refused", &env.root.join("work"));
    sess.turn(1, "sent with a stale token");
    // The briefing's 401 carries a JSON body: only a 200 text/plain answer is printed.
    let r = env.hook(&sess.input("SessionStart"));
    assert!(r.stdout.is_empty(), "{r}");
    assert_eq!(briefing_requests(&s), 1);
    env.wait_idle();
    assert!(
        env.log()
            .contains("the token was refused; run the setup command again"),
        "{}",
        env.log()
    );
    let until: u64 = read(&env.alice().join("paused-until"))
        .trim()
        .parse()
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        (now + 3500..=now + 3700).contains(&until),
        "{until} vs {now}"
    );
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(uploads(&s, "refused").len(), 1, "paused: nothing more sent");

    // Setup with the right token ends the pause.
    let r = env.setup(
        &s.url("ann"),
        &format!("{}\n", s.tenant("ann").token),
        &[("ALICE_SETUP_TOKEN_STDIN", "1")],
    );
    assert!(r.ok(), "{r}");
    assert!(!env.alice().join("paused-until").exists());
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(turns_of(&s.tenant("ann").home, &uri("refused")), 1);
}

// ---- The project and privacy ----

#[tokio::test(flavor = "multi_thread")]
async fn the_project_hint_follows_the_git_top_level() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let repo = env.root.join("code/Repo One");
    env.git_repo(&repo);
    let home_notes = env.home.join("notes");
    env.git_repo(&env.home);
    std::fs::create_dir_all(&home_notes).unwrap();
    let odd = env.root.join("code/Quote\"Back\\slash");
    env.git_repo(&odd);
    // A top-level whose name holds a control character (reached through a symlink: a hook
    // input holding one is skipped before any of this, for its `\u` escape).
    let ctl = env.root.join("code/bad\u{1}name");
    env.git_repo(&ctl);
    let ctl_link = env.root.join("code/ctl-link");
    std::os::unix::fs::symlink(&ctl, &ctl_link).unwrap();
    let cases = [
        ("hint-repo", repo.join("src"), "import:repo-one", "repo-one"),
        ("hint-home", home_notes.clone(), "global", "global"),
        ("hint-nogit", env.root.join("plain"), "global", "global"),
        (
            "hint-odd",
            odd.clone(),
            "import:quotebackslash",
            "quotebackslash",
        ),
        ("hint-ctl", ctl_link.clone(), "global", "global"),
    ];
    for (id, cwd, _, _) in &cases {
        let mut sess = Session::new(&env, id, cwd);
        sess.turn(1, id);
        env.hook(&sess.input("Stop"));
    }
    env.wait_idle();
    for (id, _, key, slug) in cases {
        assert_eq!(
            source_project(&t.home, &uri(id)),
            Some((key.to_string(), slug.to_string())),
            "{id}"
        );
        for q in uploads(&s, id) {
            assert!(!q.contains_key("project"), "{id}: header only");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_capture_in_alice_toml_skips_and_cloud_denied_sends_no_distill() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let quiet = env.root.join("code/quiet");
    env.git_repo(&quiet);
    std::fs::write(
        quiet.join(".alice.toml"),
        "[capture]\n  no_capture = true\n",
    )
    .unwrap();
    let denied = env.root.join("code/denied");
    env.git_repo(&denied);
    std::fs::write(denied.join(".alice.toml"), "cloud_denied=true\n").unwrap();
    let plain = env.root.join("plain-quiet");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join(".alice.toml"), "no_capture = true\n").unwrap();
    let locked = env.root.join("code/locked");
    env.git_repo(&locked);
    write_mode(&locked.join(".alice.toml"), b"no_capture = false\n", 0o000);
    let locked_readable = std::fs::read(locked.join(".alice.toml")).is_ok(); // root reads anything

    let mut sessions = Vec::new();
    for (id, cwd) in [
        ("toml-quiet", quiet.join("deep/dir")),
        ("toml-denied", denied.clone()),
        ("toml-plain", plain.clone()),
        ("toml-locked", locked.clone()),
    ] {
        let mut sess = Session::new(&env, id, &cwd);
        sess.turn(1, id);
        env.hook(&sess.input("Stop"));
        sessions.push(sess);
    }
    env.wait_idle();
    assert!(uploads(&s, "toml-quiet").is_empty());
    assert!(uploads(&s, "toml-plain").is_empty(), "no git: cwd's file");
    assert!(source_project(&t.home, &uri("toml-quiet")).is_none());
    assert!(env.log().contains("no_capture"), "{}", env.log());
    let denied_ups = uploads(&s, "toml-denied");
    assert!(!denied_ups.is_empty());
    assert!(
        denied_ups
            .iter()
            .all(|q| q.get("no_distill").map(String::as_str) == Some("1"))
    );
    assert_eq!(
        capture_row(&t.home, &uri("toml-denied")),
        Some((sessions[1].tr.len(), true))
    );
    if !locked_readable {
        assert!(
            uploads(&s, "toml-locked").is_empty(),
            "an unreadable file skips the event"
        );
    }
    // A plain project sends no flag.
    let mut open = Session::new(&env, "toml-open", &env.root.join("open"));
    open.turn(1, "open");
    env.hook(&open.input("Stop"));
    env.wait_idle();
    assert!(
        uploads(&s, "toml-open")
            .iter()
            .all(|q| !q.contains_key("no_distill"))
    );
}

/// Turns written while `no_capture = true` are never uploaded: the skip moves the local offset
/// to the end of the last complete line, so capture resumes after the line is removed with only
/// the new turns (the server records a forward gap).
#[tokio::test(flavor = "multi_thread")]
async fn no_capture_moves_the_offset_past_the_excluded_turns() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let repo = env.root.join("code/private-for-now");
    env.git_repo(&repo);
    std::fs::write(repo.join(".alice.toml"), "no_capture = true\n").unwrap();
    let mut sess = Session::new(&env, "excluded", &repo);
    sess.turn(1, "excludedone");
    sess.turn(2, "excludedtwo");
    // A trailing line still being written is not passed.
    let complete = sess.tr.len();
    sess.tr
        .bytes
        .extend(br#"{"type":"user","uuid":"excluded-p3","mess"#);
    sess.flush();
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert!(uploads(&s, "excluded").is_empty());
    assert_eq!(env.offset(&sess.key()), Some(complete));

    // The line is removed; the partial line is finished and a new turn written.
    std::fs::write(repo.join(".alice.toml"), "# capture again\n").unwrap();
    sess.tr.bytes.truncate(complete as usize);
    sess.written = sess.tr.bytes.len();
    std::fs::write(&sess.path, &sess.tr.bytes).unwrap();
    sess.turn(3, "includedthree");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    let ups = uploads(&s, "excluded");
    assert_eq!(ups.len(), 1, "{ups:?}");
    assert_eq!(ups[0]["offset"], complete.to_string());
    assert_eq!(turns_of(&t.home, &uri("excluded")), 1);
    assert_eq!(chunks_holding(&t.home, "excludedone"), 0);
    assert_eq!(chunks_holding(&t.home, "excludedtwo"), 0);
    assert_eq!(chunks_holding(&t.home, "includedthree"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_script_handles_utf8_and_skips_u_escapes() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let cwd = env.root.join("งาน ไทย");
    env.git_repo(&cwd);
    let path = env.root.join("บันทึก การทำงาน/เซสชัน หนึ่ง.jsonl");
    let mut sess = Session::at("thai-one", path, &cwd);
    sess.turn(1, "ภาษาไทย");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(turns_of(&t.home, &uri("thai-one")), 1);
    assert_eq!(
        source_project(&t.home, &uri("thai-one")).unwrap().1,
        alice_core::normalize::slugify("งาน ไทย")
    );
    // `\/` in the input (some encoders escape slashes) is undone.
    sess.turn(2, "escaped slashes");
    let escaped = serde_json::to_string(&sess.input("Stop"))
        .unwrap()
        .replace('/', "\\/");
    let r = env.hook_raw(escaped.as_bytes());
    assert!(r.ok(), "{r}");
    env.wait_idle();
    assert_eq!(turns_of(&t.home, &uri("thai-one")), 2);

    // A `\u` escape is not guessed, even where the decoded path exists.
    let dir = env.root.join("transcripts");
    std::fs::write(dir.join("ก.jsonl"), b"").unwrap();
    // The escape is built from its characters, so no editor can turn it into the letter.
    let escape: String = ['\\', 'u', '0', 'e', '0', '1'].iter().collect();
    let raw = format!(
        r#"{{"session_id":"uesc-session","transcript_path":"{}/{escape}.jsonl","cwd":"{}","hook_event_name":"Stop"}}"#,
        dir.display(),
        cwd.display()
    );
    assert!(raw.contains(r"/\u0e01.jsonl"), "{raw}");
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        parsed["transcript_path"]
            .as_str()
            .unwrap()
            .ends_with("/ก.jsonl")
    );
    let r = env.hook_raw(raw.as_bytes());
    assert!(r.ok() && r.stdout.is_empty() && r.stderr.is_empty(), "{r}");
    env.wait_idle();
    assert!(uploads(&s, "uesc-session").is_empty());
    assert!(!env.state_file("claude-code-uesc-session").exists());
    let log = env.log();
    assert_eq!(
        log.lines().filter(|l| l.contains("uesc-session")).count(),
        1,
        "{log}"
    );
}

// ---- The briefing ----

#[tokio::test(flavor = "multi_thread")]
async fn session_start_prints_the_briefing_as_plain_text() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    t.svc
        .remember(RememberRequest {
            content: "Repo One deploys with blue green switching".into(),
            kind: Some(Kind::Decision),
            importance: Some(5),
            project_hint: Some("Repo One".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let env = Env::new();
    env.install(&s);
    let repo = env.root.join("code/Repo One");
    env.git_repo(&repo);
    let sess = Session::new(&env, "brief", &repo);
    let r = env.hook(&sess.input("SessionStart"));
    assert!(
        r.stdout.starts_with("# Context · repo-one\n"),
        "the repository's briefing, as plain text: {r}"
    );
    assert!(r.stdout.contains("blue green"), "{r}");
    assert!(!r.stdout.starts_with('{'), "plain stdout, not JSON");

    // A resumed session already holds one: nothing is asked or printed.
    let before = briefing_requests(&s);
    let mut resumed = sess.input("SessionStart");
    resumed["source"] = json!("resume");
    let r = env.hook(&resumed);
    assert!(r.stdout.is_empty(), "{r}");
    assert_eq!(briefing_requests(&s), before);
    env.wait_idle();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_briefing_gives_up_within_two_seconds() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let sess = Session::new(&env, "slow", &env.root.join("work"));
    s.stall(true);
    let r = env.hook(&sess.input("SessionStart"));
    s.stall(false);
    assert!(
        r.elapsed < Duration::from_millis(2500),
        "the hook took {:?}",
        r.elapsed
    );
    assert!(
        r.elapsed > Duration::from_millis(1500),
        "the briefing was in flight until its 2 s limit: {:?}",
        r.elapsed
    );
    assert!(r.stdout.is_empty(), "{r}");
    assert_eq!(r.status.code(), Some(0));
    assert_eq!(briefing_requests(&s), 1);
    env.wait_idle();
}

#[tokio::test(flavor = "multi_thread")]
async fn briefing_off_prints_nothing() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let sess = Session::new(&env, "quiet-brief", &env.root.join("work"));
    // The local switch: no request at all.
    std::fs::write(env.alice().join("capture.conf"), "briefing=off\n").unwrap();
    let r = env.hook(&sess.input("SessionStart"));
    assert!(r.stdout.is_empty(), "{r}");
    assert_eq!(briefing_requests(&s), 0);
    // The tenant's setting: asked, answered 204, nothing printed.
    std::fs::write(env.alice().join("capture.conf"), "briefing=on\n").unwrap();
    let patched = reqwest::Client::new()
        .patch(format!("{}/api/v1/settings", s.url("ann")))
        .bearer_auth(&t.token)
        .json(&json!({ "capture.briefing": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200);
    let r = env.hook(&sess.input("SessionStart"));
    assert!(r.stdout.is_empty(), "{r}");
    assert_eq!(briefing_requests(&s), 1);
    env.wait_idle();
}

// ---- Self-update (Task 15b) ----

/// The version line a served hook script carries (its third line).
const VERSION_PREFIX: &str = "# alice-capture-version: ";

/// The version on a hook script's version line.
fn version_of(script: &str) -> String {
    script
        .lines()
        .find_map(|l| l.strip_prefix(VERSION_PREFIX))
        .expect("a version line")
        .to_string()
}

/// `script` with its version line saying `version`.
fn with_version(script: &str, version: &str) -> String {
    let old = format!("{VERSION_PREFIX}{}\n", version_of(script));
    let new = format!("{VERSION_PREFIX}{version}\n");
    assert_ne!(old, new);
    script.replacen(&old, &new, 1)
}

/// A 200 answer of `body` for the hook script route.
fn script_answer(body: impl Into<Vec<u8>>) -> ScriptAnswer {
    ScriptAnswer {
        status: 200,
        body: body.into(),
        hold_ms: 0,
    }
}

/// How many times the tenant was asked for the hook script (setup's download included). The route
/// is public: none of these requests carries the token.
fn hook_fetches(s: &Server) -> usize {
    let fetches: Vec<_> = s
        .seen("ann")
        .into_iter()
        .filter(|(m, pq, _, _)| m == "GET" && pq == "/api/v1/capture/hook.sh")
        .collect();
    assert!(
        fetches.iter().all(|(_, _, auth, _)| !auth),
        "the hook script is fetched without the token"
    );
    fetches.len()
}

/// Serves `script` as the tenant's hook script, makes the update check due, and runs one Stop of
/// `sess` (its turn `i`), so its worker checks.
fn update_to(s: &Server, env: &Env, sess: &mut Session, i: usize, script: &str) {
    s.serve_hook_script(Some(script_answer(script)));
    env.make_update_due();
    sess.turn(i, &format!("update turn {i}"));
    env.hook(&sess.input("Stop"));
    env.wait_idle();
}

fn ino(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().ino()
}

fn json_file(path: &Path) -> serde_json::Value {
    serde_json::from_str(&read(path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The daily check finds the served version equal to the installed one: nothing changes but the
/// time of the check. Setup's own install counts as that day's check.
#[tokio::test(flavor = "multi_thread")]
async fn self_update_does_nothing_when_the_version_is_unchanged() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let script = std::fs::read(env.script()).unwrap();
    let script_ino = ino(&env.script());
    let plugin_files = [
        "alice-capture/hooks/hooks.json",
        "alice-capture/.claude-plugin/plugin.json",
        ".claude-plugin/marketplace.json",
    ];
    let plugin = |env: &Env| -> Vec<(String, u64)> {
        plugin_files
            .iter()
            .map(|f| {
                let p = env.marketplace().join(f);
                (read(&p), ino(&p))
            })
            .collect()
    };
    let plugin_before = plugin(&env);
    let calls = env.claude_calls();
    let fetched = hook_fetches(&s);
    let mut sess = Session::new(&env, "same-version", &env.root.join("work"));

    // Setup's install was today's check: nothing is fetched.
    sess.turn(1, "before the check");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched, "{}", env.log());

    // A day later: fetched once, and the same version changes nothing.
    env.make_update_due();
    sess.turn(2, "after a day");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 1, "{}", env.log());
    assert_eq!(std::fs::read(env.script()).unwrap(), script);
    assert_eq!(ino(&env.script()), script_ino, "not even rewritten");
    assert_eq!(mode(&env.script()), 0o700);
    assert_eq!(
        plugin(&env),
        plugin_before,
        "the plugin's files are untouched"
    );
    assert_eq!(env.claude_calls(), calls, "Claude Code is asked nothing");
    assert!(!env.log().contains("capture hook"), "{}", env.log());
    let checked: u64 = read(&env.checked_file()).trim().parse().unwrap();
    assert!(now_secs().abs_diff(checked) < 60, "{checked}");
    assert!(
        env.update_leftovers().is_empty(),
        "{:?}",
        env.update_leftovers()
    );
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());
    assert_eq!(turns_of(&t.home, &uri("same-version")), 2);
}

/// A served script with another version replaces the installed one (0700, through a temporary
/// file and `mv`), the log says so, and the new copy runs the next event.
#[tokio::test(flavor = "multi_thread")]
async fn self_update_replaces_the_script_with_a_newer_one() {
    let s = Server::start(&["ann"]).await;
    let t = s.tenant("ann");
    let env = Env::new();
    env.install(&s);
    let old = read(&env.script());
    let old_version = version_of(&old);
    let calls = env.claude_calls();
    let newer = with_version(&old, "0123456789ab");
    let mut sess = Session::new(&env, "newer-version", &env.root.join("work"));
    update_to(&s, &env, &mut sess, 1, &newer);
    assert_eq!(read(&env.script()), newer, "{}", env.log());
    assert_eq!(mode(&env.script()), 0o700);
    let line = format!("capture hook updated {old_version} \u{2192} 0123456789ab");
    assert_eq!(env.log().matches(&line).count(), 1, "{}", env.log());
    assert!(
        env.update_leftovers().is_empty(),
        "{:?}",
        env.update_leftovers()
    );
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());
    // Its hooks.json is the same: the plugin is left alone.
    assert_eq!(env.claude_calls(), calls);

    // The new copy runs the next event, and `status` names its version.
    sess.turn(2, "sent by the new copy");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    assert_eq!(turns_of(&t.home, &uri("newer-version")), 2);
    let r = env.capture_sh(&["status"], &[]);
    assert!(r.stdout.contains("version 0123456789ab"), "{r}");
    assert!(!env.log().contains(&t.token));

    // A worker whose script was removed meanwhile (uninstall) never puts it back.
    let copy = env.root.join("running-copy.sh");
    std::fs::copy(env.script(), &copy).unwrap();
    std::fs::remove_file(env.script()).unwrap();
    s.serve_hook_script(Some(script_answer(with_version(&newer, "fedcba987654"))));
    env.make_update_due();
    let fetched = hook_fetches(&s);
    sess.turn(3, "after the uninstall");
    let mut c = env.command(&env.sh());
    c.arg(&copy).args([
        "run",
        "claude-code",
        "stop",
        "newer-version",
        &sess.path.display().to_string(),
        &sess.cwd.display().to_string(),
    ]);
    let r = env.run(c, b"", Duration::from_secs(60));
    assert!(r.ok(), "{r}");
    assert!(!env.script().exists(), "{}", env.log());
    assert_eq!(hook_fetches(&s), fetched, "{}", env.log());
}

/// A download that is not a whole hook script that parses is refused: the installed script
/// stays byte for byte, and each refusal is one log line.
#[tokio::test(flavor = "multi_thread")]
async fn self_update_keeps_the_old_script_on_a_bad_download() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let old = std::fs::read(env.script()).unwrap();
    let old_ino = ino(&env.script());
    let newer = with_version(&String::from_utf8(old.clone()).unwrap(), "badbadbadbad");
    let main_line = "\nmain \"$@\"\n";
    assert!(newer.ends_with(main_line));
    let before_main = |extra: &str| newer.replacen(main_line, &format!("\n{extra}{main_line}"), 1);
    let marker = newer.lines().nth(1).unwrap().to_string();
    assert!(marker.starts_with("# alice-capture-hook"));
    let version_line = format!("{VERSION_PREFIX}badbadbadbad\n");
    // Whole functions up to `main`: it parses, but it is cut short.
    let cut = newer[..newer.find("\nmain() {").unwrap() + 1].to_string();
    let dash = ["/bin/dash", "/usr/bin/dash"]
        .into_iter()
        .any(|p| Path::new(p).exists());
    // `sh` on PATH: dash refuses `function` itself, bash in sh mode leaves it to `dash -n`.
    let sh_is_dash = std::fs::canonicalize(env.sh())
        .unwrap()
        .to_string_lossy()
        .ends_with("dash");
    let mut cases: Vec<(&str, ScriptAnswer, &str)> = vec![
        ("empty", script_answer(""), "empty"),
        (
            "another shebang",
            script_answer(newer.replacen("#!/bin/sh\n", "#!/bin/bash\n", 1)),
            "#!/bin/sh",
        ),
        (
            "no marker",
            script_answer(newer.replacen(&marker, "# some other script", 1)),
            "marker",
        ),
        (
            "no version line",
            script_answer(newer.replacen(&version_line, "", 1)),
            "version line",
        ),
        ("cut short", script_answer(cut), "cut short"),
        (
            "sh -n fails",
            script_answer(before_main("if then")),
            "fails sh -n",
        ),
        (
            "server error",
            ScriptAnswer {
                status: 500,
                body: b"boom".to_vec(),
                hold_ms: 0,
            },
            "HTTP 500",
        ),
        (
            "timeout",
            ScriptAnswer {
                status: 200,
                body: newer.clone().into_bytes(),
                hold_ms: 40_000,
            },
            "curl exit 28",
        ),
    ];
    if dash {
        cases.insert(
            6,
            (
                "dash -n fails",
                script_answer(before_main("function f { :; }")),
                if sh_is_dash {
                    "fails sh -n"
                } else {
                    "fails dash -n"
                },
            ),
        );
    }
    let mut sess = Session::new(&env, "bad-download", &env.root.join("work"));
    for (i, (name, answer, expected)) in cases.into_iter().enumerate() {
        let logged = env.log().lines().count();
        let fetched = hook_fetches(&s);
        s.serve_hook_script(Some(answer));
        env.make_update_due();
        sess.turn(i + 1, &format!("turn {i}"));
        env.hook(&sess.input("Stop"));
        env.wait_idle();
        assert_eq!(hook_fetches(&s), fetched + 1, "{name}: {}", env.log());
        assert_eq!(
            std::fs::read(env.script()).unwrap(),
            old,
            "{name}: {}",
            env.log()
        );
        assert_eq!(ino(&env.script()), old_ino, "{name}");
        let log = env.log();
        let fresh: Vec<&str> = log
            .lines()
            .skip(logged)
            .filter(|l| l.contains("capture hook"))
            .collect();
        assert_eq!(fresh.len(), 1, "{name}: one line: {log}");
        assert!(fresh[0].contains(expected), "{name}: {}", fresh[0]);
        assert!(!fresh[0].contains("updated"), "{name}: {}", fresh[0]);
        // Never the downloaded text.
        for text in [
            "#!/bin/bash",
            "some other script",
            "if then",
            "function f",
            "boom",
        ] {
            assert!(!log.contains(text), "{name}: {text:?} in the log: {log}");
        }
        assert!(
            env.update_leftovers().is_empty(),
            "{name}: {:?}",
            env.update_leftovers()
        );
    }
    assert!(env.tmp_entries().is_empty(), "{:?}", env.tmp_entries());
}

/// At most one check a day: a record of the last one, read again under the update lock, and no
/// check while another worker holds that lock.
#[tokio::test(flavor = "multi_thread")]
async fn self_update_runs_at_most_once_a_day() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let work = env.root.join("work");
    let mut a = Session::new(&env, "daily-a", &work);
    let mut b = Session::new(&env, "daily-b", &work);
    let fetched = hook_fetches(&s);

    // No record of a check: the first worker checks, and records it.
    std::fs::remove_file(env.checked_file()).unwrap();
    a.turn(1, "first");
    env.hook(&a.input("Stop"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 1, "{}", env.log());
    let checked: u64 = read(&env.checked_file()).trim().parse().unwrap();
    assert!(now_secs().abs_diff(checked) < 60, "{checked}");

    // The same day: no other event checks again.
    a.turn(2, "second");
    env.hook(&a.input("Stop"));
    b.turn(1, "third");
    env.hook(&b.input("SessionStart"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 1, "{}", env.log());

    // Due, and two workers at once: one check.
    env.make_update_due();
    a.turn(3, "fourth");
    b.turn(2, "fifth");
    env.hook(&a.input("Stop"));
    env.hook(&b.input("Stop"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 2, "{}", env.log());

    // Due, but a live worker holds the update lock: no check, and its lock is left alone.
    env.make_update_due();
    let lock = env.alice().join("state/update.lock");
    std::fs::create_dir(&lock).unwrap();
    let me = std::process::id().to_string();
    std::fs::write(lock.join("pid"), format!("{me}\n")).unwrap();
    std::fs::write(lock.join("at"), format!("{}\n", now_secs())).unwrap();
    a.turn(4, "sixth");
    env.hook(&a.input("Stop"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 2, "{}", env.log());
    assert_eq!(read(&lock.join("pid")).trim(), me);
    std::fs::remove_dir_all(&lock).unwrap();
    a.turn(5, "seventh");
    env.hook(&a.input("Stop"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 3, "{}", env.log());

    // A record in the future (the clock was set back since): due.
    std::fs::write(
        env.checked_file(),
        format!("{}\n", now_secs() + 10 * 86_400),
    )
    .unwrap();
    a.turn(6, "eighth");
    env.hook(&a.input("Stop"));
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 4, "{}", env.log());
}

/// The plugin's hooks.json follows the installed script's `print-hooks-json`, and is rewritten
/// only when that differs. Then the plugin's version moves too (a Claude Code that runs plugins
/// from its cache copies a new version only), and `claude plugin` is asked to reload it in the
/// background. The Codex hooks.json follows only while it is the file setup wrote.
#[tokio::test(flavor = "multi_thread")]
async fn self_update_rewrites_hooks_json_only_when_it_changed() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    write_mode(&env.bin.join("codex"), b"#!/bin/sh\nexit 0\n", 0o755);
    env.install(&s);
    let mp = env.marketplace();
    let hooks = mp.join("alice-capture/hooks/hooks.json");
    let plugin = mp.join("alice-capture/.claude-plugin/plugin.json");
    let market = mp.join(".claude-plugin/marketplace.json");
    let codex = env.codex_home.join("hooks.json");
    let record = env.alice().join("codex-hooks.written");
    assert_eq!(read(&codex), read(&record));
    let base = read(&env.script());
    let (hooks0, hooks_ino, plugin0, market0, codex0) = (
        read(&hooks),
        ino(&hooks),
        read(&plugin),
        read(&market),
        read(&codex),
    );
    let calls0 = env.claude_calls();
    let mut sess = Session::new(&env, "hooks-sync", &env.root.join("work"));

    // 1. A newer script that prints the same hooks: nothing is rewritten, nothing is asked.
    let v1 = with_version(&base, "aaaaaaaaaaaa");
    update_to(&s, &env, &mut sess, 1, &v1);
    assert_eq!(read(&env.script()), v1, "{}", env.log());
    assert_eq!((read(&hooks), ino(&hooks)), (hooks0.clone(), hooks_ino));
    assert_eq!((read(&plugin), read(&market)), (plugin0, market0));
    assert_eq!(read(&codex), codex0);
    assert_eq!(env.claude_calls(), calls0);

    // 2. Its Claude Code hooks differ: hooks.json is rewritten, the plugin's version moves, and
    //    Claude Code is asked to reload it.
    let desc = "Alice capture: each event hands";
    assert!(v1.contains(desc));
    let v2 = with_version(
        &v1.replacen(desc, "Alice capture (v2): each event hands", 1),
        "bbbbbbbbbbbb",
    );
    update_to(&s, &env, &mut sess, 2, &v2);
    assert_eq!(read(&env.script()), v2, "{}", env.log());
    let printed = env.capture_sh(&["print-hooks-json", "claude-code"], &[]);
    assert!(printed.stdout.contains("(v2)"), "{printed}");
    assert_eq!(read(&hooks), printed.stdout, "{}", env.log());
    assert_eq!(mode(&hooks), 0o600);
    let parsed = json_file(&hooks);
    for event in FIVE_EVENTS {
        assert_eq!(
            parsed["hooks"][event][0]["hooks"][0]["command"], HOOK_COMMAND,
            "{event}"
        );
    }
    let version = format!("{}+bbbbbbbbbbbb", env!("CARGO_PKG_VERSION"));
    assert_eq!(json_file(&plugin)["version"], version.as_str());
    assert_eq!(
        json_file(&market)["plugins"][0]["version"],
        version.as_str()
    );
    assert_eq!(json_file(&market)["plugins"][0]["name"], "alice-capture");
    let calls = env.claude_calls();
    assert_eq!(
        calls[calls0.len()..].lines().collect::<Vec<_>>(),
        [
            "plugin marketplace update alice-local",
            "plugin update alice-capture@alice-local"
        ],
        "{}",
        env.log()
    );
    assert_eq!(read(&codex), codex0, "the Codex hooks print the same");
    assert!(
        env.update_leftovers().is_empty(),
        "{:?}",
        env.update_leftovers()
    );

    // 3. Its Codex hooks differ: the hooks.json setup wrote follows, with its record, and the
    //    log reminds the person to trust the hooks again.
    let codex_end = "hj_timeout=3;";
    assert!(v2.contains(codex_end));
    let v3 = with_version(&v2.replacen(codex_end, "hj_timeout=2;", 1), "cccccccccccc");
    update_to(&s, &env, &mut sess, 3, &v3);
    assert_eq!(
        codex_hooks(&env)["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"],
        2,
        "{}",
        env.log()
    );
    assert_eq!(read(&record), read(&codex));
    assert_eq!(mode(&codex), 0o600);
    assert_eq!(
        env.claude_calls(),
        calls,
        "the plugin's hooks did not change"
    );
    assert!(env.log().contains("/hooks"), "{}", env.log());

    // 4. A Codex hooks.json the person edited is never rewritten.
    let edited = format!("{}\n", read(&codex));
    std::fs::write(&codex, &edited).unwrap();
    let v4 = with_version(
        &v3.replacen("hj_timeout=2;", "hj_timeout=1;", 1),
        "dddddddddddd",
    );
    update_to(&s, &env, &mut sess, 4, &v4);
    assert_eq!(read(&env.script()), v4, "{}", env.log());
    assert_eq!(read(&codex), edited);

    // 5. `claude plugin` fails, or `claude` is missing: one line says to run setup again.
    write_mode(
        &env.bin.join("claude"),
        b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/claude-calls\"\nexit 1\n",
        0o755,
    );
    for (i, (from, to, version)) in [
        ("(v2)", "(v5)", "eeeeeeeeeeee"),
        ("(v5)", "(v6)", "ffffffffffff"),
    ]
    .into_iter()
    .enumerate()
    {
        if i == 1 {
            std::fs::remove_file(env.bin.join("claude")).unwrap();
        }
        let script = with_version(&read(&env.script()).replacen(from, to, 1), version);
        let logged = env.log().lines().count();
        update_to(&s, &env, &mut sess, 5 + i, &script);
        assert!(read(&hooks).contains(to), "{}", env.log());
        let log = env.log();
        let fresh: Vec<&str> = log
            .lines()
            .skip(logged)
            .filter(|l| l.contains("run the setup command again"))
            .collect();
        assert_eq!(fresh.len(), 1, "{log}");
        assert!(fresh[0].contains("hooks.json"), "{}", fresh[0]);
    }
}

/// Setup checks the script it downloads as the update check does: a hook script that is not
/// whole is refused, and the installed one stays.
#[tokio::test(flavor = "multi_thread")]
async fn setup_refuses_a_hook_script_that_is_not_whole() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let good = std::fs::read(env.script()).unwrap();
    let text = String::from_utf8(good.clone()).unwrap();
    let cut = text[..text.find("\nmain() {").unwrap() + 1].to_string();
    let no_marker = text.replacen(text.lines().nth(1).unwrap(), "# another script", 1);
    for (body, why) in [(cut, "cut short"), (no_marker, "marker")] {
        s.serve_hook_script(Some(script_answer(body)));
        let r = env.setup(&s.url("ann"), "", &[]);
        assert!(!r.ok(), "{why}: {r}");
        assert!(
            r.stderr.contains("not a whole Alice hook script") && r.stderr.contains(why),
            "{r}"
        );
        assert_eq!(std::fs::read(env.script()).unwrap(), good, "{why}");
        assert!(
            env.update_leftovers().is_empty(),
            "{:?}",
            env.update_leftovers()
        );
    }
}

/// The hook itself never checks: it returns at once, and its background worker does.
#[tokio::test(flavor = "multi_thread")]
async fn self_update_never_runs_in_the_fast_path() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    env.make_update_due();
    // Every request takes 1.5 s: a check made by the hook itself would hold it that long.
    s.delay_requests(1500);
    let mut sess = Session::new(&env, "fast-path", &env.root.join("work"));
    sess.turn(1, "quick");
    let fetched = hook_fetches(&s);
    let r = env.hook(&sess.input("Stop"));
    assert!(
        r.elapsed < Duration::from_millis(1000),
        "the hook took {:?}",
        r.elapsed
    );
    assert_eq!(hook_fetches(&s), fetched, "nothing fetched by the hook");
    assert!(
        !env.workers().is_empty(),
        "the worker goes on in the background"
    );
    env.wait_idle();
    assert_eq!(hook_fetches(&s), fetched + 1, "{}", env.log());
    let checked: u64 = read(&env.checked_file()).trim().parse().unwrap();
    assert!(now_secs().abs_diff(checked) < 60, "{checked}");
}

// ---- Status and uninstall ----

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_the_setup() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let r = env.capture_sh(&["status"], &[]);
    assert!(r.ok(), "{r}");
    assert!(r.stdout.contains(&s.url("ann")), "{r}");
    assert!(r.stdout.contains("token accepted"), "{r}");
    assert!(r.stdout.contains("briefing: on"), "{r}");
    assert!(!r.stdout.contains(&s.tenant("ann").token));
    // Stopped and held transcripts are listed, with how to reset one.
    let state = env.alice().join("state");
    std::fs::write(
        state.join("claude-code-s-stopped.offset"),
        "10 /tmp/x.jsonl\nsession s-stopped\ncwd \nstopped 99\n",
    )
    .unwrap();
    std::fs::write(
        state.join("claude-code-s-held.offset"),
        "0 /tmp/y.jsonl\nsession s-held\ncwd \nhold 42\n",
    )
    .unwrap();
    let r = env.capture_sh(&["status"], &[]);
    assert!(r.stdout.contains("stopped: claude-code-s-stopped"), "{r}");
    assert!(r.stdout.contains("waiting: claude-code-s-held"), "{r}");
    assert!(
        r.stdout
            .contains("~/.config/alice/state/claude-code-s-stopped.offset"),
        "{r}"
    );
}

/// After uninstall (or a failed plugin removal, or files deleted by hand) the plugin's hooks may
/// still run in open sessions. A missing script must never fail a hook: dash exits 2 on a file
/// it cannot open, and exit 2 from Stop blocks the agent.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_hook_script_never_blocks_a_session() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let hooks: serde_json::Value = serde_json::from_str(&read(
        &env.marketplace().join("alice-capture/hooks/hooks.json"),
    ))
    .unwrap();
    let command = hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let r = env.capture_sh(&["uninstall"], &[("ALICE_SETUP_ASSUME_YES", "1")]);
    assert!(r.ok(), "{r}");
    assert!(
        r.stdout.contains("restart any open Claude Code sessions"),
        "{r}"
    );
    assert!(!env.script().exists());
    let dash = ["/bin/dash", "/usr/bin/dash"]
        .into_iter()
        .find(|p| Path::new(p).exists());
    let mut shells = vec!["/bin/sh"];
    shells.extend(dash);
    let mut sess = Session::new(&env, "after-uninstall", &env.root.join("work"));
    sess.turn(1, "an open session");
    for inner in &shells {
        // The `sh` the command names, found on PATH.
        let link = env.bin.join("sh");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(inner, &link).unwrap();
        for outer in &shells {
            let mut c = env.command(outer);
            c.arg("-c").arg(&command);
            let r = env.run(
                c,
                &serde_json::to_vec(&sess.input("Stop")).unwrap(),
                Duration::from_secs(30),
            );
            assert_eq!(
                r.status.code(),
                Some(0),
                "{outer} -c with sh = {inner}: {r}"
            );
            assert!(r.stdout.is_empty(), "{r}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstall_removes_what_setup_wrote() {
    let s = Server::start(&["ann"]).await;
    let env = Env::new();
    env.install(&s);
    let mut sess = Session::new(&env, "bye", &env.root.join("work"));
    sess.turn(1, "before uninstall");
    env.hook(&sess.input("Stop"));
    env.wait_idle();
    std::fs::write(env.alice().join("paused-until"), "1\n").unwrap();
    let keep = (
        std::fs::read(env.alice().join("token")).unwrap(),
        std::fs::read(env.alice().join("url")).unwrap(),
    );
    assert!(env.alice().join("state").exists() && env.alice().join("capture.log").exists());

    let r = env.capture_sh(&["uninstall"], &[("ALICE_SETUP_ASSUME_YES", "1")]);
    assert!(r.ok(), "{r}");
    let calls = env.claude_calls();
    assert!(
        calls.contains("plugin uninstall alice-capture@alice-local"),
        "{calls}"
    );
    assert!(
        calls.contains("plugin marketplace remove alice-local"),
        "{calls}"
    );
    assert!(!env.marketplace().exists());
    for gone in [
        "alice-capture.sh",
        "state",
        "capture.conf",
        "capture.log",
        "paused-until",
        "update-checked",
    ] {
        assert!(!env.alice().join(gone).exists(), "{gone}");
    }
    assert_eq!(
        (
            std::fs::read(env.alice().join("token")).unwrap(),
            std::fs::read(env.alice().join("url")).unwrap(),
        ),
        keep,
        "token and url belong to the MCP setup"
    );
}

// ---- The scripts themselves ----

#[test]
fn the_scripts_parse_as_posix_sh_and_never_trace() {
    let dash = ["/bin/dash", "/usr/bin/dash"]
        .into_iter()
        .find(|p| Path::new(p).exists());
    let hook = read(Path::new(HOOK_SH));
    assert!(
        hook.contains("--speed-limit 2048 --speed-time 20 --max-time 300"),
        "uploads give up on a stalled link, not on a slow one"
    );
    assert!(hook.contains("--connect-timeout 1 --max-time 2"));
    assert!(
        hook.contains("curl -q -fsS --connect-timeout 5 --max-time 30 --max-filesize"),
        "the update download"
    );
    let lines: Vec<&str> = hook.lines().take(3).collect();
    assert!(lines[1].starts_with("# alice-capture-hook"), "{}", lines[1]);
    assert_eq!(
        lines[2], "# alice-capture-version: unstamped",
        "the route stamps the version"
    );
    assert!(hook.ends_with("\nmain \"$@\"\n"));
    for path in [SETUP_SH, HOOK_SH] {
        let text = read(Path::new(path));
        let mut shells = vec!["/bin/sh"];
        shells.extend(dash);
        for sh in shells {
            let out = Command::new(sh).arg("-n").arg(path).output().unwrap();
            assert!(out.status.success(), "{sh} -n {path}: {out:?}");
        }
        assert!(text.starts_with("#!/bin/sh\n"), "{path}");
        let code: Vec<&str> = text
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with('#'))
            .collect();
        assert!(code.contains(&"set -u"), "{path}: set -u");
        for l in &code {
            for banned in ["set -x", "set -o xtrace", "$'"] {
                assert!(!l.contains(banned), "{path}: `{banned}` in `{l}`");
            }
            // `[[` only as the start of a POSIX class such as `[[:space:]]`.
            assert!(
                l.match_indices("[[")
                    .all(|(i, _)| l[i + 2..].starts_with(':')),
                "{path}: `[[` in `{l}`"
            );
            for banned in ["local ", "function ", "source "] {
                assert!(!l.starts_with(banned), "{path}: `{banned}` in `{l}`");
            }
            if let Some(rest) = l.strip_prefix("export ") {
                assert!(
                    rest.starts_with("LC_ALL"),
                    "{path}: only LC_ALL is exported: {l}"
                );
            }
            // The token reaches curl only as a config line on stdin.
            if l.contains("Authorization") {
                assert!(
                    l.contains(r#"printf 'header = "Authorization: Bearer %s"\n' "$token""#),
                    "{path}: {l}"
                );
            }
            assert!(!l.contains("--header") && !l.contains("-H "), "{path}: {l}");
        }
    }
}
