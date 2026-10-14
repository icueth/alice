//! Vector storage (source of truth for the HNSW index) and the embedding work queue.

use crate::embed::memory_embed_text;
use crate::error::Result;
use half::f16;
use rusqlite::{Connection, OptionalExtension, params};

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
    /// The row's `content_hash`, read together with `text`. Completion is compare-and-set on it
    /// ([`put_if_current`], [`mark_failed_if_current`]), so an edit that lands while the old text
    /// is being embedded is never marked done with the old vector.
    pub stamp: String,
    /// The id of the project the row belongs to (the cloud policy is asked per project).
    pub project_id: String,
}

/// The `embed_state` of a row whose text was not embedded because the active embedder is a
/// remote one and the cloud policy keeps the row's project at home. Such a row stays
/// searchable by words; it is queued again (`pending`) when the policy or the provider changes.
pub const SKIPPED_CLOUD: &str = "skipped_cloud";

// `embed_state <> 'done'` repeats the WHERE term of the partial indexes `memories_embed` and
// `chunks_embed`; SQLite uses a partial index only when the query contains that term literally.
const PENDING_MEMORIES_SQL: &str = "SELECT seq, title, body, content_hash, project_id FROM memories
     WHERE embed_state <> 'done' AND embed_state IN ('pending','retry') AND status <> 'trashed'
     ORDER BY seq LIMIT ?1";
const PENDING_CHUNKS_SQL: &str = "SELECT seq, text, content_hash, project_id FROM chunks
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
            stamp: r.get(3)?,
            project_id: r.get(4)?,
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
                stamp: r.get(2)?,
                project_id: r.get(3)?,
            })
        })?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Memory,
    Chunk,
}

/// Rows after `after_seq` with no vector for `model`, in seq order (live memories / chunks).
/// What a model change has still to embed.
pub fn missing(
    conn: &Connection,
    owner: Owner,
    model: &str,
    after_seq: i64,
    limit: usize,
) -> Result<Vec<PendingItem>> {
    let sql = match owner {
        Owner::Memory => {
            "SELECT m.seq, m.title, m.body, m.content_hash, m.project_id FROM memories m
             WHERE m.seq > ?1 AND m.status <> 'trashed'
               AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.owner_type = 'memory' AND e.owner_seq = m.seq AND e.model = ?2)
             ORDER BY m.seq LIMIT ?3"
        }
        Owner::Chunk => {
            "SELECT c.seq, c.text, '', c.content_hash, c.project_id FROM chunks c
             WHERE c.seq > ?1
               AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.owner_type = 'chunk' AND e.owner_seq = c.seq AND e.model = ?2)
             ORDER BY c.seq LIMIT ?3"
        }
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params![after_seq, model, limit as i64], |r| {
        let seq: i64 = r.get(0)?;
        Ok(match owner {
            Owner::Memory => PendingItem {
                key: VecKey::Memory(seq),
                text: memory_embed_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?),
                stamp: r.get(3)?,
                project_id: r.get(4)?,
            },
            Owner::Chunk => PendingItem {
                key: VecKey::Chunk(seq),
                text: r.get(1)?,
                stamp: r.get(3)?,
                project_id: r.get(4)?,
            },
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Store `model`'s vector if the row still holds the text stamped `stamp`; `embed_state` (which
/// belongs to the active model) is not touched. Returns whether it applied.
pub fn put_model_if_current(
    conn: &Connection,
    key: VecKey,
    stamp: &str,
    model: &str,
    v: &[f32],
) -> Result<bool> {
    let (sql, seq) = match key {
        VecKey::Memory(s) => (
            "SELECT EXISTS(SELECT 1 FROM memories WHERE seq = ?1 AND content_hash = ?2)",
            s,
        ),
        VecKey::Chunk(s) => (
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE seq = ?1 AND content_hash = ?2)",
            s,
        ),
    };
    let current: bool = conn.query_row(sql, params![seq, stamp], |r| r.get(0))?;
    if current {
        put(conn, key, model, v)?;
    }
    Ok(current)
}

/// Remove `model`'s vector of one row (its text changed, so the vector is stale).
pub fn delete_vector(conn: &Connection, key: VecKey, model: &str) -> Result<()> {
    let (t, s) = key.parts();
    conn.execute(
        "DELETE FROM embeddings WHERE owner_type = ?1 AND owner_seq = ?2 AND model = ?3",
        params![t, s, model],
    )?;
    Ok(())
}

/// Delete up to `limit` stored vectors of `model`; returns how many went.
pub fn delete_model(conn: &Connection, model: &str, limit: usize) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM embeddings WHERE rowid IN (SELECT rowid FROM embeddings WHERE model = ?1 LIMIT ?2)",
        params![model, limit as i64],
    )?)
}

