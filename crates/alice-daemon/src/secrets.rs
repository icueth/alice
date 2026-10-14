//! Secrets (spec §6): `secrets.toml` readable only by its owner (default), or the OS keychain
//! when `secrets.backend = "keychain"`. Values leave this module only as a [`SecretValue`], for
//! the provider that needs them; the API sees only [`SecretStatus`]. Errors name the secret and
//! the file, never a value or a line of the file.

use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
pub use alice_core::settings::SecretValue;
use alice_core::settings::SecretsBackend;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretName {
    /// TypeSafe Jev (judge).
    JevApiKey,
    /// The HTTP embedding provider (openai, voyage).
    EmbeddingApiKey,
}

impl SecretName {
    pub const ALL: [SecretName; 2] = [SecretName::JevApiKey, SecretName::EmbeddingApiKey];

    pub fn as_str(self) -> &'static str {
        match self {
            SecretName::JevApiKey => "jev_api_key",
            SecretName::EmbeddingApiKey => "embedding_api_key",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|n| n.as_str() == s)
    }

    /// An environment variable that overrides the stored value.
    pub fn env_var(self) -> &'static str {
        match self {
            SecretName::JevApiKey => "TYPESAFE_API_KEY",
            SecretName::EmbeddingApiKey => "ALICE_EMBEDDING_API_KEY",
        }
    }
}

/// What the API shows about a secret: whether it is set, a hint, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretStatus {
    pub set: bool,
    /// The last four characters, e.g. `••••ab12` (only `••••` for short values).
    pub hint: Option<String>,
    /// `file`, `keychain` or `env`.
    pub source: Option<String>,
}

impl SecretStatus {
    fn unset() -> Self {
        Self {
            set: false,
            hint: None,
            source: None,
        }
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, name: SecretName) -> Result<Option<SecretValue>>;
    /// Stores `value` trimmed; refuses an empty or multi-line value.
    fn set(&self, name: SecretName, value: &str) -> Result<()>;
    /// `true` when there was a value to delete.
    fn delete(&self, name: SecretName) -> Result<bool>;
    /// `file` or `keychain`.
    fn backend(&self) -> &'static str;
}

/// `••••` and the last four characters; only `••••` for values of 8 characters or fewer.
pub fn hint(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        "••••".into()
    } else {
        format!("••••{}", value.chars().skip(n - 4).collect::<String>())
    }
}

fn check_value(value: &str) -> Result<()> {
    let v = value.trim();
    if v.is_empty() || v.len() > 4_096 || v.chars().any(char::is_control) {
        return Err(AliceError::invalid(
            "a secret must be one line of 1 to 4096 characters; paste the key again",
        ));
    }
    Ok(())
}

/// `secrets.toml`: one `name = "value"` per secret, readable by its owner only (0600 on Unix;
/// on Windows the file inherits the private ACL of the user's profile directory).
pub struct FileSecrets {
    path: PathBuf,
    lock: Mutex<()>,
}

