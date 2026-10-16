//! Domain types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod limits {
    pub const MAX_CONTENT_BYTES: usize = 65_536;
    pub const MAX_TITLE_CHARS: usize = 200;
    /// The longest project slug. GitHub caps a repository name at 100 characters, so a longer
    /// slug cannot be a detected project.
    pub const MAX_PROJECT_SLUG_CHARS: usize = 100;
    /// The longest project name, as `ClientProject.name` allows.
    pub const MAX_PROJECT_NAME_CHARS: usize = 200;
    pub const MAX_TOPICS: usize = 20;
    pub const MAX_TOPIC_CHARS: usize = 64;
    pub const MAX_CHUNK_BYTES: usize = 32_768;
    pub const MAX_CHUNKS_PER_REQUEST: usize = 1_000;
    pub const DEFAULT_RECALL_LIMIT: usize = 10;
    pub const MAX_RECALL_LIMIT: usize = 50;
    pub const DEFAULT_RECALL_TOKENS: usize = 1_500;
    pub const DEFAULT_CONTEXT_TOKENS: usize = 1_200;
}

/// What a memory is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Fact,
    Decision,
    Bugfix,
    Howto,
    Preference,
    Warning,
    Status,
    Note,
    Summary,
    Diary,
    Handoff,
}

impl Kind {
    pub const ALL: [Kind; 11] = [
        Kind::Fact,
        Kind::Decision,
        Kind::Bugfix,
        Kind::Howto,
        Kind::Preference,
        Kind::Warning,
        Kind::Status,
        Kind::Note,
        Kind::Summary,
        Kind::Diary,
        Kind::Handoff,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Fact => "fact",
            Kind::Decision => "decision",
            Kind::Bugfix => "bugfix",
            Kind::Howto => "howto",
            Kind::Preference => "preference",
            Kind::Warning => "warning",
            Kind::Status => "status",
            Kind::Note => "note",
            Kind::Summary => "summary",
            Kind::Diary => "diary",
            Kind::Handoff => "handoff",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        let wanted = s.trim().to_ascii_lowercase();
        Kind::ALL.into_iter().find(|k| k.as_str() == wanted)
    }

    /// Recency half-life used by ranking; `None` means the kind does not decay. Only the
    /// short-lived kinds decay: durable knowledge keeps its rank however old it is, because
    /// what stopped being true is handled by superseding it or by its status, not by its age
    /// (an imported palace is uniformly old). Task 19 changed `note` from 90 days and `fact`,
    /// `howto`, `bugfix` and `summary` from 180 days to no decay; `search.half_life.*` sets any
    /// of them again.
    pub fn half_life_days(self) -> Option<f64> {
        match self {
            Kind::Status => Some(7.0),
            Kind::Handoff => Some(14.0),
            Kind::Diary => Some(30.0),
            Kind::Note
            | Kind::Fact
            | Kind::Howto
            | Kind::Bugfix
            | Kind::Summary
            | Kind::Decision
            | Kind::Preference
            | Kind::Warning => None,
        }
    }
}

/// Half-life applied to raw chunks.
pub const RAW_HALF_LIFE_DAYS: f64 = 90.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Active,
    Superseded,
    Archived,
    Trashed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Active => "active",
            Status::Superseded => "superseded",
            Status::Archived => "archived",
            Status::Trashed => "trashed",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        [
            Status::Pending,
            Status::Active,
            Status::Superseded,
            Status::Archived,
            Status::Trashed,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Session,
    File,
    Import,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceType::Session => "session",
            SourceType::File => "file",
            SourceType::Import => "import",
        }
    }
}

/// Who wrote a memory. Metadata only; never a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Author {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// A stored knowledge memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Memory {
    pub id: String,
    pub project: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub topics: Vec<String>,
    pub importance: u8,
    pub status: Status,
    pub author: Author,
    pub created_at: String,
    pub updated_at: String,
    pub access_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serde_name<T: Serialize>(v: T) -> String {
        serde_json::to_value(v)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn kind_strings_round_trip_and_match_serde() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
            assert_eq!(serde_name(k), k.as_str());
        }
        assert_eq!(Kind::parse(" Bugfix "), Some(Kind::Bugfix));
        assert_eq!(Kind::parse("nope"), None);
    }

    #[test]
    fn status_and_source_strings_match_serde() {
        for s in [
            Status::Pending,
            Status::Active,
            Status::Superseded,
            Status::Archived,
            Status::Trashed,
        ] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
            assert_eq!(serde_name(s), s.as_str());
        }
        assert_eq!(Status::parse("deleted"), None);
        for t in [SourceType::Session, SourceType::File, SourceType::Import] {
            assert_eq!(serde_name(t), t.as_str());
        }
    }

    #[test]
    fn durable_kinds_do_not_decay() {
        assert_eq!(Kind::Status.half_life_days(), Some(7.0));
        assert_eq!(Kind::Handoff.half_life_days(), Some(14.0));
        assert_eq!(Kind::Diary.half_life_days(), Some(30.0));
        // Task 19: durable knowledge does not age out; supersede and status handle truth over time.
        for k in [
            Kind::Note,
            Kind::Fact,
            Kind::Howto,
            Kind::Bugfix,
            Kind::Summary,
            Kind::Decision,
            Kind::Preference,
            Kind::Warning,
        ] {
            assert_eq!(k.half_life_days(), None, "{k:?}");
        }
    }
}
