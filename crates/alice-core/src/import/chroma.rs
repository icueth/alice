//! The `chroma` backend: documents and metadata live in `chroma.sqlite3`; the HNSW folders hold
//! vectors only and are ignored.

use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, params_from_iter};

use super::palace::{Backend, Drawer, MetaValue, PalaceInfo, PalaceReader, meta_from_pairs};
use crate::error::{AliceError, Result};

/// Metadata key that holds the document text.
const DOCUMENT_KEY: &str = "chroma:document";

pub struct ChromaReader {
    info: PalaceInfo,
    conn: Connection,
    /// The METADATA segment of the drawers collection.
    segment: String,
}

fn text(r: ValueRef<'_>) -> String {
    match r {
        ValueRef::Text(b) | ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Null => String::new(),
    }
}

impl ChromaReader {
    pub fn open(path: &Path, collection: &str) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM collections WHERE name = ?1 ORDER BY id LIMIT 1",
                [collection],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some(collection_id) = id else {
            let mut stmt = conn.prepare("SELECT name FROM collections ORDER BY name")?;
            let names = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let names = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            return Err(AliceError::invalid(format!(
                "chroma palace has no collection \"{collection}\" (it has: {names}); set collection_name in the palace's config or pass --collection"
            )));
        };
        let segment: String = conn
            .query_row(
                "SELECT id FROM segments WHERE scope = 'METADATA' AND collection = ?1 ORDER BY id LIMIT 1",
                [&collection_id],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => AliceError::invalid(format!(
                    "chroma collection \"{collection}\" has no metadata segment; the palace may be damaged"
                )),
                other => other.into(),
            })?;
        let drawers: i64 = conn.query_row(
            "SELECT count(*) FROM embeddings WHERE segment_id = ?1",
            [&segment],
            |r| r.get(0),
        )?;
        Ok(Self {
            info: PalaceInfo {
                backend: Backend::Chroma,
                path: path.to_path_buf(),
                drawers: drawers.max(0) as u64,
            },
            conn,
            segment,
        })
    }

    /// Folds the metadata rows of the given (internal id, drawer id) pairs into drawers, in order.
    fn load(&self, ids: &[(i64, String)]) -> Result<Vec<Drawer>> {
        let mut out = Vec::with_capacity(ids.len());
        let mut stmt = self.conn.prepare_cached(
            "SELECT key, string_value, int_value, float_value, bool_value
             FROM embedding_metadata WHERE id = ?1 ORDER BY key",
        )?;
        for (internal, id) in ids {
            let mut doc = String::new();
            let mut pairs: Vec<(String, MetaValue)> = Vec::new();
            let mut rows = stmt.query([internal])?;
            while let Some(r) = rows.next()? {
                let key: String = text(r.get_ref(0)?);
                let value = match (r.get_ref(1)?, r.get_ref(2)?, r.get_ref(3)?, r.get_ref(4)?) {
                    (ValueRef::Text(_) | ValueRef::Blob(_), ..) => {
                        MetaValue::Str(text(r.get_ref(1)?))
                    }
                    (_, ValueRef::Integer(n), ..) => MetaValue::Int(n),
                    (_, _, ValueRef::Real(f), _) => MetaValue::Float(f),
                    (.., ValueRef::Integer(b)) => MetaValue::Bool(b != 0),
                    _ => continue,
                };
                if key == DOCUMENT_KEY {
                    if let MetaValue::Str(s) = value {
                        doc = s;
                    }
                } else {
                    pairs.push((key, value));
                }
            }
            out.push(Drawer {
                id: id.clone(),
                text: doc,
                meta: meta_from_pairs(pairs.into_iter()),
            });
        }
        Ok(out)
    }

    fn id_page(&self, after: &str, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, embedding_id FROM embeddings
             WHERE segment_id = ?1 AND embedding_id > ?2
             ORDER BY embedding_id LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![self.segment, after, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, text(r.get_ref(1)?)))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

