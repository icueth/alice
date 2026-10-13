//! Request and response types shared by the HTTP API, MCP tools and the CLI.

use crate::kg_types::*;
use crate::model::{Author, Kind, Memory, Status};
use crate::store::memories::MemoryUpdate;
use crate::store::sources::{ChunkInput, SourceInput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecallMode {
    /// Hybrid search without the judge (default).
    #[default]
    Fast,
    /// Re-rank the top results with the judge when one is configured.
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
    #[serde(default)]
    pub mode: RecallMode,
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
    pub score: f64,
    pub matched: Vec<MatchVia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GetResponse {
    pub memories: Vec<MemoryFull>,
    pub missing: Vec<String>,
}

/// Edit, supersede, archive, forget (soft delete) or restore a memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UpdateRequest {
    pub id: String,
    #[serde(flatten)]
    pub update: MemoryUpdate,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<String>,
    /// False while the vector index is being rebuilt (search is lexical-only meanwhile).
    pub index_ready: bool,
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
    fn requests_need_only_their_required_fields() {
        let r: RecallRequest = serde_json::from_str(r#"{"query":"lease"}"#).unwrap();
        assert_eq!(r.query, "lease");
        assert_eq!((r.mode, r.format), (RecallMode::Fast, OutputFormat::Text));
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
            matched: vec![MatchVia::Words],
            status: None,
        };
        let v = serde_json::to_value(&hit).unwrap();
        assert!(v.get("body").is_none() && v.get("status").is_none());
    }
}
