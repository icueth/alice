//! Legacy ids of imported records: where each one went (a collapsed duplicate points at its survivor),
//! and the one atomic, idempotent write the importer uses (`apply_batch`).

use crate::error::{AliceError, Result};
use crate::ids::now_rfc3339;
use crate::project::ProjectRef;
use crate::store::kg::{self, AssertFact, EntityMatcher, FactAction};
use crate::store::memories::{self, InsertOutcome, NewMemoryInput};
use crate::store::projects;
use crate::store::reviews::{self, NewReview};
use crate::store::sources::{self, ChunkInput, SourceInput};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyTarget {
    pub target_type: String,
    pub target_id: String,
}

/// Record where `legacy_id` went; the first writer wins.
pub fn legacy_put(
    conn: &Connection,
    legacy_id: &str,
    target_type: &str,
    target_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO legacy_ids (legacy_id, target_type, target_id) VALUES (?1, ?2, ?3)",
        params![legacy_id, target_type, target_id],
    )?;
    Ok(())
}

pub fn legacy_get(conn: &Connection, legacy_id: &str) -> Result<Option<LegacyTarget>> {
    Ok(conn
        .query_row(
            "SELECT target_type, target_id FROM legacy_ids WHERE legacy_id = ?1",
            [legacy_id],
            |r| {
                Ok(LegacyTarget {
                    target_type: r.get(0)?,
                    target_id: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// The subset of `ids` already stored, queried in chunks of at most 500.
pub fn legacy_existing(conn: &Connection, ids: &[String]) -> Result<HashSet<String>> {
    let mut found = HashSet::new();
    for part in ids.chunks(500) {
        let marks = vec!["?"; part.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT legacy_id FROM legacy_ids WHERE legacy_id IN ({marks})"
        ))?;
        for row in stmt.query_map(params_from_iter(part.iter()), |r| r.get::<_, String>(0))? {
            found.insert(row?);
        }
    }
    Ok(found)
}

pub struct ImportProject {
    pub slug: String,
    pub aliases: Vec<String>,
}

pub struct ImportMemory {
    pub project_slug: String,
    pub input: NewMemoryInput,
}

pub struct ImportSource {
    pub project_slug: String,
    pub source: SourceInput,
    pub chunks: Vec<ChunkInput>,
}

pub struct ImportFact {
    pub legacy_id: String,
    pub add: AssertFact,
    pub end_at: Option<String>,
    pub source_drawer_id: Option<String>,
}

/// One bounded batch of imported records.
///
/// Redaction is the importer's contract: every text in the batch (memory content and titles,
/// chunk text, fact parts) must already be redacted. Neither `apply_batch` nor the writer
/// redacts, and the event records counts only.
#[derive(Default)]
pub struct ImportBatch {
    pub projects: Vec<ImportProject>,
    pub memories: Vec<ImportMemory>,
    pub sources: Vec<ImportSource>,
    pub facts: Vec<ImportFact>,
    /// (from drawer id, to drawer id) becomes a `relates_to` link.
    pub links: Vec<(String, String)>,
    /// (collapsed drawer id, keeper drawer id).
    pub collapsed: Vec<(String, String)>,
    /// (fragment drawer id, id of the drawer its text was joined into): the fragment's id is
    /// filed against the same memory or chunk, in the same transaction as the joined drawer.
    pub joined: Vec<(String, String)>,
    /// Drawer ids of imported memories whose entity mentions to (re)link.
    pub link_mentions: Vec<String>,
    pub reviews: Vec<NewReview>,
    /// Written to the `meta` table.
    pub meta: Vec<(String, String)>,
    pub job_id: String,
}

/// Counts only: the batch holds raw drawer text, which `Debug` must never print.
impl std::fmt::Debug for ImportBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportBatch")
            .field("job_id", &self.job_id)
            .field("projects", &self.projects.len())
            .field("memories", &self.memories.len())
            .field("sources", &self.sources.len())
            .field("facts", &self.facts.len())
            .field("links", &self.links.len())
            .field("collapsed", &self.collapsed.len())
            .field("joined", &self.joined.len())
            .field("link_mentions", &self.link_mentions.len())
            .field("reviews", &self.reviews.len())
            .field("meta", &self.meta.len())
            .finish()
    }
}

impl ImportBatch {
    /// Records and text bytes the writer charges to its per-commit budget.
    pub fn weight(&self) -> (usize, usize) {
        let chunks: usize = self.sources.iter().map(|s| s.chunks.len()).sum();
        let records = self.memories.len()
            + chunks
            + self.facts.len()
            + self.links.len()
            + self.collapsed.len()
            + self.joined.len()
            + self.link_mentions.len()
            + self.reviews.len();
        let mut bytes: usize = self.memories.iter().map(|m| m.input.content.len()).sum();
        bytes += self
            .sources
            .iter()
            .flat_map(|s| s.chunks.iter())
            .map(|c| c.text.len())
            .sum::<usize>();
        bytes += self
            .facts
            .iter()
            .map(|f| f.add.subject.len() + f.add.predicate.len() + f.add.object.len())
            .sum::<usize>();
        (records, bytes)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOutcome {
    pub memories_new: usize,
    pub memories_duplicate: usize,
    pub chunks_inserted: usize,
    pub chunks_skipped: usize,
    pub sources: usize,
    pub facts_added: usize,
    pub facts_ended: usize,
    pub facts_skipped: usize,
    pub links_made: usize,
    pub links_dropped: usize,
    pub reviews_created: usize,
    pub reviews_skipped: usize,
    pub collapsed_recorded: usize,
    /// Fragment ids filed against the drawer they were joined into.
    #[serde(default)]
    pub joined_recorded: usize,
    pub projects_created: usize,
    pub mentions_linked: usize,
    /// Keeper not stored yet (the importer sends collapsed pairs last, so normally empty).
    pub deferred_collapsed: Vec<(String, String)>,
    pub warnings: Vec<String>,
    /// For example a chunk whose insert was ignored by an `ord` collision (never silent).
    pub errors: Vec<String>,
}

/// Text must already be redacted (see [`ImportBatch`]). Runs inside `SAVEPOINT import_batch`; on any error rolls back to it and returns the error.
pub fn apply_batch(
    conn: &Connection,
    b: &ImportBatch,
    hlc: &mut dyn FnMut() -> String,
) -> Result<BatchOutcome> {
    conn.execute_batch("SAVEPOINT import_batch")?;
    match apply_inner(conn, b, hlc) {
        Ok(out) => {
            conn.execute_batch("RELEASE import_batch")?;
            Ok(out)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO import_batch; RELEASE import_batch");
            Err(e)
        }
    }
}

fn project_of(conn: &Connection, slug: &str) -> Result<projects::ResolvedProject> {
    projects::resolve_slug(conn, slug)?.ok_or_else(|| {
        AliceError::invalid(format!(
            "import names project \"{slug}\" that does not exist"
        ))
    })
}

fn memory_target(conn: &Connection, drawer_id: &str) -> Result<Option<String>> {
    Ok(legacy_get(conn, drawer_id)?
        .filter(|t| t.target_type == "memory")
        .map(|t| t.target_id))
}

fn import_fact(
    conn: &Connection,
    f: &ImportFact,
    hlc: &mut dyn FnMut() -> String,
    out: &mut BatchOutcome,
) -> Result<()> {
    let mut add = f.add.clone();
    add.action = FactAction::Add;
    if let Some(d) = &f.source_drawer_id {
        add.source_memory_id = memory_target(conn, d)?;
    }
    let stamp = hlc();
    let res = kg::assert_fact(conn, &add, stamp.clone())?;
    let Some(rel) = res.relation_id else {
        out.facts_skipped += 1;
        return Ok(());
    };
    // `assert_fact` answers the open relation of the same triple when there is one; only a row
    // carrying this call's stamp was created here.
    let created: bool = conn.query_row(
        "SELECT hlc = ?2 FROM relations WHERE id = ?1",
        params![rel, stamp],
        |r| r.get(0),
    )?;
    legacy_put(conn, &f.legacy_id, "relation", &rel)?;
    if !created {
        out.facts_skipped += 1;
        if f.end_at.is_some() {
            out.warnings.push(format!(
                "fact {} is an ended interval of a fact that is open now; the open fact was left as it is",
                f.legacy_id
            ));
        }
        return Ok(());
    }
    if let Some(end) = &f.end_at {
        let mut end_fact = f.add.clone();
        end_fact.action = FactAction::End;
        end_fact.valid_from = Some(end.clone());
        // The relation is new, so it is the only open one of its triple.
        out.facts_ended += kg::assert_fact(conn, &end_fact, hlc())?.ended;
    }
    out.facts_added += 1;
    Ok(())
}

fn apply_inner(
    conn: &Connection,
    b: &ImportBatch,
    hlc: &mut dyn FnMut() -> String,
) -> Result<BatchOutcome> {
    let mut out = BatchOutcome::default();

    // 1. Projects and aliases.
    for p in &b.projects {
        let target = if p.slug == "global" {
            project_of(conn, "global")?
        } else if let Some(found) = projects::resolve_slug(conn, &p.slug)? {
            found
        } else {
            out.projects_created += 1;
            projects::upsert_project(
                conn,
                &ProjectRef {
                    key: format!("import:{}", p.slug),
                    slug: p.slug.clone(),
                    name: p.slug.clone(),
                    root: None,
                    remote: None,
                },
            )?
        };
        for alias in &p.aliases {
            if let Some(owner) = projects::resolve_slug(conn, alias)?
                && owner.id != target.id
            {
                out.warnings.push(format!(
                    "alias \"{alias}\" already points at project \"{}\"; left as it was",
                    owner.slug
                ));
                continue;
            }
            match projects::add_alias(conn, alias, &target.id) {
                Ok(()) => {}
                Err(AliceError::Invalid(m)) => out.warnings.push(format!("alias skipped: {m}")),
                Err(e) => return Err(e),
            }
        }
    }

    // 2. Memories.
    for m in &b.memories {
        let Some(legacy) = m.input.legacy_id.clone() else {
            return Err(AliceError::invalid("an imported memory has no legacy id"));
        };
        if legacy_get(conn, &legacy)?.is_some() {
            out.memories_duplicate += 1;
            continue;
        }
        let project = project_of(conn, &m.project_slug)?;
        let prepared = memories::prepare(m.input.clone(), &project, hlc())?;
        match memories::insert(conn, &prepared)? {
            InsertOutcome::New { id, .. } => {
                legacy_put(conn, &legacy, "memory", &id)?;
                conn.execute(
                    "UPDATE memories SET status = 'active', judge_state = 'skipped' WHERE id = ?1",
                    [&id],
                )?;
                out.memories_new += 1;
            }
            InsertOutcome::Duplicate { id } => {
                legacy_put(conn, &legacy, "memory", &id)?;
                out.memories_duplicate += 1;
            }
        }
    }

    // 3. Sources and chunks.
    for s in &b.sources {
        let project = project_of(conn, &s.project_slug)?;
        let ids: Vec<String> = s
            .chunks
            .iter()
            .filter_map(|c| c.legacy_id.clone())
            .collect();
        let stored = legacy_existing(conn, &ids)?;
        let fresh: Vec<ChunkInput> = s
            .chunks
            .iter()
            .filter(|c| !c.legacy_id.as_ref().is_some_and(|l| stored.contains(l)))
            .cloned()
            .collect();
        out.chunks_skipped += s.chunks.len() - fresh.len();
        let known: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sources WHERE type = ?1 AND uri = ?2)",
            params![s.source.source_type.as_str(), s.source.uri],
            |r| r.get(0),
        )?;
        let res = sources::ingest(conn, &project, &s.source, &fresh)?;
        if !known {
            out.sources += 1;
        }
        out.chunks_skipped += res.skipped;
        out.chunks_inserted += res.inserted;
        for c in &fresh {
            let Some(legacy) = &c.legacy_id else {
                continue;
            };
            let id: Option<String> = conn
                .query_row(
                    "SELECT id FROM chunks WHERE source_id = ?1 AND legacy_id = ?2",
                    params![res.source_id, legacy],
                    |r| r.get(0),
                )
                .optional()?;
            match id {
                Some(id) => legacy_put(conn, legacy, "chunk", &id)?,
                None => out.errors.push(format!(
                    "chunk {legacy} (ord {}) was not stored: its ord is taken in the source",
                    c.ord
                )),
            }
        }
    }

    // 3b. Fragments joined into a drawer of this batch point where that drawer went. Written in
    // the transaction of the drawer, so a stored drawer never lacks its fragments' ids.
    for (fragment, head) in &b.joined {
        match legacy_get(conn, head)? {
            Some(t) => {
                legacy_put(conn, fragment, &t.target_type, &t.target_id)?;
                out.joined_recorded += 1;
            }
            None => out.errors.push(format!(
                "fragment {fragment} was not filed: the drawer it was joined into ({head}) was not stored"
            )),
        }
    }

    // 4. Facts. A fact is stored only when this import creates its relation: a triple that is
    // already open (asserted by the owner, or by an earlier import line) is never changed, and
    // never ended by `end_at`. A bad fact (invalid time, empty part) is reported in `errors`
    // and does not fail the batch.
    for f in &b.facts {
        if legacy_get(conn, &f.legacy_id)?.is_some() {
            out.facts_skipped += 1;
            continue;
        }
        conn.execute_batch("SAVEPOINT import_fact")?;
        match import_fact(conn, f, hlc, &mut out) {
            Ok(()) => conn.execute_batch("RELEASE import_fact")?,
            Err(AliceError::Invalid(m)) => {
                conn.execute_batch("ROLLBACK TO import_fact; RELEASE import_fact")?;
                out.errors
                    .push(format!("fact {} was not stored: {m}", f.legacy_id));
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK TO import_fact; RELEASE import_fact");
                return Err(e);
            }
        }
    }

    // 5. Links.
    for (from, to) in &b.links {
        let (a, c) = (memory_target(conn, from)?, memory_target(conn, to)?);
        match (a, c) {
            (Some(a), Some(c)) if a != c => {
                let n = conn.execute(
                    "INSERT OR IGNORE INTO links (from_id, to_id, type, created_by, created_at)
                     VALUES (?1, ?2, 'relates_to', 'import', ?3)",
                    params![a, c, now_rfc3339()],
                )?;
                out.links_made += n;
            }
            _ => out.links_dropped += 1,
        }
    }

    // 6. Collapsed drawers point where their keeper went.
    for (gone, keeper) in &b.collapsed {
        match legacy_get(conn, keeper)? {
            Some(t) => {
                legacy_put(conn, gone, &t.target_type, &t.target_id)?;
                out.collapsed_recorded += 1;
            }
            None => out.deferred_collapsed.push((gone.clone(), keeper.clone())),
        }
    }

    // 7. Entity mentions.
    if !b.link_mentions.is_empty() {
        let matcher = EntityMatcher::load(conn)?;
        for d in &b.link_mentions {
            let Some(id) = memory_target(conn, d)? else {
                continue;
            };
            let (title, body): (String, String) = conn.query_row(
                "SELECT title, body FROM memories WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let found = matcher.find_in(&format!("{title}\n{body}"));
            out.mentions_linked += kg::link_mentions(conn, &id, &found)?;
        }
    }

    // 8. Reviews.
    for r in &b.reviews {
        let Some(key) = r.dedupe_key.as_deref() else {
            if reviews::create(conn, r)?.is_some() {
                out.reviews_created += 1;
            }
            continue;
        };
        let seen = format!("review:{key}");
        if reviews::was_rejected(conn, key)? || legacy_get(conn, &seen)?.is_some() {
            out.reviews_skipped += 1;
            continue;
        }
        match reviews::create(conn, r)? {
            Some(id) => {
                legacy_put(conn, &seen, "review", &id)?;
                out.reviews_created += 1;
            }
            None => out.reviews_skipped += 1,
        }
    }

    // 9. Meta.
    for (k, v) in &b.meta {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![k, v],
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::open_writer;

    fn conn() -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        (d, c)
    }

    #[test]
    fn legacy_ids_round_trip_and_ignore_repeats() {
        let (_d, conn) = conn();
        legacy_put(&conn, "drawer_a", "memory", "M1").unwrap();
        legacy_put(&conn, "drawer_a", "memory", "OTHER").unwrap(); // first writer wins
        let hit = legacy_get(&conn, "drawer_a").unwrap().unwrap();
        assert_eq!(
            (hit.target_type.as_str(), hit.target_id.as_str()),
            ("memory", "M1")
        );
        assert!(legacy_get(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn existing_finds_only_stored_ids_even_for_thousands() {
        let (_d, conn) = conn();
        for i in 0..1200 {
            legacy_put(&conn, &format!("d{i}"), "chunk", &format!("C{i}")).unwrap();
        }
        let ask: Vec<String> = (1100..1300).map(|i| format!("d{i}")).collect();
        let got = legacy_existing(&conn, &ask).unwrap();
        assert_eq!(got.len(), 100);
        assert!(got.contains("d1199") && !got.contains("d1200"));
    }

    use crate::model::{Kind, SourceType};
    use crate::store::kg::ObjectKind;

    fn hlc() -> impl FnMut() -> String {
        let mut n = 0u32;
        move || {
            n += 1;
            format!("{:013}-0000-dev", 1_700_000_000_000u64 + u64::from(n))
        }
    }

    fn mem(legacy: &str, kind: Kind, title: &str, body: &str) -> ImportMemory {
        ImportMemory {
            project_slug: "pg-ai".into(),
            input: NewMemoryInput {
                content: body.into(),
                kind: Some(kind),
                title: Some(title.into()),
                legacy_id: Some(legacy.into()),
                created_at: Some("2025-01-02T03:04:05.000Z".into()),
                ..Default::default()
            },
        }
    }

    fn chunk(ord: i64, legacy: &str) -> ChunkInput {
        ChunkInput {
            ord,
            text: format!("chunk text {legacy}"),
            authored_at: None,
            legacy_id: Some(legacy.into()),
        }
    }

    fn sample_batch() -> ImportBatch {
        ImportBatch {
            projects: vec![ImportProject {
                slug: "pg-ai".into(),
                aliases: vec!["wing-pg-ai".into()],
            }],
            memories: vec![
                mem(
                    "d1",
                    Kind::Decision,
                    "Use SQLite",
                    "We chose SQLite for storage",
                ),
                mem("d2", Kind::Note, "A note", "Something else entirely"),
            ],
            sources: vec![ImportSource {
                project_slug: "pg-ai".into(),
                source: SourceInput {
                    source_type: SourceType::Import,
                    uri: "file:///h/s.jsonl".into(),
                    ..Default::default()
                },
                chunks: vec![chunk(0, "c1"), chunk(1, "c2"), chunk(2, "c3")],
            }],
            job_id: "job1".into(),
            ..Default::default()
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_batch_creates_projects_aliases_active_memories_and_chunks_in_one_go() {
        let (_d, conn) = conn();
        let out = apply_batch(&conn, &sample_batch(), &mut hlc()).unwrap();
        assert_eq!(
            (
                out.memories_new,
                out.chunks_inserted,
                out.sources,
                out.projects_created
            ),
            (2, 3, 1, 1)
        );
        let m: (String, String) = conn
            .query_row(
                "SELECT judge_state, status FROM memories WHERE legacy_id='d1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            m,
            ("skipped".into(), "active".into()),
            "live at once, never judged"
        );
        assert!(
            legacy_get(&conn, "d1").unwrap().is_some()
                && legacy_get(&conn, "c2").unwrap().is_some()
        );
        assert_eq!(
            projects::resolve_slug(&conn, "wing_pg_ai")
                .unwrap()
                .unwrap()
                .slug,
            "pg-ai"
        );
    }

    #[test]
    fn applying_the_same_batch_twice_changes_nothing() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.facts.push(ImportFact {
            legacy_id: "kg:t1".into(),
            add: AssertFact {
                subject: "alice".into(),
                predicate: "uses".into(),
                object: "SQLite".into(),
                ..Default::default()
            },
            end_at: None,
            source_drawer_id: None,
        });
        b.reviews.push(NewReview {
            kind: reviews::ReviewType::Predicate,
            payload: serde_json::json!({"p": "x"}),
            confidence: None,
            dedupe_key: Some("predicate:x".into()),
        });
        apply_batch(&conn, &b, &mut hlc()).unwrap();
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!(
            (
                out.memories_new,
                out.chunks_inserted,
                out.facts_added,
                out.reviews_created
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM memories"), 2);
        assert_eq!(count(&conn, "SELECT count(*) FROM chunks"), 3);
        assert_eq!(count(&conn, "SELECT count(*) FROM relations"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM review_items"), 1);
    }

    #[test]
    fn identical_memories_collapse_and_both_old_ids_resolve_to_the_survivor() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.memories = vec![
            mem("x1", Kind::Note, "Same", "identical body"),
            mem("x2", Kind::Note, "Same", "identical body"),
        ];
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!((out.memories_new, out.memories_duplicate), (1, 1));
        assert_eq!(
            legacy_get(&conn, "x2").unwrap().unwrap().target_id,
            legacy_get(&conn, "x1").unwrap().unwrap().target_id
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM memories"), 1);
    }

    #[test]
    fn facts_keep_validity_and_resolve_their_source_drawer() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.facts.push(ImportFact {
            legacy_id: "kg:t1".into(),
            add: AssertFact {
                subject: "alice".into(),
                predicate: "uses".into(),
                object: "SQLite".into(),
                object_kind: ObjectKind::Entity,
                valid_from: Some("2024-01-01".into()),
                ..Default::default()
            },
            end_at: Some("2024-06-01".into()),
            source_drawer_id: Some("d1".into()),
        });
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!((out.facts_added, out.facts_ended), (1, 1));
        let rel = legacy_get(&conn, "kg:t1").unwrap().unwrap();
        assert_eq!(rel.target_type, "relation");
        let (to, src): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT valid_to, source_memory_id FROM relations WHERE id = ?1",
                [&rel.target_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(to.unwrap().starts_with("2024-06-01"));
        assert_eq!(
            src,
            Some(legacy_get(&conn, "d1").unwrap().unwrap().target_id)
        );
    }

    #[test]
    fn links_need_both_ends_and_unresolved_ones_are_counted() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.links = vec![("d1".into(), "d2".into()), ("d1".into(), "gone".into())];
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!((out.links_made, out.links_dropped), (1, 1));
        let by: String = conn
            .query_row(
                "SELECT created_by FROM links WHERE type='relates_to'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(by, "import");
    }

    #[test]
    fn reviews_are_not_recreated_after_the_owner_rejected_or_resolved_them() {
        let (_d, conn) = conn();
        let review = |key: &str| NewReview {
            kind: reviews::ReviewType::Predicate,
            payload: serde_json::json!({"p": key}),
            confidence: None,
            dedupe_key: Some(key.into()),
        };
        let mut b = ImportBatch {
            reviews: vec![review("predicate:had_bug")],
            job_id: "j".into(),
            ..Default::default()
        };
        assert_eq!(
            apply_batch(&conn, &b, &mut hlc()).unwrap().reviews_created,
            1
        );
        let id = reviews::find_open(&conn, "predicate:had_bug")
            .unwrap()
            .unwrap();
        reviews::mark_resolved(&conn, &id, "keep", &crate::store::events::Actor::User).unwrap();
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!((out.reviews_skipped, out.reviews_created), (1, 0));
        assert_eq!(reviews::count_open(&conn).unwrap(), 0);
        b.reviews.push(review("predicate:other"));
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!((out.reviews_skipped, out.reviews_created), (1, 1));
    }

    #[test]
    fn a_failing_row_rolls_the_whole_batch_back_even_on_a_plain_connection() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.memories
            .push(mem("big", Kind::Note, "Huge", &"x".repeat(70_000)));
        assert!(apply_batch(&conn, &b, &mut hlc()).is_err());
        assert!(projects::resolve_slug(&conn, "pg-ai").unwrap().is_none());
        assert_eq!(count(&conn, "SELECT count(*) FROM memories"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM legacy_ids"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM chunks"), 0);
    }

    #[test]
    fn an_ignored_chunk_insert_is_reported_not_silent() {
        let (_d, conn) = conn();
        let mut first = sample_batch();
        first.sources[0].chunks = vec![chunk(1, "other")];
        apply_batch(&conn, &first, &mut hlc()).unwrap();
        let out = apply_batch(&conn, &sample_batch(), &mut hlc()).unwrap();
        assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
        assert!(out.errors[0].contains("c2"));
        assert!(legacy_get(&conn, "c2").unwrap().is_none());
        assert!(
            legacy_get(&conn, "c1").unwrap().is_some()
                && legacy_get(&conn, "c3").unwrap().is_some()
        );
        assert_eq!(out.chunks_inserted, 2);
    }

    #[test]
    fn an_alias_pointing_elsewhere_is_kept_and_reported() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.projects.push(ImportProject {
            slug: "other".into(),
            aliases: vec!["wing-pg-ai".into()],
        });
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(
            projects::resolve_slug(&conn, "wing-pg-ai")
                .unwrap()
                .unwrap()
                .slug,
            "pg-ai"
        );
    }

    #[test]
    fn imported_memories_get_their_entity_mentions_when_asked() {
        let (_d, conn) = conn();
        kg::ensure_entity(&conn, "sqlite").unwrap();
        let mut b = sample_batch();
        b.link_mentions = vec!["d1".into()];
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!(out.mentions_linked, 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM mentions"), 1);
    }

    #[test]
    fn collapsed_ids_follow_their_keeper_and_unknown_keepers_are_deferred() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.collapsed = vec![("old".into(), "d1".into()), ("older".into(), "nope".into())];
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!(
            (out.collapsed_recorded, out.deferred_collapsed.len()),
            (1, 1)
        );
        assert_eq!(
            legacy_get(&conn, "old").unwrap().unwrap().target_id,
            legacy_get(&conn, "d1").unwrap().unwrap().target_id
        );
    }

    fn fact_of(legacy: &str, from: &str, end: Option<&str>) -> ImportFact {
        ImportFact {
            legacy_id: legacy.into(),
            add: AssertFact {
                subject: "alice".into(),
                predicate: "uses".into(),
                object: "SQLite".into(),
                object_kind: ObjectKind::Entity,
                valid_from: Some(from.into()),
                ..Default::default()
            },
            end_at: end.map(Into::into),
            source_drawer_id: None,
        }
    }

    fn open_relations(conn: &Connection) -> i64 {
        count(
            conn,
            "SELECT count(*) FROM relations WHERE valid_to IS NULL",
        )
    }

    #[test]
    fn an_ended_interval_imported_after_the_open_fact_leaves_the_live_fact_open() {
        let (_d, conn) = conn();
        let b = ImportBatch {
            facts: vec![
                fact_of("kg:now", "2025-01-01", None),
                fact_of("kg:old", "2023-01-01", Some("2024-01-01")),
            ],
            job_id: "j".into(),
            ..Default::default()
        };
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!(
            (out.facts_added, out.facts_ended, out.facts_skipped),
            (1, 0, 1)
        );
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(open_relations(&conn), 1, "the live fact stays open");
        assert!(legacy_get(&conn, "kg:old").unwrap().is_some());
    }

    #[test]
    fn a_pre_existing_user_fact_is_not_ended_by_an_import() {
        let (_d, conn) = conn();
        kg::assert_fact(&conn, &fact_of("", "2025-01-01", None).add, "h1".into()).unwrap();
        let b = ImportBatch {
            facts: vec![fact_of("kg:t", "2023-01-01", Some("2024-01-01"))],
            job_id: "j".into(),
            ..Default::default()
        };
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!(
            (out.facts_added, out.facts_ended, out.facts_skipped),
            (0, 0, 1)
        );
        assert_eq!(open_relations(&conn), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM relations"), 1);
    }

    #[test]
    fn a_bad_fact_is_reported_and_the_rest_of_the_batch_lands() {
        let (_d, conn) = conn();
        let mut b = sample_batch();
        b.facts = vec![
            fact_of("kg:bad", "2023-01-01", Some("not a time")),
            fact_of("kg:ok", "2023-01-01", Some("2024-01-01")),
        ];
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
        assert!(out.errors[0].contains("kg:bad"));
        assert_eq!(
            (out.facts_added, out.facts_ended, out.memories_new),
            (1, 1, 2)
        );
        assert!(legacy_get(&conn, "kg:bad").unwrap().is_none());
        assert_eq!(count(&conn, "SELECT count(*) FROM relations"), 1);
    }

    #[test]
    fn a_legacy_review_marker_skips_a_review_that_was_resolved_without_rejecting() {
        let (_d, conn) = conn();
        let b = ImportBatch {
            reviews: vec![NewReview {
                kind: reviews::ReviewType::Predicate,
                payload: serde_json::json!({"p": "y"}),
                confidence: None,
                dedupe_key: Some("predicate:y".into()),
            }],
            job_id: "j".into(),
            ..Default::default()
        };
        apply_batch(&conn, &b, &mut hlc()).unwrap();
        let id = reviews::find_open(&conn, "predicate:y").unwrap().unwrap();
        reviews::mark_resolved(&conn, &id, "map", &crate::store::events::Actor::User).unwrap();
        assert!(!reviews::was_rejected(&conn, "predicate:y").unwrap());
        let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
        assert_eq!((out.reviews_skipped, out.reviews_created), (1, 0));
        assert_eq!(count(&conn, "SELECT count(*) FROM review_items"), 1);
    }

    #[test]
    fn debug_of_a_batch_shows_counts_and_never_text() {
        let shown = format!("{:?}", sample_batch());
        assert!(shown.contains("memories: 2"), "{shown}");
        assert!(
            !shown.contains("SQLite") && !shown.contains("chunk text"),
            "{shown}"
        );
    }
}
