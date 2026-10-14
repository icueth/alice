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
    /// (`<model>.memories.usearch`, an int8 HNSW, and `<model>.chunks.bq`, sign bits).
    ///
    /// A model id such as `ollama:nomic-embed-text` is not a file name: every character outside
    /// `[A-Za-z0-9._-]` becomes `_`. When that changed the name, a short hash of the original id
    /// is added, so two ids that differ only in such characters never share files (one model's
    /// purge must not delete another's index).
    ///
    /// ```
    /// use alice_daemon::paths::AliceHome;
    /// let home = AliceHome::new("/h".into());
    /// let plain = home.vector_file("multilingual-e5-small-q8");
    /// assert!(plain.ends_with("vectors/multilingual-e5-small-q8.usearch"));
    /// let remote = home.vector_file("ollama:nomic-embed-text");
    /// let name = remote.file_name().unwrap().to_string_lossy().into_owned();
    /// assert!(name.starts_with("ollama_nomic-embed-text-") && name.ends_with(".usearch"));
    /// assert_ne!(remote, home.vector_file("ollama/nomic-embed-text"));
    /// ```
    pub fn vector_file(&self, model: &str) -> PathBuf {
        let safe: String = model
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let name = if safe == model {
            safe
        } else {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in model.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            format!("{safe}-{:08x}", h >> 32)
        };
        self.root.join("vectors").join(format!("{name}.usearch"))
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
    /// API keys when `secrets.backend = "file"` (the default); private to its owner (0600).
    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.toml")
    }
    pub fn daemon_json(&self) -> PathBuf {
        self.root.join("daemon.json")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }
    /// The pid and start time of the daemon that holds the lock, in a file anyone can read
    /// (Windows locks are mandatory: the lock file itself cannot be read while held).
    pub fn daemon_pid(&self) -> PathBuf {
        self.root.join("daemon.pid")
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
            // Backups hold every memory: private even in a home an older build created.
            for dir in [&self.root, &self.backups_dir()] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
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
        // A model id with a colon is a legal file name on every platform.
        let remote = home.vector_file("ollama:nomic-embed-text");
        let name = remote.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("ollama_nomic-embed-text-") && name.ends_with(".usearch"),
            "{name}"
        );
        assert!(!name.contains(':'));
        assert_ne!(remote, home.vector_file("ollama_nomic-embed-text"));
        assert_ne!(remote, home.vector_file("ollama/nomic-embed-text"));
        assert_eq!(
            remote,
            home.vector_file("ollama:nomic-embed-text"),
            "stable"
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
            home.secrets_file(),
            home.daemon_json(),
            home.lock_file(),
            home.daemon_pid(),
            home.token_file(),
            home.daemon_error(),
            home.daemon_starting(),
        ] {
            assert_eq!(file.parent(), Some(home.root()));
        }
        // Unix file modes (0700); Windows has no mode bits.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.root()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
