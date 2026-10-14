//! Settings (spec §6): one schema as Rust structs; layers defaults → `config.toml` →
//! environment (`ALICE_<SECTION>_<KEY>`), validated as a whole, so an invalid layer never
//! replaces a valid configuration. A repository's `.alice.toml` may only make Alice more
//! careful with that project's memories (`ProjectSettings`).
//!
//! The JSON Schema of [`Settings`] is a public contract: the web UI, `alice config` and the
//! reference docs are generated from it, so every key carries a description.
//!
//! Error messages name keys and variables. They never quote a broken line of a file, the value
//! of an unknown key, or the value of a variable that is not a setting: a secret pasted into
//! the wrong place must not reach a log, the API or a terminal.

use crate::search::fusion::{HalfLives, Weights};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

/// Keys the API and `alice config set` refuse to change.
pub const READ_ONLY_KEYS: &[&str] = &["storage.data_dir"];
/// Keys that take effect only after `alice daemon stop` and a new start.
pub const RESTART_KEYS: &[&str] = &["server.port", "secrets.backend"];

/// `ALICE_*` variables read elsewhere that are not settings (never warned about). Secrets are
/// recognised here by name; `resolve` never reads their values.
pub const NON_SETTING_ENV: &[&str] = &[
    "ALICE_HOME",
    "ALICE_LOG",
    "ALICE_EMBEDDER",
    "ALICE_AGENT",
    "ALICE_MODELS_DIR",
    "ALICE_NO_AUTOSTART",
    "ALICE_EVAL_OUT",
    "ALICE_TEST_BULK",
    "ALICE_RECALL_EXTRA_DIR",
    "ALICE_RECALL_PASSAGES",
    "ALICE_EMBEDDING_API_KEY",
];

/// The model the `local` embedding provider runs (`alice_embed::MODEL_ID`; `alice-core` does
/// not depend on `alice-embed`).
const LOCAL_MODEL: &str = "multilingual-e5-small-q8";

// ---------------------------------------------------------------- secrets

/// A secret such as an API key. `Debug` never shows it, and it has neither `Display` nor
/// `Serialize`, so it cannot reach a log line, an error, an event or a response by accident;
/// [`SecretValue::expose`] is the one way to read it, for the request that needs it.
///
/// ```
/// use alice_core::settings::SecretValue;
/// let key = SecretValue::new(concat!("ts_", "live_0123456789"));
/// assert_eq!(format!("{key:?}"), "SecretValue(<redacted>)");
/// assert!(key.expose().ends_with("6789"));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself: only for the request or the store that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

// ---------------------------------------------------------------- units

/// A duration written `90d`, `12h`, `15m`, `30s`, or `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("\"{s}\" is not a duration; write it like 90d, 12h, 15m or 30s");
        let t = s.trim();
        if t == "0" {
            return Ok(Self(Duration::ZERO));
        }
        let unit = t.chars().last().ok_or_else(bad)?;
        let n: u64 = t[..t.len() - unit.len_utf8()]
            .trim()
            .parse()
            .map_err(|_| bad())?;
        let secs_per = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return Err(bad()),
        };
        n.checked_mul(secs_per)
            .map(|secs| Self(Duration::from_secs(secs)))
            .ok_or_else(bad)
    }
}

impl std::fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0.as_secs();
        match s {
            0 => write!(f, "0"),
            _ if s.is_multiple_of(86_400) => write!(f, "{}d", s / 86_400),
            _ if s.is_multiple_of(3_600) => write!(f, "{}h", s / 3_600),
            _ if s.is_multiple_of(60) => write!(f, "{}m", s / 60),
            _ => write!(f, "{s}s"),
        }
    }
}

/// Bytes written `10GB`, `500MB`, `1.5TB` (decimal units) or a plain number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("\"{s}\" is not a size; write it like 10GB or 500MB");
        let t: String = s
            .trim()
            .to_ascii_uppercase()
            .chars()
            .filter(|c| *c != ' ')
            .collect();
        let split = t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len());
        let (num, unit) = t.split_at(split);
        let n: f64 = num.parse().map_err(|_| bad())?;
        let mult = match unit {
            "" | "B" => 1.0,
            "KB" => 1e3,
            "MB" => 1e6,
            "GB" => 1e9,
            "TB" => 1e12,
            _ => return Err(bad()),
        };
        let bytes = (n * mult).round();
        if !bytes.is_finite() || bytes < 0.0 || bytes >= u64::MAX as f64 {
            return Err(bad());
        }
        Ok(Self(bytes as u64))
    }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0;
        for (unit, m) in [("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("KB", 1e3)] {
            if b as f64 >= m && (b as f64 / m).fract() == 0.0 {
                return write!(f, "{}{unit}", b as f64 / m);
            }
        }
        write!(f, "{b}B")
    }
}

/// How long raw transcripts are kept: `forever`, an age (`90d`) or a total size (`10GB`,
/// counted as the raw chunks' text bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retention {
    #[default]
    Forever,
    Age(Duration),
    Size(u64),
}

impl Retention {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("forever") {
            return Ok(Self::Forever);
        }
        if let Ok(d) = HumanDuration::parse(t)
            && !d.0.is_zero()
        {
            return Ok(Self::Age(d.0));
        }
        match ByteSize::parse(t) {
            // "0" would delete every transcript: never a default-looking value.
            Ok(b) if b.0 > 0 => Ok(Self::Size(b.0)),
            _ => Err(format!(
                "\"{s}\" is not a retention; use forever, an age such as 90d, or a size such as 10GB"
            )),
        }
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Retention::Forever => write!(f, "forever"),
            Retention::Age(d) => write!(f, "{}", HumanDuration(*d)),
            Retention::Size(b) => write!(f, "{}", ByteSize(*b)),
        }
    }
}

/// Units are written as strings (`"90d"`); a bare number (`0`) is read as its text.
macro_rules! string_serde {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let v = serde_json::Value::deserialize(d)?;
                let s = match &v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => {
                        return Err(serde::de::Error::custom(
                            "expected a string such as \"90d\"",
                        ));
                    }
                };
                <$t>::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}
string_serde!(HumanDuration);
string_serde!(ByteSize);
string_serde!(Retention);

// ---------------------------------------------------------------- groups

