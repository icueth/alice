//! Real processes: auto-start, start races, kill -9, stale discovery files, stop then start,
//! every subcommand, and the stdio MCP shim (review focus #3).
// Unix only (M1 ruling R3): these tests probe, stop and kill the daemons they start with Unix
// signals (`libc::kill`) and read the pid from the held lock file (Windows reads `daemon.pid`
// instead; the Windows parts are unit-tested in `startup` and `service_install`).
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_alice");

/// A private ALICE_HOME plus a working directory (project `work`) next to it. Whatever daemon
/// runs for this home is stopped when the test ends, also when it fails.
struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    /// The user's home directory as the commands see it (`HOME`), where a service unit would be
    /// looked for; and the sandbox that records service commands instead of running them.
    user_home: PathBuf,
    svc: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let user_home = dir.path().join("user-home");
        let svc = dir.path().join("service-root");
        std::fs::create_dir_all(&user_home).unwrap();
        std::fs::create_dir_all(&svc).unwrap();
        Env {
            _dir: dir,
            home,
            work,
            user_home,
            svc,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        // Nothing of the developer's shell reaches the daemon: no `ALICE_*` setting or key
        // (a `TYPESAFE_API_KEY` too); only what a test sets.
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("ALICE_") {
                c.env_remove(&k);
            }
        }
        c.args(args)
            .current_dir(&self.work)
            .env("ALICE_HOME", &self.home)
            // No command of any test can reach the owner's launchd, systemd or Task Scheduler,
            // or read the owner's `~/Library/LaunchAgents`: the user's home is a temporary
            // directory and service commands are only recorded in `ALICE_SERVICE_ROOT`.
            .env("HOME", &self.user_home)
            .env("XDG_CONFIG_HOME", self.user_home.join(".config"))
            .env("ALICE_SERVICE_ROOT", &self.svc)
            .env("ALICE_EMBEDDER", "fake")
            .env("ALICE_LOG", "warn")
            .env_remove("TYPESAFE_API_KEY");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// stdout of a command that must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "alice {args:?} failed ({}):\nstdout: {}\nstderr: {}",
            o.status,
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.ok(&all);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
    }

    fn recall_count(&self, query: &str) -> usize {
        self.json(&["recall", query, "--limit", "20"])["results"]
            .as_array()
            .map_or(0, Vec::len)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.join("logs/daemon.log")).unwrap_or_default()
    }

    fn daemon_json(&self) -> Option<serde_json::Value> {
        let raw = std::fs::read_to_string(self.home.join("daemon.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn daemon_pid(&self) -> Option<i32> {
        self.daemon_json()?["pid"].as_i64().map(|p| p as i32)
    }

    /// Whether no process holds the single-instance lock (taken and released at once).
    fn lock_free(&self) -> bool {
        match std::fs::File::open(self.home.join("daemon.lock")) {
            Ok(f) => f.try_lock().is_ok(),
            Err(_) => true,
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
        // Last resort: a daemon that is still running (the test failed half-way, or stop did
        // not work) must not outlive the test.
        if !self.lock_free()
            && let Some(pid) = std::fs::read_to_string(self.home.join("daemon.lock"))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .filter(|&p| p > 1 && p != std::process::id() as i32)
        {
            // SAFETY: signalling the daemon this test started for its own home.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn eventually(mut f: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn remember_auto_starts_the_daemon_and_recall_finds_it() {
    let e = Env::new();
    // Reads never start a daemon by themselves.
    assert_eq!(e.ok(&["status"]), "not running\n");
    assert_eq!(e.json(&["status"])["running"], false);
    assert!(e.daemon_json().is_none() && e.log().is_empty());

    let out = e.ok(&[
        "remember", "--kind", "decision", "ship", "the", "single", "binary",
    ]);
    assert!(
        out.starts_with("Saved") || out.starts_with("Queued"),
        "{out}"
    );
    eventually(
        || {
            e.ok(&["recall", "single", "binary"])
                .contains("ship the single binary")
        },
        "recall",
    );
    assert!(e.ok(&["status"]).starts_with("running"));
    let hostile = e.run(&["recall", "\"NEAR(*", "a:b"]);
    assert!(
        hostile.status.success(),
        "{}",
        String::from_utf8_lossy(&hostile.stderr)
    );
}

#[test]
fn ten_concurrent_first_starts_make_one_daemon_and_lose_nothing() {
    let e = Env::new();
    let children: Vec<_> = (0..10)
        .map(|i| {
            e.cmd(&["remember", &format!("race memory number {i}")])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for c in children {
        let o = c.wait_with_output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(
            o.status.success() && (out.starts_with("Saved") || out.starts_with("Queued")),
            "{out}\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    eventually(
        || e.recall_count("race memory number") == 10,
        "all ten memories",
    );
    let log = e.log();
    assert_eq!(
        log.matches("daemon started").count(),
        1,
        "exactly one daemon acquired the lock:\n{log}"
    );
}

#[test]
fn kill_9_loses_no_acknowledged_write() {
    let e = Env::new();
    assert!(e.ok(&["daemon", "start"]).starts_with("running (pid "));
    for i in 0..5 {
        let out = e.ok(&["remember", &format!("durable fact {i}")]);
        assert!(out.starts_with("Saved"), "{out}");
    }
    let pid = e.daemon_pid().unwrap();
    // SAFETY: killing the daemon this test started.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually(|| !alive(pid), "the killed daemon to go away");
    // daemon.json still names the dead daemon: the next write starts a new one.
    assert!(e.daemon_json().is_some());
    let out = e.ok(&["remember", "after the crash"]);
    assert!(
        out.starts_with("Saved") || out.starts_with("Queued"),
        "{out}"
    );
    eventually(|| e.recall_count("durable fact") == 5, "five durable facts");
    eventually(
        || e.recall_count("after the crash") == 1,
        "the write after the crash",
    );
    assert_ne!(e.daemon_pid(), Some(pid), "a new daemon serves");
    assert_eq!(e.log().matches("daemon started").count(), 2);
}

/// One HTTP/1.1 request to the daemon on `port`; the status code, or `None` when the
/// connection broke first (the daemon was killed).
fn post(port: u16, token: &str, path: &str, body: &str) -> Option<u16> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok()?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).ok()?;
    s.write_all(body.as_bytes()).ok()?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).ok()?;
    resp.split_whitespace().nth(1)?.parse().ok()
}

/// Review finding m-8 (spec §10, crash safety): the daemon is killed (SIGKILL) while three bulk
/// ingests and forty `remember`s are in flight. After a restart every acknowledged id is there,
/// every queued write is applied, and the ingests, sent again as a capture hook would, complete
/// (an ingest resumes by `ord`).
#[test]
fn kill_9_during_bulk_ingest_and_remembers_loses_nothing_acknowledged() {
    let e = Env::new();
    assert!(e.ok(&["daemon", "start"]).starts_with("running (pid "));
    let info = e.daemon_json().unwrap();
    let port = info["port"].as_u64().unwrap() as u16;
    let pid = info["pid"].as_i64().unwrap() as i32;
    let token = std::fs::read_to_string(e.home.join("token"))
        .unwrap()
        .trim()
        .to_string();
    let words: String = (0..400).map(|i| format!(" w{i}")).collect();
    let ingest = |b: usize| {
        serde_json::json!({
            "source": {"source_type": "session", "uri": format!("crash-bulk-{b}")},
            "chunks": (0..1_000)
                .map(|i| serde_json::json!({"ord": i, "text": format!("bulk {b} chunk {i}{words}")}))
                .collect::<Vec<_>>(),
        })
        .to_string()
    };
    let bodies: Vec<String> = (0..3).map(ingest).collect();
    let ingests: Vec<_> = bodies
        .iter()
        .cloned()
        .map(|body| {
            let token = token.clone();
            std::thread::spawn(move || post(port, &token, "/api/v1/ingest", &body))
        })
        .collect();
    let remember = |i: usize| {
        e.cmd(&["--json", "remember", &format!("crash survivor number {i}")])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    // Twenty while the ingests run, twenty more still on their way when the daemon dies.
    let mut remembers: Vec<_> = (0..20).map(remember).collect();
    std::thread::sleep(Duration::from_millis(300));
    remembers.extend((20..40).map(remember));
    std::thread::sleep(Duration::from_millis(30));
    // SAFETY: killing the daemon this test started.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    let mut acknowledged = Vec::new();
    let mut queued = 0;
    for c in remembers {
        let o = c.wait_with_output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let r: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
        match r["status"].as_str() {
            Some("new" | "duplicate") => acknowledged.push(r["id"].as_str().unwrap().to_string()),
            Some("spooled") => queued += 1,
            other => panic!("unexpected reply {other:?}: {r}"),
        }
    }
    let codes: Vec<Option<u16>> = ingests.into_iter().map(|h| h.join().unwrap()).collect();
    eprintln!(
        "killed {pid}: {} remembers acknowledged, {queued} queued; ingest answers {codes:?}",
        acknowledged.len()
    );
    eventually(|| !alive(pid), "the killed daemon to go away");

    // A remember that came after the kill may already have started a new daemon.
    assert!(e.ok(&["daemon", "start"]).starts_with("running (pid "));
    assert_ne!(e.daemon_pid(), Some(pid));
    if !acknowledged.is_empty() {
        let mut args = vec!["get"];
        args.extend(acknowledged.iter().map(String::as_str));
        let got = e.json(&args);
        assert_eq!(
            got["missing"],
            serde_json::json!([]),
            "acknowledged ids lost"
        );
        assert_eq!(
            got["memories"].as_array().map(Vec::len),
            Some(acknowledged.len())
        );
    }
    for body in &bodies {
        assert_eq!(
            post(port_of(&e), &token, "/api/v1/ingest", body),
            Some(200),
            "an ingest sent again completes"
        );
    }
    let survivors = || {
        e.json(&[
            "recall",
            "crash survivor number",
            "--limit",
            "50",
            "--max-tokens",
            "100000",
        ])["results"]
            .as_array()
            .map_or(0, Vec::len)
    };
    eventually(|| survivors() == 40, "every remember, queued or not");
    assert_eq!(e.json(&["status"])["chunks"], 3_000);
}

fn port_of(e: &Env) -> u16 {
    e.daemon_json().unwrap()["port"].as_u64().unwrap() as u16
}

/// A port that answers like a web server but cannot prove the token, read until the end of
/// the request headers; everything it receives is recorded.
fn stranger() -> (u16, std::sync::Arc<std::sync::Mutex<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            record
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&req));
            let body = br#"{"service":"x","instance":"other"}"#;
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = s.write_all(body);
        }
    });
    (port, seen)
}

#[test]
fn stale_discovery_file_never_sends_the_token_to_a_stranger() {
    let e = Env::new();
    let (port, seen) = stranger();
    std::fs::write(
        e.home.join("daemon.json"),
        format!(
            "{{\"pid\":1,\"port\":{port},\"instance\":\"STALE\",\"version\":\"0\",\"started_at\":\"t\"}}"
        ),
    )
    .unwrap();
    std::fs::write(e.home.join("token"), "secret-token-value").unwrap();

    let out = e.ok(&["remember", "goes to the real daemon"]);
    assert!(
        out.starts_with("Saved") || out.starts_with("Queued"),
        "{out}"
    );
    eventually(
        || {
            e.ok(&["recall", "real daemon"])
                .contains("goes to the real daemon")
        },
        "recall after stale file",
    );

    let seen = seen.lock().unwrap().to_lowercase();
    assert!(
        seen.contains("get /api/v1/ping?nonce="),
        "the stale port was never asked to prove itself, so this test proved nothing:\n{seen}"
    );
    assert!(
        !seen.contains("authorization") && !seen.contains("secret-token-value"),
        "token leaked to a foreign process:\n{seen}"
    );
    // The real daemon was started, replaced the stale file and served the write.
    let info = e
        .daemon_json()
        .expect("the real daemon publishes daemon.json");
    assert_ne!(info["port"].as_u64(), Some(u64::from(port)));
    assert_ne!(info["instance"], "STALE");
    assert!(alive(info["pid"].as_i64().unwrap() as i32));
    assert_eq!(e.log().matches("daemon started").count(), 1);
    assert_eq!(e.json(&["status"])["pid"], info["pid"]);
}

#[test]
fn stop_waits_until_the_daemon_lets_go_so_a_restart_finds_the_data() {
    let e = Env::new();
    assert_eq!(e.ok(&["daemon", "stop"]), "not running\n");
    e.ok(&["daemon", "start"]);
    assert!(
        e.ok(&["remember", "kept across a restart"])
            .starts_with("Saved")
    );
    let pid = e.daemon_pid().unwrap();

    assert_eq!(e.ok(&["daemon", "stop"]), "stopped\n");
    // Already true when stop returns: the index is saved and the lock released.
    assert!(
        e.lock_free(),
        "stop returned while the daemon still held its lock"
    );
    assert!(e.daemon_json().is_none());
    eventually(|| !alive(pid), "the stopped daemon to exit");

    assert!(e.ok(&["daemon", "start"]).starts_with("running (pid "));
    assert_ne!(e.daemon_pid(), Some(pid));
    assert!(
        e.ok(&["recall", "restart"])
            .contains("kept across a restart")
    );
    assert_eq!(e.json(&["daemon", "stop"])["stopped"], true);
    assert_eq!(e.ok(&["daemon", "stop"]), "not running\n");
    assert_eq!(e.log().matches("daemon started").count(), 2);
}

