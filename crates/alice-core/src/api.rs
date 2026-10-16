//! Request and response types shared by the HTTP API, MCP tools and the CLI.

use crate::error::{AliceError, Result};
use crate::kg_types::*;
use crate::model::{Author, Kind, Memory, Status};
use crate::settings::{KeyInfo, SettingError, Source};
use crate::store::memories::MemoryUpdate;
use crate::store::sources::{ChunkInput, SourceInput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use crate::store::events::{EventQuery as EventsRequest, EventRow};
pub use crate::store::reviews::{ReviewCounts, ReviewItem, ReviewQuery, ReviewType};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecallMode {
    /// Hybrid search without the judge.
    #[default]
    Fast,
    /// The judge grades the top results of every tier (raw chunks too) and also looks at a few
    /// candidates just below the vector floor; needs a model judge (`judge.provider = jev`).
    Precise,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchVia {
    Vector,
    Words,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Knowledge,
    Raw,
}

/// Search memories. Use it before answering questions about past work, decisions or bugs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RecallRequest {
    /// Keywords or a question.
    pub query: String,
    /// Project slug to restrict to. Defaults to boosting the current project without excluding others.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<Kind>,
    /// Only memories created on or after this date (YYYY-MM-DD or RFC 3339).
    #[serde(default)]
    pub since: Option<String>,
    /// Only memories created before this date.
    #[serde(default)]
    pub before: Option<String>,
    /// Also search raw session transcripts.
    #[serde(default)]
    pub include_raw: bool,
    /// `fast` (local hybrid search only) or `precise` (the judge grades the top results and
    /// rescues answers just below the vector floor; needs `judge.provider = jev`). Absent: the
    /// `search.default_mode` setting, which by default is `precise` when a model judge is
    /// configured and `fast` otherwise.
    #[serde(default)]
    pub mode: Option<RecallMode>,
    /// Response budget in tokens (default 1500).
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub format: OutputFormat,
    /// Number of results (default 10, max 50).
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
    /// Server mode: the project the remote client detected (spec §4.1). Wins over `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub client_project: Option<ClientProject>,
    /// Server mode: a project name a direct MCP caller gave (tenant mode only). Set in-process
    /// by the MCP handlers; the HTTP API clears it on entry, so it never comes from the wire.
    /// It is serialised so that a spooled request keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub project_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Hit {
    pub id: String,
    pub tier: Tier,
    pub kind: Option<Kind>,
    pub project: String,
    pub title: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub created_at: String,
    /// The search's fusion score. In precise mode the results are in the judge's order, which
    /// this score does not follow; use `relevance` for that.
    pub score: f64,
    /// The judge's P(relevant) for this result: P(answers) plus half of P(partly answers).
    /// Set only for results the judge graded in precise mode, otherwise absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance: Option<f32>,
    pub matched: Vec<MatchVia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// `team` for a hit that came from the shared team memory; absent for a personal one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The tenant that wrote a team hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Set by a team store on the hits it returns: whether the cloud policy lets this hit's
    /// project be sent to a model. `Some(false)` on any doubt; absent outside the team store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RelatedRef {
    pub id: String,
    pub kind: Kind,
    pub title: String,
    /// How it is related: a link type or `entity:<name>`.
    pub via: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RecallResponse {
    pub project: Option<String>,
    pub results: Vec<Hit>,
    pub related: Vec<RelatedRef>,
    pub omitted: usize,
    pub raw_fallback: bool,
    /// True when precise mode re-ordered the results with the judge's grades (their `score`
    /// is still the fusion score). False for fast mode, and for precise mode when no judge
    /// could grade (the heuristic provider, a denied project, an exhausted budget, an outage).
    #[serde(default)]
    pub reranked: bool,
    /// True when precise mode graded the results and none of them clearly answers the query
    /// (P(answers) below 0.5 for every graded result); the results are then only the closest
    /// matches, and `notes` says so. Always false in fast mode.
    #[serde(default)]
    pub low_confidence: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Things worth knowing about this answer, such as a search that used word matches only
    /// because the project's memories may not be sent to the embedding provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Largest `.alice.toml` a remote client may send (spec §4.1).
pub const CLIENT_RULES_MAX_BYTES: usize = 64 * 1024;

/// The project a remote client detected for its working directory, with that working tree's
/// privacy rules as the client read them (server mode, spec §3.3 and §4.1). A server uses it
/// instead of running git on a path it does not have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientProject {
    /// `ProjectRef::key` as detected on the client (a normalized remote, or `path:<dir>`).
    pub key: String,
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub rules: ClientRules,
}

/// The working tree's `.alice.toml`, as the client found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClientRules {
    /// The working tree has no `.alice.toml`.
    None,
    /// Its content, at most [`CLIENT_RULES_MAX_BYTES`].
    File { text: String },
    /// It exists but could not be read; the rules fail closed.
    Unreadable { reason: String },
}

impl ClientProject {
    /// The limits of spec §4.1. A refusal names the field, never the value.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| {
            Err(AliceError::invalid(format!(
                "client_project.{what}; update the alice client, or report it if it is current"
            )))
        };
        if self.key.is_empty() || self.key.len() > 1024 {
            return bad("key must be 1 to 1024 bytes");
        }
        if self.key.chars().any(char::is_control) {
            return bad("key must not contain control characters");
        }
        if self.key == "global" {
            return bad("key must not be \"global\" (send no client_project for global)");
        }
        if self.slug.is_empty() || crate::normalize::slugify(&self.slug) != self.slug {
            return bad("slug must be a non-empty slug (lower case, digits and -)");
        }
        if self.slug.chars().count() > crate::model::limits::MAX_PROJECT_SLUG_CHARS {
            return bad("slug must be at most 100 characters");
        }
        if self.name.chars().count() > crate::model::limits::MAX_PROJECT_NAME_CHARS {
            return bad("name must be at most 200 characters");
        }
        if self.remote.as_ref().is_some_and(|r| r.len() > 1024) {
            return bad("remote must be at most 1024 bytes");
        }
        match &self.rules {
            ClientRules::File { text } if text.len() > CLIENT_RULES_MAX_BYTES => {
                bad("rules.text must be at most 64 KiB")
            }
            ClientRules::Unreadable { reason } if reason.chars().count() > 500 => {
                bad("rules.reason must be at most 500 characters")
            }
            _ => Ok(()),
        }
    }
}