/// Everything Alice can be configured with. Each group is a `[section]` of `config.toml`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Where memories are kept and for how long.
    pub storage: StorageSettings,
    /// The embedding model that turns text into vectors.
    pub embedding: EmbeddingSettings,
    /// Recall ranking and limits.
    pub search: SearchSettings,
    /// Which agents' sessions are captured.
    pub capture: CaptureSettings,
    /// The judge that decides what to do with each new memory.
    pub judge: JudgeSettings,
    /// Turning sessions into memories.
    pub distill: DistillSettings,
    /// Secret redaction and what may leave this machine.
    pub privacy: PrivacySettings,
    /// Git sync of memories between machines.
    pub sync: SyncSettings,
    /// Automatic backups of the database.
    pub backup: BackupSettings,
    /// Compatibility layers for other memory tools.
    pub compat: CompatSettings,
    /// The local web UI and HTTP API.
    pub server: ServerSettings,
    /// The MCP server that agents talk to.
    pub mcp: McpSettings,
    /// Where API keys are kept.
    pub secrets: SecretsSettings,
    /// Background jobs that keep the store tidy.
    pub maintenance: MaintenanceSettings,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSettings {
    /// Where Alice keeps its data (read-only here: set the ALICE_HOME environment variable).
    pub data_dir: Option<String>,
    /// How long raw transcripts are kept: `forever`, an age such as `90d` (at least `1d`), or
    /// a total size such as `10GB` (at least `1MB`).
    #[schemars(with = "String")]
    pub raw_retention: Retention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProvider {
    /// The built-in model, on this machine.
    #[default]
    Local,
    /// OpenAI's embeddings API (needs `embedding_api_key`).
    Openai,
    /// A local or LAN Ollama server.
    Ollama,
    /// Voyage AI's embeddings API (needs `embedding_api_key`).
    Voyage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingSettings {
    /// Who computes embeddings: `local`, `openai`, `ollama` or `voyage`.
    pub provider: EmbeddingProvider,
    /// `multilingual-e5-small-q8` for `local`; the provider's model name otherwise.
    pub model: String,
    /// Base URL for HTTP providers (defaults per provider). https:// is required when an API
    /// key is sent, except to 127.0.0.1 or localhost.
    pub endpoint: Option<String>,
    /// Texts per embedding request (1 to 256).
    pub batch_size: usize,
    /// Unload the local model after this long without use, such as `10m` (`0` = never).
    #[schemars(with = "String")]
    pub idle_unload: HumanDuration,
}

impl Default for EmbeddingSettings {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::Local,
            model: LOCAL_MODEL.into(),
            endpoint: None,
            batch_size: 32,
            idle_unload: HumanDuration(Duration::from_secs(600)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SearchSettings {
    /// Token budget of a recall answer when the agent does not ask for one (100 to 50000).
    pub default_max_tokens: usize,
    /// Cosine similarity at which a match found only by meaning counts as relevant (0 to 1).
    pub relevance_floor: f64,
    /// Fewer relevant memories than this bring in raw transcript chunks (0 to 50).
    pub raw_fallback_min_results: usize,
    /// Ranking multipliers.
    pub weights: Weights,
    /// Recency half-lives in days per kind of memory (0 = no decay).
    pub half_life: HalfLives,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            default_max_tokens: crate::model::limits::DEFAULT_RECALL_TOKENS,
            // `search::DEFAULT_RELEVANCE_FLOOR` as written (the f32 widened would show as
            // 0.8199999928474426); a test keeps the two equal.
            relevance_floor: 0.82,
            raw_fallback_min_results: 3,
            weights: Weights::default(),
            half_life: HalfLives::default(),
        }
    }
}

/// Per-agent capture switches (used by SP2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureSettings {
    /// Capture Claude Code sessions.
    pub claude_code: bool,
    /// Capture Codex sessions.
    pub codex: bool,
    /// Capture Cursor sessions.
    pub cursor: bool,
    /// Capture Windsurf sessions.
    pub windsurf: bool,
    /// Capture opencode sessions.
    pub opencode: bool,
    /// Capture Gemini CLI sessions.
    pub gemini: bool,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            claude_code: true,
            codex: true,
            cursor: true,
            windsurf: true,
            opencode: true,
            gemini: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JudgeProvider {
    /// Rules on this machine; free, no network.
    #[default]
    Heuristic,
    /// TypeSafe Jev (needs `jev_api_key`); falls back to the heuristic when unavailable.
    Jev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct JevSettings {
    /// The Jev model, pinned so decisions stay comparable.
    pub model: String,
    /// The Jev API URL (https://, or http:// to 127.0.0.1 or localhost).
    pub endpoint: String,
    /// Requests per second, above 0 and at most 10.
    pub rate_per_sec: f64,
}

impl Default for JevSettings {
    fn default() -> Self {
        Self {
            model: "jev-1.13.0".into(),
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            rate_per_sec: 10.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct JudgeSettings {
    /// Who judges new memories: `heuristic` or `jev`.
    pub provider: JudgeProvider,
    /// Decisions at or above this probability are applied without asking (0 to 1).
    pub auto_apply_threshold: f64,
    /// Decisions at or above this (and below auto) become review items; lower ones are
    /// dropped (0 to 1, not above `auto_apply_threshold`).
    pub review_threshold: f64,
    /// Hard stop for paid judges, in US dollars per UTC day (0 stops them).
    pub daily_budget_usd: f64,
    /// How long one judge request may take, in milliseconds (100 to 120000).
    pub timeout_ms: u64,
    /// Cosine similarity at which the weekly job suggests merging memories (0.5 to 1).
    pub cluster_similarity: f64,
    /// TypeSafe Jev.
    pub jev: JevSettings,
}

impl Default for JudgeSettings {
    fn default() -> Self {
        Self {
            provider: JudgeProvider::Heuristic,
            auto_apply_threshold: 0.85,
            review_threshold: 0.5,
            daily_budget_usd: 1.0,
            timeout_ms: 10_000,
            cluster_similarity: 0.92,
            jev: JevSettings::default(),
        }
    }
}

/// Distillation (used by SP2).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DistillSettings {
    /// Who turns sessions into memories (SP2).
    pub provider: Option<String>,
    /// The model the distill provider uses (SP2).
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RedactMode {
    /// Replace secrets with `[REDACTED:<type>]`.
    #[default]
    Redact,
    /// Refuse the write, naming the detector.
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomPattern {
    /// Shown as `[REDACTED:custom:<name>]`; lowercase letters, digits and `_`.
    pub name: String,
    /// A regular expression (Rust `regex` syntax).
    pub regex: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacySettings {
    /// Remove secrets (API keys, tokens, passwords) from text before it is stored.
    pub redact: bool,
    /// What happens to a write that contains a secret: `redact` it or `block` the write.
    pub redact_mode: RedactMode,
    /// Extra secrets to redact: a list of `{ name, regex }`.
    pub custom_patterns: Vec<CustomPattern>,
    /// Regular expressions; a match that one of them matches completely is kept.
    pub allowlist: Vec<String>,
    /// Project slugs never sent to a cloud judge or embedding provider.
    pub cloud_denied_projects: Vec<String>,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self {
            redact: true,
            redact_mode: RedactMode::Redact,
            custom_patterns: Vec::new(),
            allowlist: Vec::new(),
            cloud_denied_projects: Vec::new(),
        }
    }
}

/// Git sync (used by M4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SyncSettings {
    /// The git remote memories are synced with; credentials belong in git's credential
    /// helper, never in the URL.
    pub remote: Option<String>,
    /// The branch synced.
    pub branch: String,
    /// Automatic sync: `off`, `idle`, or an interval of at least `1m` such as `15m`.
    pub auto: String,
    /// Sync raw transcripts too, not only memories.
    pub include_raw: bool,
    /// Project slugs never synced.
    pub excluded_projects: Vec<String>,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            remote: None,
            branch: "main".into(),
            auto: "off".into(),
            include_raw: false,
            excluded_projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BackupSettings {
    /// Daily backups kept (at least 1).
    pub daily: u32,
    /// Weekly backups kept.
    pub weekly: u32,
    /// Backup directory (default `<data dir>/backups`).
    pub dir: Option<String>,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            daily: 7,
            weekly: 4,
            dir: None,
        }
    }
}

/// MemPalace compatibility layer (used by M3).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompatSettings {
    /// Offer MemPalace's tool names to agents that still use them (M3).
    pub mempalace: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    /// Port on 127.0.0.1 for the web UI and API; 0 = a free port, chosen at each start.
    pub port: u16,
    /// Open the web UI in a browser when `alice ui` runs.
    pub open_browser: bool,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            port: 0,
            open_browser: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettings {
    /// Offer review_list, review_resolve, merge_memories and stats to agents.
    pub admin_tools: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretsBackend {
    /// `secrets.toml` in the data directory, readable by you only.
    #[default]
    File,
    /// The OS keychain (macOS Keychain, Windows Credential Manager, Secret Service).
    Keychain,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsSettings {
    /// Where API keys are kept: `file` (secrets.toml, readable by you only) or `keychain`
    /// (the OS keychain; on macOS it may ask for permission after each update).
    pub backend: SecretsBackend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceSettings {
    /// Run background maintenance jobs.
    pub enabled: bool,
    /// Jobs start after the daemon has had no requests for this many seconds (at least 5).
    pub idle_secs: u64,
    /// Forgotten memories are deleted for good after this many days (at least 1).
    pub trash_days: u32,
    /// History events older than this many days are deleted (at least 30).
    pub events_days: u32,
}

impl Default for MaintenanceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_secs: 60,
            trash_days: 30,
            events_days: 365,
        }
    }
}

// ---------------------------------------------------------------- errors and sources

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SettingError {
    /// A dotted key (`judge.review_threshold`), `config.toml`, or a file path.
    pub key: String,
    pub message: String,
}

fn err(key: impl Into<String>, message: impl Into<String>) -> SettingError {
    SettingError {
        key: key.into(),
        message: message.into(),
    }
}

/// Where a key's effective value comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Default,
    File,
    Env { var: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub settings: Settings,
    /// Where each leaf key's value came from.
    pub sources: BTreeMap<String, Source>,
    /// `ALICE_*` variables that were ignored (not a setting, not a known non-setting variable).
    pub warnings: Vec<String>,
}

/// One key of the schema, as `alice config list` and the settings API show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KeyInfo {
    pub key: String,
    pub env_var: String,
    pub read_only: bool,
    pub restart: bool,
}

/// The environment variable that sets `key`: `judge.jev.rate_per_sec` →
/// `ALICE_JUDGE_JEV_RATE_PER_SEC`.
pub fn env_var(key: &str) -> String {
    format!("ALICE_{}", key.replace('.', "_").to_ascii_uppercase())
}

/// Leaf keys (objects are walked; arrays, scalars and nulls are leaves), with their values.
pub fn leaves(v: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, serde_json::Value)>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&key, x, out);
                }
            }
            other => out.push((prefix.to_string(), other.clone())),
        }
    }
    let mut out = Vec::new();
    walk("", v, &mut out);
    out
}