/// A live process holds the lock but nothing is published: a daemon that is starting, or one
/// that withdrew daemon.json and is saving. `stop` waits for it instead of saying "not running".
#[test]
fn stop_waits_for_a_lock_holder_without_a_daemon_json() {
    let e = Env::new();
    // On macOS the first process a test binary starts can take ~2 s to reach `main` (measured
    // here); without this warm-up `stop` could first look after the lock is released below.
    assert!(e.ok(&["--version"]).starts_with("alice "));
    let path = e.home.join("daemon.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    lock.try_lock().unwrap();
    std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();

    let started = Instant::now();
    let child = e
        .cmd(&["daemon", "stop"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    drop(lock);
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "stopped\n");
    assert!(started.elapsed() >= Duration::from_millis(800));
    assert_eq!(e.ok(&["daemon", "stop"]), "not running\n");
}

#[test]
fn every_subcommand_round_trips() {
    let e = Env::new();
    let saved: serde_json::Value = {
        let o = e.run_with_stdin(
            &[
                "--json",
                "remember",
                "-",
                "--kind",
                "fact",
                "--title",
                "Lease renewal",
                "--topic",
                "office",
                "--importance",
                "4",
            ],
            "The office lease renews every March.\n",
        );
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    };
    // Spooled if the daemon was not up within the write wait; applied shortly after.
    assert!(
        saved["id"].is_string() || saved["status"] == "spooled",
        "{saved}"
    );
    eventually(|| e.recall_count("office lease") == 1, "the memory");
    let hit = &e.json(&["recall", "office lease"])["results"][0];
    let id = hit["id"].as_str().unwrap().to_string();
    assert_eq!(hit["project"], "work");
    assert_eq!(hit["kind"], "fact");

    let got = e.ok(&["get", &id]);
    assert!(
        got.contains("Lease renewal") && got.contains("renews every March"),
        "{got}"
    );
    let full = e.json(&["get", &id, "--provenance"]);
    assert_eq!(full["memories"][0]["memory"]["importance"], 4);
    assert_eq!(full["memories"][0]["memory"]["author"]["agent"], "cli");
    assert!(
        e.ok(&["get", "01K7NOSUCHMEMORY0000000000"])
            .contains("not found: ")
    );

    let archived = e.ok(&["update", &id, "--action", "archive"]);
    assert!(
        archived.starts_with(&format!("{id} is now archived (event "))
            && archived.contains("undo with `alice undo "),
        "{archived}"
    );
    assert_eq!(
        e.json(&["update", &id, "--action", "restore"])["memory"]["status"],
        "active"
    );

    assert!(
        e.ok(&["fact", "add", "alice", "written in", "rust"])
            .starts_with("ok (")
    );
    assert!(e.ok(&["facts", "alice"]).contains("rust"));
    assert_eq!(
        e.json(&["facts", "alice", "--timeline"])["facts"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    assert!(e.ok(&["projects"]).lines().any(|l| l.starts_with("work\t")));
    let ctx = e.json(&["context"]);
    assert_eq!(ctx["project"], "work");
    assert!(e.ok(&["daemon", "status"]).starts_with("running (pid "));
    assert!(e.ok(&["daemon", "logs"]).contains("alice daemon started"));

    // Mistakes are refused with what to do instead.
    let bad = e.run(&["remember", "--kind", "nonsense", "x"]);
    assert!(!bad.status.success());
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(
        err.contains("unknown kind") && err.contains("decision"),
        "{err}"
    );
    let empty = e.run(&["remember"]);
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("nothing to remember"));
    let blank = e.run_with_stdin(&["remember", "-"], "  \n");
    assert!(!blank.status.success());

    // An author from the environment.
    let o = e
        .cmd(&["--json", "remember", "a note by another agent"])
        .env("ALICE_AGENT", "codex")
        .output()
        .unwrap();
    assert!(o.status.success());
    let r: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let other = r["id"].as_str().unwrap();
    assert_eq!(
        e.json(&["get", other])["memories"][0]["memory"]["author"]["agent"],
        "codex"
    );
    // Set but empty means the default.
    let o = e
        .cmd(&["--json", "remember", "a note by nobody in particular"])
        .env("ALICE_AGENT", "")
        .output()
        .unwrap();
    assert!(o.status.success());
    let r: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let anon = r["id"].as_str().unwrap();
    assert_eq!(
        e.json(&["get", anon])["memories"][0]["memory"]["author"]["agent"],
        "cli"
    );

    // History: who changed what, with the event ids undo takes.
    let history = e.json(&["history", other]);
    let created = &history[0];
    assert_eq!(created["action"], "memory.create", "{history}");
    assert_eq!(created["target_id"], other);
    assert_eq!(created["actor"], "agent:codex");
    let event = created["id"].as_str().unwrap().to_string();
    let listed = e.ok(&["history", "--actor", "codex", "--limit", "1"]);
    assert!(
        listed.contains(&event) && listed.contains("memory.create") && listed.lines().count() == 1,
        "{listed}"
    );
    assert_eq!(e.ok(&["undo", &event]), "Undone: 1 change(s) reverted.\n");
    let found = e.json(&["recall", "a note by another agent", "--limit", "20"]);
    assert!(
        found["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["id"] != other),
        "the undone memory is in the trash: {found}"
    );
    let undone_by = &e.json(&["history", other])[0];
    assert_eq!(undone_by["action"], "undo:memory.create");
    assert_eq!(undone_by["actor"], "user", "the CLI acts for the user");
    let twice = e.run(&["undo", &event]);
    assert!(!twice.status.success());
    let err = String::from_utf8_lossy(&twice.stderr);
    assert!(err.contains("already undone"), "{err}");
}

/// `alice projects merge`: asks (or refuses without a terminal), moves, keeps the old name
/// working, and `alice undo` brings the project back.
#[test]
fn projects_merge_confirms_moves_keeps_the_old_name_and_undoes() {
    let e = Env::new();
    let other = e.work.parent().unwrap().join("other");
    std::fs::create_dir_all(&other).unwrap();
    e.ok(&["remember", "the quokka decision lives in the work project"]);
    let o = e
        .cmd(&["remember", "the quokka note lives in the other project"])
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let slugs = |e: &Env| -> Vec<String> {
        e.ok(&["projects"])
            .lines()
            .map(|l| l.split('\t').next().unwrap().to_string())
            .collect()
    };
    assert_eq!(slugs(&e), ["global", "other", "work"]);

    // No terminal and no --yes: refused, and nothing changed.
    let o = e.run(&["projects", "merge", "other", "work"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("--yes"), "{}", stderr(&o));
    assert_eq!(slugs(&e), ["global", "other", "work"]);
    // The refusals that name a fix.
    for (from, into, said) in [
        ("global", "work", "global"),
        ("work", "work", "same project"),
        ("nope", "work", "unknown project \"nope\""),
    ] {
        let o = e.run(&["projects", "merge", from, into, "--yes"]);
        assert!(!o.status.success());
        assert!(stderr(&o).contains(said), "{from}: {}", stderr(&o));
    }

    let out = e.ok(&["projects", "merge", "other", "work", "--yes"]);
    assert!(
        out.contains("1 memories") && out.contains("To undo: alice undo "),
        "{out}"
    );
    assert_eq!(slugs(&e), ["global", "work"]);
    // Both memories are in `work`; the old slug still names it.
    let found = e.json(&["recall", "quokka", "--project", "other", "--limit", "20"]);
    assert_eq!(found["results"].as_array().unwrap().len(), 2, "{found}");
    // A write from the old checkout lands in `work` and makes no project.
    let o = e
        .cmd(&["remember", "written from the old checkout"])
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(slugs(&e), ["global", "work"]);

    let event = out.rsplit("alice undo ").next().unwrap().trim().to_string();
    let undone = e.ok(&["undo", &event]);
    assert!(undone.starts_with("Undone: 1 change(s)"), "{undone}");
    assert_eq!(slugs(&e), ["global", "other", "work"]);
    let found = e.json(&["recall", "quokka", "--project", "other", "--limit", "20"]);
    assert_eq!(found["results"].as_array().unwrap().len(), 1, "{found}");
}

/// The pending entries in the spool.
fn spooled(e: &Env) -> usize {
    std::fs::read_dir(e.home.join("spool"))
        .map(|d| {
            d.filter_map(|f| f.ok())
                .filter(|f| f.path().extension().is_some_and(|x| x == "json"))
                .count()
        })
        .unwrap_or(0)
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Review finding I-1: a daemon that cannot start (here: alice.db is not a database) says why
/// in daemon.error, and every client names that cause and the next step at once instead of
/// "starting; retry" forever. Writes are still queued, with the reason. Once the cause is
/// fixed, the next start serves the queued write and daemon.error is gone.
#[test]
fn a_damaged_database_is_named_instead_of_starting_forever() {
    let e = Env::new();
    std::fs::write(e.home.join("alice.db"), b"X".repeat(8192)).unwrap();

    let t = Instant::now();
    let o = e.run(&["recall", "damaged"]);
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(
        err.contains("could not start")
            && err.contains("alice.db")
            && err.contains("move it aside"),
        "{err}"
    );
    // Named at once: far below the minute a silent lock holder is waited for.
    assert!(
        t.elapsed() < Duration::from_secs(40),
        "took {:?}",
        t.elapsed()
    );

    let out = e.ok(&["remember", "written while the database is damaged"]);
    assert!(out.starts_with("Queued: "), "{out}");
    assert!(
        out.contains("could not start") && out.contains("alice.db") && !out.contains("starting"),
        "the reply says why it was queued: {out}"
    );
    assert_eq!(spooled(&e), 1);

    let o = e.run(&["daemon", "start"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("alice.db"), "{}", stderr(&o));
    assert!(
        e.ok(&["status"]).contains("alice.db"),
        "status shows the last failure"
    );
    let error = std::fs::read_to_string(e.home.join("daemon.error")).unwrap();
    assert!(error.contains("alice.db"), "{error}");

    // Fixed (moved aside): the next start serves, applies the queued write and clears the file.
    std::fs::rename(e.home.join("alice.db"), e.home.join("alice.db.damaged")).unwrap();
    assert!(e.ok(&["daemon", "start"]).starts_with("running (pid "));
    assert!(!e.home.join("daemon.error").exists());
    eventually(
        || e.recall_count("database is damaged") == 1,
        "the queued write",
    );
}

/// Review finding I-1: an offline first run (the model cannot be downloaded) is named too.
#[test]
fn a_failed_model_download_is_named_with_the_next_step() {
    let e = Env::new();
    let real = |args: &[&str]| {
        let mut c = e.cmd(args);
        c.env_remove("ALICE_EMBEDDER")
            // Nothing listens on port 9 (discard): the download fails at once.
            .env("HF_ENDPOINT", "http://127.0.0.1:9")
            // A developer's proxy must not stand between the daemon and that closed port.
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost");
        for var in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            c.env_remove(var);
        }
        c.output().unwrap()
    };
    let t = Instant::now();
    let o = real(&["daemon", "start"]);
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(
        err.contains("embedding model could not be downloaded")
            && err.contains("alice model fetch"),
        "{err}"
    );
    assert!(
        t.elapsed() < Duration::from_secs(40),
        "took {:?}",
        t.elapsed()
    );

    let o = real(&["remember", "kept while offline"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.starts_with("Queued: ") && out.contains("could not be downloaded"),
        "{out}"
    );
    assert_eq!(spooled(&e), 1);
}

/// Review finding I-1: `daemon start` keeps waiting while a live process holds the lock (a
/// daemon downloading the model, say) instead of giving up after the usual wait, and says what
/// that process is doing. Here the test holds the lock and announces a download for 6 s, longer
/// than a started daemon waits for the lock, so `daemon start` must also start one again.
#[test]
fn daemon_start_waits_while_a_live_process_holds_the_lock() {
    let e = Env::new();
    let path = e.home.join("daemon.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    lock.try_lock().unwrap();
    std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(
        e.home.join("daemon.starting"),
        serde_json::json!({
            "pid": std::process::id(),
            "unix_ms": now_ms,
            "step": "downloading the embedding model",
            "file": "model_quantized.onnx",
            "done": 59_154_093u64,
            "total": 118_308_185u64,
        })
        .to_string(),
    )
    .unwrap();

    let started = Instant::now();
    let mut child = e
        .cmd(&["daemon", "start"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read what it says while it waits, and hold on until it has said what the process holds
    // the lock for AND at least 6 s have passed (longer than a daemon it started waits for the
    // lock): a condition, not a guess at how long a loaded machine needs to start the command.
    let said = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let reader = {
        let said = said.clone();
        let mut err = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                said.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        })
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while !said.lock().unwrap().contains("50%") || started.elapsed() < Duration::from_secs(6) {
        assert!(
            Instant::now() < deadline,
            "daemon start never said what the holder does: {}",
            said.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::remove_file(e.home.join("daemon.starting")).unwrap();
    drop(lock);
    let o = child.wait_with_output().unwrap();
    reader.join().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        String::from_utf8_lossy(&o.stdout).starts_with("running (pid "),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert!(started.elapsed() >= Duration::from_secs(6));
    let err = said.lock().unwrap().clone();
    assert!(
        err.contains("downloading the embedding model") && err.contains("50%"),
        "{err}"
    );
}

/// `alice recall … | head` must end quietly when the reader goes away: no panic, and no death
/// by SIGPIPE either (review finding m-1). SIGPIPE stays ignored in every command, because
/// with the default action a write to a daemon that died mid-request (a socket) would kill
/// `alice remember` on Linux before it could spool; a closed stdout ends the command quietly.
#[test]
fn a_closed_stdout_ends_the_cli_quietly() {
    let e = Env::new();
    std::fs::create_dir_all(e.home.join("logs")).unwrap();
    std::fs::write(e.home.join("logs/daemon.log"), "a log line\n".repeat(500)).unwrap();
    for args in [&["daemon", "logs"][..], &["--json", "status"], &["status"]] {
        let mut child = e
            .cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let o = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            !err.contains("panicked") && o.status.code() != Some(101),
            "{args:?}: {}: {err}",
            o.status
        );
        assert_eq!(
            o.status.code(),
            Some(0),
            "{args:?}: ended by {} (a signal, if no code), not a quiet exit: {err}",
            o.status
        );
        assert!(err.is_empty(), "{args:?}: {err}");
    }
}

/// A stand-in daemon for `home` that proves the token on ping and resets every other request
/// as soon as it has read its headers, as a daemon killed while reading a request would.
/// Returns how many requests it reset.
fn dying_daemon(home: &std::path::Path) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicUsize, Ordering};
    const INSTANCE: &str = "01K7DYINGDAEMON00000000000";
    let token = "d".repeat(64);
    std::fs::write(home.join("token"), &token).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        home.join("daemon.json"),
        serde_json::json!({"pid": std::process::id(), "port": port, "instance": INSTANCE,
            "version": "t", "started_at": "t"})
        .to_string(),
    )
    .unwrap();
    let resets = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = resets.clone();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            if let Some(nonce) = head
                .strip_prefix("GET /api/v1/ping?nonce=")
                .and_then(|r| r.split_whitespace().next())
            {
                let proof = alice_daemon::discovery::ping_proof(&token, nonce, INSTANCE);
                let body = serde_json::json!({"service": "alice", "instance": INSTANCE,
                    "version": "t", "proof": proof})
                .to_string();
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                continue;
            }
            // Linger 0: closing sends a reset, and the body still on its way hits a dead socket.
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            // SAFETY: setting a socket option on a socket this thread owns.
            unsafe {
                libc::setsockopt(
                    s.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    (&linger as *const libc::linger).cast(),
                    std::mem::size_of::<libc::linger>() as libc::socklen_t,
                );
            }
            counted.fetch_add(1, Ordering::SeqCst);
            drop(s);
        }
    });
    resets
}

/// Review finding m-1: a daemon that dies while it reads a `remember` (here: a reset while the
/// body is still being sent) leaves the client a broken socket. The write is spooled and the
/// reply says why; the command is not killed (on Linux SIGPIPE used to end it first).
#[test]
fn a_daemon_dying_mid_request_leaves_the_write_spooled() {
    let e = Env::new();
    let resets = dying_daemon(&e.home);
    let content = format!("sent to a dying daemon {}", "x".repeat(60_000));
    let o = e.run(&["remember", &content]);
    assert!(
        o.status.success(),
        "{}: {}",
        o.status,
        String::from_utf8_lossy(&o.stderr)
    );
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.starts_with("Queued: ") && out.contains("stopped answering"),
        "{out}"
    );
    assert!(resets.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert_eq!(spooled(&e), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_mcp_shim_lists_tools_and_round_trips() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
    let e = Env::new();
    let shim = |e: &Env| {
        let (home, work) = (e.home.clone(), e.work.clone());
        tokio::process::Command::new(BIN).configure(move |c| {
            c.arg("mcp")
                .current_dir(&work)
                .env("ALICE_HOME", &home)
                .env("ALICE_EMBEDDER", "fake")
                .env("ALICE_LOG", "warn");
        })
    };

    // Spawn to a completed `initialize`, several times (the first exec of a new build pays a
    // one-off OS check on macOS). The handshake never starts or contacts a daemon.
    let mut took = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let client = ().serve(TokioChildProcess::new(shim(&e)).unwrap()).await.unwrap();
        took.push(started.elapsed());
        client.cancel().await.unwrap();
    }
    took.sort();
    eprintln!(
        "alice mcp spawn -> initialize: min {:?}, median {:?}, max {:?}",
        took[0], took[2], took[4]
    );

    let client = ().serve(TokioChildProcess::new(shim(&e)).unwrap()).await.unwrap();
    assert_eq!(client.list_all_tools().await.unwrap().len(), 7);
    assert!(
        e.daemon_json().is_none() && e.log().is_empty(),
        "the shim started a daemon before any tool needed one"
    );
    let mut args = serde_json::Map::new();
    args.insert("content".into(), "stdio shim memory".into());
    let r = client
        .call_tool(CallToolRequestParams::new("remember").with_arguments(args))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{r:?}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let mut args = serde_json::Map::new();
        args.insert("query".into(), "stdio shim".into());
        let r = client
            .call_tool(CallToolRequestParams::new("recall").with_arguments(args))
            .await
            .unwrap();
        let text = format!("{:?}", r.content);
        if r.is_error != Some(true) && text.contains("stdio shim memory") {
            break;
        }
        assert!(Instant::now() < deadline, "recall through the shim: {text}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    client.cancel().await.unwrap();
    assert_eq!(e.log().matches("daemon started").count(), 1);
}

/// The shim offers the MemPalace tools while the daemon says `compat.mempalace` is on (it asks
/// the daemon, cached for a moment, and fails closed): a listing never starts a daemon, so with
/// the daemon stopped the compat tools are gone and the daemon stays stopped.
#[tokio::test(flavor = "multi_thread")]
async fn the_stdio_shim_lists_compat_tools_when_the_daemon_has_them_on() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
    let e = Env::new();
    e.ok(&["remember", "seed for the compat shim"]);
    e.ok(&["config", "set", "compat.mempalace", "true"]);
    let (home, work) = (e.home.clone(), e.work.clone());
    let client = ()
        .serve(
            TokioChildProcess::new(tokio::process::Command::new(BIN).configure(move |c| {
                c.arg("mcp")
                    .current_dir(&work)
                    .env("ALICE_HOME", &home)
                    .env("ALICE_EMBEDDER", "fake")
                    .env("ALICE_LOG", "warn");
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let count = |tools: &[rmcp::model::Tool]| {
        tools
            .iter()
            .filter(|t| t.name.starts_with("mempalace_"))
            .count()
    };
    // The shim's answer about the switches is cached for a second, so wait for it.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let tools = client.list_all_tools().await.unwrap();
        if count(&tools) == 15 {
            assert_eq!(tools.len(), 22);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no compat tools: {}",
            tools.len()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let r = client
        .call_tool(CallToolRequestParams::new("mempalace_list_wings"))
        .await
        .unwrap();
    let text = r
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert_ne!(r.is_error, Some(true), "{text}");
    assert!(text.contains("wings"), "{text}");

    // Off again in the daemon: the listing follows.
    e.ok(&["config", "set", "compat.mempalace", "false"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while count(&client.list_all_tools().await.unwrap()) != 0 {
        assert!(Instant::now() < deadline, "compat tools stayed");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // On, the daemon stopped: nothing answers, so nothing is offered, and listing starts nothing.
    e.ok(&["config", "set", "compat.mempalace", "true"]);
    e.ok(&["daemon", "stop"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !e.lock_free() {
        assert!(Instant::now() < deadline, "the daemon did not stop");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(count(&client.list_all_tools().await.unwrap()), 0);
    assert!(e.lock_free(), "listing started a daemon");
    client.cancel().await.unwrap();
}

/// `alice bench` works on throw-away data: the ALICE_HOME in use is never touched (nor accepted
/// as `--dir`), the temporary home is removed, its daemon is gone, and `--dir` data is reused.
#[test]
fn bench_runs_on_throwaway_data_and_cleans_up() {
    let env = Env::new();
    let tmp = env.home.parent().unwrap().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let bench = |args: &[&str]| {
        let mut all = vec!["--json", "bench", "--samples", "4"];
        all.extend_from_slice(args);
        env.cmd(&all).env("TMPDIR", &tmp).output().unwrap()
    };
    let out = bench(&["--chunks", "1500", "--memories", "120"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["embedder"], "fake");
    assert_eq!(
        (r["chunks"].as_u64(), r["memories"].as_u64()),
        (Some(1500), Some(120))
    );
    assert_eq!(r["reused"], false);
    assert_eq!(r["latency"]["samples"], 4);
    assert_eq!(
        r["latency"]["fallback_fired"], 4,
        "raw-only words take the fallback"
    );
    for field in [
        "recall_p95_ms",
        "recall_raw_p95_ms",
        "recall_raw_thai_p95_ms",
        "recall_raw_ident_p95_ms",
        "recall_raw_dotted_p95_ms",
        "recall_raw_filtered_p95_ms",
    ] {
        assert!(r["latency"][field].as_f64().unwrap() > 0.0, "{field}");
    }
    assert!(r["startup"]["indexed_vectors"].as_u64().unwrap() >= 1_620);
    let d = &r["daemon"];
    assert_eq!(
        (d["bursts"].as_u64(), d["burst_errors"].as_u64()),
        (Some(4), Some(0))
    );
    assert!(d["ready_ms"].as_f64().unwrap() > 0.0);
    let pid = d["pid"].as_i64().unwrap() as i32;
    assert!(!alive(pid), "the bench daemon {pid} was left running");
    assert_eq!(
        std::fs::read_dir(&tmp).unwrap().count(),
        0,
        "temporary data left behind"
    );
    assert_eq!(
        std::fs::read_dir(&env.home).unwrap().count(),
        0,
        "ALICE_HOME was touched"
    );

    let out = bench(&["--dir", env.home.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ALICE_HOME in use"));

    let kept = env.home.parent().unwrap().join("kept");
    let kept_arg = kept.to_str().unwrap();
    let small = ["--chunks", "300", "--memories", "20", "--dir", kept_arg];
    let first: serde_json::Value = serde_json::from_slice(&bench(&small).stdout).unwrap();
    assert_eq!(first["reused"], false);
    let again: serde_json::Value = serde_json::from_slice(&bench(&small).stdout).unwrap();
    assert_eq!(again["reused"], true, "the data in --dir is reused");
    assert!(kept.join("alice.db").exists(), "--dir data is kept");
    let mut real = small.to_vec();
    real.push("--real");
    let out = bench(&real);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("for the fake embedder"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // --prefill-vectors only makes sense with the real model, and its data is its own kind.
    let mut prefill = small.to_vec();
    prefill.push("--prefill-vectors");
    let out = bench(&prefill);
    assert!(!out.status.success(), "--prefill-vectors needs --real");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--real"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    prefill.push("--real");
    let out = bench(&prefill);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("for the fake embedder"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every file under `dir` with its contents, to prove a refused run changed nothing.
fn snapshot(dir: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.push((p.clone(), Vec::new()));
                stack.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// Review finding I2: `bench --dir` only ever writes to bench data. A directory that holds
/// anything without the bench marker (such as another Alice home) is refused before anything is
/// written, and bench data whose lock is held (a daemon serves it) is refused too.
#[test]
fn bench_refuses_a_dir_that_is_not_bench_data_or_is_in_use() {
    let env = Env::new();
    let bench = |dir: &std::path::Path| {
        env.cmd(&[
            "bench",
            "--samples",
            "2",
            "--chunks",
            "200",
            "--memories",
            "10",
            "--dir",
            dir.to_str().unwrap(),
        ])
        .output()
        .unwrap()
    };
    let root = env.home.parent().unwrap();

    let other_home = root.join("other-home");
    std::fs::create_dir_all(other_home.join("logs")).unwrap();
    std::fs::write(other_home.join("alice.db"), b"not bench data").unwrap();
    std::fs::write(other_home.join("token"), b"secret").unwrap();
    let before = snapshot(&other_home);
    let out = bench(&other_home);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not bench data"), "{err}");
    assert_eq!(
        snapshot(&other_home),
        before,
        "a refused --dir was modified"
    );

    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("todo.txt"), b"keep me").unwrap();
    let before = snapshot(&notes);
    assert!(!bench(&notes).status.success());
    assert_eq!(snapshot(&notes), before);

    let kept = root.join("kept");
    assert!(bench(&kept).status.success(), "a new directory is fine");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(kept.join("daemon.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let out = bench(&kept);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("in use"), "{err}");
    lock.unlock().unwrap();
    assert!(
        bench(&kept).status.success(),
        "free again once the lock is released"
    );
}

// ------------------------------------------------------------------ Task 6: alice config

#[test]
fn config_set_get_secret_and_live_reload() {
    let env = Env::new();
    env.ok(&["remember", "seed"]);
    env.ok(&["config", "set", "judge.review_threshold", "0.6"]);
    assert_eq!(
        env.json(&["config", "get", "judge.review_threshold"]),
        serde_json::json!(0.6)
    );
    let bad = env.run(&["config", "set", "judge.review_threshold", "0.99"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("judge.review_threshold"));
    let key = concat!("sk-cli-", "secret-98765");
    let out = env.run_with_stdin(
        &["config", "set-secret", "embedding_api_key"],
        &format!("{key}\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = env.ok(&["config", "list"]);
    assert!(
        list.contains("embedding_api_key: set ••••8765") && !list.contains("sk-cli-secret"),
        "{list}"
    );
    // A direct edit of config.toml applies without a restart.
    std::fs::write(env.home.join("config.toml"), "[mcp]\nadmin_tools = true\n").unwrap();
    eventually(
        || env.json(&["config", "get", "mcp.admin_tools"]) == serde_json::json!(true),
        "live reload",
    );
    let log = env.log();
    assert!(
        !log.contains("sk-cli-secret"),
        "the secret never reaches the log"
    );
}

/// `list` shows every key with its source, the problems, what waits for a restart and the
/// secrets; `unset` takes a key out of config.toml; the secret is deleted on request.
#[test]
fn config_list_unset_delete_secret_and_the_problems_shown() {
    let env = Env::new();
    env.ok(&["remember", "seed"]);
    env.ok(&["config", "set", "search.relevance_floor", "0.7"]);
    env.ok(&["config", "set", "embedding.idle_unload", "5m"]);
    let list = env.ok(&["config", "list"]);
    assert!(
        list.contains("search.relevance_floor = 0.7 (config.toml)"),
        "{list}"
    );
    assert!(
        list.contains("judge.provider = \"heuristic\"")
            || list.contains("judge.provider = heuristic"),
        "{list}"
    );
    assert!(list.contains("RESTART embedding.idle_unload"), "{list}");
    assert!(list.contains("secret jev_api_key: not set"), "{list}");
    // Unset: back to the default, and gone from the file.
    let out = env.ok(&["config", "unset", "search.relevance_floor"]);
    assert!(out.contains("applied"), "{out}");
    assert!(
        !std::fs::read_to_string(env.home.join("config.toml"))
            .unwrap()
            .contains("relevance_floor")
    );
    assert_eq!(
        env.json(&["config", "get", "search.relevance_floor"]),
        serde_json::json!(0.805)
    );
    let missing = env.run(&["config", "get", "no.such.key"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("alice config list"));
    // A broken edit: the previous value stays, and `list` shows the problem.
    std::fs::write(
        env.home.join("config.toml"),
        "[judge]\nreview_threshold = 0.9\nauto_apply_threshold = 0.6\n",
    )
    .unwrap();
    eventually(
        || {
            env.ok(&["config", "list"])
                .contains("PROBLEM judge.review_threshold")
        },
        "the problem in the list",
    );
    assert_eq!(
        env.json(&["config", "get", "judge.review_threshold"]),
        serde_json::json!(0.5)
    );
    // Secrets: set from stdin, deleted on request.
    let key = concat!("tp_cli-", "secret-abcdefgh1234");
    let out = env.run_with_stdin(
        &["config", "set-secret", "jev_api_key"],
        &format!("{key}\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("cli-secret"));
    let del = env.ok(&["config", "delete-secret", "jev_api_key"]);
    assert!(del.contains("deleted"), "{del}");
    let again = env.ok(&["config", "delete-secret", "jev_api_key"]);
    assert!(again.contains("was not set"), "{again}");
    assert!(
        !env.run(&["config", "set-secret", "github"])
            .status
            .success()
    );
    assert!(!env.run(&["config", "test", "dropbox"]).status.success());
    assert!(env.ok(&["config", "test", "embedding"]).starts_with("OK"));
}

/// A variable that is not a setting is ignored with a warning in `alice status` and
/// `alice config list`; it never sends the other settings back to their defaults.
#[test]
fn an_ignored_variable_is_a_warning_and_changes_nothing_else() {
    let env = Env::new();
    std::fs::write(
        env.home.join("config.toml"),
        "[judge]\nreview_threshold = 0.6\n",
    )
    .unwrap();
    let out = env
        .cmd(&["remember", "seed"])
        .env("ALICE_JUDGE_REVIEW_TRESHOLD", "0.7")
        .output()
        .unwrap();
    assert!(out.status.success());
    let status = env.ok(&["status"]);
    assert!(
        status.contains("warning:") && status.contains("ALICE_JUDGE_REVIEW_TRESHOLD"),
        "{status}"
    );
    let list = env.ok(&["config", "list"]);
    assert!(list.contains("ALICE_JUDGE_REVIEW_TRESHOLD"), "{list}");
    assert_eq!(
        env.json(&["config", "get", "judge.review_threshold"]),
        serde_json::json!(0.6),
        "the file still applies"
    );
}

/// Every file under `dir` (database and WAL, spool, logs, vector files) whose bytes hold any of
/// `needles`.
fn files_holding(dir: &std::path::Path, needles: &[&str]) -> Vec<String> {
    let mut bad = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let bytes = std::fs::read(&p).unwrap_or_default();
            if needles
                .iter()
                .any(|n| bytes.windows(n.len()).any(|w| w == n.as_bytes()))
            {
                bad.push(p.display().to_string());
            }
        }
    }
    bad
}

/// A secret passed to `alice remember` reaches neither the database (nor its WAL), the spool,
/// nor `daemon.log`, with the daemon running, with it away, and after the spool is applied.
/// The daemon logs at debug level here, so a line that echoed a request would show.
#[test]
fn a_planted_secret_reaches_neither_the_database_the_spool_nor_the_log() {
    let e = Env::new();
    let token = concat!("gh", "p_PlantedSecretToken0123456789abcdefABCD");
    // The part without the provider prefix: a scan for the whole token alone would miss a
    // log line that cut it.
    let core = "PlantedSecretToken0123456789abcdefABCD";
    let needles = [token, core];
    let verbose = |args: &[&str]| e.cmd(args).env("ALICE_LOG", "debug").output().unwrap();

    let o = verbose(&["remember", &format!("ci uses {token} for releases")]);
    assert!(o.status.success(), "{}", stderr(&o));
    e.ok(&["daemon", "stop"]);
    assert!(
        files_holding(&e.home, &needles).is_empty(),
        "running: {:?}",
        files_holding(&e.home, &needles)
    );

    // While the daemon is down (and may not be started) the CLI queues; the spool is clean too.
    let o = e
        .cmd(&["remember", &format!("second one with {token}")])
        .env("ALICE_NO_AUTOSTART", "1")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        String::from_utf8_lossy(&o.stdout).starts_with("Queued: "),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert_eq!(spooled(&e), 1);
    assert!(
        files_holding(&e.home, &needles).is_empty(),
        "spooled: {:?}",
        files_holding(&e.home, &needles)
    );

    // The daemon applies the spool; the second memory is stored redacted.
    let o = verbose(&["daemon", "start"]);
    assert!(o.status.success(), "{}", stderr(&o));
    eventually(
        || e.recall_count("second one with") == 1,
        "the queued write",
    );
    let found = e.json(&["recall", "second one with"]);
    assert!(
        found.to_string().contains("[REDACTED:github_token]"),
        "{found}"
    );
    e.ok(&["daemon", "stop"]);
    assert!(
        files_holding(&e.home, &needles).is_empty(),
        "after the drain: {:?}",
        files_holding(&e.home, &needles)
    );
    assert!(
        e.log().contains("listening"),
        "the log was written and scanned"
    );
}

/// Task 11: the same conflict as over HTTP, through the CLI: `alice undo` answers with a review
/// item, `alice review` shows it, `alice review resolve` settles it, `alice stats` counts.
#[test]
fn review_inbox_and_stats_through_the_cli() {
    let e = Env::new();
    let r = e.json(&["remember", "first text"]);
    let id = r["id"].as_str().unwrap().to_string();
    assert_eq!(
        e.ok(&["review"]),
        "Nothing waits for a decision.\n",
        "an empty inbox says so"
    );
    let edit = |body: &str| {
        e.json(&["update", &id, "--action", "edit", "--body", body])["event_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let first = edit("second text");
    edit("third text");
    let undo = e.ok(&["undo", &first]);
    assert!(undo.starts_with("Not undone: "), "{undo}");
    // `alice history` shows the edits the undo ran into.
    assert!(e.ok(&["history", &id]).contains("memory.edit"));

    let list = e.ok(&["review", "list"]);
    let item_id = list.split_whitespace().next().unwrap().to_string();
    assert!(
        list.contains("undo  p=-")
            && list.contains(&format!("[{id}]"))
            && list.contains("choices: apply | dismiss")
            && list.contains("undo blocked: "),
        "{list}"
    );
    let typed = e.ok(&["review", "list", "--type", "undo"]);
    // The memory may be activated between the two calls: compare without its status.
    let unstatus = |t: &str| t.replace("(pending)", "(active)");
    assert_eq!(unstatus(&typed), unstatus(&list));
    assert!(
        e.ok(&["review", "list", "--type", "merge"])
            .starts_with("Nothing waits")
    );
    let bad = e.run(&["review", "list", "--type", "nonsense"]);
    assert!(!bad.status.success());
    let err = stderr(&bad);
    assert!(
        err.contains("unknown review type") && err.contains("sync_conflict"),
        "{err}"
    );
    let shown = e.ok(&["review", "show", &item_id]);
    assert!(
        shown.contains("apply - undo anyway") && shown.contains("dismiss - keep the current state"),
        "{shown}"
    );
    let js = e.json(&["review", "show", &item_id]);
    assert_eq!(js["review_type"], "undo");

    let stats = e.json(&["stats"]);
    assert_eq!(stats["reviews_open"], 1, "{stats}");
    assert_eq!(stats["judge_provider"], "heuristic");
    let text = e.ok(&["stats"]);
    assert!(
        text.contains("reviews waiting: 1") && text.contains("judge: heuristic"),
        "{text}"
    );
    let ctx = e.ok(&["context"]);
    assert!(ctx.contains("1 item(s) wait for a decision"), "{ctx}");

    let wrong = e.run(&["review", "resolve", &item_id, "maybe"]);
    assert!(!wrong.status.success());
    assert!(
        stderr(&wrong).contains("apply, dismiss"),
        "{}",
        stderr(&wrong)
    );
    assert_eq!(
        e.ok(&["review", "resolve", &item_id, "dismiss"]),
        "Resolved: dismiss.\n"
    );
    let twice = e.run(&["review", "resolve", &item_id, "dismiss"]);
    assert!(!twice.status.success());
    assert!(
        stderr(&twice).contains("already resolved"),
        "{}",
        stderr(&twice)
    );
    assert!(e.ok(&["review"]).starts_with("Nothing waits"));
    let all = e.ok(&["review", "list", "--all"]);
    assert!(all.contains("resolved: dismiss by user"), "{all}");
    // Dismissing changed nothing.
    assert!(e.ok(&["get", &id]).contains("third text"));
    assert_eq!(e.json(&["stats"])["reviews_open"], 0);

    // The same again, this time applying the undo over the later change.
    let fourth = edit("fourth text");
    edit("fifth text");
    e.ok(&["undo", &fourth]);
    let second_item = e
        .ok(&["review"])
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    e.ok(&["review", "resolve", &second_item, "apply"]);
    assert!(e.ok(&["get", &id]).contains("third text"));
}

/// Task 14: `alice backup now|list` and `alice maintenance status|run <job>` through real
/// processes.
#[test]
fn backups_and_maintenance_jobs_through_the_cli() {
    use alice_daemon::maintenance::Job;
    let e = Env::new();
    e.ok(&["remember", "a memory worth a backup"]);
    assert!(
        e.ok(&["backup", "list"]).starts_with("No backups yet"),
        "an empty list says so"
    );
    let made = e.ok(&["backup", "now"]);
    assert!(made.starts_with("Backed up to "), "{made}");
    let list = e.json(&["backup", "list"]);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    let kind = list[0]["kind"].as_str().unwrap();
    assert!(kind == "manual" || kind == "scheduled", "{kind}");
    let file = PathBuf::from(list[0]["path"].as_str().unwrap());
    assert!(file.exists(), "{}", file.display());
    assert!(file.starts_with(e.home.join("backups")));
    // The copy is a SQLite database that holds the memory's text, and is private to its owner.
    let bytes = std::fs::read(&file).unwrap();
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    assert!(
        bytes.windows(23).any(|w| w == b"a memory worth a backup"),
        "the backup holds the memory"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&e.home.join("backups")), 0o700);
    }
    assert!(e.ok(&["backup", "list"]).contains("bytes"));

    let ran = e.ok(&["maintenance", "run", "trash_purge"]);
    assert!(ran.starts_with("done"), "{ran}");
    let report = e.json(&["maintenance", "run", "judge_retry"]);
    assert_eq!(report["outcome"], "done", "{report}");
    let bad = e.run(&["maintenance", "run", "nope"]);
    assert!(!bad.status.success());
    let err = stderr(&bad);
    assert!(
        err.contains("unknown maintenance job \"nope\"")
            && Job::names().iter().all(|j| err.contains(j)),
        "{err}"
    );

    let status = e.json(&["maintenance", "status"]);
    let jobs: Vec<&str> = status
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["job"].as_str().unwrap())
        .collect();
    assert_eq!(jobs, Job::names(), "every job is listed");
    let purge = status
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["job"] == "trash_purge")
        .unwrap();
    assert!(purge["last_run"].is_string() && purge["last_ok"].is_string());
    assert_eq!(purge["every_secs"], 86_400);
    let text = e.ok(&["maintenance", "status"]);
    assert!(
        text.contains("trash_purge") && text.contains("every 1d"),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("backup") && l.ends_with("never")),
        "{text}"
    );
}

#[test]
fn recall_precise_reaches_the_daemon() {
    let e = Env::new();
    e.ok(&["remember", "precise flag check about lease timeouts"]);
    eventually(
        || e.recall_count("lease timeouts") > 0,
        "the memory to be found",
    );
    let fast = e.json(&["recall", "lease timeouts"]);
    assert_eq!(fast["reranked"], false, "fast mode never re-ranks");
    // The offline judge cannot grade: precise mode answers as fast mode does, and says so.
    let precise = e.json(&["recall", "lease timeouts", "--precise"]);
    assert_eq!(precise["reranked"], false, "{precise}");
    assert_eq!(
        precise["results"].as_array().unwrap().len(),
        fast["results"].as_array().unwrap().len()
    );
    let text = e.ok(&["recall", "lease timeouts", "--precise"]);
    assert!(text.contains("lease timeouts"), "{text}");
    assert!(!text.contains("re-ranked by the judge"), "{text}");
    let help = e.run(&["recall", "--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("--precise") && help.contains("--fast"),
        "{help}"
    );
    // Task 21: `--fast` forces the local search, and the two flags exclude each other.
    let forced = e.json(&["recall", "lease timeouts", "--fast"]);
    assert_eq!(forced["reranked"], false, "{forced}");
    assert!(
        !e.run(&["recall", "x", "--fast", "--precise"])
            .status
            .success()
    );
}

/// Task 15: `alice restore` through real processes. The backup comes back, a write made while
/// the daemon was down is applied afterwards, the replaced database is kept, and a bad file is
/// refused before the daemon is touched.
#[test]
fn restore_round_trip_brings_back_the_backup_and_keeps_writes_made_meanwhile() {
    let env = Env::new();
    env.ok(&["remember", "alpha before the backup"]);
    let b = env.json(&["backup", "now"]);
    let name = b["name"].as_str().unwrap().to_string();
    env.ok(&["remember", "bravo after the backup"]);
    // Keys and settings are not part of a restore.
    let secrets = "[keys]\nsomething = \"kept\"\n";
    std::fs::write(env.home.join("secrets.toml"), secrets).unwrap();
    env.ok(&["daemon", "stop"]);
    // Written while the daemon is down (as during a restore): spooled, applied after it.
    let out = env
        .cmd(&["remember", "charlie during the restore"])
        .env("ALICE_NO_AUTOSTART", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(spooled(&env), 1);
    // `env.json` parses the whole stdout as one JSON value: stop's own output would break it.
    let r = env.json(&["restore", &name, "--yes"]);
    assert_eq!(r["restored_from"], name);
    assert!(
        r["caveats"].as_str().unwrap().contains("purged"),
        "the report says what a restore does not undo: {r}"
    );
    eventually(
        || env.recall_count("charlie") == 1,
        "the spooled write drained into the restored database",
    );
    assert_eq!(env.recall_count("alpha"), 1);
    assert_eq!(
        env.recall_count("bravo"),
        0,
        "written after the backup: gone with the restore"
    );
    assert_eq!(
        std::fs::read_to_string(env.home.join("secrets.toml")).unwrap(),
        secrets
    );
    let list = env.json(&["backup", "list"]);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|b| b["kind"] == "before_restore")
    );
    let status = env.json(&["status"]);
    assert_eq!(status["index_ready"], true);
    // A bad file is refused before anything changes, and before the daemon is stopped.
    let pid = env.daemon_pid().expect("the restored daemon runs");
    std::fs::write(
        env.home.join("backups/alice-20200101T000000Z.db"),
        b"garbage",
    )
    .unwrap();
    let bad = env.run(&["restore", "alice-20200101T000000Z.db", "--yes"]);
    assert!(!bad.status.success());
    assert!(
        stderr(&bad).contains("not an Alice backup"),
        "{}",
        stderr(&bad)
    );
    assert_eq!(env.recall_count("alpha"), 1, "nothing was swapped");
    assert_eq!(env.daemon_pid(), Some(pid), "the daemon was left running");
}

/// Task 15: restore needs a yes on a pipe, says what it does not undo in words, and works with
/// no daemon (and no database) at all.
#[test]
fn restore_asks_for_a_yes_and_prints_what_it_does_not_undo() {
    let env = Env::new();
    env.ok(&["remember", "delta kept in the backup"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    let no = env.run(&["restore", &name]);
    assert!(!no.status.success());
    assert!(stderr(&no).contains("--yes"), "{}", stderr(&no));
    let none = env.run(&["restore", "nope.db", "--yes"]);
    assert!(!none.status.success());
    assert!(
        stderr(&none).contains("alice backup list"),
        "{}",
        stderr(&none)
    );

    env.ok(&["daemon", "stop"]);
    std::fs::remove_file(env.home.join("alice.db")).unwrap();
    // The default is the newest backup.
    let text = env.ok(&["restore", "--yes"]);
    assert!(text.starts_with("Restored alice-manual-"), "{text}");
    assert!(
        text.contains("purged after the backup") && text.contains("redaction"),
        "{text}"
    );
    assert_eq!(env.recall_count("delta"), 1);
}

fn lock_file(path: &std::path::Path) -> std::fs::File {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    f.try_lock().expect("free");
    f
}

/// Task 15 (R4): while a restore holds the daemon lock and its marker, a write with autostart ON
/// is spooled at once (no daemon is started, no 10 s wait) and applied after the restore.
#[test]
fn a_write_during_a_restore_is_spooled_at_once_and_applied_afterwards() {
    let env = Env::new();
    env.ok(&["remember", "echo before the backup"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["daemon", "stop"]);
    let (daemon_lock, marker) = (
        lock_file(&env.home.join("daemon.lock")),
        lock_file(&env.home.join("restore.lock")),
    );
    let t = Instant::now();
    let out = env.ok(&["remember", "foxtrot during the swap"]);
    assert!(out.starts_with("Queued: "), "{out}");
    assert!(
        t.elapsed() < Duration::from_secs(6),
        "spooled at once, took {:?}",
        t.elapsed()
    );
    assert_eq!(spooled(&env), 1);
    assert!(env.daemon_json().is_none(), "no daemon was started");
    drop((daemon_lock, marker));
    env.ok(&["restore", &name, "--yes"]);
    eventually(
        || env.recall_count("foxtrot") == 1,
        "the spooled write drained after the restore",
    );
}

/// Without the marker (the daemon lock alone) a write still ends up in the spool.
#[test]
fn a_write_while_the_daemon_lock_is_held_is_spooled_not_lost() {
    let env = Env::new();
    env.ok(&["remember", "golf before"]);
    env.ok(&["daemon", "stop"]);
    let held = lock_file(&env.home.join("daemon.lock"));
    let out = env.ok(&["remember", "hotel while the lock is held"]);
    assert!(out.starts_with("Queued: "), "{out}");
    assert_eq!(spooled(&env), 1);
    drop(held);
    eventually(
        || env.recall_count("hotel") == 1,
        "the spooled write drained",
    );
}

/// Task 15: a second `alice restore` is refused at once and leaves the daemon alone.
#[test]
fn a_second_restore_is_refused_at_once() {
    let env = Env::new();
    env.ok(&["remember", "india"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    let pid = env.daemon_pid().unwrap();
    let marker = lock_file(&env.home.join("restore.lock"));
    let t = Instant::now();
    let o = env.run(&["restore", &name, "--yes"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("already running"), "{}", stderr(&o));
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(env.daemon_pid(), Some(pid));
    drop(marker);
}

/// Task 15 (R3): a backup from a newer schema is refused through the CLI, with the daemon left
/// running and the data untouched.
#[test]
fn a_backup_from_a_newer_schema_is_refused_before_the_daemon_is_stopped() {
    let env = Env::new();
    env.ok(&["remember", "juliet kept"]);
    let b = env.json(&["backup", "now"]);
    let newer = env.work.join("newer.db");
    std::fs::copy(b["path"].as_str().unwrap(), &newer).unwrap();
    rusqlite::Connection::open(&newer)
        .unwrap()
        .execute("INSERT INTO schema_migrations VALUES (99, 'x')", [])
        .unwrap();
    let pid = env.daemon_pid().unwrap();
    let o = env.run(&["restore", newer.to_str().unwrap(), "--yes"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("newer"), "{}", stderr(&o));
    assert_eq!(env.daemon_pid(), Some(pid), "the daemon was not stopped");
    assert_eq!(env.recall_count("juliet"), 1);
}

/// Task 15: the output says what leaves the active database and how to undo it, and undoing
/// works.
#[test]
fn restore_says_where_the_lost_memories_went_and_undo_brings_them_back() {
    let env = Env::new();
    env.ok(&["remember", "kilo in the backup"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["remember", "lima after the backup"]);
    let text = env.ok(&["restore", &name, "--yes"]);
    assert!(
        text.contains("no longer in the active database")
            && text.contains("alice-before-restore-")
            && text.contains("To undo: alice restore alice-before-restore-")
            && text.contains("at least 30 days"),
        "{text}"
    );
    assert_eq!(env.recall_count("lima"), 0);
    let kept = env
        .json(&["backup", "list"])
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["kind"] == "before_restore")
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["restore", &kept, "--yes"]);
    assert_eq!(env.recall_count("lima"), 1, "undone");
}

// ---------------------------------------------------------------------------------------------
// `alice import` (M3 Task 12). Palaces are synthetic (alice_core::import::fixture); the real
// ~/.mempalace is never read: HOME points at an empty temp folder.

mod import {
    use super::*;
    use alice_core::import::ImportReport;
    use alice_core::import::fixture::{DrawerSpec, build_sqlite_exact, build_sqlite_exact_wal};
    use alice_core::import::palace::DrawerMeta;

    pub struct Out {
        pub status: std::process::ExitStatus,
        pub stdout: String,
        pub stderr: String,
    }

    fn meta(wing: &str, room: &str) -> DrawerMeta {
        DrawerMeta {
            wing: wing.into(),
            room: room.into(),
            added_by: Some("claude-code".into()),
            filed_at: Some("2026-04-10T10:00:00".into()),
            ..Default::default()
        }
    }

    fn specs() -> Vec<DrawerSpec> {
        let k = |id: &str, text: &str, wing: &str, room: &str| DrawerSpec {
            id: id.into(),
            text: text.into(),
            meta: meta(wing, room),
        };
        vec![
            k(
                "k_dec",
                "decision about the v4 frame",
                "wing_project_alice",
                "decisions",
            ),
            k(
                "k_two",
                "the importer reads a snapshot, never the live palace",
                "wing_project_alice",
                "design",
            ),
            k(
                "k_three",
                "pg-ai keeps its offline stalls in the snapshot proxy",
                "pg-ai",
                "notes",
            ),
        ]
    }

    impl Env {
        /// `<tmp>/palace` with a few knowledge drawers.
        pub fn fixture_palace(&self) -> PathBuf {
            let palace = self._dir.path().join("palace");
            std::fs::create_dir_all(&palace).unwrap();
            build_sqlite_exact(&palace, &specs());
            palace
        }

        /// `alice …` with an empty HOME and no palace environment variables.
        pub fn alice(&self, args: &[&str]) -> Out {
            let fake_home = self._dir.path().join("fake-home");
            std::fs::create_dir_all(&fake_home).unwrap();
            let o = self
                .cmd(args)
                .env("HOME", &fake_home)
                .env_remove("MEMPALACE_PALACE_PATH")
                .env_remove("MEMPAL_PALACE_PATH")
                .env("ALICE_JUDGE_PROVIDER", "heuristic")
                .output()
                .unwrap();
            Out {
                status: o.status,
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            }
        }
    }

    fn p(path: &std::path::Path) -> &str {
        path.to_str().unwrap()
    }

    // ---- Task 16: concurrency and crash safety

    /// Hash of every file under `dir` (names and bytes), to prove a palace was never written.
    fn tree_hash(dir: &std::path::Path) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for n in names {
            n.file_name().hash(&mut h);
            std::fs::read(&n).unwrap().hash(&mut h);
        }
        h.finish()
    }

    impl Env {
        /// `alice …` as `alice()` runs it, but as a child left running.
        fn alice_child(&self, args: &[&str]) -> std::process::Child {
            let fake_home = self._dir.path().join("fake-home");
            std::fs::create_dir_all(&fake_home).unwrap();
            self.cmd(args)
                .env("HOME", &fake_home)
                .env_remove("MEMPALACE_PALACE_PATH")
                .env_remove("MEMPAL_PALACE_PATH")
                .env("ALICE_JUDGE_PROVIDER", "heuristic")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        }
    }

    #[test]
    fn kill_minus_nine_during_an_import_loses_nothing_and_the_rerun_finishes_exactly() {
        let e = Env::new();
        let palace_dir = e._dir.path().join("palace");
        let n = std::env::var("ALICE_TEST_BULK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000usize);
        let fx = alice_core::import::fixture::synthetic_palace(&palace_dir, n);
        let before = tree_hash(&palace_dir);

        // 1. the import runs in the background; wait until 3,000 drawers are done
        let importer =
            e.alice_child(&["--json", "import", "mempalace", "--palace", p(&palace_dir)]);
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(600),
                "import never got going"
            );
            let st = e.alice(&["--json", "import", "status"]);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&st.stdout)
                && v["state"] == "running"
                && v["done"].as_u64().unwrap_or(0) >= 3_000
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }

        // 2. twenty acknowledged remembers, then SIGKILL
        let mut acked = Vec::new();
        for i in 0..20 {
            let out = e.alice(&["--json", "remember", &format!("acked before kill {i}")]);
            assert!(out.status.success(), "{}", out.stderr);
            let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
            assert_eq!(v["status"], "new", "{v}");
            acked.push(v["id"].as_str().unwrap().to_string());
        }
        // What the import has committed so far (read while the daemon runs; the kill is next).
        let kept_before_kill: i64 = {
            let db = rusqlite::Connection::open_with_flags(
                alice_daemon::paths::AliceHome::new(e.home.clone()).db(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            // Drawer rows only (a graph triple has one too). The synthetic palace stores one row
            // per drawer, which the exact counts at the end of the test check.
            db.query_row(
                "SELECT count(*) FROM legacy_ids WHERE target_type IN ('memory', 'chunk')",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(
            kept_before_kill > 0,
            "the import had committed nothing at the kill"
        );
        let pid = e.daemon_pid().unwrap();
        // SAFETY: killing the daemon this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        eventually(|| !alive(pid), "the killed daemon to go away");
        let out = importer.wait_with_output().unwrap();
        assert!(
            !out.status.success(),
            "the client of a killed daemon cannot succeed"
        );

        // 3. any call restarts the daemon, which sweeps import-tmp; the rerun finishes
        let r = e.alice(&["--json", "recall", "acked before kill"]);
        assert!(r.status.success(), "{}", r.stderr);
        let tmp = e.home.join("import-tmp");
        let left = std::fs::read_dir(&tmp).map(|d| d.count()).unwrap_or(0);
        assert_eq!(left, 0, "import-tmp was swept at daemon start");
        let run = |label: &str| -> ImportReport {
            let o = e.alice(&["--json", "import", "mempalace", "--palace", p(&palace_dir)]);
            assert!(o.status.success(), "{label}: {}\n{}", o.stderr, o.stdout);
            let r: ImportReport = serde_json::from_str(&o.stdout).unwrap();
            assert!(r.errors.is_empty(), "{label}: {:?}", r.errors);
            r
        };
        let second = run("rerun");
        assert_eq!(second.drawers_seen as usize, n);
        // The resume kept the progress: what was committed before the kill is skipped, not redone.
        assert!(
            second.already_imported as i64 >= kept_before_kill,
            "resumed run skipped {} drawers but {kept_before_kill} were stored before the kill",
            second.already_imported
        );
        assert!(
            (second.already_imported as usize) < n,
            "the kill came after the import had finished"
        );
        // 4. the third run finds everything stored
        let third = run("third");
        assert_eq!(third.tiers.knowledge, 0);
        assert_eq!(third.tiers.raw_session_chunks, 0);
        assert_eq!(third.tiers.raw_file_chunks, 0);
        assert_eq!(third.already_imported, third.drawers_seen);
        assert_eq!(third.already_imported as usize, n);

        // stop the daemon, then look at the database on its own
        e.ok(&["daemon", "stop"]);
        let db = rusqlite::Connection::open_with_flags(
            alice_daemon::paths::AliceHome::new(e.home.clone()).db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let check: String = db
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        let count = |sql: &str| -> usize {
            db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize
        };
        for id in &acked {
            let k = db
                .query_row("SELECT count(*) FROM memories WHERE id = ?1", [id], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap();
            assert_eq!(k, 1, "acknowledged {id} is lost");
        }
        assert_eq!(
            count("SELECT count(*) FROM memories WHERE legacy_id IS NOT NULL"),
            fx.knowledge
        );
        let chunks = count("SELECT count(*) FROM chunks");
        let collapsed =
            count("SELECT count(*) FROM legacy_ids WHERE target_type = 'chunk'") - chunks;
        assert_eq!(chunks + collapsed, fx.chunk_drawers);
        assert_eq!(collapsed, fx.repeats);
        assert_eq!(tree_hash(&palace_dir), before, "the palace was not touched");
    }

    #[test]
    fn restore_during_an_import_ends_it_cleanly_and_the_rerun_finishes() {
        let e = Env::new();
        e.ok(&["remember", "alpha kept in the backup"]);
        let name = e.json(&["backup", "now"])["name"]
            .as_str()
            .unwrap()
            .to_string();
        let palace_dir = e._dir.path().join("palace");
        let fx = alice_core::import::fixture::synthetic_palace(&palace_dir, 20_000);
        let importer =
            e.alice_child(&["--json", "import", "mempalace", "--palace", p(&palace_dir)]);
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(300),
                "import never got going"
            );
            let st = e.alice(&["--json", "import", "status"]);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&st.stdout)
                && v["state"] == "running"
                && v["done"].as_u64().unwrap_or(0) >= 2_000
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let r = e.run(&["restore", &name, "--yes"]);
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        let out = importer.wait_with_output().unwrap();
        assert!(
            !out.status.success(),
            "the import client outlived the restore"
        );
        // The next call starts a daemon: it sweeps import-tmp, and the restored database holds
        // the backup's memory and none of the import.
        let recall = e.alice(&["recall", "alpha kept in the backup"]);
        assert!(recall.stdout.contains("alpha kept"), "{}", recall.stdout);
        let left = std::fs::read_dir(e.home.join("import-tmp"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(left, 0, "import-tmp was swept");
        let o = e.alice(&["--json", "import", "mempalace", "--palace", p(&palace_dir)]);
        assert!(o.status.success(), "{}\n{}", o.stderr, o.stdout);
        let rep: ImportReport = serde_json::from_str(&o.stdout).unwrap();
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(
            rep.already_imported, 0,
            "the restore took the half import away"
        );
        assert_eq!(rep.tiers.knowledge as usize, fx.knowledge);
        e.ok(&["daemon", "stop"]);
        let db = rusqlite::Connection::open_with_flags(
            alice_daemon::paths::AliceHome::new(e.home.clone()).db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let check: String = db
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        let chunk_rows: i64 = db
            .query_row(
                "SELECT count(*) FROM legacy_ids WHERE target_type = 'chunk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(chunk_rows as usize, fx.chunk_drawers);
    }

    #[test]
    fn a_target_that_cannot_be_written_fails_cleanly_and_resumes() {
        use std::os::unix::fs::PermissionsExt;
        let e = Env::new();
        let palace = e.fixture_palace();
        // The daemon is up first (it sweeps import-tmp at start), then the folder is made unwritable.
        e.ok(&["daemon", "start"]);
        let tmp = e.home.join("import-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Whatever happens next, the folder is writable again when the test ends (a failed
        // assertion would otherwise leave a directory the temp dir cannot delete).
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let _restore = Restore(tmp.clone());
        let writable = std::fs::File::create(tmp.join("probe")).is_ok();
        if writable {
            // Running as root: permissions do not bind; nothing to prove.
            eprintln!("skipped: permissions do not bind this user");
            let _ = std::fs::remove_file(tmp.join("probe"));
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let o = e.alice(&["import", "mempalace", "--palace", p(&palace)]);
        assert_eq!(o.status.code(), Some(1), "{}\n{}", o.stderr, o.stdout);
        assert!(
            o.stderr.contains("import failed") && o.stderr.contains("import-tmp"),
            "the sentence names the folder: {}",
            o.stderr
        );
        let st = e.alice(&["--json", "import", "status"]);
        let v: serde_json::Value = serde_json::from_str(&st.stdout).unwrap();
        assert_eq!(v["state"], "failed", "{v}");
        // Permissions back: the rerun succeeds.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
        let o = e.alice(&["--json", "import", "mempalace", "--palace", p(&palace)]);
        assert!(o.status.success(), "{}\n{}", o.stderr, o.stdout);
        let r: ImportReport = serde_json::from_str(&o.stdout).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.tiers.knowledge, 3);
    }

    #[test]
    fn import_dry_run_prints_the_report_and_changes_nothing() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let out = e.alice(&["import", "mempalace", "--dry-run", "--palace", p(&palace)]);
        assert!(out.status.success(), "{}", out.stderr);
        for needle in [
            "MemPalace import",
            "[dry run]",
            "Knowledge memories",
            "Wing \u{2192} project map",
        ] {
            assert!(out.stdout.contains(needle), "{needle}\n{}", out.stdout);
        }
        assert_eq!(
            e.alice(&["recall", "decision about the v4 frame"])
                .stdout
                .matches("v4 frame")
                .count(),
            0,
            "nothing was imported"
        );
    }

    #[test]
    fn import_then_recall_finds_the_old_memory_and_json_is_valid() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let out = e.alice(&["--json", "import", "mempalace", "--palace", p(&palace)]);
        assert!(out.status.success(), "{}\n{}", out.stderr, out.stdout);
        let report: ImportReport = serde_json::from_str(&out.stdout)
            .unwrap_or_else(|er| panic!("not an ImportReport ({er}): {}", out.stdout));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.tiers.knowledge, 3);
        assert!(
            e.alice(&["recall", "decision about the v4 frame"])
                .stdout
                .contains("v4 frame"),
            "the old memory is found"
        );
        let st = e.alice(&["--json", "import", "status"]);
        assert!(st.status.success(), "{}", st.stderr);
        let v: serde_json::Value = serde_json::from_str(&st.stdout).unwrap();
        assert_eq!(v["state"], "done", "{v}");
        // Again: nothing is new.
        let again = e.alice(&["--json", "import", "mempalace", "--palace", p(&palace)]);
        assert!(again.status.success(), "{}\n{}", again.stderr, again.stdout);
        let r2: ImportReport = serde_json::from_str(&again.stdout).unwrap();
        assert_eq!(r2.tiers.knowledge, 0);
        assert_eq!(r2.tiers.raw_session_chunks, 0);
        assert_eq!(r2.tiers.raw_file_chunks, 0);
        // The text form of `status` prints the finished report.
        let text = e.alice(&["import", "status"]);
        assert!(text.stdout.contains("MemPalace import"), "{}", text.stdout);
    }

    #[test]
    fn bad_inputs_exit_nonzero_with_one_sentence_on_stderr() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let o = e.alice(&["import", "mempalace", "--palace", "/nonexistent/palace"]);
        assert_eq!(o.status.code(), Some(1), "{}", o.stderr);
        assert!(o.stderr.contains("does not exist"), "{}", o.stderr);

        let o = e.alice(&[
            "import",
            "mempalace",
            "--palace",
            p(&palace),
            "--since",
            "nope",
        ]);
        assert_eq!(o.status.code(), Some(1), "{}", o.stderr);
        assert!(o.stderr.contains("--since"), "{}", o.stderr);

        let o = e.alice(&[
            "import",
            "mempalace",
            "--palace",
            p(&palace),
            "--map-wing",
            "nope",
        ]);
        assert_eq!(o.status.code(), Some(2), "{}", o.stderr);

        let fresh = Env::new();
        let o = fresh.alice(&["import", "mempalace"]);
        assert_eq!(o.status.code(), Some(1), "{}", o.stderr);
        assert!(o.stderr.contains("--palace"), "{}", o.stderr);
        assert!(
            fresh.daemon_json().is_none() && fresh.lock_free(),
            "no daemon was started for a palace that cannot be found"
        );
    }

    #[test]
    fn map_wing_overrides_reach_the_daemon() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let out = e.alice(&[
            "--json",
            "import",
            "mempalace",
            "--dry-run",
            "--palace",
            p(&palace),
            "--map-wing",
            "wing_project_alice=alice",
        ]);
        assert!(out.status.success(), "{}", out.stderr);
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["wing_map"]["wing_project_alice"]["to"], "alice", "{v}");
    }

    #[test]
    fn a_remote_embedding_provider_needs_the_explicit_flag() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let set = e.alice(&["config", "set", "embedding.provider", "ollama"]);
        assert!(set.status.success(), "{}", set.stderr);
        let o = e.alice(&["import", "mempalace", "--dry-run", "--palace", p(&palace)]);
        assert_eq!(o.status.code(), Some(1), "{}", o.stderr);
        assert!(
            o.stderr.contains("--allow-remote-embedding"),
            "{}",
            o.stderr
        );
        let o = e.alice(&[
            "import",
            "mempalace",
            "--dry-run",
            "--allow-remote-embedding",
            "--palace",
            p(&palace),
        ]);
        assert!(o.status.success(), "{}", o.stderr);
    }

    #[test]
    fn the_palace_defaults_come_from_the_environment_then_the_config() {
        let e = Env::new();
        let palace = e.fixture_palace();
        // MEMPALACE_PALACE_PATH
        let o = e
            .cmd(&["import", "mempalace", "--dry-run"])
            .env("HOME", e._dir.path().join("fake-home"))
            .env("MEMPALACE_PALACE_PATH", &palace)
            .env_remove("MEMPAL_PALACE_PATH")
            .env("ALICE_JUDGE_PROVIDER", "heuristic")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let used = |o: &std::process::Output| {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.contains(palace.canonicalize().unwrap().to_str().unwrap())
                || text.contains(palace.to_str().unwrap())
        };
        assert!(used(&o), "the report names the palace from the variable");
        // ~/.mempalace/config.json
        let h = e._dir.path().join("cfg-home");
        std::fs::create_dir_all(h.join(".mempalace")).unwrap();
        std::fs::write(
            h.join(".mempalace/config.json"),
            serde_json::json!({"palace_path": palace}).to_string(),
        )
        .unwrap();
        let o = e
            .cmd(&["import", "mempalace", "--dry-run"])
            .env("HOME", &h)
            .env_remove("MEMPALACE_PALACE_PATH")
            .env_remove("MEMPAL_PALACE_PATH")
            .env("ALICE_JUDGE_PROVIDER", "heuristic")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(used(&o), "the report names the palace from config.json");
    }

    #[test]
    fn the_same_wing_mapped_twice_is_refused() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let o = e.alice(&[
            "import",
            "mempalace",
            "--dry-run",
            "--palace",
            p(&palace),
            "--map-wing",
            "w=a",
            "--map-wing",
            "w=b",
        ]);
        assert_eq!(o.status.code(), Some(1), "{}", o.stderr);
        assert!(
            o.stderr.contains("--map-wing") && o.stderr.contains("twice"),
            "{}",
            o.stderr
        );
    }

    #[test]
    fn snapshot_refuses_destinations_in_or_around_the_palace_and_the_mempalace_home() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let fake_home = e._dir.path().join("fake-home");
        let mp = fake_home.join(".mempalace");
        std::fs::create_dir_all(&mp).unwrap();
        let link = e._dir.path().join("link-to-palace");
        std::os::unix::fs::symlink(&palace, &link).unwrap();
        let entries = |d: &std::path::Path| {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .map(|x| x.unwrap().file_name())
                .collect();
            v.sort();
            v
        };
        let (pal_before, mp_before) = (entries(&palace), entries(&mp));
        let cases: Vec<PathBuf> = vec![
            palace.clone(),
            palace.join("snap"),
            palace.join("a/b/c"),
            link.join("via-link"),
            link.clone(),
            mp.clone(),
            mp.join("snap"),
            mp.join("deep/er"),
            palace.join("x/../snap"),
        ];
        for out in cases {
            let o = e.alice(&[
                "import",
                "snapshot",
                "--palace",
                p(&palace),
                "--out",
                p(&out),
            ]);
            assert_eq!(o.status.code(), Some(1), "{}: {}", out.display(), o.stdout);
            assert!(
                o.stderr.contains("palace folder") || o.stderr.contains("MemPalace home"),
                "{}: {}",
                out.display(),
                o.stderr
            );
        }
        assert_eq!(
            entries(&palace),
            pal_before,
            "nothing was created in the palace"
        );
        assert_eq!(
            entries(&mp),
            mp_before,
            "nothing was created in ~/.mempalace"
        );
        assert!(!palace.join("snap").exists() && !mp.join("snap").exists());
        // A palace under ~/.mempalace protects the whole home, too.
        let inner = mp.join("palace");
        std::fs::create_dir_all(&inner).unwrap();
        build_sqlite_exact(&inner, &specs());
        let o = e.alice(&[
            "import",
            "snapshot",
            "--palace",
            p(&inner),
            "--out",
            p(&mp.join("other")),
        ]);
        assert_eq!(o.status.code(), Some(1), "{}", o.stderr);
        assert!(!mp.join("other").exists());
    }

    #[test]
    fn snapshot_never_overwrites_a_file_in_the_destination() {
        let e = Env::new();
        let palace = e.fixture_palace();
        let out_dir = e._dir.path().join("snap");
        let first = e.alice(&[
            "import",
            "snapshot",
            "--palace",
            p(&palace),
            "--out",
            p(&out_dir),
        ]);
        assert!(first.status.success(), "{}", first.stderr);
        let copy = out_dir.join("sqlite_exact.sqlite3");
        let before = std::fs::read(&copy).unwrap();
        let again = e.alice(&[
            "import",
            "snapshot",
            "--palace",
            p(&palace),
            "--out",
            p(&out_dir),
        ]);
        assert_eq!(again.status.code(), Some(1), "{}", again.stdout);
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            before,
            "the first copy is intact"
        );
        // A foreign file already at the target name is left alone too.
        let other = e._dir.path().join("snap2");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("sqlite_exact.sqlite3"), b"mine").unwrap();
        let o = e.alice(&[
            "import",
            "snapshot",
            "--palace",
            p(&palace),
            "--out",
            p(&other),
        ]);
        assert_eq!(o.status.code(), Some(1), "{}", o.stdout);
        assert_eq!(
            std::fs::read(other.join("sqlite_exact.sqlite3")).unwrap(),
            b"mine"
        );
    }

    #[test]
    fn the_hidden_snapshot_command_copies_through_the_audited_path_and_leaves_the_source_alone() {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let e = Env::new();
        let src = e._dir.path().join("wal-palace");
        std::fs::create_dir_all(&src).unwrap();
        let mut wal = build_sqlite_exact_wal(&src, &specs());
        wal.release_writer();
        let main = src.join("sqlite_exact.sqlite3");
        let hash = |f: &std::path::Path| {
            let mut h = DefaultHasher::new();
            std::fs::read(f).unwrap().hash(&mut h);
            h.finish()
        };
        let before = hash(&main);
        let out_dir = e._dir.path().join("snap");
        let o = e.alice(&[
            "import",
            "snapshot",
            "--palace",
            p(&src),
            "--out",
            p(&out_dir),
        ]);
        assert!(o.status.success(), "{}", o.stderr);
        assert!(o.stdout.contains("sqlite_exact.sqlite3"), "{}", o.stdout);
        let copy = rusqlite::Connection::open_with_flags(
            out_dir.join("sqlite_exact.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let n: i64 = copy
            .query_row(
                "SELECT count(*) FROM documents WHERE collection_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(hash(&main), before, "the source's main file is untouched");
        assert!(e.daemon_json().is_none(), "snapshot needs no daemon");
        // Hidden from --help and the README.
        let help = e.alice(&["import", "--help"]);
        assert!(
            !help
                .stdout
                .lines()
                .any(|l| l.trim_start().starts_with("snapshot")),
            "{}",
            help.stdout
        );
        let readme =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
                .unwrap_or_default();
        assert!(!readme.contains("import snapshot"));
    }
}

// ------------------------------------------------------------------ Task 17

/// `alice service install` writes only under the directory it is given, and runs no command; a
/// dry run writes nothing at all.
#[test]
fn service_install_writes_only_under_the_target_dir_and_dry_run_writes_nothing() {
    let env = Env::new();
    let target = env.home.parent().unwrap().join("svc-root");
    let t = target.to_str().unwrap();
    let dry = env.ok(&["service", "install", "--dry-run", "--target-dir", t]);
    assert!(dry.contains("daemon") && !target.exists(), "{dry}");
    let out = env.ok(&["service", "install", "--target-dir", t]);
    assert!(out.contains("not run"), "{out}");
    let installed = if cfg!(target_os = "macos") {
        target.join("Library/LaunchAgents/dev.alice.daemon.plist")
    } else {
        target.join(".config/systemd/user/alice.service")
    };
    let unit = std::fs::read_to_string(&installed).unwrap();
    assert!(unit.contains(&env.home.display().to_string()), "{unit}");
    assert!(unit.contains("daemon") && unit.contains("run"), "{unit}");
    // No secret, no token, no address of the owner's.
    for word in ["token", "secret", "@"] {
        assert!(!unit.to_ascii_lowercase().contains(word), "{word}: {unit}");
    }
    assert!(
        !target.join("service-commands.log").exists(),
        "--target-dir runs nothing"
    );
    let dry = env.ok(&["service", "uninstall", "--dry-run", "--target-dir", t]);
    assert!(
        dry.contains(&format!("Would remove {}", installed.display())),
        "{dry}"
    );
    assert!(installed.exists(), "a dry run removes nothing");
    env.ok(&["service", "uninstall", "--target-dir", t]);
    assert!(!installed.exists());
    // The JSON of a dry run is the plan.
    let plan = env.json(&["service", "install", "--dry-run", "--target-dir", t]);
    assert_eq!(plan["dry_run"], true);
    assert!(
        plan["plan"]["files"]
            .as_array()
            .is_some_and(|f| f.len() <= 1)
    );
    assert!(!installed.exists(), "the JSON dry run wrote nothing either");
}

/// `alice doctor` reports the checks, and exits non-zero when one fails.
#[test]
fn doctor_reports_and_exits_non_zero_on_failure() {
    let env = Env::new();
    env.ok(&["remember", "doctor seed"]);
    let report = env.json(&["doctor"]);
    let checks = report["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .any(|c| c["name"] == "database" && c["status"] == "ok"),
        "{checks:?}"
    );
    // The CLI tests run with ALICE_EMBEDDER=fake and no model: a warning, not a failure.
    assert!(
        checks
            .iter()
            .any(|c| c["name"] == "embedding model" && c["status"] == "warn"),
        "{checks:?}"
    );
    // The text form: one line per check.
    let text = env.ok(&["doctor"]);
    assert!(
        text.contains("OK   database:") && text.contains("WARN embedding model:"),
        "{text}"
    );
    std::fs::write(
        env.home.join("config.toml"),
        "[judge]\nreview_threshold = 5\n",
    )
    .unwrap();
    let o = env.run(&["doctor"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("FAIL config"));
    assert!(stderr(&o).contains("failed"), "{}", stderr(&o));
}

/// With no daemon, `alice doctor` starts none and says so; a home directory others can read is
/// a failure that names the command to fix it.
#[test]
fn doctor_does_not_start_a_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    // The test made the home with the default mode (0755): a daemon would make it 0700.
    let o = env.run(&["--json", "doctor"]);
    assert!(!o.status.success());
    let report: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let perm = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "home permissions")
        .unwrap();
    assert_eq!(perm["status"], "fail");
    assert!(
        perm["fix"].as_str().unwrap().starts_with("chmod 700 "),
        "{perm}"
    );
    std::fs::set_permissions(&env.home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let report = env.json(&["doctor"]);
    let daemon = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "daemon")
        .unwrap();
    assert_eq!(daemon["status"], "warn");
    assert!(
        daemon["fix"]
            .as_str()
            .unwrap()
            .contains("alice daemon start")
    );
    assert!(env.daemon_json().is_none() && !env.home.join("daemon.lock").exists());
}

/// The status of a raw `GET /api/v1/health` with `token` (no HTTP client library in this crate).
fn health_status(port: u64, token: &str) -> u16 {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    s.read_to_string(&mut reply).unwrap();
    reply
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {reply:?}"))
}

fn token_of(env: &Env) -> String {
    std::fs::read_to_string(env.home.join("token"))
        .unwrap()
        .trim()
        .to_string()
}

/// `alice token rotate` with a running daemon: it switches at once, the old token is refused,
/// the file is private, the token is not printed unless asked, and the CLI keeps working.
#[test]
fn token_rotate_switches_a_running_daemon_at_once() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.ok(&["remember", "before the rotation"]);
    let port = env.daemon_json().unwrap()["port"].as_u64().unwrap();
    let old = token_of(&env);
    assert_eq!(health_status(port, &old), 200);
    let out = env.ok(&["token", "rotate"]);
    assert!(out.starts_with("Token rotated."), "{out}");
    let new = token_of(&env);
    assert_ne!(new, old);
    assert!(
        !out.contains(&new) && !out.contains(&old),
        "the token is not printed: {out}"
    );
    assert_eq!(
        health_status(port, &old),
        401,
        "the old token is refused at once"
    );
    assert_eq!(health_status(port, &new), 200);
    assert_eq!(
        std::fs::metadata(env.home.join("token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    // The same daemon, found again with the new token.
    assert!(env.ok(&["status"]).starts_with("running"));
    assert_eq!(env.recall_count("rotation"), 1);
    // `--show` prints it, once; the JSON form says only that it rotated unless asked.
    let shown = env.ok(&["token", "rotate", "--show"]);
    let newer = token_of(&env);
    assert!(shown.contains(&newer) && newer != new, "{shown}");
    let json = env.json(&["token", "rotate"]);
    assert_eq!(json, serde_json::json!({ "rotated": true }));
}

/// With no daemon the file is replaced under the daemon lock; the next daemon starts with it.
#[test]
fn token_rotate_without_a_daemon_replaces_the_file_and_the_next_start_uses_it() {
    let env = Env::new();
    env.ok(&["remember", "seed"]);
    env.ok(&["daemon", "stop"]);
    let old = token_of(&env);
    let out = env.ok(&["token", "rotate"]);
    assert!(out.starts_with("Token rotated."), "{out}");
    let new = token_of(&env);
    assert_ne!(new, old);
    assert!(env.daemon_json().is_none(), "no daemon was started");
    assert!(env.ok(&["daemon", "start"]).starts_with("running (pid "));
    let port = env.daemon_json().unwrap()["port"].as_u64().unwrap();
    assert_eq!(health_status(port, &new), 200);
    assert_eq!(health_status(port, &old), 401);
}

/// A process that holds the lock but does not answer (a daemon still starting) is not rotated
/// under: nothing changes and the message says why.
#[test]
fn token_rotate_refuses_while_a_daemon_that_does_not_answer_holds_the_lock() {
    let env = Env::new();
    env.ok(&["remember", "seed"]);
    env.ok(&["daemon", "stop"]);
    let old = token_of(&env);
    let lock = lock_file(&env.home.join("daemon.lock"));
    let o = env.run(&["token", "rotate"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("does not answer") && stderr(&o).contains("nothing was changed"),
        "{}",
        stderr(&o)
    );
    assert_eq!(token_of(&env), old);
    drop(lock);
}

/// `alice status` says a holder that has been running for a long time and announces nothing is
/// "running (not answering)", where a young one is "starting".
#[test]
fn status_tells_a_silent_old_holder_from_a_starting_one() {
    use std::io::Write as _;
    let env = Env::new();
    let mut lock = lock_file(&env.home.join("daemon.lock"));
    lock.set_len(0).unwrap();
    writeln!(lock, "{}", std::process::id()).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let write_pid = |age_ms: u64| {
        std::fs::write(
            env.home.join("daemon.pid"),
            serde_json::json!({ "pid": std::process::id(), "unix_ms": now_ms - age_ms })
                .to_string(),
        )
        .unwrap();
    };
    write_pid(1_000);
    let young = env.ok(&["status"]);
    assert!(young.starts_with("starting (pid "), "{young}");
    write_pid(5 * 60_000);
    let old = env.ok(&["status"]);
    assert!(
        old.starts_with("running (pid ") && old.contains("not answering"),
        "{old}"
    );
    let j = env.json(&["status"]);
    assert_eq!(j["not_answering"], std::process::id());
    assert!(j["starting"].is_null(), "{j}");
    // With a download announced it is still starting, however long it has run.
    std::fs::write(
        env.home.join("daemon.starting"),
        serde_json::json!({"pid": std::process::id(), "unix_ms": now_ms, "step": "downloading the embedding model"}).to_string(),
    )
    .unwrap();
    assert!(env.ok(&["status"]).starts_with("starting (pid "));
    drop(lock);
}

/// `alice status` shows the judge, the reviews, the last backup and the memory in use.
#[test]
fn status_shows_the_judge_the_last_backup_and_the_memory() {
    let env = Env::new();
    env.ok(&["remember", "status seed"]);
    env.ok(&["backup", "now"]);
    let text = env.ok(&["status"]);
    assert!(text.contains("judge: heuristic (backlog "), "{text}");
    assert!(text.contains("last backup: 20"), "{text}");
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert!(text.contains("memory in use: "), "{text}");
    }
    let j = env.json(&["status"]);
    assert!(j["last_backup"].as_str().is_some() && j["reviews_open"].is_number());
}

/// What the recording runner of the service sandbox saw, one command per line.
fn service_commands(root: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(root.join("service-commands.log"))
        .unwrap_or_default()
        .lines()
        // The questions asked of the manager (is it loaded?) are not actions.
        .filter(|l| {
            !["launchctl print", "show -p", "/Query"]
                .iter()
                .any(|p| l.contains(p))
        })
        .map(String::from)
        .collect()
}

/// `alice restore` stops an installed service unit before the swap and starts it afterwards (a
/// unit that restarts its daemon would start one mid-restore); with no unit it runs no command.
/// The service manager is never called: `ALICE_SERVICE_ROOT` records the commands instead.
#[test]
fn restore_stops_the_installed_service_unit_and_starts_it_again() {
    let env = Env::new();
    let root = env.svc.clone();
    let with_root = |args: &[&str]| env.run(args);
    env.ok(&["remember", "kept in the backup"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();

    // No unit: restore runs no service command.
    let o = with_root(&["restore", &name, "--yes"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        service_commands(&root).is_empty(),
        "{:?}",
        service_commands(&root)
    );

    // Installed (the sandbox writes the unit and records the commands).
    let o = with_root(&["service", "install"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let after_install = service_commands(&root).len();
    assert!(after_install >= 1);
    let o = with_root(&["restore", &name, "--yes"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let all = service_commands(&root);
    let (stop, start) = if cfg!(target_os = "macos") {
        ("launchctl bootout gui/", "launchctl bootstrap gui/")
    } else {
        (
            "systemctl --user stop alice.service",
            "systemctl --user start alice.service",
        )
    };
    let during = &all[after_install..];
    assert_eq!(during.len(), 2, "{during:?}");
    assert!(during[0].starts_with(stop), "{during:?}");
    assert!(during[1].starts_with(start), "{during:?}");
    assert_eq!(env.recall_count("backup"), 1, "the restore itself worked");
}

/// Scripted answers of the sandbox's service manager (`service-responses`): the probe says the
/// unit is stopped (launchd: not found, 113; systemd: `inactive`), or cannot be asked.
fn script_probe(svc: &std::path::Path, answer: &str) {
    let line = match (cfg!(target_os = "macos"), answer) {
        (true, "stopped") => "launchctl print\t113\t",
        (true, _) => "launchctl print\t5\t",
        (false, "stopped") => "show -p\t0\tinactive\\ndead\\n",
        (false, _) => "show -p\t1\t",
    };
    std::fs::write(svc.join("service-responses"), format!("{line}\n")).unwrap();
}

/// The service command words that stop and start the unit on this platform.
fn stop_start_words() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("bootout", "bootstrap")
    } else {
        ("--user stop", "--user start")
    }
}

/// A restore of one data directory never touches a unit that serves another (the owner's
/// production daemon, say): no service command is run for it.
#[test]
fn restore_leaves_a_unit_for_another_home_alone() {
    let env = Env::new();
    // A unit for `env.home` is installed (recorded, not run).
    env.ok(&["service", "install"]);
    let installed = service_commands(&env.svc);
    assert!(!installed.is_empty());
    // Restore a different data directory with the same user home and service root.
    let other = env.home.parent().unwrap().join("other-home");
    std::fs::create_dir_all(&other).unwrap();
    let run = |args: &[&str]| {
        let mut c = env.cmd(args);
        c.env("ALICE_HOME", &other);
        c.output().unwrap()
    };
    assert!(
        run(&["remember", "kept in the other home"])
            .status
            .success()
    );
    let b = run(&["--json", "backup", "now"]);
    let name = serde_json::from_slice::<serde_json::Value>(&b.stdout).unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    let o = run(&["restore", &name, "--yes"]);
    run(&["daemon", "stop"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(
        service_commands(&env.svc),
        installed,
        "no service command was run for another home's restore"
    );
}

/// `alice service install` creates the home and its log directory, which the unit's output
/// goes to; without it launchd and systemd fail the job at start.
#[test]
fn service_install_creates_the_log_directory_of_the_home() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    assert!(!env.home.join("logs").exists());
    env.ok(&["service", "install"]);
    assert!(env.home.join("logs").is_dir());
    assert_eq!(
        std::fs::metadata(&env.home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let logs = std::fs::metadata(env.home.join("logs"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        logs & 0o700,
        0o700,
        "the owner can write the log directory: {logs:o}"
    );
    // A dry run creates nothing.
    let fresh = Env::new();
    fresh.ok(&["service", "install", "--dry-run"]);
    assert!(!fresh.home.join("logs").exists());
}

/// A loaded unit that cannot be stopped refuses the restore with nothing changed; one that is not
/// loaded is left alone and the restore goes ahead.
#[test]
fn restore_is_refused_when_the_unit_cannot_be_stopped_and_a_daemon_runs() {
    let env = Env::new();
    let (stop, start) = stop_start_words();
    env.ok(&["service", "install"]);
    env.ok(&["remember", "alpha before the backup"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["remember", "beta after the backup"]);
    let before = service_commands(&env.svc).len();
    let mut c = env.cmd(&["restore", &name, "--yes"]);
    c.env("ALICE_SERVICE_FAIL", stop);
    let o = c.output().unwrap();
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("could not be stopped") && stderr(&o).contains("nothing was changed"),
        "{}",
        stderr(&o)
    );
    let log = service_commands(&env.svc);
    assert_eq!(log.len(), before + 1, "only the failed stop: {log:?}");
    assert!(!log[before..].iter().any(|l| l.contains(start)), "{log:?}");
    assert_eq!(env.recall_count("beta"), 1, "nothing was restored");
    // A unit the manager does not have loaded (the probe fails, whatever the language): nothing
    // to stop, whether a daemon runs or not; the restore goes on and starts nothing afterwards.
    let before = service_commands(&env.svc).len();
    let mut c = env.cmd(&["restore", &name, "--yes"]);
    script_probe(&env.svc, "stopped");
    let o = c.output().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(
        service_commands(&env.svc).len(),
        before,
        "an unloaded unit is neither stopped nor started"
    );
}

/// A restore whose swap fails still starts the unit again, and when that fails too both errors
/// are shown with the command to start the service.
#[test]
fn a_failed_swap_still_resumes_the_unit_and_a_failed_resume_is_reported() {
    let env = Env::new();
    let (_, start) = stop_start_words();
    env.ok(&["service", "install"]);
    env.ok(&["remember", "alpha"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["daemon", "stop"]);
    // The staged file's name is taken by a directory: the swap cannot stage the backup.
    let staged = env.home.join(".alice-restore.tmp");
    std::fs::create_dir(&staged).unwrap();
    std::fs::write(staged.join("keep"), b"x").unwrap();
    let before = service_commands(&env.svc).len();
    let o = env.run(&["restore", &name, "--yes"]);
    assert!(
        !o.status.success(),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let log = service_commands(&env.svc);
    assert!(
        log[before..].iter().any(|l| l.contains(start)),
        "the unit was started again after the failed swap: {log:?}"
    );
    // And when the start fails too, the error says both.
    let mut c = env.cmd(&["restore", &name, "--yes"]);
    c.env("ALICE_SERVICE_FAIL", start);
    let o = c.output().unwrap();
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(
        err.contains("could not be started again") && err.contains("alice service install"),
        "{err}"
    );
}

/// The terminal's echo is back on after `config set-secret` is killed in the middle of the
/// prompt, by SIGTERM and by SIGINT.
#[test]
fn a_killed_secret_prompt_puts_the_terminal_echo_back() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;
    let env = Env::new();
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // A pseudo-terminal pair; the child's stdin is the slave side.
        // SAFETY: plain libc calls; the descriptors are owned by the OwnedFds from then on.
        let (master, slave) = unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(m >= 0 && libc::grantpt(m) == 0 && libc::unlockpt(m) == 0);
            let name = libc::ptsname(m);
            assert!(!name.is_null());
            let s = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
            assert!(s >= 0);
            (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
        };
        let echo_on = |fd: &OwnedFd| unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd.as_raw_fd(), &mut t), 0);
            t.c_lflag & libc::ECHO != 0
        };
        assert!(echo_on(&slave), "a new terminal echoes");
        let mut child = env
            .cmd(&["config", "set-secret", "jev_api_key"])
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // The prompt is up when echo goes off (the command starts a daemon first).
        let deadline = Instant::now() + Duration::from_secs(90);
        while echo_on(&slave) {
            assert!(
                Instant::now() < deadline,
                "the prompt never turned echo off"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "the command ended early"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: signalling the child this test started.
        unsafe { libc::kill(child.id() as i32, sig) };
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(sig), "ended by the signal: {status}");
        assert!(echo_on(&slave), "echo is back after signal {sig}");
        drop(master);
    }
    // And after a normal answer.
    // SAFETY: as above.
    let (master, slave) = unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0 && libc::grantpt(m) == 0 && libc::unlockpt(m) == 0);
        let s = libc::open(libc::ptsname(m), libc::O_RDWR | libc::O_NOCTTY);
        (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
    };
    let echo_on = |fd: &OwnedFd| unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(fd.as_raw_fd(), &mut t), 0);
        t.c_lflag & libc::ECHO != 0
    };
    let mut child = env
        .cmd(&["config", "set-secret", "jev_api_key"])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while echo_on(&slave) {
        assert!(
            Instant::now() < deadline,
            "the prompt never turned echo off"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let answer = concat!("ts_", "pty_secret_value\n");
    // SAFETY: writing to the master side we own.
    let wrote = unsafe { libc::write(master.as_raw_fd(), answer.as_ptr().cast(), answer.len()) };
    assert_eq!(wrote as usize, answer.len());
    assert!(child.wait().unwrap().success());
    assert!(echo_on(&slave), "echo is back after the answer");
}

/// In sandbox mode (`ALICE_SERVICE_ROOT`) `service` and `restore` say that nothing is run.
#[test]
fn the_service_sandbox_says_so_on_stderr() {
    let env = Env::new();
    let o = env.run(&["service", "install"]);
    assert!(o.status.success());
    let want = format!(
        "service commands are recorded in {}, not run (ALICE_SERVICE_ROOT is set)",
        env.svc.display()
    );
    assert_eq!(stderr(&o).matches(&want).count(), 1, "{}", stderr(&o));
    env.ok(&["remember", "x"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    let o = env.run(&["restore", &name, "--yes"]);
    assert!(o.status.success());
    assert!(stderr(&o).contains(&want), "{}", stderr(&o));
    // And an autostarted daemon does not call the variable a typo.
    let log = std::fs::read_to_string(env.home.join("logs/daemon.log")).unwrap_or_default();
    assert!(!log.contains("ALICE_SERVICE_ROOT"), "{log}");
}

/// `alice service status` names the home the unit serves, and says when it is another one.
#[test]
fn service_status_names_the_home_the_unit_serves() {
    let env = Env::new();
    assert_eq!(env.ok(&["service", "status"]).trim(), "not installed");
    env.ok(&["service", "install"]);
    let text = env.ok(&["service", "status"]);
    assert!(
        text.starts_with("installed (") && text.contains(&format!("for {}", env.home.display())),
        "{text}"
    );
    let other = env.home.parent().unwrap().join("elsewhere");
    let o = env
        .cmd(&["service", "status"])
        .env("ALICE_HOME", &other)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        text.contains("serves") && text.contains("not this data directory"),
        "{text}"
    );
    let j = env.json(&["service", "status"]);
    assert_eq!(j["serves_this_home"], true);
}

/// `--target-dir` still prepares the home's own directories, not the target's.
#[test]
fn service_install_with_a_target_dir_creates_the_homes_logs_not_the_targets() {
    let env = Env::new();
    let target = env.home.parent().unwrap().join("elsewhere-root");
    env.ok(&[
        "service",
        "install",
        "--target-dir",
        target.to_str().unwrap(),
    ]);
    assert!(env.home.join("logs").is_dir());
    assert!(!target.join("logs").exists());
}

/// A manager that cannot be asked (no user bus, no gui domain) refuses the restore with nothing
/// changed and no command but the probe; a stopped unit is neither stopped nor started.
#[test]
fn restore_refuses_when_the_manager_cannot_be_asked_and_never_starts_a_stopped_unit() {
    let env = Env::new();
    env.ok(&["service", "install"]);
    env.ok(&["remember", "alpha before the backup"]);
    let name = env.json(&["backup", "now"])["name"]
        .as_str()
        .unwrap()
        .to_string();
    env.ok(&["remember", "beta after the backup"]);
    let before = service_commands(&env.svc).len();
    script_probe(&env.svc, "unknown");
    let o = env.run(&["restore", &name, "--yes"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("by hand") && stderr(&o).contains("nothing was changed"),
        "{}",
        stderr(&o)
    );
    assert_eq!(
        service_commands(&env.svc).len(),
        before,
        "no command but the probe"
    );
    assert_eq!(env.recall_count("beta"), 1, "nothing was restored");
    // Stopped: the restore goes ahead, with no service command at all.
    script_probe(&env.svc, "stopped");
    let o = env.run(&["restore", &name, "--yes"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(service_commands(&env.svc).len(), before);
    // Uninstall with a manager that cannot be asked: refused, the unit file stays.
    script_probe(&env.svc, "unknown");
    let o = env.run(&["service", "uninstall"]);
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stdout).contains("Service removed"));
    assert!(
        env.run(&["service", "status"])
            .stdout
            .starts_with(b"installed"),
        "the unit is still installed"
    );
}