/// Save a memory: a decision, fix, fact, how-to, preference, warning or handoff worth keeping.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RememberRequest {
    /// The memory text. The first line becomes the title unless `title` is given.
    pub content: String,
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub title: Option<String>,
    /// Project slug. Defaults to the current project.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    /// 1 (minor) to 5 (critical). Default 3.
    #[serde(default)]
    pub importance: Option<u8>,
    /// Ids of memories this one replaces.
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// Repeat-safe key: a retry with the same key returns the first memory.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
    /// Server mode: the project the remote client detected (spec §4.1). Wins over `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub client_project: Option<ClientProject>,
    /// Server mode: a project name a direct MCP caller gave (tenant mode only). Set in-process
    /// by the MCP handlers; the HTTP API clears it on entry, so it never comes from the wire.
    /// It is serialised so that a spooled request keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub project_hint: Option<String>,
    /// Set only when the user explicitly asks to save this to the shared, team or central memory
    /// ("ความจำส่วนกลาง", "for the team"). A shared memory is kept only in the team store and is
    /// visible to every team member; only its author or the admin can change it later. Leave
    /// it unset for anything personal.
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RememberStatus {
    New,
    Duplicate,
    /// The daemon was unavailable; the request is queued on disk and will be stored shortly.
    Spooled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RememberResponse {
    pub id: Option<String>,
    pub status: RememberStatus,
    pub project: String,
    /// Only for `spooled`: why the request was queued and when it will be saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// True when the memory went to the team store (its id then starts with `team:`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,
}

/// Briefing for the current project: key memories, warnings, recent decisions, last handoff, preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ContextRequest {
    #[serde(default)]
    pub project: Option<String>,
    /// Budget in tokens (default 1200).
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
    /// Server mode: the project the remote client detected (spec §4.1). Wins over `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub client_project: Option<ClientProject>,
    /// Server mode: a project name a direct MCP caller gave (tenant mode only). Set in-process
    /// by the MCP handlers; the HTTP API clears it on entry, so it never comes from the wire.
    /// It is serialised so that a spooled request keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub project_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContextResponse {
    pub project: String,
    pub text: String,
}