static DEFAULTS: LazyLock<serde_json::Value> =
    LazyLock::new(|| serde_json::to_value(Settings::default()).expect("settings serialise"));
/// Leaf keys in schema order, with their default values.
static DEFAULT_LEAVES: LazyLock<Vec<(String, serde_json::Value)>> =
    LazyLock::new(|| leaves(&DEFAULTS));
static DEFAULT_MAP: LazyLock<BTreeMap<String, serde_json::Value>> =
    LazyLock::new(|| DEFAULT_LEAVES.iter().cloned().collect());
/// Environment variable → key.
static ENV_KEYS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    DEFAULT_LEAVES
        .iter()
        .map(|(k, _)| (env_var(k), k.clone()))
        .collect()
});

pub fn key_infos() -> Vec<KeyInfo> {
    DEFAULT_LEAVES
        .iter()
        .map(|(key, _)| KeyInfo {
            key: key.clone(),
            env_var: env_var(key),
            read_only: READ_ONLY_KEYS.contains(&key.as_str()),
            restart: RESTART_KEYS.contains(&key.as_str()),
        })
        .collect()
}

/// The setting key an environment variable sets, if it is one.
pub fn key_for_env_var(var: &str) -> Option<&'static str> {
    ENV_KEYS.get(var).map(String::as_str)
}

/// The value of one key (a leaf or a group) as JSON.
pub fn get_key(s: &Settings, key: &str) -> Option<serde_json::Value> {
    let v = serde_json::to_value(s).ok()?;
    key.split('.').try_fold(&v, |v, part| v.get(part)).cloned()
}

/// Set a leaf key, creating the objects along it (merged documents start as the defaults, so
/// for known keys they already exist).
fn set_path(root: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    let mut cur = root;
    for part in key.split('.') {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        cur = cur
            .as_object_mut()
            .expect("made an object above")
            .entry(part.to_string())
            .or_insert(serde_json::Value::Null);
    }
    *cur = value;
}

/// The given JSON kind fits the default's: null defaults (optional text) take anything for
/// serde to check, and text takes numbers (units: `idle_unload = 0`).
fn same_kind(default: &serde_json::Value, given: &serde_json::Value) -> bool {
    use serde_json::Value::*;
    matches!(
        (default, given),
        (Null, _)
            | (Bool(_), Bool(_))
            | (Number(_), Number(_))
            | (String(_), String(_))
            | (String(_), Number(_))
            | (Array(_), Array(_))
    )
}

fn kind_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "nan or inf",
        serde_json::Value::Bool(_) => "true or false",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "text",
        serde_json::Value::Array(_) => "a list, written like [\"a\", \"b\"]",
        serde_json::Value::Object(_) => "a table",
    }
}

/// An environment value: TOML syntax if it parses (`0.7`, `true`, `["a"]`), else a string.
fn env_value(raw: &str) -> serde_json::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|t| t.get("v").cloned())
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_else(|| serde_json::Value::String(raw.to_string()))
}

