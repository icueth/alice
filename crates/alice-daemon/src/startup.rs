//! What a starting daemon tells clients before it serves, and who holds the daemon lock.
//!
//! - `daemon.starting`: the step a daemon is on (with download progress), written while it
//!   starts and removed once it serves or fails. Only meaningful while its `pid` is alive.
//! - `daemon.error`: why the last start failed, in the daemon's words (the cause and what to
//!   do). Written before the failed daemon releases its lock; removed when the next daemon
//!   takes the lock and again when it serves (publishes daemon.json).
//!
//! Both are written atomically (a temp file, then a rename), so a reader sees a whole file or
//! none.

use crate::paths::AliceHome;
use alice_core::ids::now_rfc3339;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Why the last start failed (`daemon.error`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupError {
    /// The daemon that failed.
    pub pid: u32,
    /// When, RFC 3339 (for people).
    pub at: String,
    /// When, milliseconds since the Unix epoch (for comparisons).
    pub unix_ms: u64,
    /// What happened and what to do, written for the person who reads it.
    pub error: String,
}

/// What a starting daemon is doing (`daemon.starting`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartProgress {
    pub pid: u32,
    pub unix_ms: u64,
    /// "downloading the embedding model", "opening the database", ...
    pub step: String,
    /// For a download: the file, and bytes done of total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

impl StartProgress {
    pub fn step(step: &str) -> Self {
        Self {
            pid: std::process::id(),
            unix_ms: unix_ms(),
            step: step.into(),
            file: None,
            done: None,
            total: None,
        }
    }

    /// One line for people, e.g. "downloading the embedding model: model_quantized.onnx 45%
    /// (53 of 118 MB)".
    pub fn describe(&self) -> String {
        let mut s = self.step.clone();
        if let (Some(done), Some(total)) = (self.done, self.total) {
            let pct = done.saturating_mul(100) / total.max(1);
            let mb = |b: u64| (b + 500_000) / 1_000_000;
            s.push(':');
            if let Some(f) = &self.file {
                s.push(' ');
                s.push_str(f);
            }
            s.push_str(&format!(" {pct}% ({} of {} MB)", mb(done), mb(total)));
        }
        s
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Write `contents` to `path` through a temp file unique to this process, then rename.
fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Record why this daemon cannot start.
pub fn write_error(home: &AliceHome, error: &str) -> std::io::Result<()> {
    let e = StartupError {
        pid: std::process::id(),
        at: now_rfc3339(),
        unix_ms: unix_ms(),
        error: error.to_string(),
    };
    write_atomic(&home.daemon_error(), &serde_json::to_vec_pretty(&e)?)
}

/// Why the last start failed, if it did and no daemon has started since.
pub fn read_error(home: &AliceHome) -> Option<StartupError> {
    read_json(&home.daemon_error())
}

pub fn clear_error(home: &AliceHome) {
    let _ = std::fs::remove_file(home.daemon_error());
}

/// Announce what this daemon is doing while it starts.
pub fn write_progress(home: &AliceHome, p: &StartProgress) -> std::io::Result<()> {
    write_atomic(&home.daemon_starting(), &serde_json::to_vec(p)?)
}

/// What a starting daemon is doing, if one that is still alive announced it.
pub fn read_progress(home: &AliceHome) -> Option<StartProgress> {
    read_json::<StartProgress>(&home.daemon_starting()).filter(|p| pid_alive(p.pid))
}

pub fn clear_progress(home: &AliceHome) {
    let _ = std::fs::remove_file(home.daemon_starting());
}

/// The pid the last daemon to take the lock wrote into it.
pub fn lock_pid(home: &AliceHome) -> Option<u32> {
    std::fs::read_to_string(home.lock_file())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether no process holds the daemon lock, probed by taking it and letting go at once.
pub fn lock_free(home: &AliceHome) -> bool {
    match std::fs::File::open(home.lock_file()) {
        Ok(f) => {
            let free = f.try_lock().is_ok();
            if free {
                let _ = f.unlock();
            }
            free
        }
        // No lock file: no daemon ever ran for this home.
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// The live process that holds the daemon lock (a daemon starting, serving or stopping).
pub fn lock_holder(home: &AliceHome) -> Option<u32> {
    lock_pid(home).filter(|&p| pid_alive(p) && !lock_free(home))
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only asks whether the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Unknown here: the lock alone tells.
#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        (d, home)
    }

    #[test]
    fn the_error_file_round_trips_and_clears() {
        let (_d, home) = home();
        assert_eq!(read_error(&home), None);
        write_error(&home, "alice.db is not a usable database; move it aside").unwrap();
        let e = read_error(&home).unwrap();
        assert_eq!(e.pid, std::process::id());
        assert_eq!(e.error, "alice.db is not a usable database; move it aside");
        assert!(e.unix_ms > 0 && e.at.ends_with('Z'));
        clear_error(&home);
        assert_eq!(read_error(&home), None);
        let names: Vec<String> = std::fs::read_dir(home.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(names.is_empty(), "temp files left: {names:?}");
    }

    #[test]
    fn progress_is_read_only_while_its_process_lives() {
        let (_d, home) = home();
        let mut p = StartProgress::step("downloading the embedding model");
        p.file = Some("model_quantized.onnx".into());
        (p.done, p.total) = (Some(53_000_000), Some(118_308_185));
        write_progress(&home, &p).unwrap();
        let read = read_progress(&home).unwrap();
        assert_eq!(read, p);
        assert_eq!(
            read.describe(),
            "downloading the embedding model: model_quantized.onnx 44% (53 of 118 MB)"
        );
        assert_eq!(
            StartProgress::step("opening the database").describe(),
            "opening the database"
        );
        // `pid_alive` cannot tell on Windows (it answers true), so a dead announcer is only
        // recognised on Unix.
        #[cfg(unix)]
        {
            p.pid = 0;
            write_progress(&home, &p).unwrap();
            assert_eq!(read_progress(&home), None, "a dead announcer is ignored");
        }
        clear_progress(&home);
        assert_eq!(read_progress(&home), None);
    }

    #[test]
    fn the_lock_holder_is_the_live_process_that_holds_the_lock() {
        use std::io::Write;
        let (_d, home) = home();
        assert!(lock_free(&home) && lock_holder(&home).is_none());
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(home.lock_file())
            .unwrap();
        f.try_lock().unwrap();
        // Through the locked handle, as the daemon writes it.
        f.set_len(0).unwrap();
        writeln!(f, "{}", std::process::id()).unwrap();
        assert!(!lock_free(&home));
        // Unix locks are advisory, so another handle reads the pid of a held lock; Windows
        // locks are mandatory and keep a held lock file unreadable (lock_pid gives None).
        #[cfg(unix)]
        {
            assert_eq!(lock_pid(&home), Some(std::process::id()));
            assert_eq!(lock_holder(&home), Some(std::process::id()));
        }
        drop(f);
        assert!(lock_free(&home));
        assert_eq!(lock_pid(&home), Some(std::process::id()));
        assert_eq!(lock_holder(&home), None, "the pid in a free lock is stale");
    }
}
