//! What a starting daemon tells clients before it serves, and who holds the daemon lock.
//!
//! - `daemon.starting`: the step a daemon is on (with download progress), written while it
//!   starts and removed once it serves or fails. Only meaningful while its `pid` is alive.
//! - `daemon.error`: why the last start failed, in the daemon's words (the cause and what to
//!   do). Written before the failed daemon releases its lock; removed when the next daemon
//!   takes the lock and again when it serves (publishes daemon.json).
//!
//! - `daemon.pid`: the pid and start time of the daemon that holds the lock. On Unix the lock
//!   file carries the pid and a held lock can be read; on Windows locks are mandatory, so a
//!   held `daemon.lock` cannot be read and this file is how a client finds the holder.
//!
//! All are written atomically (a temp file, then a rename), so a reader sees a whole file or
//! none. The temporary file is named after the whole target (`daemon.error.<pid>.tmp`), so two
//! of these files written at once never share one.

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

/// The temporary name for writing `path`: its whole file name, then this pid.
fn tmp_name(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Write `contents` to `path` through a temp file unique to this process and to the target
/// (`<file name>.<pid>.tmp`: `daemon.error` and `daemon.starting` once shared `daemon.<pid>.tmp`
/// and could overwrite each other), then rename.
fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = tmp_name(path);
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

/// Who holds (or last held) the daemon lock: the pid and when that daemon started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidFile {
    pub pid: u32,
    /// Milliseconds since the Unix epoch.
    pub unix_ms: u64,
}

/// Record this process as the daemon (called once it holds the lock).
pub fn write_pid_file(home: &AliceHome) -> std::io::Result<()> {
    let p = PidFile {
        pid: std::process::id(),
        unix_ms: unix_ms(),
    };
    write_atomic(&home.daemon_pid(), &serde_json::to_vec(&p)?)
}

pub fn read_pid_file(home: &AliceHome) -> Option<PidFile> {
    read_json(&home.daemon_pid())
}

/// Remove `daemon.pid` if it still names this process (the daemon is exiting, lock still held).
pub fn clear_pid_file(home: &AliceHome) {
    if read_pid_file(home).is_some_and(|p| p.pid == std::process::id()) {
        let _ = std::fs::remove_file(home.daemon_pid());
    }
}

/// The pid the last daemon to take the lock wrote: into the lock file (Unix: a held lock can be
/// read), or into `daemon.pid` where the held lock file cannot be read (Windows).
pub fn lock_pid(home: &AliceHome) -> Option<u32> {
    std::fs::read_to_string(home.lock_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| read_pid_file(home).map(|p| p.pid))
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
    lock_pid(home).filter(|&p| holder_alive(home, p) && !lock_free(home))
}

/// How long the process that holds the lock has been running as a daemon, from `daemon.pid`
/// (`None` when it names another process or is missing).
pub fn holder_age(home: &AliceHome, pid: u32) -> Option<std::time::Duration> {
    let p = read_pid_file(home).filter(|p| p.pid == pid)?;
    Some(std::time::Duration::from_millis(
        unix_ms().saturating_sub(p.unix_ms),
    ))
}

/// Whether `pid` is alive and, where a pid can be reused for another program, is still the
/// daemon `daemon.pid` recorded (a process that started after the record is not it).
fn holder_alive(home: &AliceHome, pid: u32) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    #[cfg(windows)]
    if let Some(rec) = read_pid_file(home).filter(|p| p.pid == pid)
        && let Some(started) = windows::process_start_ms(pid)
    {
        return started <= rec.unix_ms + windows::START_SLACK_MS;
    }
    let _ = home;
    true
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

/// Whether a process with this pid exists and has not exited.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    windows::alive(pid)
}

/// Unknown here: the lock alone tells.
#[cfg(not(any(unix, windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