/// "line N: <what is wrong>", without quoting the line (it may hold a secret).
fn toml_error(text: &str, message: &str, span: Option<std::ops::Range<usize>>) -> String {
    let message = message.trim().trim_end_matches('.');
    match span {
        Some(s) => {
            let at = s.start.min(text.len());
            let line = text.as_bytes()[..at]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1;
            format!("line {line}: {message}")
        }
        None => message.to_string(),
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The candidate within two edits of `name`, if any.
fn closest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn looks_like_a_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["key", "token", "secret", "password", "passwd", "credential"]
        .iter()
        .any(|w| n.contains(w))
}

const SET_SECRET_HINT: &str = "API keys are kept out of config.toml and the settings: set them with `alice config set-secret <name>`, or in TYPESAFE_API_KEY or ALICE_EMBEDDING_API_KEY";

fn unknown_key_message(key: &str) -> String {
    let group = format!("{key}.");
    if let Some((first, _)) = DEFAULT_LEAVES.iter().find(|(k, _)| k.starts_with(&group)) {
        return format!("is a group of settings, not a value; set its keys, such as {first}");
    }
    let last = key.rsplit('.').next().unwrap_or(key);
    if looks_like_a_secret(last) {
        return format!("not a setting; {SET_SECRET_HINT}");
    }
    match closest(key, DEFAULT_LEAVES.iter().map(|(k, _)| k.as_str())) {
        Some(near) => {
            format!("not a setting (did you mean {near}?); see `alice config list` for the keys")
        }
        None => "not a setting; see `alice config list` for the keys".to_string(),
    }
}

fn unknown_var_warning(var: &str) -> String {
    let mut w = format!("{var} is not a setting and is ignored");
    if looks_like_a_secret(var) {
        w.push_str("; ");
        w.push_str(SET_SECRET_HINT);
    } else if let Some(near) = closest(var, ENV_KEYS.keys().map(String::as_str)) {
        w.push_str(&format!(" (did you mean {near}?)"));
    }
    w.push_str("; see `alice config list` for the variables");
    w
}

/// Merge the layers and validate the result as a whole.
///
/// `file` is the text of `config.toml`; `env` holds the process's variables (only `ALICE_*`
/// names are looked at, and only setting variables' values are read). Variables that are not
/// settings are ignored with a warning, never an error.
///
/// ```
/// use alice_core::settings::{JudgeProvider, Source, resolve};
/// let file = "[judge]\nprovider = \"jev\"\n";
/// let env = vec![("ALICE_JUDGE_REVIEW_THRESHOLD".to_string(), "0.6".to_string())];
/// let r = resolve(Some(file), &env).unwrap();
/// assert_eq!(r.settings.judge.provider, JudgeProvider::Jev);
/// assert_eq!(r.settings.judge.review_threshold, 0.6);
/// assert_eq!(r.sources["judge.provider"], Source::File);
/// assert!(resolve(Some("[judge]\nreview_threshold = 2\n"), &[]).is_err());
/// ```
pub fn resolve(
    file: Option<&str>,
    env: &[(String, String)],
) -> Result<Resolved, Vec<SettingError>> {
    let mut merged = DEFAULTS.clone();
    let mut sources: BTreeMap<String, Source> = DEFAULT_LEAVES
        .iter()
        .map(|(k, _)| (k.clone(), Source::Default))
        .collect();
    let mut errors = Vec::new();

    if let Some(text) = file.filter(|t| !t.trim().is_empty()) {
        match toml::from_str::<toml::Table>(text) {
            Err(e) => errors.push(err(
                "config.toml",
                format!(
                    "{}; fix the file and save it again",
                    toml_error(text, e.message(), e.span())
                ),
            )),
            Ok(table) => {
                let given = serde_json::to_value(table).unwrap_or_default();
                for (key, value) in leaves(&given) {
                    match DEFAULT_MAP.get(&key) {
                        None => {
                            let message = unknown_key_message(&key);
                            errors.push(err(key, message));
                        }
                        Some(_) if READ_ONLY_KEYS.contains(&key.as_str()) => errors.push(err(
                            key,
                            "cannot be set in config.toml; set the ALICE_HOME environment variable instead",
                        )),
                        Some(d) if value.is_null() => errors.push(err(
                            key,
                            format!("must be {}, not nan or inf", kind_name(d)),
                        )),
                        Some(d) if !same_kind(d, &value) => {
                            let message =
                                format!("must be {}, not {}", kind_name(d), kind_name(&value));
                            errors.push(err(key, message));
                        }
                        Some(_) => {
                            set_path(&mut merged, &key, value);
                            sources.insert(key, Source::File);
                        }
                    }
                }
            }
        }
    }

    let mut warnings = Vec::new();
    for (var, raw) in env {
        match ENV_KEYS.get(var) {
            Some(key) if READ_ONLY_KEYS.contains(&key.as_str()) => errors.push(err(
                key,
                format!("{var} cannot set it; set the ALICE_HOME environment variable instead"),
            )),
            Some(key) => {
                let d = &DEFAULT_MAP[key];
                let mut value = env_value(raw);
                if (d.is_string() || d.is_null()) && !value.is_string() {
                    value = serde_json::Value::String(raw.clone());
                }
                if same_kind(d, &value) {
                    set_path(&mut merged, key, value);
                    sources.insert(key.clone(), Source::Env { var: var.clone() });
                } else {
                    // The value is not quoted: the variable may hold something else entirely.
                    errors.push(err(key, format!("{var} must be {}", kind_name(d))));
                }
            }
            // Unknown variables never invalidate the configuration: a typo in one variable must
            // not send every setting back to its default. Values are never echoed (secrets).
            None if NON_SETTING_ENV.contains(&var.as_str()) => {}
            None if var.starts_with("ALICE_") => warnings.push(unknown_var_warning(var)),
            None => {}
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    let settings: Settings = match serde_json::from_value(merged.clone()) {
        Ok(s) => s,
        Err(first) => {
            // serde's message does not name the field: find each leaf that alone breaks it
            // (an unknown enum variant, a unit that does not parse, a number out of range).
            let errs: Vec<SettingError> = leaves(&merged)
                .into_iter()
                .filter(|(k, v)| DEFAULT_MAP.get(k) != Some(v))
                .filter_map(|(k, v)| {
                    let mut probe = DEFAULTS.clone();
                    set_path(&mut probe, &k, v);
                    serde_json::from_value::<Settings>(probe)
                        .err()
                        .map(|e| err(k, e.to_string()))
                })
                .collect();
            return Err(if errs.is_empty() {
                vec![err("config.toml", first.to_string())]
            } else {
                errs
            });
        }
    };
    let invalid = settings.validate();
    if !invalid.is_empty() {
        return Err(invalid);
    }
    Ok(Resolved {
        settings,
        sources,
        warnings,
    })
}

// ---------------------------------------------------------------- validation

/// The parts of a URL the checks need; `None` when it is not `scheme://…`.
struct UrlParts<'a> {
    scheme: String,
    userinfo: Option<&'a str>,
    host: &'a str,
}

fn url_parts(url: &str) -> Option<UrlParts<'_>> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let host = match hostport.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => hostport.split(':').next().unwrap_or(""),
    };
    Some(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        userinfo,
        host,
    })
}

fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost")
}

/// An HTTP endpoint. When `carries_key`, plain http is allowed to this machine only. The URL is
/// never quoted in the message (it may hold credentials).
fn check_endpoint(e: &mut Vec<SettingError>, key: &str, url: &str, carries_key: bool) {
    let Some(u) = url_parts(url) else {
        e.push(err(key, "must be an http:// or https:// URL"));
        return;
    };
    if u.scheme != "http" && u.scheme != "https" {
        e.push(err(key, "must be an http:// or https:// URL"));
    } else if u.host.is_empty() {
        e.push(err(
            key,
            "must name a host, such as https://api.example.com/v1",
        ));
    } else if u.userinfo.is_some() {
        e.push(err(
            key,
            format!("must not contain a user name or password; {SET_SECRET_HINT}"),
        ));
    } else if carries_key && u.scheme == "http" && !is_loopback(u.host) {
        e.push(err(
            key,
            "must be an https:// URL: it receives your API key (plain http:// is allowed to 127.0.0.1 and localhost only)",
        ));
    }
}