/// Full memories by id, with links and (optionally) the raw chunks they came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GetRequest {
    pub ids: Vec<String>,
    #[serde(default)]
    pub include_provenance: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkRef {
    pub id: String,
    pub link_type: String,
    /// `out` when this memory points at `id`, `in` otherwise.
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemoryFull {
    pub memory: Memory,
    pub links: Vec<LinkRef>,
    pub provenance: Vec<String>,
    /// Set on `update` answers: the event that recorded the change (pass it to `undo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// `team` for a memory read from the team store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// For a team memory: the tenant that wrote it (the `<tenant>` part of its author).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GetResponse {
    pub memories: Vec<MemoryFull>,
    pub missing: Vec<String>,
    /// Ids that were asked for but are duplicates merged into another memory: `from` is the id
    /// asked for, `to` the survivor, which is in `memories`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<Redirect>,
    /// Things worth knowing about the answer (a merge chain that was cut short).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A merged duplicate and the memory that replaced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Redirect {
    pub from: String,
    pub to: String,
}

/// Edit, supersede, archive, forget (soft delete) or restore a memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UpdateRequest {
    pub id: String,
    #[serde(flatten)]
    pub update: MemoryUpdate,
    /// Who makes the change (filled by the daemon: the MCP client, or the CLI).
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

/// Reverse a change: the event id comes from `alice history` or `/api/v1/events`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UndoRequest {
    pub event_id: String,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UndoResponse {
    pub undone: bool,
    /// Events reverted (the whole group of the one asked for).
    pub reverted: Vec<String>,
    /// Set when the target changed since: a review item asks what to do.
    pub review_id: Option<String>,
    pub message: String,
}

/// Merge the project `from` into `into` (CLI and HTTP only; MCP does not offer it). With
/// `dry_run` nothing is changed and the response says what would move.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMergeRequest {
    /// The project to merge away: a slug or an alias. Not `global`.
    pub from: String,
    /// The project that receives its memories, sources, chunks and names.
    pub into: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMergeResponse {
    /// Slugs of the two projects, as stored.
    pub from: String,
    pub into: String,
    /// False for a dry run (nothing changed).
    pub merged: bool,
    pub memories: u64,
    pub chunks: u64,
    pub sources: u64,
    /// Facts learned from the moved memories; they follow their memories.
    pub facts: u64,
    /// The event that recorded the merge: `alice undo <event_id>` reverses it.
    pub event_id: Option<String>,
    pub message: String,
}

/// Apply one of a review item's choices.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ResolveRequest {
    pub id: String,
    pub choice: String,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolveResponse {
    pub review: ReviewItem,
    pub message: String,
}

/// Close every open review item of a type, without changing any memory (the weekly merge job
/// can flood the inbox; this clears it).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DismissRequest {
    pub review_type: ReviewType,
    /// Only items whose payload names this provider (`cluster` for the merge job).
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DismissResponse {
    /// Items closed.
    pub dismissed: u64,
}

/// Merge duplicates: the others are trashed and point at the survivor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MergeRequest {
    pub survivor: String,
    pub others: Vec<String>,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MergeResponse {
    /// The ids that became duplicates of the survivor.
    pub merged: Vec<String>,
}

/// Counts, review backlog, judge backlog and costs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Stats {
    pub memories_by_status: BTreeMap<String, u64>,
    pub memories_by_kind: BTreeMap<String, u64>,
    /// (project slug, live memories), most first.
    pub top_projects: Vec<(String, u64)>,
    pub chunks: u64,
    pub reviews_open: u64,
    pub judge_provider: String,
    /// Why the judge in force is not the one asked for (or out of budget), if it is not.
    #[serde(default)]
    pub judge_warning: Option<String>,
    pub judge_backlog: u64,
    pub judge_retry: u64,
    pub embed_backlog: u64,
    /// Judge spend of the current UTC day.
    pub cost_today_usd: f64,
    /// Judge spend of the last 7 UTC days, today included.
    pub cost_7d_usd: f64,
}

/// When a maintenance job last ran (`alice maintenance status`). Times are RFC 3339 UTC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobStatus {
    /// `backup`, `trash_purge`, ...
    pub job: String,
    /// How often the job is due, in seconds.
    pub every_secs: u64,
    /// The last time the job ran to its end or failed (a run a request interrupted does not count).
    pub last_run: Option<String>,
    /// The last time it finished without an error.
    pub last_ok: Option<String>,
    /// The last failure: its time, then what went wrong.
    pub last_error: Option<String>,
}

