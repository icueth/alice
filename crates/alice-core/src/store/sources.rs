//! Raw tier: sources and their ordered chunks.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::{SourceType, limits};
use crate::normalize::{nfc, sha256_hex};
use crate::store::fts;
use crate::store::projects::ResolvedProject;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceInput {
    pub source_type: SourceType,
    /// Stable identifier, e.g. `file:///abs/path/session.jsonl`.
    pub uri: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub legacy_ref: Option<String>,
}

impl Default for SourceInput {
    fn default() -> Self {
        Self {
            source_type: SourceType::Session,
            uri: String::new(),
            agent: None,
            session_id: None,
            started_at: None,
            ended_at: None,
            legacy_ref: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChunkInput {
    pub ord: i64,
    pub text: String,
    #[serde(default)]
    pub authored_at: Option<String>,
    #[serde(default)]
    pub legacy_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IngestOutcome {
    pub source_id: String,
    pub inserted: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub seq: i64,
    pub id: String,
    pub source_id: String,
    pub project_id: String,
    pub text: String,
    pub created_at: String,
    pub authored_at: Option<String>,
}

pub fn validate_chunks(chunks: &[ChunkInput]) -> Result<()> {
    if chunks.len() > limits::MAX_CHUNKS_PER_REQUEST {
        return Err(AliceError::invalid(format!(
            "{} chunks in one request; send at most {} per request",
            chunks.len(),
            limits::MAX_CHUNKS_PER_REQUEST
        )));
    }
    if let Some(c) = chunks
        .iter()
        .find(|c| c.text.len() > limits::MAX_CHUNK_BYTES)
    {
        return Err(AliceError::invalid(format!(
            "chunk {} is {} bytes; split chunks to at most {} bytes",
            c.ord,
            c.text.len(),
            limits::MAX_CHUNK_BYTES
        )));
    }
    Ok(())
}

/// Seconds since the epoch at or above which a bare number is read as milliseconds (year 5138
/// in seconds, 1973 in milliseconds).
const MILLIS_FROM: i64 = 100_000_000_000;

/// The time a chunk says it was written, as the store keeps it: an ISO 8601 / RFC 3339 time (or
/// a date) is kept as given; a bare Unix time in seconds or milliseconds becomes an RFC 3339
/// UTC time; anything else (empty, `10/13/2026`, words) is dropped (`ingest` logs one warning
/// per request with the count). A value that is kept is always a time, so nothing downstream
/// compares garbage; but one with an offset (`+07:00`) or a date alone is kept as written and
/// does not sort against `…Z` values as text (final review B-M8: the comment said it did).
pub fn normalise_authored(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let iso = chrono::DateTime::parse_from_rfc3339(raw).is_ok()
        || chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
        || chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").is_ok();
    if iso {
        return Some(raw.to_string());
    }
    if let Ok(n) = raw.parse::<i64>()
        && n > 0
    {
        let millis = if n >= MILLIS_FROM {
            n
        } else {
            n.saturating_mul(1000)
        };
        if let Some(t) = chrono::DateTime::from_timestamp_millis(millis) {
            return Some(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        }
    }
    None
}

/// Create the source if new (identified by type + uri) and insert chunks whose `ord` is not stored yet.
pub fn ingest(
    conn: &Connection,
    project: &ResolvedProject,
    src: &SourceInput,
    chunks: &[ChunkInput],
) -> Result<IngestOutcome> {
    validate_chunks(chunks)?;
    if src.uri.trim().is_empty() {
        return Err(AliceError::invalid(
            "source uri is empty; use a stable identifier such as the transcript path",
        ));
    }
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM sources WHERE type = ?1 AND uri = ?2",
            params![src.source_type.as_str(), src.uri],
            |r| r.get(0),
        )
        .optional()?;
    let source_id = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE sources SET ended_at = COALESCE(?1, ended_at) WHERE id = ?2",
                params![src.ended_at, id],
            )?;
            id
        }
        None => {
            let id = new_id();
            conn.execute(
                "INSERT INTO sources (id, project_id, type, agent, uri, session_id, started_at, ended_at, legacy_ref, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id,
                    project.id,
                    src.source_type.as_str(),
                    src.agent,
                    src.uri,
                    src.session_id,
                    src.started_at,
                    src.ended_at,
                    src.legacy_ref,
                    now_rfc3339()
                ],
            )?;
            id
        }
    };
    let mut inserted = 0;
    let mut skipped = 0;
    let now = now_rfc3339();
    let mut dropped_times = 0usize;
    for c in chunks {
        let text = nfc(&c.text);
        let authored_at = normalise_authored(c.authored_at.as_deref());
        if authored_at.is_none()
            && c.authored_at
                .as_deref()
                .is_some_and(|a| !a.trim().is_empty())
        {
            dropped_times += 1;
        }
        let changed = conn.execute(
            "INSERT OR IGNORE INTO chunks (id, source_id, project_id, ord, text, content_hash, authored_at, created_at, legacy_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new_id(),
                source_id,
                project.id,
                c.ord,
                text,
                sha256_hex(text.as_bytes()),
                authored_at,
                now,
                c.legacy_id
            ],
        )?;
        if changed == 1 {
            fts::index_chunk(conn, conn.last_insert_rowid(), &text)?;
            inserted += 1;
        } else {
            skipped += 1;
        }
    }
    if dropped_times > 0 {
        tracing::warn!(
            chunks = dropped_times,
            "chunks whose authored_at is not an ISO 8601 or Unix time were stored without one"
        );
    }
    Ok(IngestOutcome {
        source_id,
        inserted,
        skipped,
    })
}