/// Mark the text stamped `stamp` as held back by the cloud policy; returns whether it applied
/// (a row edited since was put back to `pending` by the edit and is looked at again).
pub fn mark_skipped_cloud_if_current(conn: &Connection, key: VecKey, stamp: &str) -> Result<bool> {
    set_state_if_current(conn, key, stamp, SKIPPED_CLOUD)
}

/// Ids of the projects that have rows held back by the cloud policy.
pub fn skipped_projects(conn: &Connection) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for table in ["memories", "chunks"] {
        let mut stmt = conn.prepare(&format!(
            "SELECT project_id FROM {table} WHERE embed_state = ?1 GROUP BY project_id"
        ))?;
        for id in stmt.query_map([SKIPPED_CLOUD], |r| r.get::<_, String>(0))? {
            let id = id?;
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// Queue up to `limit` rows held back by the cloud policy for embedding again, those of
/// `projects` only (`None`: every project); returns how many were queued. Run it (in a loop
/// until 0) when the policy or the provider changed.
pub fn requeue_skipped_cloud(
    conn: &Connection,
    projects: Option<&[String]>,
    limit: usize,
) -> Result<usize> {
    let mut n = 0;
    for table in ["memories", "chunks"] {
        let left = limit.saturating_sub(n);
        if left == 0 {
            break;
        }
        let mut sql = format!(
            "UPDATE {table} SET embed_state = 'pending' WHERE seq IN
               (SELECT seq FROM {table} WHERE embed_state = ?1"
        );
        let mut args: Vec<rusqlite::types::Value> = vec![SKIPPED_CLOUD.to_string().into()];
        if let Some(ids) = projects {
            if ids.is_empty() {
                continue;
            }
            let marks: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 3)).collect();
            sql.push_str(&format!(" AND project_id IN ({})", marks.join(",")));
            args.splice(1..1, [(left as i64).into()]);
            args.extend(ids.iter().map(|i| rusqlite::types::Value::from(i.clone())));
        } else {
            args.push((left as i64).into());
        }
        sql.push_str(" LIMIT ?2)");
        n += conn.execute(&sql, rusqlite::params_from_iter(args))?;
    }
    Ok(n)
}

/// Rows held back by the cloud policy (memories, chunks).
pub fn skipped_cloud_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT (SELECT count(*) FROM memories WHERE embed_state = ?1 AND status <> 'trashed')
              + (SELECT count(*) FROM chunks WHERE embed_state = ?1)",
        [SKIPPED_CLOUD],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// What a model change would embed, per project: `(project_id, memories, chunks)`.
pub fn embeddable_by_project(conn: &Connection) -> Result<Vec<(String, usize, usize)>> {
    let mut by: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    let mut stmt = conn.prepare(
        "SELECT project_id, count(*) FROM memories WHERE status <> 'trashed' GROUP BY project_id",
    )?;
    for r in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (p, n) = r?;
        by.entry(p).or_default().0 = n as usize;
    }
    let mut stmt = conn.prepare("SELECT project_id, count(*) FROM chunks GROUP BY project_id")?;
    for r in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (p, n) = r?;
        by.entry(p).or_default().1 = n as usize;
    }
    Ok(by.into_iter().map(|(p, (m, c))| (p, m, c)).collect())
}