/// What `/api/v1/events/stream` pushes (one JSON object per SSE message).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamItem {
    /// A committed change.
    Event { event: EventRow },
    /// A review item was added.
    Review { id: String, review_type: String },
    /// Settings changed (their keys).
    Settings { changed: Vec<String> },
    /// The daemon's health: on connect, then whenever it changes.
    Health { health: Health },
    /// This client fell behind and `missed` items were dropped; list `/api/v1/events` for them.
    Lagged { missed: u64 },
}

/// Raw evidence ingest (used by capture hooks and importers).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct IngestRequest {
    pub source: SourceInput,
    pub chunks: Vec<ChunkInput>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
    /// Server mode: the project the remote client detected (spec §4.1). Wins over `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub client_project: Option<ClientProject>,
    /// Server mode: a project name a direct MCP caller gave (tenant mode only). Set in-process
    /// by the MCP handlers; the HTTP API clears it on entry, so it never comes from the wire.
    /// It is serialised so that a spooled request keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub project_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    pub service: String,
    pub instance: String,
    pub version: String,
    pub pid: u32,
    pub memories: u64,
    pub chunks: u64,
    pub projects: u64,
    pub embed_backlog: u64,
    /// Memories waiting for the judge (embedded or not yet).
    #[serde(default)]
    pub judge_backlog: u64,
    /// The judge in force (`heuristic` or `jev`).
    #[serde(default)]
    pub judge_provider: String,
    /// Why the judge in force is not the one asked for (or out of budget), if it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_warning: Option<String>,
    /// The embedding model in force (what vectors and queries use; `multilingual-e5-small-q8`
    /// for the local model, `<provider>:<model>` otherwise).
    #[serde(default)]
    pub embedding_model: String,
    /// Progress of a re-embed into another model after a change of `embedding.*`; the model in
    /// force serves searches until it is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reembed: Option<ReembedStatus>,
    /// Why the embedding model asked for is not (yet) the one in force: the provider cannot be
    /// reached, a key is missing, or a re-embed stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_error: Option<String>,
    /// Memories and chunks not embedded because the embedder is remote and the cloud policy keeps
    /// their project at home; they are searched by words only.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub embed_skipped_cloud: u64,
    /// The backups are behind: the newest is older than two backup periods (or none was made in
    /// a data directory that old).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<String>,
    /// Review items waiting for a decision (`alice review`).
    #[serde(default)]
    pub reviews_open: u64,
    /// False while the local embedding model is unloaded (idle) or being loaded again.
    #[serde(default)]
    pub model_loaded: bool,
    /// Writes waiting in the spool (made while the daemon could not take them).
    #[serde(default)]
    pub spool_pending: u64,
    /// When the newest scheduled or manual backup was made (RFC 3339), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup: Option<String>,
    /// The daemon's own memory: physical footprint on macOS, resident set on Linux; `None`
    /// where it cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// False while the vector index is being rebuilt (search is lexical-only meanwhile).
    pub index_ready: bool,
    /// `ALICE_*` variables that are not settings (a typo, say) and were ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings_warnings: Vec<String>,
    /// Settings that are not in effect because they are invalid: a bad edit of `config.toml`
    /// (the previous settings stay), a bad value in a variable (the file still applies). The
    /// same list as `/api/v1/settings` (`errors`) and `alice config list`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings_errors: Vec<SettingError>,
    /// No valid `config.toml` has loaded since the daemon started, so the settings in force may
    /// be missing a privacy setting; cloud providers are off meanwhile.
    #[serde(default, skip_serializing_if = "is_false")]
    pub config_degraded: bool,
    /// Why the link to the team store is off or failing (server mode); never the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_warning: Option<String>,
    /// Automatic capture of agent sessions (SP2 spec §8.4); absent while capture never ran.
    /// Boxed: it would make every stream item as large as itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Box<CaptureHealth>>,
}

