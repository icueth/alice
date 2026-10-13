//! Vector storage (source of truth for the HNSW index) and the embedding work queue.

use crate::embed::memory_embed_text;
use crate::error::Result;
use half::f16;
use rusqlite::{Connection, params};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VecKey {
    Memory(i64),
    Chunk(i64),
}

impl VecKey {
    fn parts(self) -> (&'static str, i64) {
        match self {
            VecKey::Memory(s) => ("memory", s),
            VecKey::Chunk(s) => ("chunk", s),
        }
    }
}

pub fn encode_f16(v: &[f32]) -> Vec<u8> {
    v.iter()
        .flat_map(|x| f16::from_f32(*x).to_le_bytes())
        .collect()
}

pub fn decode_f16(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|c| f16::from_le_bytes(*c).to_f32())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingItem {
    pub key: VecKey,
    pub text: String,
}

// `embed_state <> 'done'` repeats the WHERE term of the partial indexes `memories_embed` and
// `chunks_embed`; SQLite uses a partial index only when the query contains that term literally.
const PENDING_MEMORIES_SQL: &str = "SELECT seq, title, body FROM memories
     WHERE embed_state <> 'done' AND embed_state IN ('pending','retry') AND status <> 'trashed'
     ORDER BY seq LIMIT ?1";
const PENDING_CHUNKS_SQL: &str = "SELECT seq, text FROM chunks
     WHERE embed_state <> 'done' AND embed_state IN ('pending','retry')
     ORDER BY seq LIMIT ?1";
const BACKLOG_SQL: &str = "SELECT
       (SELECT count(*) FROM memories
         WHERE embed_state <> 'done' AND embed_state IN ('pending','retry') AND status <> 'trashed')
     + (SELECT count(*) FROM chunks WHERE embed_state <> 'done' AND embed_state IN ('pending','retry'))";

