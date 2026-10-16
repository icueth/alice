//! `daemon.json` (how clients find the daemon) and the API token file.

use crate::paths::AliceHome;
use alice_core::ids::new_id;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
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
    read_token_file(&home.token_file())
}

/// The trimmed, non-empty contents of a token file.
pub fn read_token_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
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
        // Someone may have loosened it; the token must stay readable by its owner only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = home.token_file();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
                |e| {
                    std::io::Error::new(
                        e.kind(),
                        format!(
                            "cannot make {} private: {e}; check who owns it",
                            path.display()
                        ),
                    )
                },
            )?;
        }
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

/// Replace the token with a new random one and return it. The new file is written privately
/// (mode 0600 from its first byte) under a temporary name and renamed over the old one, so a
/// reader sees the old token or the new one, never a partial file, and a failure leaves the old
/// token in place. A running daemon must be told (`POST /api/v1/admin/token/rotate` does both);
/// rotating under a daemon that does not know would lock every client out.
pub fn rotate_token(home: &AliceHome) -> std::io::Result<String> {
    rotate_token_with(home, &write_private)
}

/// `rotate_token` with the file write passed in (a test makes it fail half-way).
fn rotate_token_with(
    home: &AliceHome,
    write: &dyn Fn(&Path, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token = hex::encode(buf);
    let tmp = home.root().join(format!("token.{}.tmp", new_id()));
    // Whatever fails (the write, the flush, the rename), the temporary file goes.
    write(&tmp, token.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, home.token_file()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .map(|()| token)
}

/// What `/api/v1/ping?nonce=<nonce>` answers:
/// `hex(HMAC-SHA256(key = token, msg = "<nonce>:<instance>"))` over ASCII bytes, where
/// `instance` is the answering daemon's own instance id. Only a process that knows the token
/// can produce it, and it vouches for that one instance: an impostor on a stale port that
/// relays the client's nonce to the real daemon gets a proof for the real instance, which
/// fails against the stale id the client read from daemon.json. A client checks it (with
/// `verify_ping_proof`) before it sends the token anywhere.
pub fn ping_proof(token: &str, nonce: &str, instance: &str) -> String {
    let msg = format!("{nonce}:{instance}");
    hex::encode(hmac_sha256(token.as_bytes(), msg.as_bytes()))
}

/// Whether `proof` is `ping_proof(token, nonce, instance)`, compared in constant time.
pub fn verify_ping_proof(token: &str, nonce: &str, instance: &str, proof: &str) -> bool {
    crate::http::ct_eq(
        ping_proof(token, nonce, instance).as_bytes(),
        proof.as_bytes(),
    )
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
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
        // Unix file modes (0600); Windows has no mode bits.
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

    #[test]
    fn a_rotated_token_is_new_private_and_leaves_no_temporary_file() {
        let (_d, home) = home();
        let old = load_or_create_token(&home).unwrap();
        let new = rotate_token(&home).unwrap();
        assert_ne!(new, old);
        assert_eq!(new.len(), 64);
        assert!(new.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(read_token(&home), Some(new.clone()));
        // Unix file modes (0600); Windows has no mode bits.
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
        assert_ne!(rotate_token(&home).unwrap(), new, "every rotation is new");
        let leftovers: Vec<_> = std::fs::read_dir(home.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("token.") && n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    /// A write that fails half-way (a full disk) leaves the old token and no temporary file.
    #[test]
    fn a_failed_write_leaves_no_temporary_token_file() {
        let (_d, home) = home();
        let old = load_or_create_token(&home).unwrap();
        let failing = |path: &Path, bytes: &[u8]| -> std::io::Result<()> {
            std::fs::write(path, &bytes[..8])?;
            Err(std::io::Error::other("no space left on device"))
        };
        let e = rotate_token_with(&home, &failing).unwrap_err();
        assert!(e.to_string().contains("no space"), "{e}");
        assert_eq!(read_token(&home), Some(old));
        let left = std::fs::read_dir(home.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("token."))
            .count();
        assert_eq!(left, 0, "no token.*.tmp is left");
    }

    /// A rotation that cannot replace the file leaves the old token (and no temporary file).
    #[test]
    fn a_failed_rotation_keeps_the_old_token() {
        let (_d, home) = home();
        // A directory where the token file should be: the rename cannot replace it.
        std::fs::create_dir(home.token_file()).unwrap();
        assert!(rotate_token(&home).is_err());
        assert!(home.token_file().is_dir());
        let leftovers = std::fs::read_dir(home.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("token."))
            .count();
        assert_eq!(leftovers, 0);
    }

    /// RFC 4231 test cases 2 (short key) and 6 (key longer than the 64-byte block).
    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// The message is `<nonce>:<instance>`; Task 12's client relies on the exact form.
    #[test]
    fn ping_proof_signs_the_nonce_and_the_instance() {
        let token = "a".repeat(64);
        let nonce = "0123456789abcdef";
        let instance = "01K7EXAMPLEINSTANCE0000000";
        assert_eq!(
            ping_proof(&token, nonce, instance),
            hex::encode(hmac_sha256(
                token.as_bytes(),
                format!("{nonce}:{instance}").as_bytes()
            ))
        );
    }

    #[test]
    fn verify_ping_proof_accepts_only_the_exact_proof() {
        let token = "a".repeat(64);
        let nonce = "0123456789abcdef";
        let instance = "01K7AAAAAAAAAAAAAAAAAAAAAA";
        let proof = ping_proof(&token, nonce, instance);
        assert_eq!(proof.len(), 64);
        assert!(verify_ping_proof(&token, nonce, instance, &proof));
        assert!(
            !verify_ping_proof(&"b".repeat(64), nonce, instance, &proof),
            "wrong token"
        );
        assert!(
            !verify_ping_proof(&token, "0123456789abcdee", instance, &proof),
            "other nonce"
        );
        assert!(
            !verify_ping_proof(&token, nonce, instance, &proof[..63]),
            "truncated"
        );
        assert!(
            !verify_ping_proof(&token, nonce, instance, &format!("{proof}0")),
            "extended"
        );
        assert!(!verify_ping_proof(&token, nonce, instance, ""), "empty");
    }

    /// A proof made by the daemon with instance A (relayed by an impostor) does not vouch
    /// for the stale instance B that daemon.json names.
    #[test]
    fn a_proof_for_one_instance_does_not_verify_for_another() {
        let token = "a".repeat(64);
        let nonce = "0123456789abcdef";
        let a = "01K7AAAAAAAAAAAAAAAAAAAAAA";
        let b = "01K7BBBBBBBBBBBBBBBBBBBBBB";
        let proof_a = ping_proof(&token, nonce, a);
        assert_ne!(proof_a, ping_proof(&token, nonce, b));
        assert!(verify_ping_proof(&token, nonce, a, &proof_a));
        assert!(!verify_ping_proof(&token, nonce, b, &proof_a));
    }

    // Unix file modes (0600); Windows has no mode bits.
    #[cfg(unix)]
    #[test]
    fn an_existing_token_file_is_made_private_again() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, home) = home();
        std::fs::write(home.token_file(), b"existing-token").unwrap();
        std::fs::set_permissions(home.token_file(), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_eq!(load_or_create_token(&home).unwrap(), "existing-token");
        let mode = std::fs::metadata(home.token_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