impl PalaceReader for ChromaReader {
    fn info(&self) -> &PalaceInfo {
        &self.info
    }

    fn page(&self, after: Option<&str>, limit: usize, since: Option<&str>) -> Result<Vec<Drawer>> {
        // Same rule as the sqlite_exact reader: a drawer without `filed_at` is kept; the filter runs
        // here, after the fold, so paging goes on until `limit` drawers pass or the ids run out.
        let mut out: Vec<Drawer> = Vec::new();
        let mut cursor = after.unwrap_or("").to_string();
        while out.len() < limit {
            let ids = self.id_page(&cursor, limit)?;
            let Some(last) = ids.last() else { break };
            cursor = last.1.clone();
            let exhausted = ids.len() < limit;
            for d in self.load(&ids)? {
                let keep = match (since, d.meta.filed_at.as_deref()) {
                    (Some(s), Some(f)) => f >= s,
                    _ => true,
                };
                if keep && out.len() < limit {
                    out.push(d);
                }
            }
            if exhausted {
                break;
            }
        }
        Ok(out)
    }

    fn get(&self, ids: &[String]) -> Result<Vec<Drawer>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, embedding_id FROM embeddings WHERE segment_id = ? AND embedding_id IN ({marks}) ORDER BY embedding_id"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = std::iter::once(self.segment.clone()).chain(chunk.iter().cloned());
            let found = stmt
                .query_map(params_from_iter(params), |r| {
                    Ok((r.get::<_, i64>(0)?, text(r.get_ref(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.extend(self.load(&found)?);
        }
        Ok(out)
    }

    fn wing_counts(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(CAST(COALESCE(m.string_value, m.int_value, m.float_value) AS TEXT), ''),
                    count(*)
             FROM embeddings e
             LEFT JOIN embedding_metadata m ON m.id = e.id AND m.key = 'wing'
             WHERE e.segment_id = ?1 GROUP BY 1 ORDER BY 1",
        )?;
        let rows = stmt.query_map([&self.segment], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::fixture::{DrawerSpec, build_chroma, build_chroma_in};
    use crate::import::palace::{DrawerMeta, open_snapshot, snapshot};

    fn sample_specs(n: usize) -> Vec<DrawerSpec> {
        (0..n)
            .map(|i| DrawerSpec {
                id: format!("drawer_{:04}", (i * 7919) % 10007),
                text: format!("text {i} {}", "z".repeat(50)),
                meta: DrawerMeta {
                    wing: if i % 3 == 0 { "alpha" } else { "beta" }.into(),
                    room: "r".into(),
                    // Only some carry the optional keys, so missing keys stay None.
                    hall: (i % 2 == 0).then(|| "h".into()),
                    filed_at: (i % 5 != 0).then(|| format!("2026-0{}-01T00:00:00", 1 + i % 9)),
                    chunk_index: (i % 4 != 0).then_some(i as i64),
                    ..Default::default()
                },
            })
            .collect()
    }

    fn all(r: &dyn PalaceReader, since: Option<&str>, step: usize) -> Vec<Drawer> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let p = r.page(after.as_deref(), step, since).unwrap();
            if p.is_empty() {
                break;
            }
            after = Some(p.last().unwrap().id.clone());
            all.extend(p);
        }
        all
    }

    fn open(specs: &[DrawerSpec]) -> (tempfile::TempDir, Box<dyn PalaceReader>) {
        let src = tempfile::tempdir().unwrap();
        build_chroma(src.path(), specs);
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        let r = open_snapshot(&snap, None).unwrap();
        // The snapshot dir must outlive the reader's file; keep it alive via the returned guard.
        (dst, r)
    }

    #[test]
    fn chroma_drawers_come_out_like_sqlite_exact_ones() {
        let src = tempfile::tempdir().unwrap();
        let specs = sample_specs(120);
        build_chroma(src.path(), &specs);
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        assert_eq!(snap.backend, Backend::Chroma);
        let r = open_snapshot(&snap, None).unwrap();
        assert_eq!(r.info().drawers, 120);
        let got = all(r.as_ref(), None, 50);
        assert_eq!(got.len(), 120);
        let mut ids: Vec<_> = got.iter().map(|d| d.id.clone()).collect();
        let sorted = {
            let mut s = ids.clone();
            s.sort();
            s
        };
        assert_eq!(ids, sorted, "pages are in id order");
        ids.dedup();
        assert_eq!(ids.len(), 120);
        for i in [0usize, 4, 7, 8, 119] {
            let a = got.iter().find(|d| d.id == specs[i].id).unwrap();
            assert_eq!(a.text, specs[i].text);
            assert_eq!(a.meta, specs[i].meta); // int chunk_index, string wing, missing keys stay None
        }
    }

    #[test]
    fn a_chroma_folder_without_the_drawers_collection_names_the_collections_it_has() {
        let src = tempfile::tempdir().unwrap();
        build_chroma_in(src.path(), "other", &sample_specs(3));
        let dst = tempfile::tempdir().unwrap();
        let snap = snapshot(src.path(), dst.path(), None).unwrap();
        let e = match open_snapshot(&snap, None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("must fail"),
        };
        assert!(
            e.contains("mempalace_drawers") && e.contains("other"),
            "{e}"
        );
        // Naming the right collection works.
        assert_eq!(
            open_snapshot(&snap, Some("other")).unwrap().info().drawers,
            3
        );
    }

    #[test]
    fn since_and_wing_counts_work_on_chroma_too() {
        let mk = |id: &str, wing: &str, f: Option<&str>| DrawerSpec {
            id: id.into(),
            text: id.into(),
            meta: DrawerMeta {
                wing: wing.into(),
                room: "r".into(),
                filed_at: f.map(Into::into),
                ..Default::default()
            },
        };
        let (_g, r) = open(&[
            mk("a", "w1", Some("2026-01-01T10:00:00Z")),
            mk("b", "w1", Some("2026-06-01T10:00:00")),
            mk("c", "w2", None),
        ]);
        let ids = |v: Vec<Drawer>| v.into_iter().map(|d| d.id).collect::<Vec<_>>();
        assert_eq!(
            ids(r.page(None, 10, Some("2026-03-01")).unwrap()),
            ["b", "c"]
        );
        assert_eq!(r.page(None, 10, None).unwrap().len(), 3);
        assert_eq!(ids(r.page(None, 1, Some("2026-03-01")).unwrap()), ["b"]);
        assert_eq!(
            ids(r.page(Some("b"), 5, Some("2026-03-01")).unwrap()),
            ["c"]
        );
        assert_eq!(
            r.wing_counts().unwrap(),
            vec![("w1".to_string(), 2), ("w2".to_string(), 1)]
        );
        let got = r.get(&["b".into(), "missing".into()]).unwrap();
        assert_eq!(ids(got), ["b"]);
    }

    #[test]
    fn since_pages_past_filtered_out_drawers_until_the_limit_is_met() {
        // 60 old drawers sort before 3 new ones; a page of 2 must still find them.
        let mut specs: Vec<DrawerSpec> = (0..60)
            .map(|i| DrawerSpec {
                id: format!("a{i:03}"),
                text: "old".into(),
                meta: DrawerMeta {
                    wing: "w".into(),
                    room: "r".into(),
                    filed_at: Some("2020-01-01".into()),
                    ..Default::default()
                },
            })
            .collect();
        for i in 0..3 {
            let mut s = specs[0].clone();
            s.id = format!("z{i}");
            s.meta.filed_at = Some("2026-05-01".into());
            specs.push(s);
        }
        let (_g, r) = open(&specs);
        let p = all(r.as_ref(), Some("2026-01-01"), 2);
        assert_eq!(p.len(), 3);
    }
}
