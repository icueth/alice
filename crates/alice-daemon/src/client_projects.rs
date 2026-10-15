//! Server mode: the server's copy (mirror) of each remote project's `.alice.toml` (spec §2.2
//! item 3, ruling R1). The mirror directory is stored as the project's root, so every privacy path
//! that reads `<root>/.alice.toml` (the project rules cache, the redactors, the cloud policy, the
//! judge) applies a client's rules exactly as it applies a local working tree's.

use crate::paths::AliceHome;
use alice_core::api::{ClientProject, ClientRules};
use alice_core::error::{AliceError, Result};
use alice_core::project::ProjectRef;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `<home>/client-projects/<first 32 hex characters of SHA-256(key)>`: never a client string.
pub fn mirror_dir(home: &AliceHome, key: &str) -> PathBuf {
    let digest = hex::encode(Sha256::digest(key.as_bytes()));
    home.client_projects_dir().join(&digest[..32])
}

fn io(e: std::io::Error) -> AliceError {
    AliceError::Other(format!(
        "cannot keep this project's privacy rules on the server ({e}); nothing was stored. Check the server's disk"
    ))
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Written to a new private file, synced, renamed over `path`.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(".alice.toml.{}.tmp", alice_core::ids::new_id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let done = (|| {
        let mut f = o.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// Bring the mirror of `cp`'s project up to date and return the project with the mirror as
/// its root. Blocking (file system): call it on the blocking pool.
pub fn sync_mirror(home: &AliceHome, cp: &ClientProject) -> Result<ProjectRef> {
    cp.validate()?;
    let dir = mirror_dir(home, &cp.key);
    private_dir(&dir).map_err(io)?;
    let file = dir.join(".alice.toml");
    let wanted = match &cp.rules {
        ClientRules::None => None,
        ClientRules::File { text } => Some(text.clone()),
        ClientRules::Unreadable { reason } => {
            Some(alice_core::settings::unreadable_marker_text(reason))
        }
    };
    match wanted {
        None => match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        },
        Some(text) => {
            if std::fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
                write_private(&file, &text).map_err(io)?;
            }
        }
    }
    Ok(ProjectRef {
        key: cp.key.clone(),
        slug: cp.slug.clone(),
        name: cp.name.clone(),
        root: Some(dir),
        remote: cp.remote.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::api::{ClientProject, ClientRules};

    fn home() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        h.ensure_dirs().unwrap();
        (d, h)
    }

    fn cp(rules: ClientRules) -> ClientProject {
        ClientProject {
            key: "github.com/me/app".into(),
            slug: "app".into(),
            name: "app".into(),
            remote: Some("github.com/me/app".into()),
            rules,
        }
    }

    #[test]
    fn the_mirror_is_named_by_a_hash_and_holds_the_text() {
        let (_d, h) = home();
        let r = sync_mirror(
            &h,
            &cp(ClientRules::File {
                text: "[privacy]\ncloud_denied = true\n".into(),
            }),
        )
        .unwrap();
        let dir = mirror_dir(&h, "github.com/me/app");
        assert_eq!(r.root.as_deref(), Some(dir.as_path()));
        assert_eq!(
            (r.key.as_str(), r.slug.as_str(), r.name.as_str()),
            ("github.com/me/app", "app", "app")
        );
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 32);
        assert!(name.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(dir.starts_with(h.client_projects_dir()));
        assert_eq!(
            std::fs::read_to_string(dir.join(".alice.toml")).unwrap(),
            "[privacy]\ncloud_denied = true\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(dir.join(".alice.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn none_removes_the_file_and_unreadable_writes_the_marker() {
        let (_d, h) = home();
        sync_mirror(
            &h,
            &cp(ClientRules::File {
                text: "[privacy]\nblock_secrets = true\n".into(),
            }),
        )
        .unwrap();
        let file = mirror_dir(&h, "github.com/me/app").join(".alice.toml");
        sync_mirror(&h, &cp(ClientRules::None)).unwrap();
        assert!(!file.exists());
        assert!(
            file.parent().unwrap().is_dir(),
            "the directory stays: it is the project's root"
        );
        sync_mirror(
            &h,
            &cp(ClientRules::Unreadable {
                reason: "permission denied".into(),
            }),
        )
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(alice_core::settings::UNREADABLE_MARKER));
        assert!(
            alice_core::settings::load_project(file.parent().unwrap()).is_err(),
            "fails closed"
        );
    }

    #[test]
    fn an_invalid_client_project_writes_nothing() {
        let (_d, h) = home();
        let mut bad = cp(ClientRules::None);
        bad.slug = "Not A Slug".into();
        assert!(sync_mirror(&h, &bad).is_err());
        assert!(
            !h.client_projects_dir().exists()
                || std::fs::read_dir(h.client_projects_dir())
                    .unwrap()
                    .next()
                    .is_none()
        );
    }

    #[test]
    fn the_same_text_is_not_rewritten() {
        let (_d, h) = home();
        let rules = ClientRules::File {
            text: "[privacy]\ncloud_denied = true\n".into(),
        };
        sync_mirror(&h, &cp(rules.clone())).unwrap();
        let file = mirror_dir(&h, "github.com/me/app").join(".alice.toml");
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        sync_mirror(&h, &cp(rules)).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before
        );
    }
}