impl Settings {
    /// Every rule the types cannot express. Empty when the settings are valid.
    pub fn validate(&self) -> Vec<SettingError> {
        let mut e = Vec::new();
        let unit = |e: &mut Vec<SettingError>, key: &str, v: f64| {
            if !(0.0..=1.0).contains(&v) {
                e.push(err(key, format!("{v} is outside 0 to 1")));
            }
        };

        let st = &self.storage;
        match st.raw_retention {
            Retention::Age(d) if d < Duration::from_secs(86_400) => e.push(err(
                "storage.raw_retention",
                "an age must be at least 1d (did you mean days, such as 90d?)",
            )),
            Retention::Size(b) if b < 1_000_000 => e.push(err(
                "storage.raw_retention",
                "a size must be at least 1MB (write the unit, such as 10GB)",
            )),
            _ => {}
        }

        let j = &self.judge;
        unit(&mut e, "judge.auto_apply_threshold", j.auto_apply_threshold);
        unit(&mut e, "judge.review_threshold", j.review_threshold);
        if j.review_threshold > j.auto_apply_threshold {
            e.push(err(
                "judge.review_threshold",
                format!(
                    "{} is above judge.auto_apply_threshold ({}); the review threshold must not be higher",
                    j.review_threshold, j.auto_apply_threshold
                ),
            ));
        }
        if j.daily_budget_usd < 0.0 {
            e.push(err(
                "judge.daily_budget_usd",
                "must be 0 or more (0 stops paid judges)",
            ));
        }
        if !(100..=120_000).contains(&j.timeout_ms) {
            e.push(err("judge.timeout_ms", "must be between 100 and 120000"));
        }
        if !(0.5..=1.0).contains(&j.cluster_similarity) {
            e.push(err("judge.cluster_similarity", "must be between 0.5 and 1"));
        }
        if !(j.jev.rate_per_sec > 0.0 && j.jev.rate_per_sec <= 10.0) {
            e.push(err(
                "judge.jev.rate_per_sec",
                "must be above 0 and at most 10",
            ));
        }
        check_endpoint(&mut e, "judge.jev.endpoint", &j.jev.endpoint, true);
        if j.jev.model.trim().is_empty() {
            e.push(err("judge.jev.model", "must not be empty"));
        }

        let s = &self.search;
        unit(&mut e, "search.relevance_floor", s.relevance_floor);
        if !(100..=50_000).contains(&s.default_max_tokens) {
            e.push(err(
                "search.default_max_tokens",
                "must be between 100 and 50000",
            ));
        }
        if s.raw_fallback_min_results > crate::model::limits::MAX_RECALL_LIMIT {
            e.push(err(
                "search.raw_fallback_min_results",
                format!("must be at most {}", crate::model::limits::MAX_RECALL_LIMIT),
            ));
        }
        for (group, v) in [
            ("weights", serde_json::to_value(&s.weights)),
            ("half_life", serde_json::to_value(&s.half_life)),
        ] {
            for (k, x) in leaves(&v.unwrap_or_default()) {
                if x.as_f64().is_some_and(|x| x < 0.0) {
                    e.push(err(format!("search.{group}.{k}"), "must be 0 or more"));
                }
            }
        }

        let m = &self.embedding;
        if !(1..=256).contains(&m.batch_size) {
            e.push(err("embedding.batch_size", "must be between 1 and 256"));
        }
        if m.model.trim().is_empty() {
            e.push(err("embedding.model", "must not be empty"));
        }
        if m.provider == EmbeddingProvider::Local && m.model != LOCAL_MODEL {
            e.push(err(
                "embedding.model",
                format!("the local provider runs {LOCAL_MODEL} only; use provider openai, ollama or voyage for other models"),
            ));
        }
        if let Some(url) = &m.endpoint {
            let carries_key = matches!(
                m.provider,
                EmbeddingProvider::Openai | EmbeddingProvider::Voyage
            );
            check_endpoint(&mut e, "embedding.endpoint", url, carries_key);
        }

        let p = &self.privacy;
        for (i, c) in p.custom_patterns.iter().enumerate() {
            if c.name.is_empty()
                || !c
                    .name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            {
                e.push(err(
                    "privacy.custom_patterns",
                    format!(
                        "pattern {i}: name \"{}\" must be lowercase letters, digits and _",
                        c.name
                    ),
                ));
            }
            if let Err(x) = regex::Regex::new(&c.regex) {
                e.push(err(
                    "privacy.custom_patterns",
                    format!("pattern {}: {x}", c.name),
                ));
            }
        }
        for a in &p.allowlist {
            if let Err(x) = regex::Regex::new(a) {
                e.push(err("privacy.allowlist", format!("{a}: {x}")));
            }
        }
        if p.cloud_denied_projects.iter().any(|s| s.trim().is_empty()) {
            e.push(err(
                "privacy.cloud_denied_projects",
                "project slugs must not be empty",
            ));
        }

        let y = &self.sync;
        if let Some(remote) = &y.remote
            && let Some(u) = url_parts(remote)
            && u.userinfo.is_some_and(|i| i.contains(':'))
        {
            e.push(err(
                "sync.remote",
                "must not contain a password; use git's credential helper",
            ));
        }
        if y.branch.trim().is_empty() || y.branch.chars().any(char::is_whitespace) {
            e.push(err("sync.branch", "must be a branch name without spaces"));
        }
        let auto = y.auto.trim();
        let interval_ok = HumanDuration::parse(auto).is_ok_and(|d| d.0 >= Duration::from_secs(60));
        if auto != "off" && auto != "idle" && !interval_ok {
            e.push(err(
                "sync.auto",
                "must be off, idle, or an interval of at least 1m such as 15m",
            ));
        }

        if self.backup.daily < 1 {
            e.push(err("backup.daily", "keep at least 1 daily backup"));
        }
        if self
            .backup
            .dir
            .as_deref()
            .is_some_and(|d| d.trim().is_empty())
        {
            e.push(err(
                "backup.dir",
                "must not be empty; remove it for the default",
            ));
        }

        let mt = &self.maintenance;
        if mt.idle_secs < 5 {
            e.push(err("maintenance.idle_secs", "must be at least 5"));
        }
        if mt.trash_days < 1 {
            e.push(err("maintenance.trash_days", "must be at least 1"));
        }
        if mt.events_days < 30 {
            e.push(err(
                "maintenance.events_days",
                "must be at least 30 (undo needs the history)",
            ));
        }
        e
    }
}

// ---------------------------------------------------------------- editing config.toml

fn json_to_toml(v: &serde_json::Value) -> Result<toml_edit::Value, String> {
    use serde_json::Value as J;
    Ok(match v {
        J::Bool(b) => (*b).into(),
        J::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64().ok_or("a number TOML cannot hold")?.into(),
        },
        J::String(s) => s.as_str().into(),
        J::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(json_to_toml(x)?);
            }
            toml_edit::Value::Array(arr)
        }
        J::Object(m) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, x) in m {
                t.insert(k, json_to_toml(x)?);
            }
            toml_edit::Value::InlineTable(t)
        }
        J::Null => return Err("null cannot be inside a list or a table".into()),
    })
}

/// `text` with `key` set to `value` (removed when `value` is null), keeping comments and layout.
/// Only checks the key; the caller resolves the result to validate the value.
///
/// ```
/// use alice_core::settings::edit_toml;
/// let text = "# mine\n[judge]\nprovider = \"heuristic\" # for now\n";
/// let out = edit_toml(text, "judge.provider", &serde_json::json!("jev")).unwrap();
/// assert_eq!(out, "# mine\n[judge]\nprovider = \"jev\" # for now\n");
/// ```
pub fn edit_toml(text: &str, key: &str, value: &serde_json::Value) -> Result<String, SettingError> {
    if READ_ONLY_KEYS.contains(&key) {
        return Err(err(
            key,
            "is read-only; set the ALICE_HOME environment variable to move the data directory",
        ));
    }
    if !DEFAULT_MAP.contains_key(key) {
        return Err(err(key, unknown_key_message(key)));
    }
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        err(
            "config.toml",
            format!(
                "cannot edit it: {}; fix the file first",
                toml_error(text, e.message(), e.span())
            ),
        )
    })?;
    let parts: Vec<&str> = key.split('.').collect();
    let (last, tables) = parts.split_last().expect("non-empty key");
    let not_a_table = || err(key, "config.toml has a value where a table is expected");
    let mut item = doc.as_item_mut();
    for t in tables {
        let table = item.as_table_like_mut().ok_or_else(not_a_table)?;
        if table.get(t).is_none() {
            // Implicit: no empty `[search]` header above a new `[search.weights]`.
            let mut new = toml_edit::Table::new();
            new.set_implicit(true);
            table.insert(t, toml_edit::Item::Table(new));
        }
        item = table.get_mut(t).expect("inserted above");
    }
    let table = item.as_table_like_mut().ok_or_else(not_a_table)?;
    if value.is_null() {
        table.remove(last);
    } else {
        let mut v = json_to_toml(value).map_err(|m| err(key, m))?;
        match table.get_mut(last) {
            // Replace the value in place: the key's comments and the value's trailing comment
            // stay (inserting anew would drop them).
            Some(toml_edit::Item::Value(old)) => {
                *v.decor_mut() = old.decor().clone();
                *old = v;
            }
            _ => {
                table.insert(last, toml_edit::Item::Value(v));
            }
        }
    }
    Ok(doc.to_string())
}

// ---------------------------------------------------------------- project scope

/// `<repo>/.alice.toml`. Only settings that make Alice more careful are accepted.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectSettings {
    /// Privacy for this project's memories, on top of the global settings.
    pub privacy: ProjectPrivacy,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectPrivacy {
    /// Refuse writes that contain secrets instead of redacting them.
    pub block_secrets: bool,
    /// Extra patterns redacted in this project.
    pub custom_patterns: Vec<CustomPattern>,
    /// Never send this project's memories to a cloud judge or embedding provider.
    pub cloud_denied: bool,
}

impl ProjectSettings {
    /// The project's privacy on top of the global one: never weaker.
    pub fn apply_to(&self, global: &PrivacySettings) -> PrivacySettings {
        let mut p = global.clone();
        let touched = self.privacy.block_secrets || !self.privacy.custom_patterns.is_empty();
        p.redact = p.redact || touched;
        if self.privacy.block_secrets {
            p.redact_mode = RedactMode::Block;
        }
        p.custom_patterns
            .extend(self.privacy.custom_patterns.iter().cloned());
        p
    }
}