pub fn get_chunks_by_seqs(conn: &Connection, seqs: &[i64]) -> Result<Vec<ChunkRow>> {
    if seqs.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; seqs.len()].join(",");
    let sql = format!(
        "SELECT seq, id, source_id, project_id, text, created_at, authored_at FROM chunks WHERE seq IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<ChunkRow> = stmt
        .query_map(params_from_iter(seqs.iter()), |r| {
            Ok(ChunkRow {
                seq: r.get(0)?,
                id: r.get(1)?,
                source_id: r.get(2)?,
                project_id: r.get(3)?,
                text: r.get(4)?,
                created_at: r.get(5)?,
                authored_at: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    let pos = |s: i64| seqs.iter().position(|x| *x == s).unwrap_or(usize::MAX);
    rows.sort_by_key(|r| pos(r.seq));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceType;
    use crate::project::ProjectRef;
    use crate::store::{open_writer, projects};

    fn setup() -> (
        tempfile::TempDir,
        rusqlite::Connection,
        projects::ResolvedProject,
    ) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        (d, c, p)
    }

    /// I-1: `authored_at` is kept when it is ISO 8601, converted when it is a Unix time, and
    /// dropped (with a warning) when it is anything else, so no stored value sorts wrongly.
    #[test]
    fn authored_at_is_normalised_at_ingest() {
        let cases: [(&str, Option<&str>); 9] = [
            ("2026-01-02T03:04:05.678Z", Some("2026-01-02T03:04:05.678Z")),
            (
                "2026-01-02T03:04:05+07:00",
                Some("2026-01-02T03:04:05+07:00"),
            ),
            ("2026-01-02", Some("2026-01-02")),
            ("1767323045", Some("2026-01-02T03:04:05.000Z")),
            ("1767323045678", Some("2026-01-02T03:04:05.678Z")),
            ("", None),
            ("   ", None),
            ("10/13/2026", None),
            ("yesterday", None),
        ];
        for (raw, want) in cases {
            assert_eq!(normalise_authored(Some(raw)).as_deref(), want, "{raw:?}");
        }
        assert_eq!(normalise_authored(None), None);
        assert_eq!(normalise_authored(Some("0")), None);
        assert_eq!(normalise_authored(Some("-5")), None);
        let (_d, c, p) = setup();
        let mut a = chunk(0, "epoch chunk");
        a.authored_at = Some("1767323045".into());
        let mut b = chunk(1, "us date chunk");
        b.authored_at = Some("10/13/2026".into());
        ingest(&c, &p, &src(), &[a, b]).unwrap();
        let stored: Vec<Option<String>> = c
            .prepare("SELECT authored_at FROM chunks ORDER BY ord")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            stored,
            vec![Some("2026-01-02T03:04:05.000Z".to_string()), None]
        );
    }

    fn src() -> SourceInput {
        SourceInput {
            source_type: SourceType::Session,
            uri: "file:///t/s1.jsonl".into(),
            ..Default::default()
        }
    }

    fn chunk(ord: i64, text: &str) -> ChunkInput {
        ChunkInput {
            ord,
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn ingest_is_incremental_by_ord() {
        let (_d, c, p) = setup();
        let a = ingest(&c, &p, &src(), &[chunk(0, "hello"), chunk(1, "world")]).unwrap();
        assert_eq!((a.inserted, a.skipped), (2, 0));
        let b = ingest(&c, &p, &src(), &[chunk(1, "world"), chunk(2, "again")]).unwrap();
        assert_eq!((b.inserted, b.skipped), (1, 1));
        assert_eq!(a.source_id, b.source_id);
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '\"again\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn limits_are_enforced_before_writing() {
        let (_d, c, p) = setup();
        let too_many: Vec<ChunkInput> = (0..1_001).map(|i| chunk(i, "x")).collect();
        assert!(
            ingest(&c, &p, &src(), &too_many)
                .unwrap_err()
                .to_string()
                .contains("1000")
        );
        let big = chunk(0, &"y".repeat(40_000));
        assert!(
            ingest(&c, &p, &src(), &[big])
                .unwrap_err()
                .to_string()
                .contains("32768")
        );
        let n: i64 = c
            .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "nothing partial is written");
    }

    fn count(c: &rusqlite::Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn invalid_text(r: Result<IngestOutcome>) -> String {
        match r {
            Err(AliceError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn oversized_ingests_are_invalid_errors_naming_the_limit_and_write_nothing() {
        let (_d, c, p) = setup();
        let five_thousand: Vec<ChunkInput> = (0..5_000).map(|i| chunk(i, "x")).collect();
        assert!(invalid_text(ingest(&c, &p, &src(), &five_thousand)).contains("1000"));
        // Valid chunks ahead of the oversized one must not be stored either.
        let mixed = [
            chunk(0, "fine"),
            chunk(1, "also fine"),
            chunk(2, &"y".repeat(40 * 1024)),
        ];
        let msg = invalid_text(ingest(&c, &p, &src(), &mixed));
        assert!(msg.contains("32768") && msg.contains("chunk 2"), "{msg}");
        let no_uri = SourceInput {
            uri: "  ".into(),
            ..src()
        };
        assert!(invalid_text(ingest(&c, &p, &no_uri, &[chunk(0, "fine")])).contains("uri"));
        for t in ["sources", "chunks", "chunks_fts", "chunks_tri"] {
            assert_eq!(count(&c, t), 0, "{t}: nothing partial is written");
        }
    }

    #[test]
    fn chunks_read_back_in_the_requested_order() {
        let (_d, c, p) = setup();
        ingest(&c, &p, &src(), &[chunk(0, "zero"), chunk(1, "one")]).unwrap();
        let seqs: Vec<i64> = c
            .prepare("SELECT seq FROM chunks ORDER BY ord")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let rows = get_chunks_by_seqs(&c, &[seqs[1], 12_345, seqs[0]]).unwrap();
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["one", "zero"]);
        assert_eq!(rows[0].project_id, p.id);
        assert!(get_chunks_by_seqs(&c, &[]).unwrap().is_empty());
    }
}
