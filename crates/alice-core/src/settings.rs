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
/// Keys that take effect only after `alice daemon stop` and a new start. The embedding
/// provider, model and endpoint are not here: a change starts a background re-embed and the
/// daemon switches models when it is done.
pub const RESTART_KEYS: &[&str] = &[
    "server.port",
    "secrets.backend",
    "embedding.idle_unload",
    "embedding.batch_size",
];

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
    // Test hooks of `alice service` and `alice restore`: a directory that records service
    // commands instead of running them, and a word that makes a recorded command fail.
    "ALICE_SERVICE_ROOT",
    "ALICE_SERVICE_FAIL",
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

/// The longest age a retention may name (100 years): beyond it `forever` says the same.
const MAX_RETENTION_AGE: Duration = Duration::from_secs(100 * 365 * 86_400);

impl Retention {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("forever") {
            return Ok(Self::Forever);
        }
        if let Ok(d) = HumanDuration::parse(t)
            && !d.0.is_zero()
        {
            if d.0 > MAX_RETENTION_AGE {
                return Err(format!(
                    "\"{s}\" is longer than 100 years; use forever to keep everything"
                ));
            }
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
    /// a total size such as `10GB` (at least `1MB`). An age counts from when Alice stored a
    /// chunk, not from the date the chunk claims; a size counts the chunks' text bytes. The
    /// database file does not shrink when chunks go.
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
    /// Who computes embeddings: `local`, `openai`, `ollama` or `voyage`. Changing the provider
    /// or the model starts a background re-embed of everything; searches use the current model
    /// until the new index is complete, then switch at once. A provider that is not on this
    /// machine only gets text of projects the cloud policy allows.
    pub provider: EmbeddingProvider,
    /// `multilingual-e5-small-q8` for `local`; the provider's model name otherwise.
    pub model: String,
    /// Base URL for HTTP providers (defaults per provider). https:// is required when an API
    /// key is sent, except to 127.0.0.1 or localhost. A server on this machine (127.0.0.1,
    /// localhost) counts as local: the cloud policy does not apply to it.
    pub endpoint: Option<String>,
    /// Texts per embedding request (1 to 256).
    pub batch_size: usize,
    /// Unload the local model after this long without use, such as `10m` (`0` = never).
    #[schemars(with = "String")]
    pub idle_unload: HumanDuration,
}

impl EmbeddingSettings {
    /// Where texts go: `None` for the built-in model and for an HTTP endpoint on this machine
    /// (`127.0.0.1`, `::1`, any `127.x.x.x` or `localhost`: a local Ollama), else the provider
    /// and the host of its endpoint (the provider's default when none is set). The cloud policy
    /// applies to a remote destination only.
    pub fn remote_destination(&self) -> Option<(EmbeddingProvider, String)> {
        let default_host = match self.provider {
            EmbeddingProvider::Local => return None,
            EmbeddingProvider::Openai => "api.openai.com",
            EmbeddingProvider::Ollama => "127.0.0.1",
            EmbeddingProvider::Voyage => "api.voyageai.com",
        };
        let host = match self.endpoint.as_deref() {
            None => default_host.to_string(),
            // An endpoint that cannot be read is not assumed to be the provider's own, nor this
            // machine: it counts as remote (fail closed).
            Some(e) => match url_parts(e).map(|u| u.host.to_ascii_lowercase()) {
                Some(h) if !h.is_empty() => h,
                _ => return Some((self.provider, "(unreadable endpoint)".to_string())),
            },
        };
        (!host_is_local(&host)).then_some((self.provider, host))
    }