/// Rows a model change would send to a remote provider: live memories and all chunks.
pub fn embeddable_count(conn: &Connection) -> Result<(usize, usize)> {
    let m: i64 = conn.query_row(
        "SELECT count(*) FROM memories WHERE status <> 'trashed'",
        [],
        |r| r.get(0),
    )?;
    let c: i64 = conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
    Ok((m as usize, c as usize))
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

/// Set `embed_state` only while the row exists and its `content_hash` still equals `stamp`.
/// Returns whether the row was updated.
fn set_state_if_current(conn: &Connection, key: VecKey, stamp: &str, state: &str) -> Result<bool> {
    let sql = match key {
        VecKey::Memory(_) => {
            "UPDATE memories SET embed_state = ?1 WHERE seq = ?2 AND content_hash = ?3"
        }
        VecKey::Chunk(_) => {
            "UPDATE chunks SET embed_state = ?1 WHERE seq = ?2 AND content_hash = ?3"
        }
    };
    let (VecKey::Memory(seq) | VecKey::Chunk(seq)) = key;
    let changed = conn
        .prepare_cached(sql)?
        .execute(params![state, seq, stamp])?;
    Ok(changed > 0)
}

/// Store the vector of the text stamped `stamp` and mark the row done, but only if the row still
/// exists and still holds that text (its `content_hash` equals `stamp`). Returns whether it
/// applied. When it does not, nothing is written: the edit that changed the text already put the
/// row back to `pending`, so the new text is embedded on the next round.
pub fn put_if_current(
    conn: &Connection,
    key: VecKey,
    stamp: &str,
    model: &str,
    v: &[f32],
) -> Result<bool> {
    if !set_state_if_current(conn, key, stamp, "done")? {
        return Ok(false);
    }
    put(conn, key, model, v)?;
    Ok(true)
}

/// `mark_failed` for the text stamped `stamp` only; returns whether it applied. A row whose text
/// changed since it was read stays as the edit left it (`pending`).
pub fn mark_failed_if_current(
    conn: &Connection,
    key: VecKey,
    stamp: &str,
    retry: bool,
) -> Result<bool> {
    set_state_if_current(conn, key, stamp, if retry { "retry" } else { "failed" })
}

pub fn count(conn: &Connection, model: &str) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM embeddings WHERE model = ?1",
        [model],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Stored vectors of `model` for `keys`, in the order given; keys without one are left out.
/// One primary-key lookup per key (a recall rescores a few hundred chunk candidates).
pub fn get_many(
    conn: &Connection,
    model: &str,
    keys: &[VecKey],
) -> Result<Vec<(VecKey, Vec<f32>)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT vector FROM embeddings WHERE owner_type = ?1 AND owner_seq = ?2 AND model = ?3",
    )?;
    let mut out = Vec::with_capacity(keys.len());
    for &key in keys {
        let (t, s) = key.parts();
        if let Some(blob) = stmt
            .query_row(params![t, s, model], |r| r.get::<_, Vec<u8>>(0))
            .optional()?
        {
            out.push((key, decode_f16(&blob)));
        }
    }
    Ok(out)
}

/// How many chunk vectors of `model` are stored, counting no further than `limit`.
pub fn count_chunks_up_to(conn: &Connection, model: &str, limit: usize) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM embeddings
           WHERE owner_type = 'chunk' AND model = ?1 LIMIT ?2)",
        params![model, limit as i64],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Up to `n` stored chunk vectors of `model`, spread evenly over the chunk seqs from the