/// The capture line of [`Health`] (SP2 spec §8.4). The distill fields keep their defaults until
/// distillation exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureHealth {
    /// Sessions that captured something in the last 24 hours.
    pub sessions_24h: u64,
    /// Chunks captured in the last 24 hours.
    pub chunks_24h: u64,
    /// When something was last captured (RFC 3339).
    pub last_capture_at: Option<String>,
    /// Sessions not yet marked ended.
    pub open_sessions: u64,
    /// `off` or `jev` (`distill.provider`).
    pub distill_provider: String,
    /// `off`, `ready`, `waiting_judge`, `waiting_budget`, `breaker_open` or `no_key`.
    pub distill_state: String,
    /// Sessions waiting to be distilled.
    pub distill_due: u64,
    pub distilled_24h: u64,
    /// Proposals dropped in the last 24 hours (duplicates and uncertain ones).
    pub distill_dropped_24h: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distill_warning: Option<String>,
    /// Transcript lines of a shape the reader did not recognise in the last 7 days (a hint that
    /// an agent's format changed). Counted in memory: it starts from zero with the daemon.
    pub unrecognised_lines_7d: u64,
}

/// `GET /api/v1/capture/status`: the capture switch, distillation and the recent sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureStatus {
    /// The daemon runs as a server tenant (the setup script checks its token with this).
    pub tenant: bool,
    /// `capture.enabled`.
    pub enabled: bool,
    pub distill_provider: String,
    pub distill_state: String,
    /// The 50 sessions captured most recently, newest first.
    pub sessions: Vec<CaptureSessionRow>,
}

/// One captured session in [`CaptureStatus`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureSessionRow {
    /// `<agent>:session:<id>[:agent:<id>]`.
    pub uri: String,
    /// The slug of the session's project.
    pub project: String,
    /// Chunks stored for the session.
    pub chunks: u64,
    /// The cursor: the byte offset in the transcript where the next pass reads.
    pub byte_offset: u64,
    pub last_capture_at: Option<String>,
    pub ended: bool,
    pub distill_state: String,
    /// Uploads accepted from beyond the cursor (server tenants).
    pub gaps: i64,
}

/// The largest transcript upload (`POST /api/v1/capture/transcript`): one pass's worth.
pub const TRANSCRIPT_BODY_LIMIT: usize = 8 << 20;

/// The query of a transcript upload (SP2 spec §5.2). The body is the transcript's bytes from
/// `offset`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TranscriptQuery {
    /// `claude-code` or `codex`.
    pub agent: String,
    /// The session id.
    pub session: String,
    /// A subagent's id, for its own transcript.
    #[serde(default)]
    pub subagent: Option<String>,
    /// The byte offset of the body's first byte in the transcript.
    #[serde(default)]
    pub offset: u64,
    /// `session-start`, `stop`, `subagent-stop`, `pre-compact`, `session-end` or `sweep`.
    pub event: String,
    /// Server tenants: the project name (a repository's basename), slugified and resolved on
    /// the server as a direct MCP caller's hint is. Ignored by a local daemon.
    #[serde(default)]
    pub project: Option<String>,
    /// Server tenants: the `X-Alice-Project` header, which wins over `project`. Set by the
    /// route from the header; never read from the query string.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_header: Option<String>,
    /// Local daemons: the session's working directory, which decides its project. Ignored by a
    /// server tenant (it cannot see the client's paths).
    #[serde(default)]
    pub cwd: Option<String>,
    /// `1`: the session is captured but never distilled (the client's `.alice.toml`).
    #[serde(default)]
    pub no_distill: Option<u8>,
    /// `1`: nothing is captured; the answer only moves the client past the body.
    #[serde(default)]
    pub no_capture: Option<u8>,
}

/// The answer to an accepted transcript upload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TranscriptAnswer {
    /// Where the client's next upload starts: the start of the first incomplete turn, or the
    /// end of the last complete line.
    pub next_offset: u64,
    pub turns_stored: u64,
    /// Turns that were stored already, or not stored by the rules (an excluded project).
    pub turns_skipped: u64,
    pub chunks_stored: u64,
    /// The body is a whole pass without a line end: the client skips that line.
    pub oversized_line: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A re-embed into another embedding model in progress (rows done of rows to do).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReembedStatus {
    /// The model being filled.
    pub model: String,
    pub done: u64,
    pub total: u64,
}

/// What the API shows about a secret: whether it is set, a hint, and where it comes from. The
/// value itself is never part of an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretStatus {
    pub set: bool,
    /// The last four characters, e.g. `••••ab12`, for a value of 16 characters or more; `set`
    /// for a shorter one (four characters are too much of it).
    pub hint: Option<String>,
    /// `file`, `keychain` or `env`.
    pub source: Option<String>,
}