    /// Whether texts leave this machine (see [`remote_destination`](Self::remote_destination)).
    pub fn is_remote(&self) -> bool {
        self.remote_destination().is_some()
    }
}

/// Whether `host` (from a URL) is this machine: `localhost` or a loopback address.
pub fn host_is_local(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether an embedding endpoint URL (or the provider's default when `None`) names a host other
/// than this machine.
pub fn endpoint_is_remote(provider: EmbeddingProvider, endpoint: Option<&str>) -> bool {
    EmbeddingSettings {
        provider,
        endpoint: endpoint.map(String::from),
        ..EmbeddingSettings::default()
    }
    .is_remote()
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
    /// Rescue floor for memories found only by meaning (cosine, 0 to 1): below the sure floor
    /// such a match counts as relevant at or above this and within the margin of the best one.
    /// This used to be the single floor; for that behaviour set `relevance_sure` to the same
    /// value. A floor above the default sure floor, with no `relevance_sure` of its own, raises
    /// the sure floor with it (a single floor).
    pub relevance_floor: f64,
    /// Sure floor for memories found only by meaning (cosine, 0 to 1, not below the rescue
    /// floor): at or above it a match is always relevant. Unset, it is 0.825 or the rescue
    /// floor if that is higher.
    pub relevance_sure: f64,
    /// A memory found only by meaning below the sure floor is dropped when it is more than this
    /// below the best such match of the query (0 to 1; 0 turns the relative cut off). Word
    /// matches always stay. Transcript chunks use a fixed floor of 0.82.
    pub relevance_margin: f64,
    /// Fewer relevant memories than this bring in raw transcript chunks (0 to 50).
    pub raw_fallback_min_results: usize,
    /// The k of reciprocal rank fusion (1 to 200): the smaller it is, the more the first places of
    /// a candidate list count against the later ones.
    pub rrf_k: f64,
    /// How strongly the boosts (tier, project, recency, importance, use, status) weigh against
    /// relevance (0 to 1): the score is the fused rank score times the boost product to this
    /// power. 1 multiplies the boosts in whole; 0 ignores them; a smaller value lets them break
    /// ties between about equally relevant results.
    pub boost_exponent: f64,
    /// Ranking multipliers.
    pub weights: Weights,
    /// Recency half-lives in days per kind of memory (0 = no decay).
    pub half_life: HalfLives,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            default_max_tokens: crate::model::limits::DEFAULT_RECALL_TOKENS,
            // `search::DEFAULT_RELEVANCE_FLOOR` and `DEFAULT_RELEVANCE_MARGIN` as written (the
            // f32 widened would show as 0.8199999928474426); a test keeps them equal.
            relevance_floor: 0.805,
            relevance_sure: 0.825,
            relevance_margin: 0.015,
            raw_fallback_min_results: 3,
            rrf_k: crate::search::fusion::DEFAULT_RRF_K,
            boost_exponent: crate::search::fusion::DEFAULT_BOOST_EXPONENT,
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
    /// Saved, but nothing reads it yet. No key is at the moment; the field stays for clients.
    #[serde(default)]
    pub not_used_yet: bool,
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
            not_used_yet: false,
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

/// A value typed on a command line or in an environment variable: TOML syntax if it parses
/// (`0.7`, `true`, `["a"]`, `"quoted"`), else a string (`90d`, `heuristic`). The one rule for
/// `alice config set` and for `ALICE_*` variables.
///
/// ```
/// use alice_core::settings::parse_cli_value;
/// assert_eq!(parse_cli_value("0.6"), serde_json::json!(0.6));
/// assert_eq!(parse_cli_value("true"), serde_json::json!(true));
/// assert_eq!(parse_cli_value("90d"), serde_json::json!("90d"));
/// assert_eq!(parse_cli_value("[\"a\", \"b\"]"), serde_json::json!(["a", "b"]));
/// ```
pub fn parse_cli_value(raw: &str) -> serde_json::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|t| t.get("v").cloned())
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_else(|| serde_json::Value::String(raw.to_string()))
}

/// An environment value: the same rule as [`parse_cli_value`].
fn env_value(raw: &str) -> serde_json::Value {
    parse_cli_value(raw)
}

/// The 1-based line of byte `offset` in `text` (past the end: the last line). Error messages
/// name a line and never quote it, because the line may hold a secret.
pub fn toml_line(text: &str, offset: usize) -> usize {
    let at = offset.min(text.len());
    text.as_bytes()[..at]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// "line N: <what is wrong>", without quoting the line (it may hold a secret).
fn toml_error(text: &str, message: &str, span: Option<std::ops::Range<usize>>) -> String {
    let message = message.trim().trim_end_matches('.');
    match span {
        Some(s) => format!("line {}: {message}", toml_line(text, s.start)),
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
/// This is the strict, all-or-nothing rule: one bad leaf fails the whole call. Code that must
/// always end up with settings goes through [`resolve_salvaging`], and the daemon and the CLI
/// through `alice_daemon::config` (`SettingsHandle`, `load_effective`), which also check the
/// variables one by one first so that one bad variable cannot take the others down with it.
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
    resolve_leaves(parse_file(file), env)
}

type FileLeaves = Result<Vec<(String, serde_json::Value)>, SettingError>;

/// The leaves of `config.toml`'s text; no leaves for a missing or blank file.
fn parse_file(file: Option<&str>) -> FileLeaves {
    let Some(text) = file.filter(|t| !t.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    match toml::from_str::<toml::Table>(text) {
        Err(e) => Err(err(
            "config.toml",
            format!(
                "{}; fix the file and save it again",
                toml_error(text, e.message(), e.span())
            ),
        )),
        Ok(table) => Ok(leaves(&serde_json::to_value(table).unwrap_or_default())),
    }
}

fn resolve_leaves(
    file: FileLeaves,
    env: &[(String, String)],
) -> Result<Resolved, Vec<SettingError>> {
    let mut merged = DEFAULTS.clone();
    let mut sources: BTreeMap<String, Source> = DEFAULT_LEAVES
        .iter()
        .map(|(k, _)| (k.clone(), Source::Default))
        .collect();
    let mut errors = Vec::new();

    match file {
        Err(e) => errors.push(e),
        Ok(given) => {
            for (key, value) in given {
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
    let safe = |errs: Vec<SettingError>| -> Vec<SettingError> {
        errs.into_iter()
            .map(|e| env_safe(e, &sources, env))
            .collect()
    };
    // A sure floor nobody set follows the rescue floor up: `relevance_floor = 0.85` alone means
    // a single floor of 0.85, not an error about a key the owner never wrote.
    if sources.get("search.relevance_sure") == Some(&Source::Default)
        && let Some(floor) = merged["search"]["relevance_floor"].as_f64()
        && floor > SearchSettings::default().relevance_sure
    {
        set_path(
            &mut merged,
            "search.relevance_sure",
            serde_json::json!(floor),
        );
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
                safe(errs)
            });
        }
    };
    let invalid = settings.validate();
    if !invalid.is_empty() {
        return Err(safe(invalid));
    }
    Ok(Resolved {
        settings,
        sources,
        warnings,
    })
}

/// An error about a key that an environment variable set, without the variable's value: it may
/// be anything (a pasted API key), and an error reaches logs, the API and the terminal. Text
/// settings get a fixed message; numbers and booleans are not secrets and keep theirs.
fn env_safe(
    mut e: SettingError,
    sources: &BTreeMap<String, Source>,
    env: &[(String, String)],
) -> SettingError {
    let Some(Source::Env { var }) = sources.get(&e.key) else {
        return e;
    };
    if DEFAULT_MAP
        .get(&e.key)
        .is_some_and(|d| d.is_boolean() || d.is_number())
    {
        return e;
    }
    let raw = env
        .iter()
        .find(|(v, _)| v == var)
        .map_or("", |(_, r)| r.trim());
    let unquoted = raw.trim_matches(['"', '\'']);
    let echoes = |m: &str| {
        (!raw.is_empty() && m.contains(raw)) || (!unquoted.is_empty() && m.contains(unquoted))
    };
    e.message = match e.key.as_str() {
        "privacy.custom_patterns" | "privacy.allowlist" => format!(
            "{var} holds an invalid pattern (a name or a regular expression); the value is not shown"
        ),
        "embedding.idle_unload" => {
            format!("{var} must be a duration such as 90d, 12h, 15m or 30s")
        }
        "storage.raw_retention" => format!(
            "{var} must be forever, an age of at least 1d such as 90d, or a size of at least 1MB such as 10GB"
        ),
        _ => match e.message.split_once(", expected ") {
            // serde's "unknown variant `x`, expected `a` or `b`": keep what is accepted.
            Some((_, accepted)) => format!("{var} must be {}", accepted.replace('`', "")),
            None if echoes(&e.message) => format!(
                "{var} is not valid for this setting; see `alice config list` for what it accepts"
            ),
            None => return e,
        },
    };
    e
}

/// Like [`resolve`], but a bad leaf of `config.toml` costs only itself: the leaves the errors
/// name are dropped (a bad entry of a privacy list costs only that entry) and the rest applies,
/// so one typo cannot switch off `privacy.cloud_denied_projects` or `privacy.custom_patterns`.
/// Returns the settings and the errors for what was left out. When the file does not parse,
/// nothing can be salvaged and the environment over the defaults applies. When an error is not
/// about a leaf of the file (a conflict with a variable), the valid `privacy.*` leaves still
/// apply.
///
/// ```
/// use alice_core::settings::resolve_salvaging;
/// let file = "[privacy]\ncloud_denied_projects = [\"client\"]\n[judge]\nreview_treshold = 0.7\n";
/// let (r, left_out) = resolve_salvaging(Some(file), &[]);
/// assert_eq!(r.settings.privacy.cloud_denied_projects, vec!["client"]);
/// assert_eq!(left_out[0].key, "judge.review_treshold");
/// ```
pub fn resolve_salvaging(
    file: Option<&str>,
    env: &[(String, String)],
) -> (Resolved, Vec<SettingError>) {
    let env_only = || {
        resolve_leaves(Ok(Vec::new()), env)
            .unwrap_or_else(|_| resolve_leaves(Ok(Vec::new()), &[]).expect("defaults are valid"))
    };
    let mut given = match parse_file(file) {
        Ok(l) => l,
        Err(e) => return (env_only(), vec![e]),
    };
    let mut left_out = Vec::new();
    loop {
        let errs = match resolve_leaves(Ok(given.clone()), env) {
            Ok(r) => return (r, left_out),
            Err(errs) => errs,
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut progressed = false;
        for e in &errs {
            if !seen.insert(e.key.clone()) {
                continue;
            }
            let Some(at) = given.iter().position(|(k, _)| *k == e.key) else {
                continue;
            };
            progressed = true;
            left_out.push(e.clone());
            match prune_invalid_entries(&e.key, &given[at].1) {
                Some(kept) => given[at].1 = kept,
                None => {
                    given.remove(at);
                }
            }
        }
        if !progressed {
            // Not about a leaf of the file: keep what privacy needs and is valid.
            left_out.extend(errs);
            // `given` holds the leaves as pruned so far: a list keeps its valid entries.
            let privacy: Vec<_> = given
                .into_iter()
                .filter(|(k, _)| k.starts_with("privacy."))
                .filter(|l| resolve_leaves(Ok(vec![l.clone()]), env).is_ok())
                .collect();
            let r = resolve_leaves(Ok(privacy), env).unwrap_or_else(|_| env_only());
            return (r, left_out);
        }
    }
}

/// A privacy list without its invalid entries; `None` when no entry is invalid (or the key is
/// not such a list).
fn prune_invalid_entries(key: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
    let items = value.as_array()?;
    let ok = |v: &serde_json::Value| match key {
        "privacy.custom_patterns" => serde_json::from_value::<CustomPattern>(v.clone())
            .is_ok_and(|c| pattern_name_ok(&c.name) && regex::Regex::new(&c.regex).is_ok()),
        "privacy.allowlist" => v.as_str().is_some_and(|a| regex::Regex::new(a).is_ok()),
        "privacy.cloud_denied_projects" => v.as_str().is_some_and(|s| !s.trim().is_empty()),
        _ => true,
    };
    let kept: Vec<_> = items.iter().filter(|v| ok(v)).cloned().collect();
    (kept.len() < items.len()).then_some(serde_json::Value::Array(kept))
}

fn pattern_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
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
        unit(&mut e, "search.relevance_sure", s.relevance_sure);
        unit(&mut e, "search.relevance_margin", s.relevance_margin);
        if s.relevance_sure < s.relevance_floor {
            e.push(err(
                "search.relevance_sure",
                format!(
                    "{} is below search.relevance_floor ({}); the sure floor must not be lower",
                    s.relevance_sure, s.relevance_floor
                ),
            ));
        }
        if !(100..=50_000).contains(&s.default_max_tokens) {
            e.push(err(
                "search.default_max_tokens",
                "must be between 100 and 50000",
            ));
        }
        if !(1.0..=200.0).contains(&s.rrf_k) {
            e.push(err(
                "search.rrf_k",
                format!("{} is outside 1 and 200", s.rrf_k),
            ));
        }
        if !(0.0..=1.0).contains(&s.boost_exponent) {
            e.push(err(
                "search.boost_exponent",
                format!("{} is outside 0 and 1", s.boost_exponent),
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
            if !pattern_name_ok(&c.name) {
                e.push(err(
                    "privacy.custom_patterns",
                    format!(
                        "pattern {i}: name \"{}\" must be lowercase letters, digits and _",
                        c.name
                    ),
                ));
            }
            match regex::Regex::new(&c.regex) {
                Err(x) => e.push(err(
                    "privacy.custom_patterns",
                    format!("pattern {}: {x}", c.name),
                )),
                // Such a pattern would "match" between every two characters.
                Ok(re) if re.is_match("") => e.push(err(
                    "privacy.custom_patterns",
                    format!(
                        "pattern {}: the regular expression matches empty text; make it match something (for example \\d+ instead of \\d*)",
                        c.name
                    ),
                )),
                Ok(_) => {}
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

// ---------------------------------------------------------------- loosening

/// How many polls in a row must see the same loosening before it applies.
pub const LOOSEN_POLLS: u32 = 3;

/// The protective keys that `new` makes weaker than `old` (empty when nothing got weaker):
/// redaction off, `block` mode back to `redact`, an entry gone from the cloud denylist or the
/// custom patterns, a longer allowlist, a higher daily judge budget, or a switch to a cloud
/// judge, to an embedding provider or endpoint that is not on this machine (or to another remote
/// host than before), or the admin MCP tools switched on.
///
/// A half-written `config.toml` (an editor truncating then writing) reads as a file that
/// loosens everything; see [`LooseningGate`].
pub fn loosened_protections(old: &Settings, new: &Settings) -> Vec<String> {
    let (o, n) = (&old.privacy, &new.privacy);
    let checks = [
        ("privacy.redact", o.redact && !n.redact),
        (
            "privacy.redact_mode",
            o.redact_mode == RedactMode::Block && n.redact_mode != RedactMode::Block,
        ),
        (
            "privacy.cloud_denied_projects",
            o.cloud_denied_projects
                .iter()
                .any(|p| !n.cloud_denied_projects.contains(p)),
        ),
        (
            "privacy.custom_patterns",
            o.custom_patterns
                .iter()
                .any(|p| !n.custom_patterns.contains(p)),
        ),
        (
            "privacy.allowlist",
            n.allowlist.iter().any(|a| !o.allowlist.contains(a)),
        ),
        (
            "judge.daily_budget_usd",
            new.judge.daily_budget_usd > old.judge.daily_budget_usd,
        ),
        (
            "judge.provider",
            old.judge.provider != JudgeProvider::Jev && new.judge.provider == JudgeProvider::Jev,
        ),
        // Texts start going to a remote host, or to another one than before.
        (
            "embedding.provider",
            new.embedding.is_remote() && old.embedding.provider != new.embedding.provider,
        ),
        (
            "embedding.endpoint",
            new.embedding.is_remote()
                && old.embedding.provider == new.embedding.provider
                && old.embedding.remote_destination() != new.embedding.remote_destination(),
        ),
        // The admin tools let an agent resolve reviews and merge memories.
        (
            "mcp.admin_tools",
            !old.mcp.admin_tools && new.mcp.admin_tools,
        ),
    ];
    checks
        .into_iter()
        .filter(|(_, loosened)| *loosened)
        .map(|(k, _)| k.to_string())
        .collect()
}

/// Fail-closed change control. Each poll of a source (the file, a project file) says whether
/// what it read would loosen a protection and fingerprints what it read. A change that does
/// not loosen is admitted at once; one that does is admitted only when the same fingerprint was
/// seen on [`LOOSEN_POLLS`] polls in a row. An editor that truncates the file and then writes
/// it, a sync tool that swaps the file, or a half-saved copy therefore never switches
/// protections off for a moment, while a deliberate edit takes effect a few polls later.
#[derive(Debug, Default)]
pub struct LooseningGate {
    pending: Option<(u64, u32)>,
}

impl LooseningGate {
    /// `true` when this read may be applied.
    pub fn admit(&mut self, fingerprint: u64, loosens: bool) -> bool {
        if !loosens {
            self.pending = None;
            return true;
        }
        let n = match self.pending {
            Some((f, n)) if f == fingerprint => n + 1,
            _ => 1,
        };
        if n >= LOOSEN_POLLS {
            self.pending = None;
            true
        } else {
            self.pending = Some((fingerprint, n));
            false
        }
    }

    /// A loosening was waiting and is waiting still.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The source is back to what is in force (or could not be read): the count restarts.
    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// A look that does not count as a poll saw `fingerprint`: content other than the one being
    /// counted breaks its run of identical polls (A counted, B not counted, A counted is not two
    /// in a row).
    pub fn saw_other(&mut self, fingerprint: u64) {
        if self.pending.is_some_and(|(f, _)| f != fingerprint) {
            self.pending = None;
        }
    }
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

/// The privacy in force for one project: the global settings with the project's file on top,
/// and whether the project's own file keeps it away from cloud providers. Ask
/// [`EffectivePrivacy::is_cloud_denied`]; the flag and the global list are two halves of one
/// answer, and a caller that checked only the list would send a denied project to the cloud.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectivePrivacy {
    pub privacy: PrivacySettings,
    /// `cloud_denied` from the project's `.alice.toml`.
    pub cloud_denied: bool,
}

impl EffectivePrivacy {
    /// `true` when memories of `project_slug` must never leave this machine for a cloud judge
    /// or embedding provider: the project's file says so, or the global list names it.
    ///
    /// Names are compared in the form Alice gives project slugs (trimmed, lower case, separators
    /// collapsed to `-`) on both sides, so an entry written `Client X` denies `client-x`. An
    /// entry that names an alias or the display name denies a project only when the caller asks
    /// about that name too (the daemon asks about every identity of a project).
    pub fn is_cloud_denied(&self, project_slug: &str) -> bool {
        let slug = crate::normalize::slugify(project_slug.trim());
        self.cloud_denied
            || (!slug.is_empty()
                && self
                    .privacy
                    .cloud_denied_projects
                    .iter()
                    .any(|p| crate::normalize::slugify(p.trim()) == slug))
    }
}

impl ProjectSettings {
    /// The project's privacy on top of the global one: never weaker.
    pub fn apply_to(&self, global: &PrivacySettings) -> EffectivePrivacy {
        let mut p = global.clone();
        let touched = self.privacy.block_secrets || !self.privacy.custom_patterns.is_empty();
        p.redact = p.redact || touched;
        if self.privacy.block_secrets {
            p.redact_mode = RedactMode::Block;
        }
        p.custom_patterns
            .extend(self.privacy.custom_patterns.iter().cloned());
        EffectivePrivacy {
            privacy: p,
            cloud_denied: self.privacy.cloud_denied,
        }
    }

    /// What a project whose file cannot be read gets: secrets blocked and no cloud.
    pub fn fail_closed() -> Self {
        Self::default().closed()
    }

    /// These settings with the two flags forced on (the custom patterns stay).
    fn closed(mut self) -> Self {
        self.privacy.block_secrets = true;
        self.privacy.cloud_denied = true;
        self
    }

    /// Whether `new` protects this project less than `self` does.
    pub fn loosened_by(&self, new: &Self) -> bool {
        let (o, n) = (&self.privacy, &new.privacy);
        (o.block_secrets && !n.block_secrets)
            || (o.cloud_denied && !n.cloud_denied)
            || o.custom_patterns
                .iter()
                .any(|p| !n.custom_patterns.contains(p))
    }
}

/// `<repo>/.alice.toml` read as text; `None` when there is none.
fn read_project_text(path: &Path) -> Result<Option<String>, SettingError> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(
            path.display().to_string(),
            format!("cannot read it: {e}"),
        )),
    }
}

fn parse_project(name: &str, text: &str) -> Result<ProjectSettings, SettingError> {
    let p: ProjectSettings = toml::from_str(text).map_err(|e| {
        err(
            name,
            format!(
                "{}; a project file accepts only [privacy] block_secrets, custom_patterns and cloud_denied",
                toml_error(text, e.message(), e.span())
            ),
        )
    })?;
    let check = Settings {
        privacy: p.apply_to(&PrivacySettings::default()).privacy,
        ..Default::default()
    };
    let problems: Vec<String> = check.validate().into_iter().map(|e| e.message).collect();
    if problems.is_empty() {
        Ok(p)
    } else {
        Err(err(name, problems.join("; ")))
    }
}

/// A missing file is no project settings; an invalid one is an error naming the file.
///
/// An `Err` is not "no settings": a caller that falls back to the defaults on it sends a denied
/// project to the cloud. Use [`ProjectSettingsCache::get`], which fails closed.
pub fn load_project(root: &Path) -> Result<ProjectSettings, SettingError> {
    let path = root.join(".alice.toml");
    match read_project_text(&path)? {
        None => Ok(ProjectSettings::default()),
        Some(text) => parse_project(&path.display().to_string(), &text),
    }
}

/// What [`ProjectSettingsCache::get`] decided for one repository.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectLoad {
    /// The settings to apply: never weaker than the last good ones unless a loosening has held
    /// for [`LOOSEN_POLLS`] polls.
    pub settings: ProjectSettings,
    /// Why `.alice.toml` is not in effect (invalid or unreadable); `settings` is then closed.
    pub error: Option<SettingError>,
    /// The file now loosens something and has not yet held long enough to apply.
    pub loosening_pending: bool,
}

/// Per-repository `.alice.toml` settings that fail closed.
///
/// - a file that is invalid or cannot be read gives `block_secrets` and `cloud_denied` on (and
///   the last good custom patterns), plus the error, until it is valid again;
/// - a file that loosens the last good settings (a flag off, a pattern removed, an emptied or
///   deleted file) applies only when the same content is read on [`LOOSEN_POLLS`] polls in a
///   row, so a half-written save never switches protections off; until then the last good
///   settings stay;
/// - a look counts as a poll only when at least the cache's interval has passed since the last
///   counted look at that repository, so many calls inside one truncate-then-write window are
///   one poll (the interval is the settings poll interval, 2 s in the daemon);
/// - a file that tightens, or changes nothing protective, applies at once;
/// - the first look at a repository has nothing to compare with and applies as read, except a
///   blank file: that is not yet "no protections" but a possible half-written save, and it
///   fails closed until it has been seen on the poll rule. A repository with no file at all
///   has no project settings, and that applies at once.
pub struct ProjectSettingsCache {
    every: Duration,
    clock: Clock,
    entries: parking_lot::Mutex<std::collections::HashMap<std::path::PathBuf, ProjectEntry>>,
}

type Clock = std::sync::Arc<dyn Fn() -> std::time::Instant + Send + Sync>;

impl std::fmt::Debug for ProjectSettingsCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectSettingsCache")
            .field("every", &self.every)
            .finish_non_exhaustive()
    }
}

impl Default for ProjectSettingsCache {
    /// The daemon's 2 s settings poll interval.
    fn default() -> Self {
        Self::new(Duration::from_secs(2))
    }
}

struct ProjectEntry {
    /// The settings in force and whether their file had content.
    good: Option<(ProjectSettings, bool)>,
    gate: LooseningGate,
    /// When the last look that counted as a poll was made.
    last_counted: Option<std::time::Instant>,
}

impl ProjectSettingsCache {
    /// A look counts as a poll when `every` has passed since the last one that did.
    pub fn new(every: Duration) -> Self {
        Self::with_clock(every, std::sync::Arc::new(std::time::Instant::now))
    }

    /// [`new`](Self::new) on a clock of the caller's (tests move it by hand).
    pub fn with_clock(every: Duration, clock: Clock) -> Self {
        Self {
            every,
            clock,
            entries: Default::default(),
        }
    }

    pub fn get(&self, root: &Path) -> ProjectLoad {
        // Two spellings of one repository (a symlink, `..`) share one entry and one gate.
        let root = &std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let path = root.join(".alice.toml");
        let name = path.display().to_string();
        let now = (self.clock)();
        let read = read_project_text(&path).and_then(|text| match text {
            None => Ok((ProjectSettings::default(), false, false, 0)),
            Some(t) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                t.hash(&mut h);
                parse_project(&name, &t).map(|p| (p, true, !t.trim().is_empty(), h.finish()))
            }
        });
        let mut entries = self.entries.lock();
        let entry = entries
            .entry(root.to_path_buf())
            .or_insert_with(|| ProjectEntry {
                good: None,
                gate: LooseningGate::default(),
                last_counted: None,
            });
        let (new, exists, nonempty, fingerprint) = match read {
            Ok(r) => r,
            Err(error) => {
                entry.gate.reset();
                let last = entry.good.as_ref().map(|(g, _)| g.clone());
                return ProjectLoad {
                    settings: last.unwrap_or_default().closed(),
                    error: Some(error),
                    loosening_pending: false,
                };
            }
        };
        if entry.good.is_none() {
            if exists && !nonempty {
                // A blank file on the first look: wait for it, closed.
                entry.good = Some((ProjectSettings::fail_closed(), true));
            } else {
                entry.good = Some((new.clone(), nonempty));
                return ProjectLoad {
                    settings: new,
                    error: None,
                    loosening_pending: false,
                };
            }
        }
        let (good, good_nonempty) = entry.good.clone().expect("set above");
        let loosens = good.loosened_by(&new) || (good_nonempty && !nonempty);
        let counts = entry
            .last_counted
            .is_none_or(|t| now.saturating_duration_since(t) >= self.every);
        let admitted = if !loosens {
            entry.gate.admit(fingerprint, false)
        } else if counts {
            entry.last_counted = Some(now);
            entry.gate.admit(fingerprint, true)
        } else {
            entry.gate.saw_other(fingerprint);
            false
        };
        if admitted {
            entry.good = Some((new.clone(), nonempty));
            return ProjectLoad {
                settings: new,
                error: None,
                loosening_pending: false,
            };
        }
        ProjectLoad {
            settings: good,
            error: None,
            loosening_pending: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N-1: an absurd age is refused (it once overflowed into "zero" and deleted everything).
    #[test]
    fn a_retention_longer_than_a_hundred_years_is_refused() {
        for bad in ["999999999999d", "40000d", "99999999999h"] {
            let e = Retention::parse(bad).unwrap_err();
            assert!(
                e.contains("100 years") && e.contains("forever"),
                "{bad}: {e}"
            );
        }
        assert_eq!(
            Retention::parse("36000d").unwrap(),
            Retention::Age(Duration::from_secs(36_000 * 86_400))
        );
        // Through the settings: the key is named and the rest of the file still applies.
        let file = "[storage]\nraw_retention = \"999999999999d\"\n[maintenance]\ntrash_days = 12\n";
        assert_eq!(
            error_keys(file, &[]),
            vec!["storage.raw_retention".to_string()]
        );
        let (kept, left_out) = resolve_salvaging(Some(file), &[]);
        assert_eq!(left_out.len(), 1);
        assert_eq!(left_out[0].key, "storage.raw_retention");
        assert_eq!(kept.settings.storage.raw_retention, Retention::Forever);
        assert_eq!(kept.settings.maintenance.trash_days, 12);
    }

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
        assert_eq!(s.search.relevance_floor, 0.805);
        assert_eq!(s.search.relevance_sure, 0.825);
        assert_eq!(s.search.relevance_margin, 0.015);
        assert_eq!(s.storage.raw_retention, Retention::Forever);
        assert_eq!((s.backup.daily, s.backup.weekly), (7, 4));
        assert_eq!(s.secrets.backend, SecretsBackend::File);
        assert!(!s.mcp.admin_tools);
    }

    /// Task 19: `search.rrf_k` and `search.boost_exponent` are ordinary search settings with
    /// ranges, a place in the key list and the schema, and an environment variable.
    #[test]
    fn rrf_k_and_boost_exponent_are_validated_search_settings() {
        let d = Settings::default();
        assert_eq!(d.search.rrf_k, crate::search::fusion::DEFAULT_RRF_K);
        assert_eq!(
            d.search.boost_exponent,
            crate::search::fusion::DEFAULT_BOOST_EXPONENT
        );
        let ok = resolve(Some("[search]\nrrf_k = 20\nboost_exponent = 0.5\n"), &[]).unwrap();
        assert_eq!(
            (ok.settings.search.rrf_k, ok.settings.search.boost_exponent),
            (20.0, 0.5)
        );
        for (file, key, needle) in [
            ("[search]\nrrf_k = 0\n", "search.rrf_k", "1 and 200"),
            ("[search]\nrrf_k = 201\n", "search.rrf_k", "1 and 200"),
            (
                "[search]\nboost_exponent = -0.1\n",
                "search.boost_exponent",
                "0 and 1",
            ),
            (
                "[search]\nboost_exponent = 1.1\n",
                "search.boost_exponent",
                "0 and 1",
            ),
        ] {
            let errs = resolve(Some(file), &[]).unwrap_err();
            let e = errs.iter().find(|e| e.key == key).unwrap_or_else(|| {
                panic!("{file:?}: no error for {key}: {errs:?}");
            });
            assert!(e.message.contains(needle), "{file:?}: {}", e.message);
        }
        for (v, ok) in [("1", true), ("200", true), ("0", true), ("1.0", true)] {
            let key = if v == "0" || v == "1.0" {
                ("ALICE_SEARCH_BOOST_EXPONENT", "search.boost_exponent")
            } else {
                ("ALICE_SEARCH_RRF_K", "search.rrf_k")
            };
            let r = resolve(Some(""), &env(&[(key.0, v)]));
            assert_eq!(r.is_ok(), ok, "{key:?}={v}: {r:?}");
        }
        let keys = key_infos();
        for k in ["search.rrf_k", "search.boost_exponent"] {
            let info = keys
                .iter()
                .find(|i| i.key == k)
                .unwrap_or_else(|| panic!("{k}"));
            assert!(
                !info.restart && !info.read_only && !info.not_used_yet,
                "{k}"
            );
        }
        // Neither is a protective key: loosening them says nothing about protections.
        let mut loose = Settings::default();
        loose.search.rrf_k = 1.0;
        loose.search.boost_exponent = 0.0;
        assert!(loosened_protections(&Settings::default(), &loose).is_empty());
    }

    /// The new keys can be set from the settings file by `edit_toml` and read back.
    #[test]
    fn rrf_k_and_boost_exponent_round_trip_through_the_file_and_the_view() {
        let t = edit_toml("", "search.rrf_k", &serde_json::json!(10)).unwrap();
        let t = edit_toml(&t, "search.boost_exponent", &serde_json::json!(0.25)).unwrap();
        let s = resolve(Some(&t), &[]).unwrap().settings;
        assert_eq!((s.search.rrf_k, s.search.boost_exponent), (10.0, 0.25));
        assert_eq!(get_key(&s, "search.rrf_k"), Some(serde_json::json!(10.0)));
        assert_eq!(
            get_key(&s, "search.boost_exponent"),
            Some(serde_json::json!(0.25))
        );
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
            s.search.relevance_margin as f32,
            crate::search::DEFAULT_RELEVANCE_MARGIN
        );
        assert_eq!(
            s.search.relevance_sure as f32,
            crate::search::DEFAULT_RELEVANCE_SURE
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
            Some(serde_json::json!(0.805))
        );
        assert_eq!(
            get_key(&s, "search.relevance_margin"),
            Some(serde_json::json!(0.015))
        );
        assert_eq!(
            get_key(&s, "judge.cluster_similarity"),
            Some(serde_json::json!(0.92))
        );
    }

    /// The variables the service commands read are known, not typos: no warning.
    #[test]
    fn the_service_test_hooks_are_not_unknown_variables() {
        let r = resolve(
            None,
            &env(&[
                ("ALICE_SERVICE_ROOT", "/tmp/x"),
                ("ALICE_SERVICE_FAIL", "stop"),
                ("ALICE_SERVICE_ROOTT", "typo"),
            ]),
        )
        .unwrap();
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(
            r.warnings[0].contains("ALICE_SERVICE_ROOTT"),
            "{:?}",
            r.warnings
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
                "[search]\nrelevance_floor = 0.9\nrelevance_sure = 0.8\n",
                &[],
                "search.relevance_sure",
            ),
            (
                "[search]\nrelevance_margin = 1.5\n",
                &[],
                "search.relevance_margin",
            ),
            (
                "",
                &[("ALICE_SEARCH_RELEVANCE_MARGIN", "-0.1")],
                "search.relevance_margin",
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
        // 0 turns the relative cut off; a bare 0 and a number from the environment both parse.
        let r = resolve(Some("[search]\nrelevance_margin = 0\n"), &[]).unwrap();
        assert_eq!(r.settings.search.relevance_margin, 0.0);
        let r = resolve(None, &env(&[("ALICE_SEARCH_RELEVANCE_MARGIN", "0.05")])).unwrap();
        assert_eq!(r.settings.search.relevance_margin, 0.05);
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
        assert!(eff.privacy.redact, "a project file turns redaction on");
        assert_eq!(eff.privacy.redact_mode, RedactMode::Block);
        assert_eq!(eff.privacy.custom_patterns.len(), 1);
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
        for k in ["embedding.idle_unload", "embedding.batch_size"] {
            assert!(keys.iter().any(|i| i.key == k && i.restart), "{k}");
        }
        // The provider, model and endpoint apply live (a re-embed), and are used.
        for k in [
            "embedding.provider",
            "embedding.model",
            "embedding.endpoint",
        ] {
            assert!(
                keys.iter()
                    .any(|i| i.key == k && !i.restart && !i.not_used_yet),
                "{k}"
            );
        }
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

    // ------------------------------------------------------------ fix round 1

    #[test]
    fn a_typo_in_an_unrelated_key_keeps_the_privacy_settings() {
        let file = "[privacy]\ncloud_denied_projects = [\"client-x\"]\nredact_mode = \"block\"\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }]\n\n[judge]\nreview_treshold = 0.7\nprovider = \"jev\"\n";
        let (r, dropped) = resolve_salvaging(Some(file), &env(&[]));
        assert_eq!(r.settings.privacy.cloud_denied_projects, vec!["client-x"]);
        assert_eq!(r.settings.privacy.redact_mode, RedactMode::Block);
        assert_eq!(r.settings.privacy.custom_patterns.len(), 1);
        assert_eq!(
            r.settings.judge.provider,
            JudgeProvider::Jev,
            "valid leaves outside privacy still apply"
        );
        assert_eq!(r.sources["privacy.redact_mode"], Source::File);
        assert_eq!(dropped.len(), 1, "{dropped:?}");
        assert_eq!(dropped[0].key, "judge.review_treshold");
    }

    #[test]
    fn a_bad_entry_in_a_privacy_list_costs_only_that_entry() {
        let file = "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }, { name = \"bad\", regex = \"(\" }]\nallowlist = [\"^ok$\", \"(\"]\ncloud_denied_projects = [\"a\", \"\"]\n";
        let (r, dropped) = resolve_salvaging(Some(file), &env(&[]));
        let p = &r.settings.privacy;
        assert_eq!(p.custom_patterns.len(), 1);
        assert_eq!(p.custom_patterns[0].name, "ticket");
        assert_eq!(p.allowlist, vec!["^ok$"]);
        assert_eq!(p.cloud_denied_projects, vec!["a"]);
        let mut keys: Vec<_> = dropped.iter().map(|e| e.key.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "privacy.allowlist",
                "privacy.cloud_denied_projects",
                "privacy.custom_patterns"
            ]
        );
    }

    #[test]
    fn a_file_that_does_not_parse_has_nothing_to_salvage() {
        let (r, dropped) = resolve_salvaging(
            Some("[judge\n"),
            &env(&[("ALICE_JUDGE_REVIEW_THRESHOLD", "0.7")]),
        );
        assert_eq!(
            r.settings.judge.review_threshold, 0.7,
            "the environment applies"
        );
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].key, "config.toml");
    }

    #[test]
    fn a_conflict_with_the_environment_still_keeps_the_privacy_leaves() {
        // The error names judge.review_threshold, which is the variable's, not a file leaf.
        let file =
            "[privacy]\ncloud_denied_projects = [\"p\"]\n[judge]\nauto_apply_threshold = 0.4\n";
        let (r, dropped) =
            resolve_salvaging(Some(file), &env(&[("ALICE_JUDGE_REVIEW_THRESHOLD", "0.7")]));
        assert_eq!(r.settings.privacy.cloud_denied_projects, vec!["p"]);
        assert_eq!(r.settings.judge.review_threshold, 0.7);
        assert!(!dropped.is_empty());
    }

    /// An environment value may be anything (a pasted key); no error repeats it.
    #[test]
    fn an_environment_value_is_never_echoed_in_an_error() {
        const MARK: &str = "ZQXSECRETMARKER";
        for info in key_infos().into_iter().filter(|k| !k.read_only) {
            let default = &DEFAULT_MAP[&info.key];
            if !(default.is_string() || default.is_null() || default.is_array()) {
                continue;
            }
            for raw in [
                MARK.to_string(),
                format!("\"{MARK}\""),
                format!("[{{ name = \"{MARK}\", regex = \"({MARK}\" }}]"),
                format!("[\"({MARK}\"]"),
            ] {
                if let Err(errs) = resolve(None, &env(&[(&info.env_var, &raw)])) {
                    for e in errs {
                        assert!(
                            !e.message.contains(MARK),
                            "{} = {raw} echoed: {}",
                            info.env_var,
                            e.message
                        );
                    }
                }
            }
        }
        let errs = resolve(None, &env(&[("ALICE_JUDGE_PROVIDER", "jevv")])).unwrap_err();
        assert_eq!(errs[0].key, "judge.provider");
        let m = &errs[0].message;
        assert!(
            m.contains("ALICE_JUDGE_PROVIDER") && m.contains("heuristic") && m.contains("jev"),
            "the fixed text still says what is accepted: {m}"
        );
    }

    fn pattern(name: &str) -> CustomPattern {
        CustomPattern {
            name: name.into(),
            regex: "TCK-[0-9]+".into(),
        }
    }

    fn guarded() -> Settings {
        let mut s = Settings::default();
        s.privacy.redact_mode = RedactMode::Block;
        s.privacy.custom_patterns = vec![pattern("ticket")];
        s.privacy.cloud_denied_projects = vec!["a".into(), "b".into()];
        s.privacy.allowlist = vec!["^ok$".into()];
        s.judge.daily_budget_usd = 1.0;
        s
    }

    #[test]
    fn loosened_protections_names_each_protective_key_that_got_weaker() {
        let base = guarded();
        assert!(loosened_protections(&base, &base).is_empty());
        // Tightening is not loosening.
        let mut t = base.clone();
        t.privacy.cloud_denied_projects.push("c".into());
        t.privacy.custom_patterns.push(pattern("other"));
        t.privacy.allowlist.clear();
        t.judge.daily_budget_usd = 0.5;
        assert!(loosened_protections(&base, &t).is_empty());

        type Loosen = Box<dyn Fn(&mut Settings)>;
        let cases: Vec<(&str, Loosen)> = vec![
            ("privacy.redact", Box::new(|s| s.privacy.redact = false)),
            (
                "privacy.redact_mode",
                Box::new(|s| s.privacy.redact_mode = RedactMode::Redact),
            ),
            (
                "privacy.cloud_denied_projects",
                Box::new(|s| {
                    s.privacy.cloud_denied_projects.pop();
                }),
            ),
            (
                "privacy.custom_patterns",
                Box::new(|s| s.privacy.custom_patterns.clear()),
            ),
            (
                "privacy.allowlist",
                Box::new(|s| s.privacy.allowlist.push(".*".into())),
            ),
            (
                "judge.daily_budget_usd",
                Box::new(|s| s.judge.daily_budget_usd = 5.0),
            ),
            (
                "judge.provider",
                Box::new(|s| s.judge.provider = JudgeProvider::Jev),
            ),
            (
                "embedding.provider",
                Box::new(|s| s.embedding.provider = EmbeddingProvider::Voyage),
            ),
            ("mcp.admin_tools", Box::new(|s| s.mcp.admin_tools = true)),
        ];
        for (key, change) in cases {
            let mut looser = base.clone();
            change(&mut looser);
            assert_eq!(
                loosened_protections(&base, &looser),
                vec![key.to_string()],
                "{key}"
            );
        }
        // Moving between cloud providers, or to a local one, loosens nothing.
        let mut cloud = base.clone();
        cloud.judge.provider = JudgeProvider::Jev;
        assert!(loosened_protections(&cloud, &base).is_empty());
        // Turning the admin tools off tightens.
        let mut admin = base.clone();
        admin.mcp.admin_tools = true;
        assert!(loosened_protections(&admin, &base).is_empty());
    }

    fn embedding(provider: EmbeddingProvider, endpoint: Option<&str>) -> Settings {
        let mut s = Settings::default();
        s.embedding.provider = provider;
        if provider != EmbeddingProvider::Local {
            s.embedding.model = "m".into();
        }
        s.embedding.endpoint = endpoint.map(String::from);
        s
    }

    #[test]
    fn a_loopback_endpoint_is_local_and_any_other_host_is_remote() {
        use EmbeddingProvider::*;
        for (p, e, remote) in [
            (Local, None, false),
            (Ollama, None, false),
            (Ollama, Some("http://127.0.0.1:11434/v1"), false),
            (Ollama, Some("http://localhost:11434"), false),
            (Ollama, Some("http://LOCALHOST:11434"), false),
            (Ollama, Some("http://127.0.0.2:11434"), false),
            (Ollama, Some("http://[::1]:11434/v1"), false),
            (Openai, Some("http://127.0.0.1:8080/v1"), false),
            (Openai, None, true),
            (Voyage, None, true),
            (Openai, Some("https://api.example.com/v1"), true),
            (Ollama, Some("http://10.0.0.5:11434"), true),
            (Ollama, Some("http://localhost.example.com/v1"), true),
            (Ollama, Some("http://127.0.0.1.example.com/v1"), true),
        ] {
            assert_eq!(embedding(p, e).embedding.is_remote(), remote, "{p:?} {e:?}");
        }
        assert!(endpoint_is_remote(Voyage, None));
        assert!(!endpoint_is_remote(Ollama, Some("http://localhost:1/v1")));
    }

    #[test]
    fn an_endpoint_that_cannot_be_read_is_remote_never_the_providers_default_or_local() {
        use EmbeddingProvider::*;
        for e in ["", "not a url", "http://", "ftp:///x"] {
            assert!(embedding(Ollama, Some(e)).embedding.is_remote(), "{e:?}");
            assert!(embedding(Openai, Some(e)).embedding.is_remote(), "{e:?}");
        }
    }

    #[test]
    fn switching_to_a_remote_embedder_loosens_a_protection_and_a_local_one_does_not() {
        use EmbeddingProvider::*;
        let local = embedding(Local, None);
        let lo = |a: &Settings, b: &Settings| loosened_protections(a, b);
        assert_eq!(
            lo(&local, &embedding(Openai, None)),
            vec!["embedding.provider"]
        );
        assert_eq!(
            lo(&local, &embedding(Voyage, Some("https://example.org/v1"))),
            vec!["embedding.provider"]
        );
        // A local Ollama is local; moving to one loosens nothing.
        assert!(lo(&local, &embedding(Ollama, None)).is_empty());
        assert!(lo(&local, &embedding(Ollama, Some("http://localhost:1/v1"))).is_empty());
        // The endpoint of a local Ollama moved to another host does.
        assert_eq!(
            lo(
                &embedding(Ollama, None),
                &embedding(Ollama, Some("http://10.0.0.5:11434"))
            ),
            vec!["embedding.endpoint"]
        );
        assert_eq!(
            lo(
                &embedding(Openai, None),
                &embedding(Openai, Some("https://proxy.example.com/v1"))
            ),
            vec!["embedding.endpoint"]
        );
        // Another remote provider is another recipient.
        assert_eq!(
            lo(&embedding(Openai, None), &embedding(Voyage, None)),
            vec!["embedding.provider"]
        );
        // Staying on the same remote host, or going back to local, loosens nothing.
        let mut other_model = embedding(Openai, None);
        other_model.embedding.model = "other".into();
        assert!(lo(&embedding(Openai, None), &other_model).is_empty());
        assert!(
            lo(
                &embedding(Openai, None),
                &embedding(Openai, Some("https://api.openai.com/v1"))
            )
            .is_empty()
        );
        assert!(lo(&embedding(Openai, None), &local).is_empty());
        assert!(
            lo(
                &embedding(Ollama, Some("http://10.0.0.5:11434")),
                &embedding(Ollama, None)
            )
            .is_empty()
        );
    }

    #[test]
    fn the_gate_lets_a_loosening_through_on_the_third_identical_poll() {
        let mut g = LooseningGate::default();
        assert!(g.admit(7, false), "tightening applies at once");
        assert!(!g.admit(7, true) && g.is_pending());
        assert!(!g.admit(7, true));
        assert!(g.admit(7, true), "third time in a row");
        assert!(!g.is_pending());
        // A different content starts the count again.
        assert!(!g.admit(1, true));
        assert!(!g.admit(1, true));
        assert!(!g.admit(2, true));
        assert!(!g.admit(2, true));
        // Going back to something that does not loosen clears it.
        assert!(g.admit(2, false));
        assert!(!g.admit(2, true), "count restarted after the reset");
        g.reset();
        assert!(!g.is_pending());
    }

    #[test]
    fn a_project_is_cloud_denied_by_its_own_flag_or_the_global_list() {
        let global = PrivacySettings {
            cloud_denied_projects: vec!["listed".into()],
            ..Default::default()
        };
        let plain = ProjectSettings::default().apply_to(&global);
        assert!(plain.is_cloud_denied("listed"), "the global list alone");
        assert!(!plain.is_cloud_denied("other"));
        let mut flagged = ProjectSettings::default();
        flagged.privacy.cloud_denied = true;
        let eff = flagged.apply_to(&global);
        assert!(eff.is_cloud_denied("other"), "the project's own flag alone");
        assert!(eff.is_cloud_denied("listed"));
    }

    fn write_project(dir: &Path, text: &str) {
        std::fs::write(dir.join(".alice.toml"), text).unwrap();
    }

    const STRICT: &str = "[privacy]\nblock_secrets = true\ncloud_denied = true\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }]\n";

    #[test]
    fn a_project_file_that_cannot_be_read_fails_closed() {
        let d = tempfile::tempdir().unwrap();
        let cache = ProjectSettingsCache::default();
        // Never seen a good one: closed.
        write_project(d.path(), "[privacy\n");
        let l = cache.get(d.path());
        assert!(l.error.is_some());
        assert!(l.settings.privacy.block_secrets && l.settings.privacy.cloud_denied);
        // A good one, then a broken one: closed, and the last good patterns are kept.
        write_project(d.path(), STRICT);
        let l = cache.get(d.path());
        assert!(l.error.is_none() && l.settings.privacy.custom_patterns.len() == 1);
        write_project(d.path(), "[privacy]\nredact = false\n");
        let l = cache.get(d.path());
        assert!(l.error.is_some());
        assert!(l.settings.privacy.block_secrets && l.settings.privacy.cloud_denied);
        assert_eq!(l.settings.privacy.custom_patterns.len(), 1);
        // Unreadable (a directory in its place) is an error too.
        std::fs::remove_file(d.path().join(".alice.toml")).unwrap();
        std::fs::create_dir(d.path().join(".alice.toml")).unwrap();
        let l = cache.get(d.path());
        assert!(l.error.is_some() && l.settings.privacy.cloud_denied);
        assert_eq!(
            ProjectSettings::fail_closed().privacy,
            ProjectPrivacy {
                block_secrets: true,
                cloud_denied: true,
                custom_patterns: vec![]
            }
        );
    }

    const EVERY: Duration = Duration::from_secs(2);

    /// A cache on a clock the test moves by hand.
    fn clocked() -> (
        ProjectSettingsCache,
        std::sync::Arc<parking_lot::Mutex<std::time::Instant>>,
    ) {
        let now = std::sync::Arc::new(parking_lot::Mutex::new(std::time::Instant::now()));
        let n = now.clone();
        let cache = ProjectSettingsCache::with_clock(EVERY, std::sync::Arc::new(move || *n.lock()));
        (cache, now)
    }

    fn tick(now: &parking_lot::Mutex<std::time::Instant>) {
        *now.lock() += EVERY;
    }

    #[test]
    fn a_project_file_that_loosens_must_hold_for_three_spaced_polls() {
        let d = tempfile::tempdir().unwrap();
        let (cache, now) = clocked();
        write_project(d.path(), STRICT);
        assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        // Emptied (a half-written save): the last good one stays for two polls.
        write_project(d.path(), "");
        for _ in 0..2 {
            tick(&now);
            let l = cache.get(d.path());
            assert!(l.settings.privacy.cloud_denied && l.loosening_pending);
        }
        // Back to the strict content: nothing was applied, and the count is gone.
        write_project(d.path(), STRICT);
        tick(&now);
        assert!(!cache.get(d.path()).loosening_pending);
        write_project(d.path(), "");
        for _ in 0..2 {
            tick(&now);
            assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        }
        tick(&now);
        let l = cache.get(d.path());
        assert!(
            !l.settings.privacy.cloud_denied && !l.loosening_pending,
            "applied on the third spaced poll in a row"
        );
        // Tightening applies at once, with no waiting.
        write_project(d.path(), STRICT);
        assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        // Deleting the file counts as emptying.
        std::fs::remove_file(d.path().join(".alice.toml")).unwrap();
        for _ in 0..2 {
            tick(&now);
            assert!(cache.get(d.path()).settings.privacy.block_secrets);
        }
        tick(&now);
        assert!(!cache.get(d.path()).settings.privacy.block_secrets);
    }

    /// N1: three looks inside one truncate-then-write window are one poll, not three.
    #[test]
    fn rapid_looks_are_one_poll() {
        let d = tempfile::tempdir().unwrap();
        let (cache, now) = clocked();
        write_project(d.path(), STRICT);
        assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        write_project(d.path(), "");
        for i in 0..10 {
            let l = cache.get(d.path());
            assert!(
                l.settings.privacy.cloud_denied && l.loosening_pending,
                "look {i} applied an emptied file"
            );
        }
        // Spaced looks count: this one was the first (above), two more complete it.
        tick(&now);
        assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        tick(&now);
        assert!(!cache.get(d.path()).settings.privacy.cloud_denied);
        // Looks at different repositories do not share a count.
        let other = tempfile::tempdir().unwrap();
        write_project(other.path(), STRICT);
        assert!(cache.get(other.path()).settings.privacy.cloud_denied);
    }

    #[test]
    fn a_slug_with_stray_whitespace_is_still_the_denied_project() {
        let global = PrivacySettings {
            cloud_denied_projects: vec!["client-x".into()],
            ..Default::default()
        };
        let eff = ProjectSettings::default().apply_to(&global);
        for slug in ["client-x", " client-x ", "\tclient-x\n"] {
            assert!(eff.is_cloud_denied(slug), "{slug:?}");
        }
        assert!(!eff.is_cloud_denied("client-y"));
    }

    #[test]
    fn a_list_entry_in_another_spelling_still_denies_the_slug() {
        let list = |e: &str| {
            ProjectSettings::default().apply_to(&PrivacySettings {
                cloud_denied_projects: vec![e.into()],
                ..Default::default()
            })
        };
        for entry in [
            "Client X",
            "CLIENT-X",
            " client_x ",
            "client   x",
            "Client-X",
        ] {
            assert!(list(entry).is_cloud_denied("client-x"), "{entry:?}");
            assert!(list("client-x").is_cloud_denied(entry), "{entry:?} asked");
        }
        assert!(!list("Client X").is_cloud_denied("client-y"));
        assert!(
            !list("---").is_cloud_denied(""),
            "an empty name matches nothing"
        );
    }

    /// Carried from Task 5's review: A counted, B not counted, A counted is not two polls in a row.
    #[test]
    fn a_different_uncounted_look_breaks_the_run_of_identical_polls() {
        let d = tempfile::tempdir().unwrap();
        let (cache, now) = clocked();
        write_project(d.path(), STRICT);
        assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        write_project(d.path(), ""); // A
        assert!(cache.get(d.path()).loosening_pending, "A counted: 1");
        write_project(d.path(), "[privacy]\nblock_secrets = true\n"); // B, looked at at once
        assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        write_project(d.path(), ""); // A again
        tick(&now);
        assert!(
            cache.get(d.path()).settings.privacy.cloud_denied,
            "A counted: 1 again"
        );
        tick(&now);
        assert!(
            cache.get(d.path()).settings.privacy.cloud_denied,
            "A counted: 2, so far from three"
        );
        tick(&now);
        assert!(
            !cache.get(d.path()).settings.privacy.cloud_denied,
            "A counted: 3"
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_spellings_of_one_repository_share_one_gate() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("repo");
        std::fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (cache, now) = clocked();
        write_project(&real, STRICT);
        assert!(cache.get(&real).settings.privacy.cloud_denied);
        write_project(&real, "");
        assert!(cache.get(&real).settings.privacy.cloud_denied, "poll 1");
        tick(&now);
        assert!(
            cache.get(&link).settings.privacy.cloud_denied,
            "poll 2, by the other spelling"
        );
        tick(&now);
        assert!(
            !cache.get(&real).settings.privacy.cloud_denied,
            "poll 3 completes the run: one entry, one gate"
        );
    }

    #[test]
    fn cli_values_follow_the_toml_rule_and_fall_back_to_text() {
        for (raw, want) in [
            ("0.6", serde_json::json!(0.6)),
            ("10", serde_json::json!(10)),
            ("true", serde_json::json!(true)),
            ("\"quoted\"", serde_json::json!("quoted")),
            ("[\"a\", \"b\"]", serde_json::json!(["a", "b"])),
            ("90d", serde_json::json!("90d")),
            ("heuristic", serde_json::json!("heuristic")),
            ("", serde_json::json!("")),
        ] {
            assert_eq!(parse_cli_value(raw), want, "{raw:?}");
        }
    }

    /// N3: the first look at a blank file is not "no protections".
    #[test]
    fn a_blank_project_file_on_the_first_look_is_not_settled() {
        let d = tempfile::tempdir().unwrap();
        let (cache, now) = clocked();
        write_project(d.path(), "");
        let l = cache.get(d.path());
        assert!(l.error.is_none() && l.loosening_pending);
        assert!(l.settings.privacy.block_secrets && l.settings.privacy.cloud_denied);
        tick(&now);
        assert!(cache.get(d.path()).settings.privacy.cloud_denied);
        tick(&now);
        let l = cache.get(d.path());
        assert!(
            !l.settings.privacy.cloud_denied && !l.loosening_pending,
            "settled after three spaced looks"
        );
        // No file at all is the normal state of most repositories: nothing to wait for.
        let none = tempfile::tempdir().unwrap();
        let l = cache.get(none.path());
        assert_eq!(l.settings, ProjectSettings::default());
        assert!(!l.loosening_pending);
    }

    /// N2: the privacy leaves kept after a conflict are the pruned ones.
    #[test]
    fn a_conflict_keeps_the_valid_entries_of_a_privacy_list() {
        let file = "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }, { name = \"bad\", regex = \"(\" }]\n[judge]\nauto_apply_threshold = 0.4\n";
        let (r, _) =
            resolve_salvaging(Some(file), &env(&[("ALICE_JUDGE_REVIEW_THRESHOLD", "0.7")]));
        assert_eq!(r.settings.privacy.custom_patterns.len(), 1);
        assert_eq!(r.settings.privacy.custom_patterns[0].name, "ticket");
        assert_eq!(r.settings.judge.review_threshold, 0.7);
    }

    /// A pattern that matches empty text is refused by validation (it would fire between every
    /// two characters), in the global file and in a project file.
    #[test]
    fn an_empty_matching_custom_pattern_is_refused() {
        let file = "[privacy]\ncustom_patterns = [{ name = \"ticket\", regex = \"TCK-[0-9]+\" }, { name = \"all\", regex = \".*\" }, { name = \"digits\", regex = \"\\\\d*\" }]\n";
        let (_r, errors) = resolve_salvaging(Some(file), &[]);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("matches empty text") && e.message.contains("all")),
            "{errors:?}"
        );
        let e = parse_project(
            "f",
            "[privacy]\ncustom_patterns = [{ name = \"all\", regex = \".*\" }]\n",
        )
        .unwrap_err();
        assert!(e.message.contains("matches empty text"), "{}", e.message);
    }

    #[test]
    fn toml_line_counts_lines_from_one() {
        assert_eq!(toml_line("a = 1\nb = 2\nc", 0), 1);
        assert_eq!(toml_line("a = 1\nb = 2\nc", 6), 2);
        assert_eq!(toml_line("a = 1\nb = 2\nc", 12), 3);
        assert_eq!(toml_line("a", 99), 1, "past the end is the last line");
    }

    /// N1: a floor above the default sure floor, with no sure floor of the owner's own, is a
    /// single floor, not an error about a key nobody wrote; an explicit sure floor below the
    /// floor is reported on that key and costs only itself.
    #[test]
    fn a_high_floor_raises_an_unset_sure_floor_and_an_explicit_one_is_checked() {
        let r = resolve(Some("[search]\nrelevance_floor = 0.85\n"), &[]).unwrap();
        assert_eq!(
            (
                r.settings.search.relevance_floor,
                r.settings.search.relevance_sure
            ),
            (0.85, 0.85)
        );
        assert_eq!(
            r.sources.get("search.relevance_sure"),
            Some(&Source::Default),
            "still a default"
        );
        let r = resolve(None, &env(&[("ALICE_SEARCH_RELEVANCE_FLOOR", "0.9")])).unwrap();
        assert_eq!(r.settings.search.relevance_sure, 0.9);
        // A floor at or below the default sure floor leaves it alone.
        let r = resolve(Some("[search]\nrelevance_floor = 0.81\n"), &[]).unwrap();
        assert_eq!(r.settings.search.relevance_sure, 0.825);
        // Both set, the sure floor lower: the error names the sure floor.
        let file = "[search]\nrelevance_floor = 0.85\nrelevance_sure = 0.81\n[judge]\nreview_threshold = 0.6\n";
        let errs = resolve(Some(file), &[]).unwrap_err();
        assert!(
            errs.iter().any(|e| e.key == "search.relevance_sure"),
            "{errs:?}"
        );
        // Salvaging drops that leaf only; the floor and the rest of the file apply.
        let (r, left_out) = resolve_salvaging(Some(file), &[]);
        assert_eq!(left_out.len(), 1, "{left_out:?}");
        assert_eq!(left_out[0].key, "search.relevance_sure");
        assert_eq!(r.settings.judge.review_threshold, 0.6);
        assert_eq!(r.settings.search.relevance_floor, 0.85);
        assert_eq!(r.settings.search.relevance_sure, 0.85);
        // The same through the environment.
        let errs = resolve(
            Some("[search]\nrelevance_floor = 0.85\n"),
            &env(&[("ALICE_SEARCH_RELEVANCE_SURE", "0.8")]),
        )
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e.key == "search.relevance_sure"),
            "{errs:?}"
        );
    }
}
