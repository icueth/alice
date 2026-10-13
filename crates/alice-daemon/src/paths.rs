//! Filesystem layout of ALICE_HOME (spec §2.3).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AliceHome {
    root: PathBuf,
}

impl AliceHome {
    /// `ALICE_HOME` if set, otherwise `~/.alice`.
    pub fn from_env() -> Self {
        let root = std::env::var_os("ALICE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".alice")))
            .unwrap_or_else(|| PathBuf::from(".alice"));
        Self { root }
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn db(&self) -> PathBuf {
        self.root.join("alice.db")
    }
    /// Base path of the vector index for `model`; the index itself is two files next to it
    /// (`<model>.memories.usearch`, `<model>.chunks.usearch`).
    pub fn vector_file(&self, model: &str) -> PathBuf {
        self.root.join("vectors").join(format!("{model}.usearch"))
    }
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }
    pub fn spool_dir(&self) -> PathBuf {
        self.root.join("spool")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn daemon_json(&self) -> PathBuf {
        self.root.join("daemon.json")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }
    /// Why the last daemon start failed (see `startup`).
    pub fn daemon_error(&self) -> PathBuf {
        self.root.join("daemon.error")
    }
    /// What a starting daemon is doing (see `startup`).
    pub fn daemon_starting(&self) -> PathBuf {
        self.root.join("daemon.starting")
    }
    pub fn token_file(&self) -> PathBuf {
        self.root.join("token")
    }

    /// Create the directory tree. On Unix the root is private (0700).
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [
            self.root.clone(),
            self.root.join("vectors"),
            self.models_dir(),
            self.spool_dir(),
            self.backups_dir(),
            self.logs_dir(),
        ] {
            std::fs::create_dir_all(&d)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_lives_under_the_root_and_ensure_dirs_creates_it() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().join("home"));
        assert_eq!(home.db(), d.path().join("home/alice.db"));
        assert_eq!(
            home.vector_file("multilingual-e5-small"),
            d.path().join("home/vectors/multilingual-e5-small.usearch")
        );
        home.ensure_dirs().unwrap();
        home.ensure_dirs().unwrap();
        for dir in [
            home.root().join("vectors"),
            home.models_dir(),
            home.spool_dir(),
            home.backups_dir(),
            home.logs_dir(),
        ] {
            assert!(dir.is_dir(), "{}", dir.display());
        }
        for file in [
            home.config_file(),
            home.daemon_json(),
            home.lock_file(),
            home.token_file(),
            home.daemon_error(),
            home.daemon_starting(),
        ] {
            assert_eq!(file.parent(), Some(home.root()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.root()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
