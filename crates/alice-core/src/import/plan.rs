//! Mapping one MemPalace drawer to what Alice stores: classification, kinds, times, redaction
//! order and sizing. Pure: no database, no writer.
//!
//! Safety order inside `Mapper::map`: clean the text, redact the WHOLE text, only then split by
//! size, and re-check byte sizes after redaction (a marker can be longer than the secret).

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{FixedOffset, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone};

use super::palace::{Drawer, DrawerMeta};
use super::wings::{ProjectDecision, WingMap};
use crate::model::{Author, Kind, SourceType, limits};
use crate::normalize::{derive_title, nfc, normalize_topic, slugify};
use crate::store::memories::NewMemoryInput;
use crate::store::sources::{ChunkInput, SourceInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier {
    Knowledge,
    RawSession,
    RawFile,
    Skip(&'static str),
}

/// Knowledge text over this is split (the store's content limit).
const SPLIT_ABOVE_CONTENT: usize = limits::MAX_CONTENT_BYTES;
const CONTENT_PIECE: usize = 60_000;
const CHUNK_PIECE: usize = 32_000;
/// Offset between the ords of the pieces of one oversize chunk (piece i gets `ord + i * PIECE_ORD_STEP`).
const PIECE_ORD_STEP: i64 = 10_000_000;
const SPLIT_PREFER: [&str; 2] = ["\n\n", "\n"];

const CURATED_ROOMS: [&str; 8] = [
    "decisions",
    "knowledge",
    "status",
    "progress",
    "runbook",
    "methods",
    "bug-classes",
    "issues",
];

fn is_diary(id: &str, meta: &DrawerMeta) -> bool {
    slugify(&meta.room) == "diary"
        || meta.entry_type.as_deref() == Some("diary_entry")
        || id.starts_with("diary_")
}

pub fn classify(id: &str, meta: &DrawerMeta) -> Tier {
    if meta.unreadable {
        return Tier::Skip("unreadable metadata");
    }
    if meta.ingest_mode.as_deref() == Some("registry")
        || meta.room == "_registry"
        || id.starts_with("_reg_")
    {
        return Tier::Skip("MemPalace registry entry");
    }
    if meta.ingest_mode.as_deref() == Some("convos")
        || meta
            .source_file
            .as_deref()
            .is_some_and(|f| f.ends_with(".jsonl"))
        || slugify(&meta.wing) == "sessions"
    {
        return Tier::RawSession;
    }
    if is_diary(id, meta) || meta.source_file.is_none() {
        return Tier::Knowledge;
    }
    let curated = CURATED_ROOMS.contains(&slugify(&meta.room).as_str());
    let by_agent = meta
        .added_by
        .as_deref()
        .is_some_and(|a| !a.is_empty() && a != "mempalace");
    if curated && by_agent {
        Tier::Knowledge
    } else {
        Tier::RawFile
    }
}

pub fn kind_for_room(room: &str) -> Kind {
    match slugify(room).as_str() {
        "decisions" => Kind::Decision,
        "bug-classes" | "issues" | "bugs" => Kind::Bugfix,
        "methods" | "runbook" => Kind::Howto,
        "status" | "progress" => Kind::Status,
        "diary" => Kind::Diary,
        "knowledge" => Kind::Fact,
        _ => Kind::Note,
    }
}

fn rfc3339_millis(utc: chrono::DateTime<chrono::Utc>) -> String {
    utc.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// RFC 3339 (with `Z` or an offset) is taken as written; a naive time or a bare date is read in
/// `assume` (MemPalace writes naive local times). Output: UTC, milliseconds, `Z`.
pub fn parse_time(s: &str, assume: FixedOffset) -> Option<String> {
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(rfc3339_millis(dt.to_utc()));
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })?;
    let local = assume.from_local_datetime(&naive).single()?;
    Some(rfc3339_millis(local.to_utc()))
}

/// Splits `text` into parts of at most `max` bytes at the first of `prefer` that occurs in the
/// window (the separator stays with the left part), else at a char boundary. Nothing is lost.
pub fn split_utf8(text: &str, max: usize, prefer: &[&str]) -> Vec<String> {
    let max = max.max(4); // a UTF-8 char is at most 4 bytes
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= max {
            out.push(rest.to_string());
            break;
        }
        let mut end = max;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let window = &rest[..end];
        let cut = prefer
            .iter()
            .filter(|sep| !sep.is_empty())
            .find_map(|sep| window.rfind(sep).map(|i| i + sep.len()))
            .unwrap_or(end);
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    out
}

/// Redaction hook (the importer wraps `Redactor::check`; tests pass `NoFilter`). `Err` carries
/// detector names: the drawer is blocked.
pub trait TextFilter: Sync {
    fn filter(&self, field: &str, text: &str) -> Result<String, Vec<String>>;
    /// True when something was replaced (for the report).
    fn changed(&self, before: &str, after: &str) -> bool {
        before != after
    }
}

pub struct NoFilter;
impl TextFilter for NoFilter {
    fn filter(&self, _field: &str, text: &str) -> Result<String, Vec<String>> {
        Ok(text.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct PlannedMemory {
    pub legacy_id: String,
    pub project: ProjectDecision,
    pub input: NewMemoryInput,
}
#[derive(Debug, Clone)]
pub struct PlannedChunk {
    pub legacy_id: String,
    pub project: ProjectDecision,
    pub source: SourceInput,
    pub chunk: ChunkInput,
}
#[derive(Debug, Clone)]
pub enum Planned {
    /// All parts of one drawer; they are written together or not at all.
    Memories(Vec<PlannedMemory>),
    Chunks(Vec<PlannedChunk>),
    Skipped {
        reason: &'static str,
    },
    Blocked {
        detectors: Vec<String>,
    },
    Collapsed {
        keeper: String,
    },
}

/// Runs every emitted string through the filter and remembers which detectors blocked.
struct Guard<'a> {
    filter: &'a dyn TextFilter,
    blocked: BTreeSet<String>,
}

impl Guard<'_> {
    fn apply(&mut self, field: &str, text: &str) -> String {
        match self.filter.filter(field, text) {
            Ok(t) => t,
            Err(detectors) => {
                self.blocked.extend(detectors);
                String::new()
            }
        }
    }
    fn opt(&mut self, field: &str, text: Option<String>) -> Option<String> {
        text.map(|t| self.apply(field, &t))
    }
    fn is_blocked(&self) -> bool {
        !self.blocked.is_empty()
    }
    fn into_planned(self) -> Planned {
        Planned::Blocked {
            detectors: self.blocked.into_iter().collect(),
        }
    }
}

/// `text` in parts: whole when it fits `limit`, else pieces of at most `piece` bytes. Every part
/// is re-checked against `limit` afterwards and re-split when it is still too long.
fn size_parts(text: String, limit: usize, piece: usize) -> Vec<String> {
    let first = if text.len() <= limit {
        vec![text]
    } else {
        split_utf8(&text, piece, &SPLIT_PREFER)
    };
    first
        .into_iter()
        .flat_map(|p| {
            if p.len() <= limit {
                vec![p]
            } else {
                split_utf8(&p, piece, &SPLIT_PREFER)
            }
        })
        .collect()
}

fn part_title(base: Option<&str>, part_text: &str, k: usize, n: usize) -> String {
    let suffix = format!(" (part {k}/{n})");
    let base = base
        .map(str::to_string)
        .unwrap_or_else(|| derive_title(part_text));
    let room = limits::MAX_TITLE_CHARS.saturating_sub(suffix.chars().count());
    let cut: String = base.chars().take(room).collect();
    format!("{}{suffix}", cut.trim_end())
}

fn agent_from_path(path: &str) -> Option<&'static str> {
    if path.contains("/.claude/") {
        Some("claude-code")
    } else if path.contains("/.codex/") {
        Some("codex")
    } else {
        None
    }
}

pub struct Mapper<'a> {
    pub wings: &'a WingMap,
    pub assume: FixedOffset,
    pub filter: &'a dyn TextFilter,
}