impl std::fmt::Debug for FileSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSecrets")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl FileSecrets {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<toml::Table> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
            Err(e) => {
                return Err(AliceError::Other(format!(
                    "cannot read {}: {e}",
                    self.path.display()
                )));
            }
        };
        self.make_private();
        toml::from_str(&text).map_err(|e| {
            // The line number only: the line itself holds a secret.
            let line = e
                .span()
                .map(|s| {
                    let at = s.start.min(text.len());
                    let n = text.as_bytes()[..at]
                        .iter()
                        .filter(|b| **b == b'\n')
                        .count();
                    format!(" (line {})", n + 1)
                })
                .unwrap_or_default();
            AliceError::Other(format!(
                "{} is not valid TOML{line}; fix or delete it, then set the secrets again",
                self.path.display()
            ))
        })
    }

    /// A file others can read (made by hand, or copied) is made private again.
    #[cfg(unix)]
    fn make_private(&self) {
        use std::os::unix::fs::PermissionsExt;
        let Ok(m) = std::fs::metadata(&self.path) else {
            return;
        };
        if m.permissions().mode() & 0o077 == 0 {
            return;
        }
        match std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => tracing::warn!(
                path = %self.path.display(),
                "the secrets file was readable by others; it is now private (0600)"
            ),
            Err(e) => tracing::warn!(
                path = %self.path.display(),
                "the secrets file is readable by others and could not be made private: {e}"
            ),
        }
    }

    #[cfg(not(unix))]
    fn make_private(&self) {}

    /// Written to a new private file, synced, then renamed over the old one.
    fn write(&self, t: &toml::Table) -> Result<()> {
        let text = toml::to_string(t).map_err(|e| AliceError::Other(e.to_string()))?;
        let tmp = self
            .path
            .with_file_name(format!(".secrets.toml.{}.tmp", alice_core::ids::new_id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let done = (|| -> std::io::Result<()> {
            let mut f = opts.open(&tmp)?;
            std::io::Write::write_all(&mut f, text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done.map_err(|e| AliceError::Other(format!("cannot write {}: {e}", self.path.display())))
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, name: SecretName) -> Result<Option<SecretValue>> {
        let _l = self.lock.lock();
        Ok(self
            .read()?
            .get(name.as_str())
            .and_then(|v| v.as_str())
            .map(SecretValue::new))
    }

    fn set(&self, name: SecretName, value: &str) -> Result<()> {
        check_value(value)?;
        let _l = self.lock.lock();
        // A file that cannot be read is not overwritten: it may hold the other secrets.
        let mut t = self.read()?;
        t.insert(
            name.as_str().into(),
            toml::Value::String(value.trim().into()),
        );
        self.write(&t)
    }

    fn delete(&self, name: SecretName) -> Result<bool> {
        let _l = self.lock.lock();
        let mut t = self.read()?;
        let had = t.remove(name.as_str()).is_some();
        if had {
            self.write(&t)?;
        }
        Ok(had)
    }

    fn backend(&self) -> &'static str {
        "file"
    }
}

/// The OS keychain calls the keychain store makes: one account per secret (its name) under the
/// service `alice`. Errors are descriptions that never contain a value. `OsKeychain` (the
/// `keychain` feature) is the real one; tests use an in-memory stand-in, so no test touches the
/// OS keychain.
pub trait KeychainApi: Send + Sync {
    fn get(&self, account: &str) -> std::result::Result<Option<String>, String>;
    fn set(&self, account: &str, value: &str) -> std::result::Result<(), String>;
    /// `true` when there was an entry to delete.
    fn delete(&self, account: &str) -> std::result::Result<bool, String>;
}

/// Secrets in the OS keychain. Opt-in: on macOS every rebuilt or updated binary is a new
/// identity to the Keychain and blocks on a permission prompt a background daemon cannot
/// answer; on Linux the Secret Service needs a D-Bus session that headless machines often lack.
pub struct KeychainSecrets {
    api: Box<dyn KeychainApi>,
}

impl std::fmt::Debug for KeychainSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeychainSecrets")
    }
}

impl KeychainSecrets {
    pub fn with_api(api: Box<dyn KeychainApi>) -> Self {
        Self { api }
    }

    /// The OS keychain. Nothing is read until the first `get`, `set` or `delete`.
    #[cfg(feature = "keychain")]
    pub fn os() -> Self {
        Self::with_api(Box::new(OsKeychain))
    }

    fn failed(what: &str, name: SecretName, e: String) -> AliceError {
        AliceError::Other(format!(
            "{what} {} in the OS keychain failed ({e}); unlock the keychain and try again, or set secrets.backend = \"file\" to use secrets.toml",
            name.as_str()
        ))
    }
}

impl SecretStore for KeychainSecrets {
    fn get(&self, name: SecretName) -> Result<Option<SecretValue>> {
        self.api
            .get(name.as_str())
            .map(|v| v.map(SecretValue::new))
            .map_err(|e| Self::failed("reading", name, e))
    }

    fn set(&self, name: SecretName, value: &str) -> Result<()> {
        check_value(value)?;
        self.api
            .set(name.as_str(), value.trim())
            .map_err(|e| Self::failed("storing", name, e))
    }

    fn delete(&self, name: SecretName) -> Result<bool> {
        self.api
            .delete(name.as_str())
            .map_err(|e| Self::failed("deleting", name, e))
    }

    fn backend(&self) -> &'static str {
        "keychain"
    }
}

