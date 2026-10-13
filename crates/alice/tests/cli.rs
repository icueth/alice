//! Real processes: auto-start, start races, kill -9, stale discovery files, stop then start,
//! every subcommand, and the stdio MCP shim (review focus #3).
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
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Env {
            _dir: dir,
            home,
            work,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .current_dir(&self.work)
            .env("ALICE_HOME", &self.home)
            .env("ALICE_EMBEDDER", "fake")
            .env("ALICE_LOG", "warn")
            .env_remove("ALICE_AGENT");
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

    assert_eq!(
        e.ok(&["update", &id, "--action", "archive"]),
        format!("{id} is now archived\n")
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
    assert!(
        t.elapsed() < Duration::from_secs(8),
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
        e.cmd(args)
            .env_remove("ALICE_EMBEDDER")
            // Nothing listens on port 9 (discard): the download fails at once.
            .env("HF_ENDPOINT", "http://127.0.0.1:9")
            .output()
            .unwrap()
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
        t.elapsed() < Duration::from_secs(8),
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
    let child = e
        .cmd(&["daemon", "start"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(6));
    std::fs::remove_file(e.home.join("daemon.starting")).unwrap();
    drop(lock);
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        String::from_utf8_lossy(&o.stdout).starts_with("running (pid "),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert!(started.elapsed() >= Duration::from_secs(6));
    let err = stderr(&o);
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