impl SecretStatus {
    /// A secret that is not set.
    pub fn unset() -> Self {
        Self {
            set: false,
            hint: None,
            source: None,
        }
    }
}

/// What a settings change did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchOutcome {
    /// Keys whose effective value changed.
    pub changed: Vec<String>,
    /// Keys written that apply only after a daemon restart.
    pub restart_required: Vec<String>,
    /// Keys written to config.toml but overridden by an environment variable.
    pub overridden_by_env: Vec<String>,
}

/// Every setting with its source, the problems with them, and the secrets' status: what the
/// web UI and `alice config list` show (`GET /api/v1/settings`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SettingsView {
    /// The settings in effect, as nested JSON (the shape of `config.toml`).
    pub settings: serde_json::Value,
    /// Where each leaf key's value comes from.
    pub sources: BTreeMap<String, Source>,
    /// Every key with its environment variable and whether it is read-only or needs a restart.
    pub keys: Vec<KeyInfo>,
    /// Why a setting is not in effect (an invalid edit of `config.toml`, a bad variable). The
    /// previous settings stay in force.
    pub errors: Vec<SettingError>,
    /// Ignored `ALICE_*` variables.
    pub warnings: Vec<String>,
    /// No valid `config.toml` has loaded since the daemon started (see `Health`).
    #[serde(default)]
    pub config_degraded: bool,
    /// Things worth knowing about the current state that are not errors: a change waiting to
    /// hold still, secrets that did not move with a backend, a keychain that did not answer.
    #[serde(default)]
    pub notes: Vec<String>,
    /// Each secret (`jev_api_key`, `embedding_api_key`): set or not, never the value.
    pub secrets: BTreeMap<String, SecretStatus>,
    /// Keys whose configured value differs from the one the running daemon started with.
    pub restart_pending: Vec<String>,
}

/// The answer to `PATCH /api/v1/settings`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PatchResponse {
    pub changed: Vec<String>,
    pub restart_required: Vec<String>,
    pub overridden_by_env: Vec<String>,
    /// What the change did that is worth knowing: a protective setting that was loosened (a
    /// change through the API applies at once, where the same edit of config.toml waits for
    /// three polls), keys saved but not used yet.
    #[serde(default)]
    pub notes: Vec<String>,
    /// The settings after the change.
    pub view: SettingsView,
}

/// Body of `PUT /api/v1/secrets/{name}`.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretRequest {
    pub value: String,
}

/// The value is a secret: it is never printed.
impl std::fmt::Debug for SecretRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretRequest { value: <hidden> }")
    }
}

/// The answer to `POST /api/v1/settings/test/{provider}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderTest {
    pub ok: bool,
    pub message: String,
    pub latency_ms: u64,
}