/// The OS keychain through `keyring` (macOS Keychain, Windows Credential Manager, Secret
/// Service).
#[cfg(feature = "keychain")]
pub struct OsKeychain;

#[cfg(feature = "keychain")]
impl OsKeychain {
    const SERVICE: &str = "alice";

    fn entry(account: &str) -> std::result::Result<keyring::Entry, String> {
        keyring::Entry::new(Self::SERVICE, account).map_err(|e| e.to_string())
    }
}

/// Errors are shown with `Display` only: keyring's `Debug` of a bad entry carries its bytes.
#[cfg(feature = "keychain")]
impl KeychainApi for OsKeychain {
    fn get(&self, account: &str) -> std::result::Result<Option<String>, String> {
        match Self::entry(account)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, account: &str, value: &str) -> std::result::Result<(), String> {
        Self::entry(account)?
            .set_password(value)
            .map_err(|e| e.to_string())
    }

    fn delete(&self, account: &str) -> std::result::Result<bool, String> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The store `secrets.backend` names. Without the `keychain` feature, `keychain` falls back to
/// `secrets.toml` with a warning.
pub fn open_store(home: &AliceHome, backend: SecretsBackend) -> Arc<dyn SecretStore> {
    match backend {
        #[cfg(feature = "keychain")]
        SecretsBackend::Keychain => Arc::new(KeychainSecrets::os()),
        #[cfg(not(feature = "keychain"))]
        SecretsBackend::Keychain => {
            tracing::warn!("this build has no keychain support; using secrets.toml");
            Arc::new(FileSecrets::new(home.secrets_file()))
        }
        SecretsBackend::File => Arc::new(FileSecrets::new(home.secrets_file())),
    }
}

/// Reads one environment variable (the process environment, or a test's stand-in).
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

fn process_env(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

/// The value to use and where it came from (`env`, `file` or `keychain`): a non-empty
/// environment variable wins over the store.
pub fn resolve(
    store: &dyn SecretStore,
    name: SecretName,
) -> Result<Option<(SecretValue, &'static str)>> {
    resolve_with(store, name, &process_env)
}

/// [`resolve`] with the environment passed in.
pub fn resolve_with(
    store: &dyn SecretStore,
    name: SecretName,
    env: EnvLookup<'_>,
) -> Result<Option<(SecretValue, &'static str)>> {
    if let Some(v) = env(name.env_var()) {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(Some((SecretValue::new(v), "env")));
        }
    }
    Ok(store.get(name)?.map(|v| (v, store.backend())))
}

/// Whether `name` is set, with a hint; never the value.
pub fn status(store: &dyn SecretStore, name: SecretName) -> SecretStatus {
    status_with(store, name, &process_env)
}

/// [`status`] with the environment passed in. A store that cannot be read counts as unset
/// (and is logged).
pub fn status_with(store: &dyn SecretStore, name: SecretName, env: EnvLookup<'_>) -> SecretStatus {
    match resolve_with(store, name, env) {
        Ok(Some((v, source))) => SecretStatus {
            set: true,
            hint: Some(hint(v.expose())),
            source: Some(source.into()),
        },
        Ok(None) => SecretStatus::unset(),
        Err(e) => {
            tracing::warn!(secret = name.as_str(), "cannot read the secret: {e}");
            SecretStatus::unset()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Secret-shaped test values are split (GitHub push protection).
    const JEV_KEY: &str = concat!("ts_", "live_abcdef1234567890");
    const OTHER_KEY: &str = concat!("sk-", "other");

    fn secret(s: &str) -> Option<SecretValue> {
        Some(SecretValue::new(s))
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn file_store_sets_gets_deletes_and_stays_private() {
        let d = tempfile::tempdir().unwrap();
        let store = FileSecrets::new(d.path().join("secrets.toml"));
        assert_eq!(store.get(SecretName::JevApiKey).unwrap(), None);
        store.set(SecretName::JevApiKey, JEV_KEY).unwrap();
        store.set(SecretName::EmbeddingApiKey, OTHER_KEY).unwrap();
        assert_eq!(store.get(SecretName::JevApiKey).unwrap(), secret(JEV_KEY));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("secrets.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store.delete(SecretName::JevApiKey).unwrap());
        assert!(!store.delete(SecretName::JevApiKey).unwrap());
        assert_eq!(
            store.get(SecretName::EmbeddingApiKey).unwrap(),
            secret(OTHER_KEY)
        );
        assert!(store.set(SecretName::JevApiKey, "").is_err());
        assert!(store.set(SecretName::JevApiKey, "two\nlines").is_err());
        let left: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, vec!["secrets.toml"], "no temporary file is left");
    }

    #[test]
    fn status_shows_only_a_hint_and_the_environment_wins() {
        // The environment is passed in, so the test does not depend on (or touch) the real
        // TYPESAFE_API_KEY / ALICE_EMBEDDING_API_KEY of the machine running it.
        let d = tempfile::tempdir().unwrap();
        let store = FileSecrets::new(d.path().join("secrets.toml"));
        store.set(SecretName::EmbeddingApiKey, JEV_KEY).unwrap();
        let s = status_with(&store, SecretName::EmbeddingApiKey, &no_env);
        assert!(s.set);
        assert_eq!(
            (s.hint.as_deref(), s.source.as_deref()),
            (Some("••••7890"), Some("file"))
        );
        // The variable wins over the store, and only its hint is shown.
        let from_env = concat!("sk-", "from-env-5555");
        let env = |var: &str| (var == "ALICE_EMBEDDING_API_KEY").then(|| format!(" {from_env} "));
        let s = status_with(&store, SecretName::EmbeddingApiKey, &env);
        assert_eq!(
            (s.hint.as_deref(), s.source.as_deref()),
            (Some("••••5555"), Some("env"))
        );
        assert_eq!(
            resolve_with(&store, SecretName::EmbeddingApiKey, &env).unwrap(),
            Some((SecretValue::new(from_env), "env"))
        );
        // An empty variable does not count.
        let empty = |_: &str| Some("  ".to_string());
        assert_eq!(
            status_with(&store, SecretName::EmbeddingApiKey, &empty)
                .source
                .as_deref(),
            Some("file")
        );
        // Jev's key is read from TYPESAFE_API_KEY.
        let ts =
            |var: &str| (var == "TYPESAFE_API_KEY").then(|| concat!("ts-", "env-9999").to_string());
        assert_eq!(
            status_with(&store, SecretName::JevApiKey, &ts)
                .source
                .as_deref(),
            Some("env")
        );
        assert_eq!(hint("short"), "••••");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("abcdef"), "{json}");
        assert_eq!(
            SecretName::parse("jev_api_key"),
            Some(SecretName::JevApiKey)
        );
        assert_eq!(SecretName::parse("nope"), None);
        for n in SecretName::ALL {
            assert_eq!(SecretName::parse(n.as_str()), Some(n));
        }
    }

    #[test]
    fn values_never_show_in_debug_output_or_errors() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("secrets.toml");
        let store = FileSecrets::new(path.clone());
        store.set(SecretName::JevApiKey, JEV_KEY).unwrap();
        let got = resolve_with(&store, SecretName::JevApiKey, &no_env).unwrap();
        let shown = format!(
            "{got:?} {store:?} {:?}",
            status_with(&store, SecretName::JevApiKey, &no_env)
        );
        assert!(!shown.contains("abcdef"), "{shown}");
        // A broken file: the error names the file and the line, never what is in it.
        std::fs::write(&path, format!("jev_api_key = \"{JEV_KEY}\nother = 1\n")).unwrap();
        let e = store.get(SecretName::JevApiKey).unwrap_err().to_string();
        assert!(e.contains("secrets.toml") && e.contains("line 1"), "{e}");
        assert!(!e.contains("abcdef"), "{e}");
        let e = store
            .set(SecretName::EmbeddingApiKey, OTHER_KEY)
            .unwrap_err()
            .to_string();
        assert!(!e.contains("abcdef"), "{e}");
        assert!(!status_with(&store, SecretName::JevApiKey, &no_env).set);
        // A refused value is not echoed either.
        let e = store
            .set(SecretName::JevApiKey, &format!("{JEV_KEY}\nmore"))
            .unwrap_err()
            .to_string();
        assert!(!e.contains("abcdef"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_secrets_file_others_can_read_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("secrets.toml");
        std::fs::write(&path, format!("jev_api_key = \"{JEV_KEY}\"\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let store = FileSecrets::new(path.clone());
        assert_eq!(store.get(SecretName::JevApiKey).unwrap(), secret(JEV_KEY));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// An in-memory keychain: the tests never touch the real OS keychain.
    #[derive(Clone, Default)]
    struct FakeKeychain {
        entries: Arc<Mutex<HashMap<String, String>>>,
        failure: Arc<Mutex<Option<String>>>,
    }

    impl FakeKeychain {
        fn check(&self) -> std::result::Result<(), String> {
            match self.failure.lock().clone() {
                Some(f) => Err(f),
                None => Ok(()),
            }
        }
    }

    impl KeychainApi for FakeKeychain {
        fn get(&self, account: &str) -> std::result::Result<Option<String>, String> {
            self.check()?;
            Ok(self.entries.lock().get(account).cloned())
        }
        fn set(&self, account: &str, value: &str) -> std::result::Result<(), String> {
            self.check()?;
            self.entries.lock().insert(account.into(), value.into());
            Ok(())
        }
        fn delete(&self, account: &str) -> std::result::Result<bool, String> {
            self.check()?;
            Ok(self.entries.lock().remove(account).is_some())
        }
    }

    #[test]
    fn the_keychain_store_keeps_each_secret_under_its_name() {
        let fake = FakeKeychain::default();
        let store = KeychainSecrets::with_api(Box::new(fake.clone()));
        assert_eq!(store.backend(), "keychain");
        assert_eq!(store.get(SecretName::JevApiKey).unwrap(), None);
        store
            .set(SecretName::JevApiKey, &format!("  {JEV_KEY} "))
            .unwrap();
        assert_eq!(
            fake.entries.lock().get("jev_api_key").map(String::as_str),
            Some(JEV_KEY),
            "stored trimmed, under the secret's name"
        );
        assert_eq!(store.get(SecretName::JevApiKey).unwrap(), secret(JEV_KEY));
        assert!(store.set(SecretName::JevApiKey, " ").is_err());
        assert!(store.delete(SecretName::JevApiKey).unwrap());
        assert!(!store.delete(SecretName::JevApiKey).unwrap());
        // A keychain failure says what to do, without the value.
        *fake.failure.lock() = Some("the keychain is locked".into());
        let e = store
            .set(SecretName::JevApiKey, JEV_KEY)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("the keychain is locked") && e.contains("secrets.backend"),
            "{e}"
        );
        assert!(!e.contains("abcdef"), "{e}");
        assert!(store.get(SecretName::JevApiKey).is_err());
        assert!(store.delete(SecretName::JevApiKey).is_err());
        assert!(!status_with(&store, SecretName::JevApiKey, &no_env).set);
    }

    #[test]
    fn open_store_follows_the_backend_setting() {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        let s = open_store(&h, SecretsBackend::File);
        assert_eq!(s.backend(), "file");
        s.set(SecretName::JevApiKey, JEV_KEY).unwrap();
        assert!(h.secrets_file().exists());
        // Choosing the keychain names the backend only; nothing reads the keychain here.
        let k = open_store(&h, SecretsBackend::Keychain);
        #[cfg(feature = "keychain")]
        assert_eq!(k.backend(), "keychain");
        #[cfg(not(feature = "keychain"))]
        assert_eq!(k.backend(), "file", "a build without keychain support");
    }
}