/// A missing file is no project settings; an invalid one is an error naming the file.
pub fn load_project(root: &Path) -> Result<ProjectSettings, SettingError> {
    let path = root.join(".alice.toml");
    let name = path.display().to_string();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectSettings::default());
        }
        Err(e) => return Err(err(name, format!("cannot read it: {e}"))),
    };
    let p: ProjectSettings = toml::from_str(&text).map_err(|e| {
        err(
            &name,
            format!(
                "{}; a project file accepts only [privacy] block_secrets, custom_patterns and cloud_denied",
                toml_error(&text, e.message(), e.span())
            ),
        )
    })?;
    let check = Settings {
        privacy: p.apply_to(&PrivacySettings::default()),
        ..Default::default()
    };
    let problems: Vec<String> = check.validate().into_iter().map(|e| e.message).collect();
    if problems.is_empty() {
        Ok(p)
    } else {
        Err(err(name, problems.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn error_keys(file: &str, vars: &[(&str, &str)]) -> Vec<String> {
        match resolve(Some(file), &env(vars)) {
            Ok(_) => Vec::new(),
            Err(errs) => errs.into_iter().map(|e| e.key).collect(),
        }
    }

    #[test]
    fn defaults_are_valid_and_match_the_spec() {
        let s = Settings::default();
        assert!(s.validate().is_empty(), "{:?}", s.validate());
        assert_eq!(s.judge.provider, JudgeProvider::Heuristic);
        assert_eq!(
            (s.judge.auto_apply_threshold, s.judge.review_threshold),
            (0.85, 0.5)
        );
        assert_eq!(s.judge.jev.model, "jev-1.13.0");
        assert_eq!(s.search.relevance_floor, 0.82);
        assert_eq!(s.storage.raw_retention, Retention::Forever);
        assert_eq!((s.backup.daily, s.backup.weekly), (7, 4));
        assert_eq!(s.secrets.backend, SecretsBackend::File);
        assert!(!s.mcp.admin_tools);
    }

    /// The settings defaults are the values the code used before settings existed.
    #[test]
    fn defaults_agree_with_the_constants_they_replace() {
        let s = Settings::default();
        assert_eq!(
            s.search.relevance_floor as f32,
            crate::search::DEFAULT_RELEVANCE_FLOOR
        );
        assert_eq!(
            s.search.default_max_tokens,
            crate::model::limits::DEFAULT_RECALL_TOKENS
        );
        assert_eq!(s.search.weights, crate::search::fusion::Weights::default());
        // `alice_embed::MODEL_ID`; alice-core does not depend on alice-embed.
        assert_eq!(s.embedding.model, "multilingual-e5-small-q8");
        assert_eq!(s.embedding.idle_unload.0.as_secs(), 600);
        // Shown as written, not as an f32 widened to f64 (0.8199999928474426).
        assert_eq!(
            get_key(&s, "search.relevance_floor"),
            Some(serde_json::json!(0.82))
        );
        assert_eq!(
            get_key(&s, "judge.cluster_similarity"),
            Some(serde_json::json!(0.92))
        );
    }

    #[test]
    fn layers_apply_in_order_and_report_their_source() {
        let file =
            "[judge]\nprovider = \"jev\"\nreview_threshold = 0.6\n[search.weights]\nraw = 0.5\n";
        let r = resolve(
            Some(file),
            &env(&[
                ("ALICE_JUDGE_REVIEW_THRESHOLD", "0.7"),
                ("ALICE_LOG", "debug"),
            ]),
        )
        .unwrap();
        assert_eq!(r.settings.judge.provider, JudgeProvider::Jev);
        assert_eq!(
            r.settings.judge.review_threshold, 0.7,
            "env wins over the file"
        );
        assert_eq!(r.settings.search.weights.raw, 0.5);
        assert_eq!(
            r.settings.search.weights.knowledge, 1.0,
            "partial tables keep defaults"
        );
        assert_eq!(r.sources["judge.provider"], Source::File);
        assert_eq!(
            r.sources["judge.review_threshold"],
            Source::Env {
                var: "ALICE_JUDGE_REVIEW_THRESHOLD".into()
            }
        );
        assert_eq!(r.sources["judge.auto_apply_threshold"], Source::Default);
    }

    /// (config.toml, environment, a key that must be in the errors)
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);

    #[test]
    fn invalid_input_is_reported_with_its_key() {
        let cases: &[Case] = &[
            ("[judge]\nprovder = \"jev\"\n", &[], "judge.provder"),
            (
                "[judge]\nauto_apply_threshold = \"high\"\n",
                &[],
                "judge.auto_apply_threshold",
            ),
            (
                "[judge]\nauto_apply_threshold = 0.4\n",
                &[],
                "judge.review_threshold",
            ),
            (
                "[privacy]\ncustom_patterns = [{name = \"x\", regex = \"(\"}]\n",
                &[],
                "privacy.custom_patterns",
            ),
            ("[judge\n", &[], "config.toml"),
            (
                "",
                &[("ALICE_SEARCH_RELEVANCE_FLOOR", "very")],
                "search.relevance_floor",
            ),
            (
                "[storage]\nraw_retention = \"soon\"\n",
                &[],
                "storage.raw_retention",
            ),
            (
                "[embedding]\nprovider = \"local\"\nmodel = \"other\"\n",
                &[],
                "embedding.model",
            ),
        ];
        for (file, vars, key) in cases {
            let errs = resolve(Some(file), &env(vars)).unwrap_err();
            assert!(
                errs.iter().any(|e| e.key == *key),
                "{file:?} {vars:?}: {errs:?}"
            );
            assert!(errs.iter().all(|e| !e.message.is_empty()));
        }
    }

    #[test]
    fn unknown_alice_variables_are_ignored_with_a_warning() {
        let embedding_key = concat!("sk-", "test-1234");
        let typesafe_key = concat!("ts-", "test");
        let vars = env(&[
            ("ALICE_JUDGE_PROVDER", "jev"),             // a typo: warned, ignored
            ("ALICE_SOMETHING_ELSE", "1"),              // no such section: warned, ignored
            ("ALICE_JUDGE_REVIEW_THRESHOLD", "0.7"),    // a setting: applied
            ("ALICE_EMBEDDING_API_KEY", embedding_key), // a secret: recognised, not a setting
            ("ALICE_HOME", "/tmp/x"),
            ("ALICE_LOG", "debug"),
            ("ALICE_EMBEDDER", "fake"),
            ("TYPESAFE_API_KEY", typesafe_key), // not ALICE_*: never looked at
        ]);
        let r =
            resolve(None, &vars).expect("unknown variables never make the configuration invalid");
        assert_eq!(
            r.settings.judge.review_threshold, 0.7,
            "the valid variable still applies"
        );
        assert_eq!(
            r.settings.judge.provider,
            JudgeProvider::Heuristic,
            "the typo changes nothing"
        );
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("ALICE_JUDGE_PROVDER") && w.contains("alice config list")),
            "{:?}",
            r.warnings
        );
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("ALICE_SOMETHING_ELSE")),
            "{:?}",
            r.warnings
        );
        assert!(
            r.warnings
                .iter()
                .all(|w| !w.contains(embedding_key) && !w.contains(typesafe_key)),
            "values are never echoed"
        );
        // With a file error as well, the environment's valid part is still what the daemon uses.
        let errs = resolve(Some("[judge\n"), &vars).unwrap_err();
        assert!(
            errs.iter().all(|e| !e.key.starts_with("ALICE_")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_warning_names_the_setting_a_typo_was_probably_meant_to_be() {
        let r = resolve(None, &env(&[("ALICE_JUDGE_PROVDER", "jev")])).unwrap();
        assert!(
            r.warnings[0].contains("did you mean ALICE_JUDGE_PROVIDER"),
            "{:?}",
            r.warnings
        );
        let r = resolve(
            None,
            &env(&[("ALICE_JUDGE_JEV_API_KEY", concat!("ts_", "live_zzzz9"))]),
        )
        .unwrap();
        assert!(
            r.warnings[0].contains("alice config set-secret") && !r.warnings[0].contains("zzzz9"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn a_variable_of_the_wrong_kind_is_an_error_that_does_not_echo_its_value() {
        let errs = resolve(None, &env(&[("ALICE_JUDGE_TIMEOUT_MS", "abc-value")])).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].key, "judge.timeout_ms");
        assert!(
            errs[0].message.contains("ALICE_JUDGE_TIMEOUT_MS"),
            "{errs:?}"
        );
        assert!(!errs[0].message.contains("abc-value"), "{errs:?}");
    }

    #[test]
    fn config_errors_never_echo_the_file() {
        let secret = concat!("sk-ant-", "api03-AAAABBBBCCCCDDDD");
        // A broken line holding a secret: the TOML error must not quote it.
        let file = format!("[judge]\nnote = \"{secret}\n");
        let errs = resolve(Some(&file), &[]).unwrap_err();
        assert_eq!(errs[0].key, "config.toml");
        assert!(errs[0].message.contains("line 2"), "{errs:?}");
        assert!(errs.iter().all(|e| !e.message.contains(secret)), "{errs:?}");
        // A key in the wrong place: named, with where keys go, and the value is not shown.
        let file = format!("[judge.jev]\napi_key = \"{secret}\"\n");
        let errs = resolve(Some(&file), &[]).unwrap_err();
        assert_eq!(errs[0].key, "judge.jev.api_key");
        assert!(errs[0].message.contains("set-secret"), "{errs:?}");
        assert!(errs.iter().all(|e| !e.message.contains(secret)), "{errs:?}");
    }

    #[test]
    fn edit_toml_keeps_comments_creates_tables_and_removes_keys() {
        let text = "# my settings\n[judge]\n# careful\nprovider = \"heuristic\"\n";
        let t = edit_toml(text, "judge.provider", &serde_json::json!("jev")).unwrap();
        assert!(
            t.contains("# careful") && t.contains("provider = \"jev\""),
            "{t}"
        );
        let t = edit_toml(&t, "search.weights.raw", &serde_json::json!(0.4)).unwrap();
        assert_eq!(
            resolve(Some(&t), &[]).unwrap().settings.search.weights.raw,
            0.4
        );
        let t = edit_toml(
            &t,
            "privacy.cloud_denied_projects",
            &serde_json::json!(["secret-proj"]),
        )
        .unwrap();
        assert_eq!(
            resolve(Some(&t), &[])
                .unwrap()
                .settings
                .privacy
                .cloud_denied_projects,
            vec!["secret-proj"]
        );
        let t = edit_toml(&t, "judge.provider", &serde_json::Value::Null).unwrap();
        assert_eq!(
            resolve(Some(&t), &[]).unwrap().settings.judge.provider,
            JudgeProvider::Heuristic
        );
        assert!(edit_toml(&t, "judge.nope", &serde_json::json!(1)).is_err());
        assert!(
            edit_toml(&t, "storage.data_dir", &serde_json::json!("/x")).is_err(),
            "read-only"
        );
    }

    #[test]
    fn edit_toml_keeps_trailing_comments_and_writes_into_inline_tables() {
        let text = "[judge]\nreview_threshold = 0.5 # agreed with the team\n";
        let t = edit_toml(text, "judge.review_threshold", &serde_json::json!(0.6)).unwrap();
        assert!(
            t.contains("review_threshold = 0.6 # agreed with the team"),
            "{t}"
        );
        let t = edit_toml(
            "search = { weights = { raw = 0.5 } }\n",
            "search.weights.knowledge",
            &serde_json::json!(0.9),
        )
        .unwrap();
        let s = resolve(Some(&t), &[]).unwrap().settings;
        assert_eq!(
            (s.search.weights.raw, s.search.weights.knowledge),
            (0.5, 0.9),
            "{t}"
        );
        // A new nested table does not leave an empty `[search]` header behind.
        let t = edit_toml("", "search.half_life.status", &serde_json::json!(3)).unwrap();
        assert_eq!(t.trim(), "[search.half_life]\nstatus = 3", "{t}");
        assert_eq!(
            edit_toml("[judge\n", "judge.provider", &serde_json::json!("jev"))
                .unwrap_err()
                .key,
            "config.toml"
        );
    }

    #[test]
    fn human_units_round_trip() {
        for (s, d) in [
            ("90d", 90 * 86_400),
            ("12h", 43_200),
            ("15m", 900),
            ("30s", 30),
            ("0", 0),
        ] {
            let h = HumanDuration::parse(s).unwrap();
            assert_eq!(h.0.as_secs(), d);
            assert_eq!(HumanDuration::parse(&h.to_string()).unwrap(), h);
        }
        assert!(HumanDuration::parse("5 parsecs").is_err());
        assert_eq!(ByteSize::parse("10GB").unwrap().0, 10_000_000_000);
        assert_eq!(ByteSize::parse("1.5 MB").unwrap().0, 1_500_000);
        assert_eq!(Retention::parse("forever").unwrap(), Retention::Forever);
        assert_eq!(
            Retention::parse("90d").unwrap(),
            Retention::Age(std::time::Duration::from_secs(90 * 86_400))
        );
        assert_eq!(
            Retention::parse("10GB").unwrap(),
            Retention::Size(10_000_000_000)
        );
        assert!(
            Retention::parse("0").is_err(),
            "0 would delete every transcript"
        );
    }

    #[test]
    fn odd_units_are_errors_and_never_panic() {
        for bad in [
            "5é",
            "é",
            "",
            "d",
            "-5d",
            "1.5h",
            "999999999999999d",
            "18446744073709551616s",
        ] {
            assert!(HumanDuration::parse(bad).is_err(), "{bad:?}");
        }
        for bad in [
            "-5GB",
            "nan",
            "inf",
            "1e3GB",
            "99999999999TB",
            "5 XB",
            "GB",
            "",
        ] {
            assert!(ByteSize::parse(bad).is_err(), "{bad:?}");
        }
        for bad in ["0", "0d", "0GB", "never", ""] {
            assert!(Retention::parse(bad).is_err(), "{bad:?}");
        }
        // A plausible typo (seconds for days, bytes for gigabytes) would delete nearly every
        // transcript: retention below a day or a megabyte is refused.
        assert!(
            error_keys("[storage]\nraw_retention = \"90s\"\n", &[])
                .contains(&"storage.raw_retention".to_string())
        );
        assert!(
            error_keys("[storage]\nraw_retention = \"10\"\n", &[])
                .contains(&"storage.raw_retention".to_string())
        );
        assert!(error_keys("[storage]\nraw_retention = \"2d\"\n", &[]).is_empty());
    }

    #[test]
    fn unit_settings_accept_a_bare_zero_and_a_number_from_the_environment() {
        let r = resolve(Some("[embedding]\nidle_unload = 0\n"), &[]).unwrap();
        assert!(r.settings.embedding.idle_unload.0.is_zero());
        let r = resolve(None, &env(&[("ALICE_EMBEDDING_IDLE_UNLOAD", "30s")])).unwrap();
        assert_eq!(r.settings.embedding.idle_unload.0.as_secs(), 30);
        let r = resolve(None, &env(&[("ALICE_STORAGE_RAW_RETENTION", "90d")])).unwrap();
        assert_eq!(
            r.settings.storage.raw_retention,
            Retention::Age(std::time::Duration::from_secs(90 * 86_400))
        );
        let r = resolve(None, &env(&[("ALICE_JUDGE_JEV_MODEL", "1.13")])).unwrap();
        assert_eq!(
            r.settings.judge.jev.model, "1.13",
            "a string setting keeps the text as written"
        );
    }

    #[test]
    fn urls_that_carry_credentials_or_send_a_key_in_the_clear_are_refused() {
        let bad: &[(&str, &str)] = &[
            (
                "[judge.jev]\nendpoint = \"http://127.0.0.1.evil.example/v1\"\n",
                "judge.jev.endpoint",
            ),
            (
                "[judge.jev]\nendpoint = \"http://api.typesafe.ai/v1/systemone\"\n",
                "judge.jev.endpoint",
            ),
            (
                "[judge.jev]\nendpoint = \"https://user:pw@api.typesafe.ai/v1\"\n",
                "judge.jev.endpoint",
            ),
            (
                "[judge.jev]\nendpoint = \"https://\"\n",
                "judge.jev.endpoint",
            ),
            (
                "[embedding]\nprovider = \"openai\"\nmodel = \"m\"\nendpoint = \"http://10.0.0.5/v1\"\n",
                "embedding.endpoint",
            ),
            (
                "[embedding]\nprovider = \"ollama\"\nmodel = \"m\"\nendpoint = \"ftp://x\"\n",
                "embedding.endpoint",
            ),
            (
                "[embedding]\nprovider = \"voyage\"\nmodel = \"m\"\nendpoint = \"https://key@api.voyageai.com\"\n",
                "embedding.endpoint",
            ),
            (
                "[sync]\nremote = \"https://me:tok@github.com/me/mem.git\"\n",
                "sync.remote",
            ),
        ];
        for (file, key) in bad {
            assert!(
                error_keys(file, &[]).contains(&key.to_string()),
                "{file}: {:?}",
                error_keys(file, &[])
            );
            let errs = resolve(Some(file), &[]).unwrap_err();
            assert!(
                errs.iter()
                    .all(|e| !e.message.contains("pw@") && !e.message.contains("tok@")),
                "{errs:?}"
            );
        }
        let good = [
            "[judge.jev]\nendpoint = \"http://127.0.0.1:8080/v1\"\n",
            "[judge.jev]\nendpoint = \"http://localhost:9/v1\"\n",
            "[embedding]\nprovider = \"ollama\"\nmodel = \"m\"\nendpoint = \"http://10.0.0.5:11434\"\n",
            "[embedding]\nprovider = \"openai\"\nmodel = \"m\"\nendpoint = \"http://localhost:8000/v1\"\n",
            "[sync]\nremote = \"git@github.com:me/mem.git\"\n",
            "[sync]\nremote = \"ssh://git@github.com/me/mem.git\"\n",
        ];
        for file in good {
            assert!(
                error_keys(file, &[]).is_empty(),
                "{file}: {:?}",
                error_keys(file, &[])
            );
        }
    }

    #[test]
    fn sync_auto_is_off_idle_or_an_interval() {
        for ok in ["off", "idle", "15m", "2h"] {
            assert!(
                error_keys(&format!("[sync]\nauto = \"{ok}\"\n"), &[]).is_empty(),
                "{ok}"
            );
        }
        for bad in ["sometimes", "0", "10s"] {
            assert_eq!(
                error_keys(&format!("[sync]\nauto = \"{bad}\"\n"), &[]),
                vec!["sync.auto"],
                "{bad}"
            );
        }
    }

    #[test]
    fn odd_documents_are_errors_and_never_panic() {
        let docs = [
            "when = 1979-05-27T07:32:00Z\n",
            "[search]\nweights = 5\n",
            "[[judge]]\nprovider = \"jev\"\n",
            "search.weights = { raw = { x = 1 } }\n",
            "[embedding]\nendpoint = {}\n",
            "[embedding]\nendpoint = 5\n",
            "[server]\nport = 70000\n",
            "[maintenance]\nidle_secs = -1\n",
            "[judge]\nreview_threshold = nan\n",
            "[judge]\nreview_threshold = inf\n",
            "[embedding]\nbatch_size = 3.5\n",
            "[privacy]\ncustom_patterns = [{name = \"x\"}]\n",
            "[privacy]\ncustom_patterns = [1, 2]\n",
            "\"judge.provider\" = \"jev\"\n",
            "[judge]\nprovider = \"JEV\"\n",
        ];
        for doc in docs {
            if let Err(errs) = resolve(Some(doc), &[]) {
                assert!(!errs.is_empty(), "{doc}");
                assert!(
                    errs.iter()
                        .all(|e| !e.key.is_empty() && !e.message.is_empty()),
                    "{doc}: {errs:?}"
                );
            }
        }
        assert!(resolve(Some("[server]\nport = 70000\n"), &[]).is_err());
        assert!(resolve(Some("[judge]\nreview_threshold = nan\n"), &[]).is_err());
    }

    #[test]
    fn a_project_file_can_only_tighten_privacy() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".alice.toml"),
            "[privacy]\nblock_secrets = true\ncloud_denied = true\ncustom_patterns = [{name = \"ticket\", regex = \"TCK-[0-9]+\"}]\n").unwrap();
        let p = load_project(d.path()).unwrap();
        let global = PrivacySettings {
            redact: false,
            ..Default::default()
        };
        let eff = p.apply_to(&global);
        assert!(eff.redact, "a project file turns redaction on");
        assert_eq!(eff.redact_mode, RedactMode::Block);
        assert_eq!(eff.custom_patterns.len(), 1);
        assert!(p.privacy.cloud_denied);
        std::fs::write(d.path().join(".alice.toml"), "[privacy]\nredact = false\n").unwrap();
        assert!(
            load_project(d.path()).is_err(),
            "a project file cannot turn redaction off"
        );
        assert_eq!(
            load_project(&d.path().join("missing")).unwrap(),
            ProjectSettings::default()
        );
        std::fs::write(
            d.path().join(".alice.toml"),
            "[privacy]\ncustom_patterns = [{name = \"Bad Name\", regex = \"x\"}]\n",
        )
        .unwrap();
        let e = load_project(d.path()).unwrap_err();
        assert!(
            e.key.ends_with(".alice.toml") && e.message.contains("Bad Name"),
            "{e:?}"
        );
    }

    #[test]
    fn every_key_has_an_env_var_and_restart_keys_are_listed() {
        let keys = key_infos();
        assert!(
            keys.iter().any(|k| k.key == "judge.jev.rate_per_sec"
                && k.env_var == "ALICE_JUDGE_JEV_RATE_PER_SEC")
        );
        assert!(keys.iter().any(|k| k.key == "server.port" && k.restart));
        assert!(
            keys.iter()
                .any(|k| k.key == "storage.data_dir" && k.read_only)
        );
        assert!(keys.len() >= 50, "{}", keys.len());
        for k in RESTART_KEYS.iter().chain(READ_ONLY_KEYS) {
            assert!(keys.iter().any(|i| i.key == *k), "{k} is not a key");
        }
        // No setting's variable collides with a variable read for something else.
        for k in &keys {
            assert!(
                !NON_SETTING_ENV.contains(&k.env_var.as_str()),
                "{}",
                k.env_var
            );
        }
    }

    /// The web UI and `alice config` render their help from the JSON Schema: every key has a
    /// description there.
    #[test]
    fn every_key_is_described_in_the_schema() {
        let schema = serde_json::to_value(schemars::schema_for!(Settings)).unwrap();
        let deref = |v: &serde_json::Value| -> serde_json::Value {
            match v.get("$ref").and_then(|r| r.as_str()) {
                Some(r) => {
                    let name = r.trim_start_matches("#/$defs/");
                    schema["$defs"][name].clone()
                }
                None => v.clone(),
            }
        };
        let mut missing = Vec::new();
        for k in key_infos() {
            let mut node = schema.clone();
            let mut prop = serde_json::Value::Null;
            for part in k.key.split('.') {
                prop = deref(&node)["properties"][part].clone();
                node = prop.clone();
            }
            let described = prop
                .get("description")
                .and_then(|d| d.as_str())
                .is_some_and(|d| !d.trim().is_empty());
            if !described {
                missing.push(k.key);
            }
        }
        assert!(
            missing.is_empty(),
            "keys without a description: {missing:?}"
        );
    }
}