/// Facts about an entity: current, as of a date, or the full timeline.
pub type FactsRequest = FactsQuery;
/// Add, supersede or end a fact.
pub type AssertFactRequest = AssertFact;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FactsResponse {
    pub entity: String,
    pub facts: Vec<Fact>,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memories::UpdateAction;

    #[test]
    fn remember_request_schema_keeps_its_description() {
        let v = serde_json::to_value(schemars::schema_for!(RememberRequest)).unwrap();
        let d = v["description"].as_str().unwrap_or("");
        assert!(d.contains("Save a memory"), "description: {d:?}");
    }

    fn cp(key: &str, slug: &str) -> ClientProject {
        ClientProject {
            key: key.into(),
            slug: slug.into(),
            name: slug.into(),
            remote: None,
            rules: ClientRules::None,
        }
    }

    #[test]
    fn client_project_round_trips_in_the_documented_shape() {
        let p = ClientProject {
            key: "github.com/icueth/alice".into(),
            slug: "alice".into(),
            name: "alice".into(),
            remote: Some("github.com/icueth/alice".into()),
            rules: ClientRules::File {
                text: "[privacy]\ncloud_denied = true\n".into(),
            },
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["rules"]["state"], "file");
        assert_eq!(v["rules"]["text"], "[privacy]\ncloud_denied = true\n");
        let back: ClientProject = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
        let none: ClientRules = serde_json::from_str(r#"{"state":"none"}"#).unwrap();
        assert_eq!(none, ClientRules::None);
        let bad: ClientRules =
            serde_json::from_str(r#"{"state":"unreadable","reason":"permission denied"}"#).unwrap();
        assert_eq!(
            bad,
            ClientRules::Unreadable {
                reason: "permission denied".into()
            }
        );
    }

    #[test]
    fn a_request_without_client_project_serialises_as_before() {
        let r = RememberRequest {
            content: "x".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("client_project").is_none());
        let r: RecallRequest = serde_json::from_str(r#"{"query":"q"}"#).unwrap();
        assert!(r.client_project.is_none());
    }

    #[test]
    fn client_project_validation_refuses_what_the_spec_refuses() {
        assert!(cp("github.com/a/b", "b").validate().is_ok());
        let long = "k".repeat(1025);
        for (case, p) in [
            ("empty key", cp("", "b")),
            ("global key", cp("global", "b")),
            ("long key", cp(&long, "b")),
            ("control char in key", cp("a\u{7}b", "b")),
            ("empty slug", cp("k", "")),
            ("slug not slugified", cp("k", "My Repo")),
        ] {
            let e = p.validate().unwrap_err().to_string();
            assert!(
                !e.contains("My Repo") && !e.contains(&long),
                "{case}: echoed: {e}"
            );
        }
        let mut p = cp("k", "b");
        p.name = "n".repeat(201);
        assert!(p.validate().is_err(), "long name");
        let slug100 = "s".repeat(100);
        assert!(cp("k", &slug100).validate().is_ok(), "a 100-character slug");
        let slug101 = "s".repeat(101);
        let e = cp("k", &slug101).validate().unwrap_err().to_string();
        assert!(!e.contains(&slug101), "echoed: {e}");
        let mut p = cp("k", "b");
        p.remote = Some("r".repeat(1025));
        assert!(p.validate().is_err(), "long remote");
        let mut p = cp("k", "b");
        p.rules = ClientRules::File {
            text: "#".repeat(CLIENT_RULES_MAX_BYTES + 1),
        };
        assert!(p.validate().is_err(), "large rules");
        let mut p = cp("k", "b");
        p.rules = ClientRules::Unreadable {
            reason: "r".repeat(501),
        };
        assert!(p.validate().is_err(), "long reason");
    }

    #[test]
    fn requests_need_only_their_required_fields() {
        let r: RecallRequest = serde_json::from_str(r#"{"query":"lease"}"#).unwrap();
        assert_eq!(r.query, "lease");
        assert_eq!((r.mode, r.format), (None, OutputFormat::Text));
        let old: RecallRequest =
            serde_json::from_str(r#"{"query":"lease","mode":"fast"}"#).unwrap();
        assert_eq!(
            old.mode,
            Some(RecallMode::Fast),
            "an old client's explicit mode parses"
        );
        let p: RecallRequest =
            serde_json::from_str(r#"{"query":"lease","mode":"precise"}"#).unwrap();
        assert_eq!(p.mode, Some(RecallMode::Precise));
        assert!(!r.include_raw && r.limit.is_none() && r.cwd.is_none());
        let m: RememberRequest = serde_json::from_str(r#"{"content":"x"}"#).unwrap();
        assert_eq!(m.author, Author::default());
        let u: UpdateRequest =
            serde_json::from_str(r#"{"id":"01J","action":"archive","title":"t"}"#).unwrap();
        assert_eq!(
            (u.id.as_str(), u.update.action),
            ("01J", UpdateAction::Archive)
        );
        assert_eq!(u.update.title.as_deref(), Some("t"));
    }

    #[test]
    fn schemas_hide_daemon_filled_fields() {
        for (name, schema) in [
            ("recall", schemars::schema_for!(RecallRequest)),
            ("remember", schemars::schema_for!(RememberRequest)),
            ("context", schemars::schema_for!(ContextRequest)),
            ("ingest", schemars::schema_for!(IngestRequest)),
        ] {
            let props = schema.as_value()["properties"].clone();
            assert!(props.is_object(), "{name}");
            assert!(props.get("cwd").is_none(), "{name} exposes cwd");
            assert!(props.get("author").is_none(), "{name} exposes author");
        }
        let update = schemars::schema_for!(UpdateRequest);
        let props = &update.as_value()["properties"];
        assert!(props.get("id").is_some() && props.get("action").is_some());
        // Who changed something is the daemon's to fill (the MCP client's name, or the CLI).
        for (name, schema) in [
            ("update", update.clone()),
            ("assert_fact", schemars::schema_for!(AssertFactRequest)),
            ("undo", schemars::schema_for!(UndoRequest)),
        ] {
            let props = schema.as_value()["properties"].clone();
            assert!(props.is_object(), "{name}");
            assert!(props.get("author").is_none(), "{name} exposes author");
        }
        let u: UpdateRequest =
            serde_json::from_str(r#"{"id":"01J","action":"archive","author":{"agent":"codex"}}"#)
                .unwrap();
        assert_eq!(u.author.agent.as_deref(), Some("codex"));
        let f: AssertFactRequest =
            serde_json::from_str(r#"{"subject":"a","predicate":"uses","object":"b"}"#).unwrap();
        assert_eq!(f.author, Author::default());
        let undo: UndoRequest = serde_json::from_str(r#"{"event_id":"01E"}"#).unwrap();
        assert_eq!((undo.event_id.as_str(), undo.author.agent), ("01E", None));
    }

    /// Only `update` answers carry the event id (`undo` takes it); elsewhere it is left out.
    #[test]
    fn memory_full_omits_an_absent_event_id() {
        let m: Memory = serde_json::from_value(serde_json::json!({
            "id": "M", "project": "global", "kind": "fact", "title": "t", "body": "b",
            "topics": [], "importance": 3, "status": "active", "author": {},
            "created_at": "2026-10-14T00:00:00.000Z", "updated_at": "2026-10-14T00:00:00.000Z",
            "access_count": 0
        }))
        .unwrap();
        let mut full = MemoryFull {
            memory: m,
            links: Vec::new(),
            provenance: Vec::new(),
            event_id: None,
            source: None,
            author_name: None,
        };
        assert!(
            serde_json::to_value(&full)
                .unwrap()
                .get("event_id")
                .is_none()
        );
        full.event_id = Some("E".into());
        assert_eq!(serde_json::to_value(&full).unwrap()["event_id"], "E");
    }

    #[test]
    fn stream_items_are_tagged_by_type() {
        let event = EventRow {
            id: "E".into(),
            ts: "2026-10-14T00:00:00.000Z".into(),
            hlc: "h".into(),
            actor: "user".into(),
            action: "memory.edit".into(),
            target_type: "memory".into(),
            target_id: "M".into(),
            before: None,
            after: Some(serde_json::json!({"body": "b"})),
            group_id: None,
        };
        let v = serde_json::to_value(StreamItem::Event { event }).unwrap();
        assert_eq!(
            (v["type"].as_str(), v["event"]["target_id"].as_str()),
            (Some("event"), Some("M"))
        );
        let v = serde_json::to_value(StreamItem::Review {
            id: "R".into(),
            review_type: "undo".into(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "review", "id": "R", "review_type": "undo"})
        );
        let v = serde_json::to_value(StreamItem::Settings {
            changed: vec!["judge.provider".into()],
        })
        .unwrap();
        assert_eq!(v["type"], "settings");
        let v = serde_json::to_value(StreamItem::Lagged { missed: 3 }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "lagged", "missed": 3}));
        let q: EventsRequest = serde_json::from_str(r#"{"target_id":"M","limit":5}"#).unwrap();
        assert_eq!(
            (q.target_id.as_deref(), q.limit, q.actor),
            (Some("M"), Some(5), None)
        );
    }

    #[test]
    fn enums_use_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&MatchVia::Vector).unwrap(),
            "\"vector\""
        );
        assert_eq!(serde_json::to_string(&Tier::Raw).unwrap(), "\"raw\"");
        assert_eq!(
            serde_json::to_string(&RememberStatus::Spooled).unwrap(),
            "\"spooled\""
        );
        assert_eq!(
            serde_json::to_string(&RecallMode::Precise).unwrap(),
            "\"precise\""
        );
        let hit = Hit {
            id: "A".into(),
            tier: Tier::Knowledge,
            kind: Some(Kind::Fact),
            project: "app".into(),
            title: "t".into(),
            snippet: "s".into(),
            body: None,
            created_at: "2026-10-13T00:00:00.000Z".into(),
            score: 0.5,
            relevance: None,
            matched: vec![MatchVia::Words],
            status: None,
            source: None,
            author: None,
            cloud_ok: None,
        };
        let v = serde_json::to_value(&hit).unwrap();
        assert!(v.get("body").is_none() && v.get("status").is_none());
    }
}