impl Mapper<'_> {
    /// `ord` is the chunk position inside its source (Task 6 supplies it); ignored for knowledge.
    pub fn map(&self, d: &Drawer, ord: i64, span: (Option<String>, Option<String>)) -> Planned {
        let tier = classify(&d.id, &d.meta);
        if let Tier::Skip(reason) = tier {
            return Planned::Skipped { reason };
        }
        // (a) clean, (b) empty check
        let cleaned = nfc(&d.text.replace('\0', ""));
        if cleaned.trim().is_empty() {
            return Planned::Skipped {
                reason: "empty text",
            };
        }
        // (c) redact the whole text; nothing is split before this
        let mut g = Guard {
            filter: self.filter,
            blocked: BTreeSet::new(),
        };
        let field = if tier == Tier::Knowledge {
            "content"
        } else {
            "chunk"
        };
        let text = g.apply(field, &cleaned);
        if g.is_blocked() {
            return g.into_planned();
        }
        let project = self.wings.decide(&d.meta, &text);
        let out = if tier == Tier::Knowledge {
            self.knowledge(&mut g, d, text, project)
        } else {
            self.chunks(&mut g, d, &tier, text, project, ord, span)
        };
        if g.is_blocked() {
            g.into_planned()
        } else {
            out
        }
    }

    fn knowledge(
        &self,
        g: &mut Guard,
        d: &Drawer,
        text: String,
        project: ProjectDecision,
    ) -> Planned {
        let m = &d.meta;
        let diary = is_diary(&d.id, m);
        let (kind, importance) = if diary {
            (Kind::Diary, 2)
        } else {
            (kind_for_room(&m.room), 3)
        };
        let topic_source = if diary {
            m.topic.as_deref()
        } else {
            Some(&*m.room)
        };
        let topics: Vec<String> = topic_source
            .and_then(normalize_topic)
            .map(|t| g.apply("topic", &t))
            .into_iter()
            .collect();
        let agent = if diary {
            m.agent.clone()
        } else {
            m.added_by.clone().filter(|a| a != "mempalace")
        }
        .or_else(|| project.agent.clone());
        let agent = g.opt("author.agent", agent);
        let base_title = match (diary, &m.date, &m.topic) {
            (true, Some(date), Some(topic)) => {
                let t = format!("Diary {date}: {topic}");
                let t: String = nfc(&t).chars().take(limits::MAX_TITLE_CHARS).collect();
                Some(g.apply("title", &t))
            }
            _ => None,
        };
        let created_at = m
            .filed_at
            .as_deref()
            .and_then(|t| parse_time(t, self.assume))
            .or_else(|| m.date.as_deref().and_then(|t| parse_time(t, self.assume)));
        // (d) split only now; (e) sizes re-checked inside size_parts
        let parts = size_parts(text, SPLIT_ABOVE_CONTENT, CONTENT_PIECE);
        let n = parts.len();
        let planned = parts
            .into_iter()
            .enumerate()
            .map(|(i, content)| {
                let legacy_id = if i == 0 {
                    d.id.clone()
                } else {
                    format!("{}#{}", d.id, i + 1)
                };
                let title = if i == 0 {
                    base_title.clone()
                } else {
                    Some(part_title(base_title.as_deref(), &content, i + 1, n))
                };
                PlannedMemory {
                    legacy_id: legacy_id.clone(),
                    project: project.clone(),
                    input: NewMemoryInput {
                        content,
                        kind: Some(kind),
                        title,
                        topics: topics.clone(),
                        importance: Some(importance),
                        author: Author {
                            agent: agent.clone(),
                            ..Default::default()
                        },
                        legacy_id: Some(legacy_id),
                        created_at: created_at.clone(),
                        ..Default::default()
                    },
                }
            })
            .collect();
        Planned::Memories(planned)
    }

    #[allow(clippy::too_many_arguments)]
    fn chunks(
        &self,
        g: &mut Guard,
        d: &Drawer,
        tier: &Tier,
        text: String,
        project: ProjectDecision,
        ord: i64,
        span: (Option<String>, Option<String>),
    ) -> Planned {
        let m = &d.meta;
        let session = *tier == Tier::RawSession;
        let uri = match m.source_file.as_deref() {
            Some(f) if f.starts_with('/') => format!("file://{f}"),
            Some(f) => f.to_string(),
            None => format!("mempalace-wing:{}", m.wing),
        };
        let uri = g.apply("source.uri", &uri);
        let path = m.source_file.as_deref().unwrap_or("");
        let agent = g.opt("source.agent", agent_from_path(path).map(str::to_string));
        let session_id = if session {
            m.source_file
                .as_deref()
                .and_then(|f| Path::new(f).file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        } else {
            None
        };
        let session_id = g.opt("source.session_id", session_id);
        let source = SourceInput {
            source_type: if session {
                SourceType::Session
            } else {
                SourceType::File
            },
            uri,
            agent,
            session_id,
            started_at: span.0,
            ended_at: span.1,
            ..Default::default()
        };
        let stamp = if session { &m.authored_at } else { &m.filed_at };
        let authored_at = stamp.as_deref().and_then(|t| parse_time(t, self.assume));
        let pieces = size_parts(text, limits::MAX_CHUNK_BYTES, CHUNK_PIECE);
        Planned::Chunks(
            pieces
                .into_iter()
                .enumerate()
                .map(|(i, text)| {
                    let legacy_id = if i == 0 {
                        d.id.clone()
                    } else {
                        format!("{}#{}", d.id, i + 1)
                    };
                    PlannedChunk {
                        legacy_id: legacy_id.clone(),
                        project: project.clone(),
                        source: source.clone(),
                        chunk: ChunkInput {
                            ord: ord + i as i64 * PIECE_ORD_STEP,
                            text,
                            authored_at: authored_at.clone(),
                            legacy_id: Some(legacy_id),
                        },
                    }
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::FixedOffset;

    use super::*;
    use crate::import::palace::{Drawer, DrawerMeta};
    use crate::import::wings::WingMap;
    use crate::model::{Kind, SourceType};

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }
    fn plus7() -> FixedOffset {
        FixedOffset::east_opt(7 * 3600).unwrap()
    }
    fn wings() -> WingMap {
        WingMap::build(
            &[
                ("sessions".to_string(), 10),
                ("alice".to_string(), 5),
                ("wing_claude_code".to_string(), 3),
            ],
            &BTreeMap::new(),
        )
    }
    fn d(id: &str, text: &str, meta: DrawerMeta) -> Drawer {
        Drawer {
            id: id.to_string(),
            text: text.to_string(),
            meta,
        }
    }
    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }
    fn raw_meta() -> DrawerMeta {
        DrawerMeta {
            wing: "sessions".into(),
            room: "technical".into(),
            source_file: s("/Users/x/.claude/projects/-Users-x-alice/abc.jsonl"),
            ingest_mode: s("convos"),
            authored_at: s("2026-04-16T11:34:59.427116"),
            ..Default::default()
        }
    }
    fn raw_drawer(id: &str, text: &str) -> Drawer {
        d(id, text, raw_meta())
    }
    fn knowledge_meta(wing: &str, room: &str) -> DrawerMeta {
        DrawerMeta {
            wing: wing.into(),
            room: room.into(),
            added_by: s("claude-code"),
            ..Default::default()
        }
    }

    /// Replaces a GitHub-shaped token by a marker; blocks any text containing "blockme".
    #[derive(Default)]
    struct RedactingFilter;
    impl TextFilter for RedactingFilter {
        fn filter(&self, _field: &str, text: &str) -> Result<String, Vec<String>> {
            if text.to_lowercase().contains("blockme") {
                return Err(vec!["blockme".to_string()]);
            }
            let prefix = concat!("gh", "p_");
            let mut out = String::new();
            let mut rest = text;
            while let Some(i) = rest.find(prefix) {
                out.push_str(&rest[..i]);
                let tail = &rest[i + prefix.len()..];
                let n = tail
                    .find(|c: char| !c.is_ascii_alphanumeric())
                    .unwrap_or(tail.len());
                out.push_str("[REDACTED:github_token]");
                rest = &tail[n..];
            }
            out.push_str(rest);
            Ok(out)
        }
    }
    /// Turns every "XX" into a 40-byte marker.
    struct MarkerFilter;
    impl TextFilter for MarkerFilter {
        fn filter(&self, _f: &str, text: &str) -> Result<String, Vec<String>> {
            Ok(text.replace("XX", &"M".repeat(40)))
        }
    }

    #[test]
    fn classification_follows_the_real_palace_shapes() {
        let convos = DrawerMeta {
            wing: "sessions".into(),
            room: "technical".into(),
            source_file: s("/h/.claude/projects/x/s.jsonl"),
            ingest_mode: s("convos"),
            ..Default::default()
        };
        assert_eq!(classify("a", &convos), Tier::RawSession);
        let diary = DrawerMeta {
            wing: "wing_claude_code".into(),
            room: "diary".into(),
            ..Default::default()
        };
        assert_eq!(
            classify("diary_wing_claude_code_1_ab", &diary),
            Tier::Knowledge
        );
        let agent_note = DrawerMeta {
            wing: "wing_claude_code".into(),
            room: "technical".into(),
            added_by: s("claude-code"),
            ..Default::default()
        };
        assert_eq!(classify("n", &agent_note), Tier::Knowledge);
        let code_chunk = DrawerMeta {
            wing: "alice".into(),
            room: "technical".into(),
            source_file: s("/h/alice/src/main.rs"),
            added_by: s("claude-code"),
            ..Default::default()
        };
        assert_eq!(
            classify("c", &code_chunk),
            Tier::RawFile,
            "added_by alone is not knowledge"
        );
        let curated = DrawerMeta {
            room: "decisions".into(),
            ..code_chunk.clone()
        };
        assert_eq!(classify("k", &curated), Tier::Knowledge);
        let mined_curated = DrawerMeta {
            added_by: s("mempalace"),
            ..curated.clone()
        };
        assert_eq!(classify("k2", &mined_curated), Tier::RawFile);
        let registry = DrawerMeta {
            wing: "alice".into(),
            room: "technical".into(),
            ingest_mode: s("registry"),
            ..Default::default()
        };
        assert_eq!(
            classify("r", &registry),
            Tier::Skip("MemPalace registry entry")
        );
        assert_eq!(
            classify("_reg_x", &DrawerMeta::default()),
            Tier::Skip("MemPalace registry entry")
        );
        let broken = DrawerMeta {
            unreadable: true,
            ..convos
        };
        assert_eq!(classify("u", &broken), Tier::Skip("unreadable metadata"));
    }

    #[test]
    fn rooms_become_kinds_and_topics() {
        assert_eq!(kind_for_room("decisions"), Kind::Decision);
        assert_eq!(kind_for_room("Bug_Classes"), Kind::Bugfix);
        assert_eq!(kind_for_room("runbook"), Kind::Howto);
        assert_eq!(kind_for_room("progress"), Kind::Status);
        assert_eq!(kind_for_room("architecture"), Kind::Note);
        let f = NoFilter;
        let m = Mapper {
            wings: &wings(),
            assume: utc(),
            filter: &f,
        };
        let Planned::Memories(p) = m.map(
            &d("k", "Chose SQLite.", knowledge_meta("alice", "Bug_Classes")),
            0,
            (None, None),
        ) else {
            panic!("expected memories")
        };
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].input.kind, Some(Kind::Bugfix));
        assert_eq!(p[0].input.topics, vec!["bug-classes".to_string()]);
        assert_eq!(p[0].input.importance, Some(3));
        assert_eq!(p[0].input.title, None);
        assert_eq!(p[0].input.author.agent.as_deref(), Some("claude-code"));
        assert_eq!(p[0].project.slug, "alice");
    }

    #[test]
    fn a_diary_drawer_becomes_a_diary_memory_with_its_agent_topic_and_date() {
        let meta = DrawerMeta {
            wing: "wing_claude_code".into(),
            room: "diary".into(),
            agent: s("claude-code"),
            topic: s("system-maintenance"),
            date: s("2026-04-16"),
            filed_at: s("2026-04-16T11:34:59.427116"),
            ..Default::default()
        };
        let id = "diary_wing_claude_code_20260416_ab12";
        let f = NoFilter;
        let m = Mapper {
            wings: &wings(),
            assume: plus7(),
            filter: &f,
        };
        let Planned::Memories(p) = m.map(&d(id, "Cleaned up the cache.", meta), 0, (None, None))
        else {
            panic!("expected memories")
        };
        assert_eq!(p.len(), 1);
        let i = &p[0].input;
        assert_eq!(i.kind, Some(Kind::Diary));
        assert_eq!(
            i.title.as_deref(),
            Some("Diary 2026-04-16: system-maintenance")
        );
        assert_eq!(i.topics, vec!["system-maintenance".to_string()]);
        assert_eq!(i.importance, Some(2));
        assert_eq!(i.author.agent.as_deref(), Some("claude-code"));
        assert_eq!(i.created_at.as_deref(), Some("2026-04-16T04:34:59.427Z"));
        assert_eq!(i.legacy_id.as_deref(), Some(id));
        assert_eq!(p[0].legacy_id, id);
    }

    #[test]
    fn naive_and_zoned_times_parse_and_junk_is_none() {
        let o = plus7();
        assert_eq!(
            parse_time("2026-04-16T11:34:59.427116", o).as_deref(),
            Some("2026-04-16T04:34:59.427Z")
        );
        assert_eq!(
            parse_time("2026-04-16T11:34:59", o).as_deref(),
            Some("2026-04-16T04:34:59.000Z")
        );
        assert_eq!(
            parse_time("2026-04-16T11:34:59.5Z", o).as_deref(),
            Some("2026-04-16T11:34:59.500Z")
        );
        assert_eq!(
            parse_time("2026-04-16T11:34:59+02:00", o).as_deref(),
            Some("2026-04-16T09:34:59.000Z")
        );
        assert_eq!(
            parse_time("2026-04-16", o).as_deref(),
            Some("2026-04-15T17:00:00.000Z")
        );
        assert_eq!(parse_time("yesterday", o), None);
        assert_eq!(parse_time("", o), None);
    }

    #[test]
    fn a_secret_straddling_the_split_point_is_redacted_before_splitting() {
        let secret = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let text = format!("{}{}{}", "a ".repeat(15_995), secret, " b".repeat(1_000));
        // The fixture really straddles: splitting the raw text first would cut the token in two.
        let naive = split_utf8(&text, 32_000, &["\n\n", "\n"]);
        assert!(naive[0].contains(concat!("gh", "p_")) && !naive[0].contains(secret));
        assert!(!naive[1].contains(concat!("gh", "p_")) && naive[1].contains("wxyz0123"));

        let f = RedactingFilter;
        let m = Mapper {
            wings: &wings(),
            assume: utc(),
            filter: &f,
        };
        let Planned::Chunks(parts) = m.map(&raw_drawer("d1", &text), 0, (None, None)) else {
            panic!("expected chunks")
        };
        assert!(parts.len() >= 2);
        let all: String = parts
            .iter()
            .map(|p| p.chunk.text.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert!(!all.contains(concat!("gh", "p_")) && all.contains("[REDACTED:github_token]"));
        for p in &parts {
            assert!(!p.chunk.text.contains("abcdefgh"), "a secret half survived");
            assert!(!p.chunk.text.contains("wxyz0123"), "a secret half survived");
            assert!(p.chunk.text.len() <= 32_768);
        }
    }

    #[test]
    fn a_redaction_marker_longer_than_the_secret_cannot_push_a_part_over_the_limit() {
        let text = "XX".repeat(16_000); // 32,000 bytes: fits before redaction
        assert_eq!(text.len(), 32_000);
        let f = MarkerFilter;
        let m = Mapper {
            wings: &wings(),
            assume: utc(),
            filter: &f,
        };
        let Planned::Chunks(parts) = m.map(&raw_drawer("d2", &text), 5, (None, None)) else {
            panic!("expected chunks")
        };
        assert!(
            parts.len() > 1,
            "the redacted text is longer than one chunk"
        );
        assert!(parts.iter().all(|p| p.chunk.text.len() <= 32_768));
        let all: String = parts.iter().map(|p| p.chunk.text.as_str()).collect();
        assert_eq!(all, "M".repeat(40 * 16_000));
        // Knowledge text grows the same way.
        let k = "XX".repeat(30_000); // 60,000 bytes -> 1.2 MB redacted
        let Planned::Memories(mem) = m.map(
            &d("k", &k, knowledge_meta("alice", "decisions")),
            0,
            (None, None),
        ) else {
            panic!("expected memories")
        };
        assert!(mem.len() > 1);
        assert!(mem.iter().all(|p| p.input.content.len() <= 65_536));
    }

    #[test]
    fn fields_other_than_the_text_are_filtered_too() {
        let f = RedactingFilter;
        let m = Mapper {
            wings: &wings(),
            assume: utc(),
            filter: &f,
        };
        let path = DrawerMeta {
            source_file: s("/h/BLOCKME/s.jsonl"),
            ..raw_meta()
        };
        assert!(matches!(
            m.map(&d("p", "fine text", path), 0, (None, None)),
            Planned::Blocked { detectors } if detectors == vec!["blockme".to_string()]
        ));
        let room = DrawerMeta {
            wing: "alice".into(),
            room: "BLOCKME".into(),
            ..Default::default()
        };
        assert!(matches!(
            m.map(&d("r", "fine text", room), 0, (None, None)),
            Planned::Blocked { .. }
        ));
        let added = DrawerMeta {
            added_by: s("BLOCKME"),
            ..knowledge_meta("alice", "notes")
        };
        assert!(matches!(
            m.map(&d("a", "fine text", added), 0, (None, None)),
            Planned::Blocked { .. }
        ));
        // The text itself blocks too, and a clean drawer passes.
        assert!(matches!(
            m.map(&raw_drawer("t", "please BLOCKME"), 0, (None, None)),
            Planned::Blocked { .. }
        ));
        assert!(matches!(
            m.map(&raw_drawer("ok", "fine"), 0, (None, None)),
            Planned::Chunks(_)
        ));
    }

    #[test]
    fn oversize_empty_nul_and_decomposed_thai_are_handled_never_an_error() {
        let f = NoFilter;
        let m = Mapper {
            wings: &wings(),
            assume: utc(),
            filter: &f,
        };
        let big = format!("{}\n\n", "x".repeat(999)).repeat(130); // ~130 KB in paragraphs
        assert!(big.len() > 130_000);
        let Planned::Memories(p) = m.map(
            &d("big", &big, knowledge_meta("alice", "decisions")),
            0,
            (None, None),
        ) else {
            panic!("expected memories")
        };
        assert_eq!(p.len(), 3);
        let ids: Vec<&str> = p.iter().map(|x| x.legacy_id.as_str()).collect();
        assert_eq!(ids, vec!["big", "big#2", "big#3"]);
        assert!(p.iter().all(|x| x.input.content.len() <= 60_000));
        assert_eq!(p[1].input.legacy_id.as_deref(), Some("big#2"));
        assert!(p[1].input.title.as_deref().unwrap().ends_with("(part 2/3)"));
        assert!(p[2].input.title.as_deref().unwrap().ends_with("(part 3/3)"));
        let joined: String = p.iter().map(|x| x.input.content.as_str()).collect();
        assert_eq!(joined, big);

        let forty = "y".repeat(40_000);
        let Planned::Chunks(c) = m.map(&raw_drawer("c", &forty), 7, (None, None)) else {
            panic!("expected chunks")
        };
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].chunk.ord, c[1].chunk.ord), (7, 7 + 10_000_000));
        assert_eq!(c[0].chunk.legacy_id.as_deref(), Some("c"));
        assert_eq!(c[1].chunk.legacy_id.as_deref(), Some("c#2"));
        assert!(c.iter().all(|x| x.chunk.text.len() <= 32_000));

        assert!(matches!(
            m.map(&raw_drawer("e", "   \n\t"), 0, (None, None)),
            Planned::Skipped {
                reason: "empty text"
            }
        ));
        assert!(matches!(
            m.map(&raw_drawer("e2", "\0\0 \n"), 0, (None, None)),
            Planned::Skipped {
                reason: "empty text"
            }
        ));
        let Planned::Memories(n) = m.map(
            &d("n", "a\0b", knowledge_meta("alice", "notes")),
            0,
            (None, None),
        ) else {
            panic!("expected memories")
        };
        assert_eq!(n[0].input.content, "ab");
        let Planned::Memories(t) = m.map(
            &d("t", "น\u{0e4d}\u{0e32}", knowledge_meta("alice", "notes")),
            0,
            (None, None),
        ) else {
            panic!("expected memories")
        };
        assert_eq!(t[0].input.content, "นำ");
    }

    #[test]
    fn raw_chunks_carry_source_agent_session_and_times() {
        let f = NoFilter;
        let m = Mapper {
            wings: &wings(),
            assume: plus7(),
            filter: &f,
        };
        let span = (s("2026-04-16T00:00:00.000Z"), s("2026-04-16T01:00:00.000Z"));
        let Planned::Chunks(c) = m.map(&raw_drawer("r1", "hello"), 4, span) else {
            panic!("expected chunks")
        };
        let p = &c[0];
        assert_eq!(p.source.source_type, SourceType::Session);
        assert_eq!(
            p.source.uri,
            "file:///Users/x/.claude/projects/-Users-x-alice/abc.jsonl"
        );
        assert_eq!(p.source.agent.as_deref(), Some("claude-code"));
        assert_eq!(p.source.session_id.as_deref(), Some("abc"));
        assert_eq!(
            p.source.started_at.as_deref(),
            Some("2026-04-16T00:00:00.000Z")
        );
        assert_eq!(p.chunk.ord, 4);
        assert_eq!(
            p.chunk.authored_at.as_deref(),
            Some("2026-04-16T04:34:59.427Z")
        );
        assert_eq!(p.project.slug, "alice");

        let file = DrawerMeta {
            wing: "alice".into(),
            room: "technical".into(),
            source_file: s("relative/main.rs"),
            filed_at: s("2026-04-16T11:34:59"),
            ..Default::default()
        };
        let Planned::Chunks(c) = m.map(&d("f1", "fn main() {}", file), 0, (None, None)) else {
            panic!("expected chunks")
        };
        assert_eq!(c[0].source.source_type, SourceType::File);
        assert_eq!(c[0].source.uri, "relative/main.rs");
        assert_eq!(c[0].source.agent, None);
        assert_eq!(
            c[0].chunk.authored_at.as_deref(),
            Some("2026-04-16T04:34:59.000Z")
        );

        let none = DrawerMeta {
            wing: "alice".into(),
            room: "technical".into(),
            ingest_mode: s("convos"),
            ..Default::default()
        };
        let Planned::Chunks(c) = m.map(&d("g", "x", none), 0, (None, None)) else {
            panic!("expected chunks")
        };
        assert_eq!(c[0].source.uri, "mempalace-wing:alice");
    }

    #[test]
    fn split_utf8_never_cuts_a_character_and_loses_nothing() {
        // Thai words with spaces: parts end at a space.
        let spaced = "สวัสดีครับ ".repeat(10_000);
        assert!(spaced.len() > 100_000);
        let parts = split_utf8(&spaced, 32_000, &["\n\n", "\n", " "]);
        assert!(parts.len() >= 4);
        assert!(parts.iter().all(|p| p.len() <= 32_000));
        assert_eq!(parts.concat(), spaced);
        assert!(parts[..parts.len() - 1].iter().all(|p| p.ends_with(' ')));
        // Thai with no separator at all: cut at a char boundary, nothing lost.
        let solid = "ทดสอบภาษาไทย".repeat(8_000);
        assert!(solid.len() > 100_000);
        let parts = split_utf8(&solid, 32_000, &["\n\n", "\n"]);
        assert!(parts.iter().all(|p| p.len() <= 32_000 && !p.is_empty()));
        assert_eq!(parts.concat(), solid);
        // Paragraph beats line beats nothing.
        let t = "aaaa\n\nbbbb\ncccc dddd";
        assert_eq!(
            split_utf8(t, 12, &["\n\n", "\n"]),
            vec!["aaaa\n\n", "bbbb\n", "cccc dddd"]
        );
        assert_eq!(split_utf8("short", 100, &["\n"]), vec!["short"]);
        assert!(split_utf8("", 10, &["\n"]).is_empty());
    }
}