/// oldest to the newest: one index seek each, so a mean can be fitted on the whole corpus
/// without reading all of it.
pub fn sample_chunks(conn: &Connection, model: &str, n: usize) -> Result<Vec<Vec<f32>>> {
    let end = |order: &str| -> Result<Option<i64>> {
        Ok(conn
            .query_row(
                &format!(
                    "SELECT owner_seq FROM embeddings WHERE owner_type = 'chunk' AND model = ?1
                     ORDER BY owner_seq {order} LIMIT 1"
                ),
                [model],
                |r| r.get(0),
            )
            .optional()?)
    };
    let (Some(first), Some(last)) = (end("ASC")?, end("DESC")?) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare_cached(
        "SELECT owner_seq, vector FROM embeddings
         WHERE owner_type = 'chunk' AND owner_seq >= ?1 AND model = ?2
         ORDER BY owner_seq LIMIT 1",
    )?;
    let span = i128::from(last - first);
    let mut out = Vec::with_capacity(n);
    let mut next = first;
    for j in 0..n {
        let target = (i128::from(first) + span * j as i128 / n as i128) as i64;
        let Some((seq, blob)) = stmt
            .query_row(params![target.max(next), model], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .optional()?
        else {
            break;
        };
        out.push(decode_f16(&blob));
        next = seq + 1;
    }
    Ok(out)
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

    fn embedding_rows(c: &Connection) -> i64 {
        c.query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .unwrap()
    }

    fn embed_state(c: &Connection, key: VecKey) -> String {
        let sql = match key {
            VecKey::Memory(_) => "SELECT embed_state FROM memories WHERE seq = ?1",
            VecKey::Chunk(_) => "SELECT embed_state FROM chunks WHERE seq = ?1",
        };
        let (VecKey::Memory(s) | VecKey::Chunk(s)) = key;
        c.query_row(sql, [s], |r| r.get(0)).unwrap()
    }

    /// An edit can land while the worker embeds the old text: the stale vector must not be
    /// stored and the row must stay queued, so the new text is embedded next.
    #[test]
    fn a_stale_stamp_neither_stores_the_vector_nor_marks_the_row_done() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let m = memories::prepare(
            memories::NewMemoryInput {
                content: "port is 8080".into(),
                ..Default::default()
            },
            &p,
            "h".into(),
        )
        .unwrap();
        let memories::InsertOutcome::New { id, seq } = memories::insert(&c, &m).unwrap() else {
            panic!()
        };
        let old = pending(&c, 10).unwrap().remove(0);
        assert_eq!(old.key, VecKey::Memory(seq));
        assert_eq!(
            old.stamp, m.content_hash,
            "the stamp is the row's content hash"
        );

        memories::apply_update(
            &c,
            &id,
            &memories::MemoryUpdate {
                body: Some("port is 9090".into()),
                ..Default::default()
            },
            "h2".into(),
        )
        .unwrap();
        assert!(!put_if_current(&c, old.key, &old.stamp, "m", &[1.0, 0.0]).unwrap());
        assert_eq!(embed_state(&c, old.key), "pending");
        assert_eq!(embedding_rows(&c), 0, "the stale vector is not stored");

        let new = pending(&c, 10).unwrap().remove(0);
        assert!(new.text.contains("9090"), "{}", new.text);
        assert_ne!(new.stamp, old.stamp);
        assert!(put_if_current(&c, new.key, &new.stamp, "m", &[0.0, 1.0]).unwrap());
        assert_eq!(embed_state(&c, new.key), "done");
        let mut seen = Vec::new();
        for_each(&c, "m", |k, v| seen.push((k, v))).unwrap();
        assert_eq!(seen, vec![(VecKey::Memory(seq), vec![0.0, 1.0])]);
        assert!(pending(&c, 10).unwrap().is_empty());
    }

    #[test]
    fn chunk_stamps_are_their_hash_and_a_missing_row_never_applies() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "u".into(),
                ..Default::default()
            },
            &[sources::ChunkInput {
                ord: 0,
                text: "delta".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        let item = pending(&c, 10).unwrap().remove(0);
        assert_eq!(item.stamp, crate::normalize::sha256_hex(b"delta"));
        assert!(!put_if_current(&c, item.key, "other", "m", &[1.0]).unwrap());
        assert_eq!(embed_state(&c, item.key), "pending");
        assert!(!put_if_current(&c, VecKey::Chunk(9_999), &item.stamp, "m", &[1.0]).unwrap());
        assert!(!put_if_current(&c, VecKey::Memory(9_999), &item.stamp, "m", &[1.0]).unwrap());
        assert_eq!(embedding_rows(&c), 0);
        assert!(put_if_current(&c, item.key, &item.stamp, "m", &[1.0]).unwrap());
        assert_eq!(embed_state(&c, item.key), "done");
        assert_eq!(embedding_rows(&c), 1);
    }

    /// Giving up on an item must not bury text that an edit put in its place meanwhile.
    #[test]
    fn failure_marks_apply_only_to_the_text_that_failed() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let m = memories::prepare(
            memories::NewMemoryInput {
                content: "flaky text".into(),
                ..Default::default()
            },
            &p,
            "h".into(),
        )
        .unwrap();
        let memories::InsertOutcome::New { id, .. } = memories::insert(&c, &m).unwrap() else {
            panic!()
        };
        let item = pending(&c, 10).unwrap().remove(0);
        assert!(mark_failed_if_current(&c, item.key, &item.stamp, true).unwrap());
        assert_eq!(embed_state(&c, item.key), "retry");
        memories::apply_update(
            &c,
            &id,
            &memories::MemoryUpdate {
                body: Some("fixed text".into()),
                ..Default::default()
            },
            "h2".into(),
        )
        .unwrap();
        assert!(!mark_failed_if_current(&c, item.key, &item.stamp, false).unwrap());
        assert_eq!(
            embed_state(&c, item.key),
            "pending",
            "the edit stays queued"
        );
        let again = pending(&c, 10).unwrap().remove(0);
        assert!(mark_failed_if_current(&c, again.key, &again.stamp, false).unwrap());
        assert_eq!(embed_state(&c, again.key), "failed");
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

    /// A rebuild fits the chunk index's mean on a sample spread over every chunk, not the
    /// first rows, and asks only whether there are enough chunks for one.
    #[test]
    fn chunk_samples_spread_over_all_seqs_and_counts_stop_at_their_limit() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        for s in 1..=1_000i64 {
            put(&c, VecKey::Chunk(s), "m", &[s as f32, 0.0]).unwrap();
        }
        put(&c, VecKey::Chunk(5_000), "other", &[1.0, 1.0]).unwrap();
        put(&c, VecKey::Memory(3), "m", &[1.0, 1.0]).unwrap();
        assert_eq!(count_chunks_up_to(&c, "m", 10).unwrap(), 10);
        assert_eq!(count_chunks_up_to(&c, "m", 5_000).unwrap(), 1_000);
        assert_eq!(count_chunks_up_to(&c, "none", 10).unwrap(), 0);
        let seqs: Vec<f32> = sample_chunks(&c, "m", 10)
            .unwrap()
            .iter()
            .map(|v| v[0])
            .collect();
        assert_eq!(seqs.len(), 10);
        assert_eq!(seqs[0], 1.0);
        assert!(
            seqs[9] >= 900.0,
            "the sample reaches the newest chunks: {seqs:?}"
        );
        assert!(
            seqs.windows(2).all(|w| w[1] - w[0] >= 90.0),
            "evenly spread: {seqs:?}"
        );
        assert_eq!(
            sample_chunks(&c, "m", 5_000).unwrap().len(),
            1_000,
            "asking for more than there are takes each once"
        );
        assert!(sample_chunks(&c, "none", 10).unwrap().is_empty());
    }

    #[test]
    fn get_many_returns_stored_vectors_of_one_model_and_skips_missing() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        put(&c, VecKey::Chunk(1), "m", &[1.0, 0.0]).unwrap();
        put(&c, VecKey::Chunk(2), "m", &[0.0, 1.0]).unwrap();
        put(&c, VecKey::Chunk(2), "other", &[0.5, 0.5]).unwrap();
        let got = get_many(
            &c,
            "m",
            &[VecKey::Chunk(2), VecKey::Chunk(3), VecKey::Chunk(1)],
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (VecKey::Chunk(2), vec![0.0, 1.0]));
        assert_eq!(got[1].0, VecKey::Chunk(1));
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
    /// Three live memories in the global project; their seqs.
    fn three_memories(c: &Connection) -> Vec<i64> {
        let p = projects::upsert_project(c, &ProjectRef::global()).unwrap();
        let mut seqs = Vec::new();
        for t in ["one", "two", "three"] {
            let m = memories::prepare(
                memories::NewMemoryInput {
                    content: t.into(),
                    ..Default::default()
                },
                &p,
                "h".into(),
            )
            .unwrap();
            if let memories::InsertOutcome::New { seq, .. } = memories::insert(c, &m).unwrap() {
                seqs.push(seq);
            }
        }
        seqs
    }

    #[test]
    fn missing_lists_rows_without_a_vector_for_the_model_after_a_cursor() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let seqs = three_memories(&c);
        put(&c, VecKey::Memory(seqs[1]), "new", &[1.0]).unwrap();
        let got: Vec<VecKey> = missing(&c, Owner::Memory, "new", 0, 10)
            .unwrap()
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert_eq!(got, vec![VecKey::Memory(seqs[0]), VecKey::Memory(seqs[2])]);
        assert_eq!(
            missing(&c, Owner::Memory, "new", seqs[0], 10)
                .unwrap()
                .len(),
            1
        );
        assert!(
            missing(&c, Owner::Chunk, "new", 0, 10).unwrap().is_empty(),
            "no chunks"
        );
        let stamp: String = c
            .query_row(
                "SELECT content_hash FROM memories WHERE seq = ?1",
                [seqs[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !put_model_if_current(&c, VecKey::Memory(seqs[0]), "stale", "new", &[0.5]).unwrap()
        );
        assert!(put_model_if_current(&c, VecKey::Memory(seqs[0]), &stamp, "new", &[0.5]).unwrap());
        let state: String = c
            .query_row(
                "SELECT embed_state FROM memories WHERE seq = ?1",
                [seqs[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            state, "pending",
            "embed_state belongs to the active model and is untouched"
        );
        delete_vector(&c, VecKey::Memory(seqs[0]), "new").unwrap();
        assert_eq!(count(&c, "new").unwrap(), 1);
        assert_eq!(delete_model(&c, "new", 10).unwrap(), 1);
        assert_eq!(count(&c, "new").unwrap(), 0);
    }

    #[test]
    fn missing_skips_trashed_memories_and_lists_chunks_with_their_project() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let seqs = three_memories(&c);
        memories::set_status_by_seq(&c, seqs[0], crate::model::Status::Trashed).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        sources::ingest(
            &c,
            &p,
            &sources::SourceInput {
                uri: "u".into(),
                ..Default::default()
            },
            &[sources::ChunkInput {
                ord: 0,
                text: "a chunk".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        let mem = missing(&c, Owner::Memory, "m", 0, 10).unwrap();
        assert_eq!(mem.len(), 2, "the trashed memory is left out");
        assert!(mem.iter().all(|i| i.project_id == p.id));
        let chunks = missing(&c, Owner::Chunk, "m", 0, 10).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            (chunks[0].text.as_str(), chunks[0].project_id.as_str()),
            ("a chunk", p.id.as_str())
        );
        assert_eq!(embeddable_count(&c).unwrap(), (2, 1));
    }

    #[test]
    fn rows_held_back_by_the_cloud_policy_leave_the_queue_and_come_back() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let seqs = three_memories(&c);
        let stamp = |seq: i64| -> String {
            c.query_row(
                "SELECT content_hash FROM memories WHERE seq = ?1",
                [seq],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(
            mark_skipped_cloud_if_current(&c, VecKey::Memory(seqs[0]), &stamp(seqs[0])).unwrap()
        );
        assert!(
            !mark_skipped_cloud_if_current(&c, VecKey::Memory(seqs[1]), "stale").unwrap(),
            "an edited row is not marked"
        );
        assert_eq!(
            pending(&c, 10).unwrap().len(),
            2,
            "a skipped row is not queued"
        );
        assert_eq!(backlog(&c).unwrap(), 2);
        assert_eq!(skipped_cloud_count(&c).unwrap(), 1);
        assert_eq!(requeue_skipped_cloud(&c, None, 10).unwrap(), 1);
        assert_eq!(pending(&c, 10).unwrap().len(), 3);
        assert_eq!(skipped_cloud_count(&c).unwrap(), 0);
        assert_eq!(requeue_skipped_cloud(&c, None, 10).unwrap(), 0);
    }

    #[test]
    fn held_back_rows_are_requeued_by_project_and_counted_per_project() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let seqs = three_memories(&c);
        let stamp = |seq: i64| -> String {
            c.query_row(
                "SELECT content_hash FROM memories WHERE seq = ?1",
                [seq],
                |r| r.get(0),
            )
            .unwrap()
        };
        for s in &seqs[..2] {
            assert!(mark_skipped_cloud_if_current(&c, VecKey::Memory(*s), &stamp(*s)).unwrap());
        }
        let global = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        assert_eq!(skipped_projects(&c).unwrap(), vec![global.id.clone()]);
        assert_eq!(
            embeddable_by_project(&c).unwrap(),
            vec![(global.id.clone(), 3, 0)]
        );
        // A limit is honoured; another project's rows are left alone.
        assert_eq!(
            requeue_skipped_cloud(&c, Some(&["other".into()]), 10).unwrap(),
            0
        );
        assert_eq!(
            requeue_skipped_cloud(&c, Some(std::slice::from_ref(&global.id)), 1).unwrap(),
            1
        );
        assert_eq!(skipped_cloud_count(&c).unwrap(), 1);
        assert_eq!(requeue_skipped_cloud(&c, None, 10).unwrap(), 1);
        assert!(skipped_projects(&c).unwrap().is_empty());
    }
}
