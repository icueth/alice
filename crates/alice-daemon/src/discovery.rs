//! `daemon.json` (how clients find the daemon) and the API token file.

use crate::paths::AliceHome;
use alice_core::ids::new_id;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    /// Random per start. Clients compare it with `/api/v1/ping` before sending the token.
    pub instance: String,
    pub version: String,
    pub started_at: String,
}

pub fn read_info(home: &AliceHome) -> Option<DaemonInfo> {
    let raw = std::fs::read_to_string(home.daemon_json()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Atomic: write a temp file, then rename. Only the lock holder writes it.
pub fn write_info(home: &AliceHome, info: &DaemonInfo) -> std::io::Result<()> {
    let tmp = home.daemon_json().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(info)?)?;
    std::fs::rename(tmp, home.daemon_json())
}

/// Remove daemon.json only if it still describes this instance (a newer daemon may have replaced it).
pub fn remove_info_if(home: &AliceHome, instance: &str) {
    if read_info(home).is_some_and(|i| i.instance == instance) {
        let _ = std::fs::remove_file(home.daemon_json());
    }
}

pub fn read_token(home: &AliceHome) -> Option<String> {
    std::fs::read_to_string(home.token_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A new file readable only by its owner (mode 0600 on Unix), synced to disk.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents)?;
    f.sync_all()
}

/// 32 random bytes as hex, stored with mode 0600. Reused across restarts.
///
/// The token file appears complete or not at all (written to a temp file, then linked into
/// place), so a process killed half-way never leaves an empty file that would block every
/// later start; an empty file found anyway is replaced.
pub fn load_or_create_token(home: &AliceHome) -> std::io::Result<String> {
    if let Some(t) = read_token(home) {
        return Ok(t);
    }
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token = hex::encode(buf);
    let dest = home.token_file();
    let tmp = home.root().join(format!("token.{}.tmp", new_id()));
    write_private(&tmp, token.as_bytes())?;
    // hard_link fails if the token exists, so two processes creating it at once agree on one.
    let placed = match std::fs::hard_link(&tmp, &dest) {
        Ok(()) => Ok(token),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_token(home) {
            // Another process created it first: use theirs.
            Some(theirs) => Ok(theirs),
            // An empty leftover: replace it.
            None => std::fs::rename(&tmp, &dest).map(|()| token),
        },
        // No hard links on this file system.
        Err(_) => std::fs::rename(&tmp, &dest).map(|()| token),
    };
    let _ = std::fs::remove_file(&tmp);
    placed
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
    fn info_round_trip_and_conditional_remove() {
        let (_d, home) = home();
        let info = DaemonInfo {
            pid: 1,
            port: 2,
            instance: "a".into(),
            version: "v".into(),
            started_at: "t".into(),
        };
        write_info(&home, &info).unwrap();
        assert_eq!(read_info(&home), Some(info));
        remove_info_if(&home, "b");
        assert!(read_info(&home).is_some());
        remove_info_if(&home, "a");
        assert!(read_info(&home).is_none());
    }

    #[test]
    fn a_damaged_daemon_json_reads_as_absent() {
        let (_d, home) = home();
        std::fs::write(home.daemon_json(), b"{\"pid\": 1, \"port\":").unwrap();
        assert_eq!(read_info(&home), None);
        remove_info_if(&home, "anything");
        assert!(
            home.daemon_json().exists(),
            "only a matching instance is removed"
        );
    }

    #[test]
    fn token_is_stable_and_private() {
        let (_d, home) = home();
        let a = load_or_create_token(&home).unwrap();
        let b = load_or_create_token(&home).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(read_token(&home), Some(a));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.token_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(home.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("token."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    /// A daemon killed while creating the token must not leave a file that blocks every later start.
    #[test]
    fn an_empty_token_file_is_replaced() {
        let (_d, home) = home();
        std::fs::write(home.token_file(), b"").unwrap();
        assert_eq!(read_token(&home), None);
        let t = load_or_create_token(&home).unwrap();
        assert_eq!(t.len(), 64);
        assert_eq!(read_token(&home), Some(t));
    }

    #[test]
    fn an_existing_token_is_kept() {
        let (_d, home) = home();
        std::fs::write(home.token_file(), b"  existing-token\n").unwrap();
        assert_eq!(load_or_create_token(&home).unwrap(), "existing-token");
    }
}