/// Up to `limit` items waiting for embeddings: memories first (they are what agents search), then chunks.
pub fn pending(conn: &Connection, limit: usize) -> Result<Vec<PendingItem>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached(PENDING_MEMORIES_SQL)?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(PendingItem {
            key: VecKey::Memory(r.get(0)?),
            text: memory_embed_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?),
        })
    })?;
    for r in rows {
        out.push(r?);
    }
    if out.len() < limit {
        let mut stmt = conn.prepare_cached(PENDING_CHUNKS_SQL)?;
        let rows = stmt.query_map([(limit - out.len()) as i64], |r| {
            Ok(PendingItem {
                key: VecKey::Chunk(r.get(0)?),
                text: r.get(1)?,
            })
        })?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

pub fn put(conn: &Connection, key: VecKey, model: &str, v: &[f32]) -> Result<()> {
    let (t, s) = key.parts();
    conn.execute(
        "INSERT INTO embeddings (owner_type, owner_seq, model, dim, vector) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(owner_type, owner_seq, model) DO UPDATE SET dim = excluded.dim, vector = excluded.vector",
        params![t, s, model, v.len() as i64, encode_f16(v)],
    )?;
    Ok(())
}

fn set_state(conn: &Connection, key: VecKey, state: &str) -> Result<()> {
    match key {
        VecKey::Memory(s) => conn.execute(
            "UPDATE memories SET embed_state = ?1 WHERE seq = ?2",
            params![state, s],
        )?,
        VecKey::Chunk(s) => conn.execute(
            "UPDATE chunks SET embed_state = ?1 WHERE seq = ?2",
            params![state, s],
        )?,
    };
    Ok(())
}

pub fn mark_done(conn: &Connection, key: VecKey) -> Result<()> {
    set_state(conn, key, "done")
}

pub fn mark_failed(conn: &Connection, key: VecKey, retry: bool) -> Result<()> {
    set_state(conn, key, if retry { "retry" } else { "failed" })
}

pub fn count(conn: &Connection, model: &str) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM embeddings WHERE model = ?1",
        [model],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

pub fn backlog(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(BACKLOG_SQL, [], |r| r.get(0))?;
    Ok(n as usize)
}

pub fn for_each(conn: &Connection, model: &str, mut f: impl FnMut(VecKey, Vec<f32>)) -> Result<()> {
    let mut stmt =
        conn.prepare("SELECT owner_type, owner_seq, vector FROM embeddings WHERE model = ?1")?;
    let mut rows = stmt.query([model])?;
    while let Some(r) = rows.next()? {
        let t: String = r.get(0)?;
        let s: i64 = r.get(1)?;
        let blob: Vec<u8> = r.get(2)?;
        let key = if t == "memory" {
            VecKey::Memory(s)
        } else {
            VecKey::Chunk(s)
        };
        f(key, decode_f16(&blob));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::{memories, open_writer, projects, sources};

    #[test]
    fn f16_round_trip_is_close() {
        let v = vec![0.1_f32, -0.5, 0.25, 1.0];
        let back = decode_f16(&encode_f16(&v));
        for (a, b) in v.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn pending_lists_memories_then_chunks_and_mark_done_removes_them() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let m = memories::prepare(
            memories::NewMemoryInput {
                content: "alpha".into(),
                ..Default::default()
            },
            &p,
            "h".into(),
        )
        .unwrap();
        let memories::InsertOutcome::New { seq, .. } = memories::insert(&c, &m).unwrap() else {
            panic!()
        };
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "u".into(),
                ..Default::default()
            },
            &[sources::ChunkInput {
                ord: 0,
                text: "beta".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        let items = pending(&c, 10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, VecKey::Memory(seq));
        assert!(items[0].text.contains("alpha"));
        assert!(matches!(items[1].key, VecKey::Chunk(_)));
        put(&c, items[0].key, "m", &[0.0, 1.0]).unwrap();
        mark_done(&c, items[0].key).unwrap();
        assert_eq!(pending(&c, 10).unwrap().len(), 1);
        assert_eq!(count(&c, "m").unwrap(), 1);
        assert_eq!(backlog(&c).unwrap(), 1);
        let mut seen = Vec::new();
        for_each(&c, "m", |k, v| seen.push((k, v))).unwrap();
        assert_eq!(seen[0].0, VecKey::Memory(seq));
    }

    #[test]
    fn failed_items_retry_then_stop() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let m = memories::prepare(
            memories::NewMemoryInput {
                content: "gamma".into(),
                ..Default::default()
            },
            &p,
            "h".into(),
        )
        .unwrap();
        let memories::InsertOutcome::New { seq, .. } = memories::insert(&c, &m).unwrap() else {
            panic!()
        };
        mark_failed(&c, VecKey::Memory(seq), true).unwrap();
        assert_eq!(pending(&c, 10).unwrap().len(), 1, "retry stays queued");
        mark_failed(&c, VecKey::Memory(seq), false).unwrap();
        assert!(pending(&c, 10).unwrap().is_empty(), "failed is not retried");
    }

    #[test]
    fn pending_honours_the_limit_and_skips_trashed_memories() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let mut seqs = Vec::new();
        for content in ["one", "two"] {
            let m = memories::prepare(
                memories::NewMemoryInput {
                    content: content.into(),
                    ..Default::default()
                },
                &p,
                "h".into(),
            )
            .unwrap();
            let memories::InsertOutcome::New { seq, .. } = memories::insert(&c, &m).unwrap() else {
                panic!()
            };
            seqs.push(seq);
        }
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "u".into(),
                ..Default::default()
            },
            &[sources::ChunkInput {
                ord: 0,
                text: "three".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        let keys =
            |n| -> Vec<VecKey> { pending(&c, n).unwrap().into_iter().map(|i| i.key).collect() };
        assert_eq!(keys(1), vec![VecKey::Memory(seqs[0])]);
        assert_eq!(
            keys(2),
            vec![VecKey::Memory(seqs[0]), VecKey::Memory(seqs[1])]
        );
        assert_eq!(keys(3).len(), 3);
        assert_eq!(backlog(&c).unwrap(), 3);
        memories::set_status_by_seq(&c, seqs[0], crate::model::Status::Trashed).unwrap();
        assert_eq!(
            keys(1),
            vec![VecKey::Memory(seqs[1])],
            "trashed is not embedded"
        );
        assert_eq!(backlog(&c).unwrap(), 2);
    }

    #[test]
    fn put_replaces_the_vector_of_the_same_owner_and_model() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        put(&c, VecKey::Chunk(7), "m", &[1.0, 0.0]).unwrap();
        put(&c, VecKey::Chunk(7), "m", &[0.0, 0.5, 0.25]).unwrap();
        put(&c, VecKey::Memory(7), "m", &[1.0]).unwrap();
        put(&c, VecKey::Chunk(7), "other", &[1.0]).unwrap();
        assert_eq!(count(&c, "m").unwrap(), 2);
        let mut seen = Vec::new();
        for_each(&c, "m", |k, v| seen.push((k, v))).unwrap();
        seen.sort_by_key(|(k, _)| *k);
        assert_eq!(
            seen,
            vec![
                (VecKey::Memory(7), vec![1.0]),
                (VecKey::Chunk(7), vec![0.0, 0.5, 0.25])
            ]
        );
    }

    /// The queue queries must use the partial indexes `memories_embed` / `chunks_embed`
    /// (`WHERE embed_state <> 'done'`); SQLite only does so when the query repeats that term.
    #[test]
    fn queue_queries_use_the_partial_embed_indexes() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let plan = |sql: &str| -> String {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let n = stmt.parameter_count();
            let params: Vec<i64> = vec![10; n];
            stmt.query_map(rusqlite::params_from_iter(params), |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ")
        };
        let m = plan(PENDING_MEMORIES_SQL);
        assert!(m.contains("USING INDEX memories_embed"), "{m}");
        let ch = plan(PENDING_CHUNKS_SQL);
        assert!(ch.contains("USING INDEX chunks_embed"), "{ch}");
        let b = plan(BACKLOG_SQL);
        assert!(
            b.contains("memories_embed") && b.contains("chunks_embed"),
            "{b}"
        );
    }
}