/// Process queries through the Win32 API (`OpenProcess` and friends).
#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, FILETIME, GetLastError, HANDLE, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// A process created this long after the record of `daemon.pid` is not that daemon (the
    /// record is written right after the lock is taken, so the real daemon started before it).
    pub const START_SLACK_MS: u64 = 2_000;
    /// Milliseconds from 1601-01-01 (the FILETIME epoch) to 1970-01-01.
    const EPOCH_DIFF_MS: u64 = 11_644_473_600_000;

    /// Closes the handle when dropped.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful `OpenProcess` and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn open(pid: u32) -> Result<Handle, u32> {
        // SAFETY: plain call with a pid; a null handle means failure, checked below.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            // SAFETY: reads the calling thread's last error.
            Err(unsafe { GetLastError() })
        } else {
            Ok(Handle(h))
        }
    }

    pub fn alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        match open(pid) {
            Ok(h) => {
                let mut code = 0u32;
                // SAFETY: `h.0` is a live process handle and `code` is a valid out pointer.
                let ok = unsafe { GetExitCodeProcess(h.0, &mut code) };
                // STILL_ACTIVE is 259 (an `i32` constant); a program may exit with that code, which
                // this cannot tell from a running one.
                ok != 0 && code == STILL_ACTIVE as u32
            }
            // The process exists but belongs to someone we may not open.
            Err(e) => e == ERROR_ACCESS_DENIED,
        }
    }

    /// When the process started, in milliseconds since the Unix epoch.
    pub fn process_start_ms(pid: u32) -> Option<u64> {
        let h = open(pid).ok()?;
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: `h.0` is a live process handle and the four out pointers are valid.
        let ok = unsafe { GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }
        let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        (ticks / 10_000).checked_sub(EPOCH_DIFF_MS)
    }
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

    /// `daemon.error` and `daemon.starting` once shared `daemon.<pid>.tmp`; a write of one could
    /// be renamed into place as the other.
    #[test]
    fn the_two_status_files_never_share_a_temporary_name() {
        let (_d, home) = home();
        let (a, b) = (
            tmp_name(&home.daemon_error()),
            tmp_name(&home.daemon_starting()),
        );
        assert_ne!(a, b);
        let pid = std::process::id();
        assert_eq!(
            a.file_name().unwrap().to_string_lossy(),
            format!("daemon.error.{pid}.tmp")
        );
        assert_eq!(
            b.file_name().unwrap().to_string_lossy(),
            format!("daemon.starting.{pid}.tmp")
        );
        assert_eq!(a.parent(), Some(home.root()));
        // Both written one after the other leave exactly their own contents and no temp file.
        write_error(&home, "an error").unwrap();
        write_progress(&home, &StartProgress::step("opening the database")).unwrap();
        assert_eq!(read_error(&home).unwrap().error, "an error");
        assert_eq!(read_progress(&home).unwrap().step, "opening the database");
        let tmp: Vec<_> = std::fs::read_dir(home.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(tmp.is_empty(), "{tmp:?}");
    }

    /// `daemon.pid` is how a client finds the holder where a held lock file cannot be read
    /// (Windows): with the lock file unreadable or empty, the pid comes from it.
    #[test]
    fn the_pid_file_stands_in_for_a_lock_file_that_cannot_be_read() {
        let (_d, home) = home();
        assert_eq!(read_pid_file(&home), None);
        write_pid_file(&home).unwrap();
        let rec = read_pid_file(&home).unwrap();
        assert_eq!(rec.pid, std::process::id());
        assert!(rec.unix_ms.abs_diff(unix_ms()) < 60_000);
        // The lock file says nothing (as when it cannot be read): the pid file answers.
        std::fs::write(home.lock_file(), b"").unwrap();
        assert_eq!(lock_pid(&home), Some(std::process::id()));
        // Where the lock file can be read, it is preferred.
        std::fs::write(home.lock_file(), b"4242\n").unwrap();
        assert_eq!(lock_pid(&home), Some(4242));
        let age = holder_age(&home, std::process::id()).unwrap();
        assert!(age < std::time::Duration::from_secs(60), "{age:?}");
        assert_eq!(
            holder_age(&home, 4242),
            None,
            "the record names another process"
        );
        // Only the daemon that wrote it removes it.
        let other = PidFile {
            pid: std::process::id() + 1,
            unix_ms: unix_ms(),
        };
        std::fs::write(home.daemon_pid(), serde_json::to_vec(&other).unwrap()).unwrap();
        clear_pid_file(&home);
        assert_eq!(read_pid_file(&home), Some(other));
        write_pid_file(&home).unwrap();
        clear_pid_file(&home);
        assert_eq!(read_pid_file(&home), None);
    }

    /// A process that has exited is not alive (the whole of Windows' process check, `OpenProcess`
    /// and the exit code, runs here on Windows); this process is.
    #[test]
    fn a_pid_is_alive_until_its_process_has_exited() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid), "pid {pid} exited");
    }

    /// Windows: a pid recorded by a daemon that is gone and now belongs to a newer process is not
    /// the daemon (the record is older than that process).
    #[cfg(windows)]
    #[test]
    fn a_reused_pid_is_not_the_recorded_daemon_on_windows() {
        let (_d, home) = home();
        let started = windows::process_start_ms(std::process::id()).unwrap();
        assert!(started.abs_diff(unix_ms()) < 24 * 3_600_000, "{started}");
        let pid = std::process::id();
        // Recorded just now: this process started before the record, so it is the daemon.
        std::fs::write(
            home.daemon_pid(),
            serde_json::to_vec(&PidFile {
                pid,
                unix_ms: unix_ms(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(holder_alive(&home, pid));
        // Recorded an hour before this process started: a different process by the same pid.
        std::fs::write(
            home.daemon_pid(),
            serde_json::to_vec(&PidFile {
                pid,
                unix_ms: started.saturating_sub(3_600_000),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(!holder_alive(&home, pid));
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
        // Files are opened close-on-exec, but a sibling test's fork keeps a copy of the open file
        // (and so the lock) until its child execs: wait that out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !lock_free(&home) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(lock_free(&home));
        assert_eq!(lock_pid(&home), Some(std::process::id()));
        assert_eq!(lock_holder(&home), None, "the pid in a free lock is stale");
    }
}
