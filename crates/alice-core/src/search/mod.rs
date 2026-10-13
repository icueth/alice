//! Recall pipeline: candidates -> fusion -> filters and boosts -> raw fallback -> related.

pub mod fusion;
pub mod lexical;
pub mod pack;

use crate::api::{Hit, MatchVia, RelatedRef, Tier};
use crate::embed::Embedder;
use crate::error::{AliceError, Result};
use crate::model::{Kind, Memory, RAW_HALF_LIFE_DAYS, Status, limits};
use crate::normalize::{nfc, normalize_topic};
use crate::store::embeddings::VecKey;
use crate::store::projects::GLOBAL_ID;
use crate::store::sources::ChunkRow;
use crate::store::{memories, sources};
use crate::vector::{KeyFilter, VectorIndex};
use chrono::{DateTime, SecondsFormat, Utc};
use fusion::{BoostInput, ItemRef, Weights, boost, rrf};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// Candidates taken from each list.
const CANDIDATES: usize = 100;
/// Candidates per list when filters will drop many of them.
const FILTERED_CANDIDATES: usize = 400;
/// A word list shorter than this also brings in trigram candidates (spec §4.4 step 1).
const SHORT_LIST: usize = 10;
const SNIPPET_CHARS: usize = 240;
const MAX_RELATED: usize = 5;

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    /// Restrict to this project id.
    pub project_filter_id: Option<String>,
    /// Boost this project id.
    pub current_project_id: Option<String>,
    pub topics: Vec<String>,
    pub kinds: Vec<Kind>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub include_raw: bool,
    pub limit: usize,
    pub relevance_floor: f32,
    pub raw_min_results: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOutput {
    pub hits: Vec<Hit>,
    pub related: Vec<RelatedRef>,
    pub raw_fallback: bool,
    /// Sequence numbers of returned memories (for access counting).
    pub memory_seqs: Vec<i64>,
}

#[derive(Default)]
struct Signals {
    via: HashSet<MatchVia>,
    lexical: bool,
    similarity: f32,
}

type Candidates = (HashMap<ItemRef, f64>, HashMap<ItemRef, Signals>);

/// What every candidate list needs.
struct Query<'a> {
    conn: &'a Connection,
    index: Option<&'a VectorIndex>,
    vector: Option<Vec<f32>>,
    text: &'a str,
    k: usize,
}

impl Query<'_> {
    /// Nearest neighbours in one sub-index. A failure is logged and means no vector candidates.
    fn vector_hits(&self, filter: KeyFilter) -> Vec<(VecKey, f32)> {
        let (Some(index), Some(v)) = (self.index, self.vector.as_deref()) else {
            return Vec::new();
        };
        index.search(v, self.k, filter).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "vector search failed; using word matches only");
            Vec::new()
        })
    }

    fn knowledge(&self) -> Candidates {
        let (c, q, k) = (self.conn, self.text, self.k);
        let words = lexical::memory_words(c, q, k);
        let use_trigram = lexical::should_use_trigram(q) || words.len() < SHORT_LIST;
        let mut lists = vec![
            (MatchVia::Words, refs(words, ItemRef::Memory)),
            (
                MatchVia::Code,
                refs(lexical::memory_code(c, q, k), ItemRef::Memory),
            ),
        ];
        if use_trigram {
            lists.push((
                MatchVia::Words,
                refs(lexical::memory_trigram(c, q, k), ItemRef::Memory),
            ));
        }
        let like = lexical::memory_like(c, q, k);
        if !like.is_empty() {
            lists.push((MatchVia::Words, refs(like, ItemRef::Memory)));
        }
        let vector = self
            .vector_hits(KeyFilter::Memories)
            .into_iter()
            .filter_map(|(key, sim)| match key {
                VecKey::Memory(s) => Some((ItemRef::Memory(s), sim)),
                VecKey::Chunk(_) => None,
            })
            .collect();
        collect(lists, vector)
    }

    fn raw(&self) -> Candidates {
        let (c, q, k) = (self.conn, self.text, self.k);
        let words = lexical::chunk_words(c, q, k);
        let use_trigram = lexical::should_use_trigram(q) || words.len() < SHORT_LIST;
        let mut lists = vec![
            (MatchVia::Words, refs(words, ItemRef::Chunk)),
            (
                MatchVia::Code,
                refs(lexical::chunk_code(c, q, k), ItemRef::Chunk),
            ),
        ];
        if use_trigram {
            lists.push((
                MatchVia::Words,
                refs(lexical::chunk_trigram(c, q, k), ItemRef::Chunk),
            ));
        }
        let vector = self
            .vector_hits(KeyFilter::Chunks)
            .into_iter()
            .filter_map(|(key, sim)| match key {
                VecKey::Chunk(s) => Some((ItemRef::Chunk(s), sim)),
                VecKey::Memory(_) => None,
            })
            .collect();
        collect(lists, vector)
    }
}

fn refs(seqs: Vec<i64>, f: fn(i64) -> ItemRef) -> Vec<ItemRef> {
    seqs.into_iter().map(f).collect()
}

/// Fuse the lexical lists and the vector list, remembering how each item was found.
fn collect(lists: Vec<(MatchVia, Vec<ItemRef>)>, vector: Vec<(ItemRef, f32)>) -> Candidates {
    let mut signals: HashMap<ItemRef, Signals> = HashMap::new();
    let mut ranked: Vec<Vec<ItemRef>> = Vec::with_capacity(lists.len() + 1);
    for (via, list) in lists {
        for item in &list {
            let s = signals.entry(*item).or_default();
            s.via.insert(via);
            s.lexical = true;
        }
        ranked.push(list);
    }
    for (item, sim) in &vector {
        let s = signals.entry(*item).or_insert_with(|| Signals {
            similarity: f32::NEG_INFINITY,
            ..Default::default()
        });
        s.via.insert(MatchVia::Vector);
        s.similarity = s.similarity.max(*sim);
    }
    ranked.push(vector.into_iter().map(|(i, _)| i).collect());
    (rrf(&ranked), signals)
}

fn ordered_via(s: &Signals) -> Vec<MatchVia> {
    [MatchVia::Vector, MatchVia::Words, MatchVia::Code]
        .into_iter()
        .filter(|v| s.via.contains(v))
        .collect()
}

fn age_days(created_at: &str, now: DateTime<Utc>) -> f64 {
    DateTime::parse_from_rfc3339(created_at)
        .map(|t| (now - t.with_timezone(&Utc)).num_milliseconds() as f64 / 86_400_000.0)
        .unwrap_or(0.0)
}

/// A `since`/`before` bound comparable with stored timestamps as text: RFC 3339 input becomes
/// UTC with milliseconds; a date (YYYY-MM-DD) or anything else is compared as given.
fn time_bound(s: &Option<String>) -> Option<String> {
    let s = s.as_deref()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(match DateTime::parse_from_rfc3339(s) {
        Ok(t) => t
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        Err(_) => s.to_string(),
    })
}

struct Window {
    since: Option<String>,
    before: Option<String>,
}

impl Window {
    fn contains(&self, created_at: &str) -> bool {
        self.since.as_deref().is_none_or(|s| created_at >= s)
            && self.before.as_deref().is_none_or(|b| created_at < b)
    }
}

enum Row {
    Memory { seq: i64, memory: Memory },
    Chunk { chunk: ChunkRow, project: String },
}

struct Scored {
    row: Row,
    score: f64,
    relevant: bool,
    matched: Vec<MatchVia>,
}

impl Scored {
    fn is_knowledge(&self) -> bool {
        matches!(self.row, Row::Memory { .. })
    }

    /// Ties go to knowledge, then to the newer item.
    fn tie_key(&self) -> (bool, i64) {
        match &self.row {
            Row::Memory { seq, .. } => (true, *seq),
            Row::Chunk { chunk, .. } => (false, chunk.seq),
        }
    }

    fn into_hit(self, terms: &[String]) -> Hit {
        match self.row {
            Row::Memory { memory: m, .. } => Hit {
                snippet: pack::make_snippet(&m.body, terms, SNIPPET_CHARS),
                status: (m.status != Status::Active && m.status != Status::Pending)
                    .then_some(m.status),
                id: m.id,
                tier: Tier::Knowledge,
                kind: Some(m.kind),
                project: m.project,
                title: m.title,
                body: Some(m.body),
                created_at: m.created_at,
                score: self.score,
                matched: self.matched,
            },
            Row::Chunk { chunk: c, project } => {
                let created_at = c.authored_at.unwrap_or(c.created_at);
                Hit {
                    snippet: pack::make_snippet(&c.text, terms, SNIPPET_CHARS),
                    id: c.id,
                    tier: Tier::Raw,
                    kind: None,
                    project,
                    title: format!("raw · {}", c.source_id),
                    body: Some(c.text),
                    created_at,
                    score: self.score,
                    matched: self.matched,
                    status: None,
                }
            }
        }
    }
}

/// Run the recall pipeline. Fails only for an empty query or a database error; a missing
/// index (`None` while it is rebuilt) or a failing embedder leaves the word and code lists.
pub fn run(
    conn: &Connection,
    index: Option<&VectorIndex>,
    embedder: &dyn Embedder,
    weights: &Weights,
    p: &SearchParams,
    now: DateTime<Utc>,
) -> Result<SearchOutput> {
    let normalized = nfc(p.query.trim());
    if normalized.is_empty() {
        return Err(AliceError::invalid(
            "query is empty; pass keywords or a question",
        ));
    }
    let limit = p.limit.clamp(1, limits::MAX_RECALL_LIMIT);
    let topics: Vec<String> = p.topics.iter().filter_map(|t| normalize_topic(t)).collect();
    let window = Window {
        since: time_bound(&p.since),
        before: time_bound(&p.before),
    };
    let filtered = p.project_filter_id.is_some()
        || !p.kinds.is_empty()
        || !topics.is_empty()
        || window.since.is_some()
        || window.before.is_some();
    let vector = match index {
        Some(_) => embedder
            .embed_query(&normalized)
            .inspect_err(
                |e| tracing::warn!(error = %e, "query embedding failed; using word matches only"),
            )
            .ok(),
        None => None,
    };
    let query = Query {
        conn,
        index,
        vector,
        text: &normalized,
        k: if filtered {
            FILTERED_CANDIDATES
        } else {
            CANDIDATES
        },
    };
    let filters = Filters {
        p,
        topics: &topics,
        window: &window,
        now,
        weights,
    };

    let (mut scores, mut signals) = query.knowledge();
    if p.include_raw {
        // Both tiers ranked together; the raw tier weight (0.6) keeps knowledge ahead on ties.
        let (s, sig) = query.raw();
        scores.extend(s);
        signals.extend(sig);
    }
    // Only relevant results are returned: a lexical match or vector similarity >= relevance_floor.
    let mut ranked: Vec<Scored> = filters
        .score(conn, &scores, &signals)?
        .into_iter()
        .filter(|s| s.relevant)
        .collect();
    let mut raw_fallback = false;
    let knowledge_relevant = ranked.iter().filter(|s| s.is_knowledge()).count();
    if !p.include_raw && knowledge_relevant < p.raw_min_results {
        let (s, sig) = query.raw();
        let extra: Vec<Scored> = filters
            .score(conn, &s, &sig)?
            .into_iter()
            .filter(|s| s.relevant)
            .collect();
        raw_fallback = !extra.is_empty();
        // Spec §4.4 step 5: appended after the knowledge results.
        ranked.extend(extra);
    }
    ranked.truncate(limit);

    let mut terms = lexical::word_terms(&normalized);
    terms.extend(lexical::variant_terms(&normalized));
    let mut memory_seqs = Vec::new();
    let mut memory_ids = Vec::new();
    let hits: Vec<Hit> = ranked
        .into_iter()
        .map(|s| {
            if let Row::Memory { seq, memory } = &s.row {
                memory_seqs.push(*seq);
                memory_ids.push(memory.id.clone());
            }
            s.into_hit(&terms)
        })
        .collect();
    let related = related_for(conn, &memory_ids)?;
    Ok(SearchOutput {
        hits,
        related,
        raw_fallback,
        memory_seqs,
    })
}

struct Filters<'a> {
    p: &'a SearchParams,
    topics: &'a [String],
    window: &'a Window,
    now: DateTime<Utc>,
    weights: &'a Weights,
}

impl Filters<'_> {
    /// Fetch the rows, drop trashed and filtered-out items, apply boosts; best first.
    fn score(
        &self,
        conn: &Connection,
        scores: &HashMap<ItemRef, f64>,
        signals: &HashMap<ItemRef, Signals>,
    ) -> Result<Vec<Scored>> {
        let p = self.p;
        let mut mem_seqs: Vec<i64> = Vec::new();
        let mut chunk_seqs: Vec<i64> = Vec::new();
        for item in scores.keys() {
            match item {
                ItemRef::Memory(s) => mem_seqs.push(*s),
                ItemRef::Chunk(s) => chunk_seqs.push(*s),
            }
        }
        if mem_seqs.is_empty() && chunk_seqs.is_empty() {
            return Ok(Vec::new());
        }
        let slug_by_id = project_slugs(conn)?;
        let id_by_slug: HashMap<&str, &str> = slug_by_id
            .iter()
            .map(|(id, slug)| (slug.as_str(), id.as_str()))
            .collect();
        let empty = Signals::default();
        let mut out: Vec<Scored> = Vec::new();
        let mut push = |item: ItemRef, row: Row, input: BoostInput| {
            let s = signals.get(&item).unwrap_or(&empty);
            let score = scores[&item] * boost(self.weights, &input);
            out.push(Scored {
                row,
                score,
                relevant: s.lexical || s.similarity >= p.relevance_floor,
                matched: ordered_via(s),
            });
        };
        for (seq, m) in memories::get_by_seqs(conn, &mem_seqs)? {
            if m.status == Status::Trashed {
                continue;
            }
            let project_id = id_by_slug.get(m.project.as_str()).copied().unwrap_or("");
            if p.project_filter_id
                .as_deref()
                .is_some_and(|f| f != project_id)
                || (!p.kinds.is_empty() && !p.kinds.contains(&m.kind))
                || (!self.topics.is_empty() && !self.topics.iter().any(|t| m.topics.contains(t)))
                || !self.window.contains(&m.created_at)
            {
                continue;
            }
            let input = BoostInput {
                raw: false,
                same_project: p.current_project_id.as_deref() == Some(project_id),
                is_global: project_id == GLOBAL_ID,
                age_days: age_days(&m.created_at, self.now),
                half_life_days: m.kind.half_life_days(),
                importance: m.importance,
                access_count: m.access_count,
                status: m.status,
            };
            push(ItemRef::Memory(seq), Row::Memory { seq, memory: m }, input);
        }
        for c in sources::get_chunks_by_seqs(conn, &chunk_seqs)? {
            // Chunks carry no kind or topics: such filters leave only knowledge results.
            if p.project_filter_id
                .as_deref()
                .is_some_and(|f| f != c.project_id)
                || !p.kinds.is_empty()
                || !self.topics.is_empty()
            {
                continue;
            }
            let when = c.authored_at.as_deref().unwrap_or(&c.created_at);
            if !self.window.contains(when) {
                continue;
            }
            let input = BoostInput {
                raw: true,
                same_project: p.current_project_id.as_deref() == Some(c.project_id.as_str()),
                is_global: c.project_id == GLOBAL_ID,
                age_days: age_days(when, self.now),
                half_life_days: Some(RAW_HALF_LIFE_DAYS),
                importance: 3,
                access_count: 0,
                status: Status::Active,
            };
            let project = slug_by_id.get(&c.project_id).cloned().unwrap_or_default();
            push(
                ItemRef::Chunk(c.seq),
                Row::Chunk { chunk: c, project },
                input,
            );
        }
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.tie_key().cmp(&a.tie_key()))
        });
        Ok(out)
    }
}

fn project_slugs(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare_cached("SELECT id, slug FROM projects")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn related_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

/// Up to five memories one hop from the top three results, through links or shared entities.
/// Results themselves and trashed memories are left out.
fn related_for(conn: &Connection, ids: &[String]) -> Result<Vec<RelatedRef>> {
    let exclude: HashSet<&String> = ids.iter().collect();
    let mut out: Vec<RelatedRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |out: &mut Vec<RelatedRef>,
                   (rid, kind, title, via): (String, String, String, String)| {
        if !exclude.contains(&rid) && seen.insert(rid.clone()) {
            out.push(RelatedRef {
                id: rid,
                kind: Kind::parse(&kind).unwrap_or(Kind::Note),
                title,
                via,
            });
        }
    };
    for id in ids.iter().take(3) {
        let mut stmt = conn.prepare_cached(
            "SELECT m.id, m.kind, m.title, l.type FROM links l
             JOIN memories m ON m.id = CASE WHEN l.from_id = ?1 THEN l.to_id ELSE l.from_id END
             WHERE (l.from_id = ?1 OR l.to_id = ?1) AND m.status <> 'trashed'
             ORDER BY l.created_at DESC LIMIT 5",
        )?;
        for r in stmt.query_map([id], related_row)? {
            add(&mut out, r?);
        }
        let mut stmt = conn.prepare_cached(
            "SELECT m.id, m.kind, m.title, 'entity:' || e.name FROM mentions a
             JOIN mentions b ON a.entity_id = b.entity_id AND b.memory_id <> a.memory_id
             JOIN memories m ON m.id = b.memory_id JOIN entities e ON e.id = a.entity_id
             WHERE a.memory_id = ?1 AND m.status <> 'trashed'
             ORDER BY m.importance DESC, m.updated_at DESC LIMIT 5",
        )?;
        for r in stmt.query_map([id], related_row)? {
            add(&mut out, r?);
        }
    }
    out.truncate(MAX_RELATED);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{MatchVia, Tier};
    use crate::embed::{Embedder, FakeEmbedder, memory_embed_text};
    use crate::model::{Kind, Status};
    use crate::project::ProjectRef;
    use crate::store::embeddings::VecKey;
    use crate::store::{memories, open_writer, projects, sources};
    use crate::vector::VectorIndex;

    struct Fx {
        _d: tempfile::TempDir,
        c: rusqlite::Connection,
        idx: VectorIndex,
        e: FakeEmbedder,
        app: projects::ResolvedProject,
        other: projects::ResolvedProject,
    }

    fn fx() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let mk = |k: &str, s: &str| ProjectRef {
            key: k.into(),
            slug: s.into(),
            name: s.into(),
            root: None,
            remote: None,
        };
        let app = projects::upsert_project(&c, &mk("k/app", "app")).unwrap();
        let other = projects::upsert_project(&c, &mk("k/other", "other")).unwrap();
        Fx {
            _d: d,
            c,
            idx: VectorIndex::new(384).unwrap(),
            e: FakeEmbedder::new(384),
            app,
            other,
        }
    }

    impl Fx {
        fn add_input(
            &self,
            p: &projects::ResolvedProject,
            input: memories::NewMemoryInput,
        ) -> (String, i64) {
            let m = memories::prepare(input, p, "h".into()).unwrap();
            let memories::InsertOutcome::New { id, seq } = memories::insert(&self.c, &m).unwrap()
            else {
                panic!()
            };
            let v = self
                .e
                .embed_passages(&[memory_embed_text(&m.title, &m.body)])
                .unwrap()
                .remove(0);
            self.idx.upsert(VecKey::Memory(seq), &v).unwrap();
            (id, seq)
        }

        fn add(&self, p: &projects::ResolvedProject, kind: Kind, content: &str) -> (String, i64) {
            self.add_input(
                p,
                memories::NewMemoryInput {
                    content: content.into(),
                    kind: Some(kind),
                    ..Default::default()
                },
            )
        }

        fn chunk(&self, p: &projects::ResolvedProject, uri: &str, text: &str) {
            let out = sources::ingest(
                &self.c,
                p,
                &sources::SourceInput {
                    uri: uri.into(),
                    ..Default::default()
                },
                &[sources::ChunkInput {
                    ord: 0,
                    text: text.into(),
                    ..Default::default()
                }],
            )
            .unwrap();
            assert_eq!(out.inserted, 1);
        }

        fn params(&self, q: &str) -> SearchParams {
            SearchParams {
                query: q.into(),
                current_project_id: Some(self.app.id.clone()),
                limit: 10,
                relevance_floor: 0.82,
                raw_min_results: 3,
                ..Default::default()
            }
        }

        fn search(&self, p: &SearchParams) -> SearchOutput {
            run(
                &self.c,
                Some(&self.idx),
                &self.e,
                &Weights::default(),
                p,
                chrono::Utc::now(),
            )
            .unwrap()
        }

        fn update(&self, id: &str, action: memories::UpdateAction) {
            memories::apply_update(
                &self.c,
                id,
                &memories::MemoryUpdate {
                    action,
                    ..Default::default()
                },
                "h".into(),
            )
            .unwrap();
        }
    }

    #[test]
    fn finds_thai_english_and_code_identifiers() {
        let f = fx();
        f.add(
            &f.app,
            Kind::Decision,
            "ใช้ SQLite แบบ WAL เพื่อให้ระบบความจำไม่ค้าง",
        );
        f.add(
            &f.app,
            Kind::Bugfix,
            "HostLinkReconnect retried forever after sleep; capped at 5 attempts",
        );
        f.add(&f.app, Kind::Note, "Lunch menu ideas");
        assert!(
            f.search(&f.params("ความจำ")).hits[0]
                .title
                .contains("ความจำ")
        );
        let code = f.search(&f.params("reconnect"));
        assert!(code.hits[0].title.contains("HostLinkReconnect"));
        assert!(
            code.hits[0].matched.contains(&MatchVia::Code)
                || code.hits[0].matched.contains(&MatchVia::Words)
        );
        let exact = f.search(&f.params("HostLinkReconnect"));
        assert!(
            exact.hits[0].matched.contains(&MatchVia::Code),
            "{:?}",
            exact.hits[0]
        );
        assert!(exact.hits[0].snippet.contains("HostLinkReconnect"));
    }

    #[test]
    fn current_project_and_status_shape_the_ranking() {
        let f = fx();
        let (_o, _) = f.add(&f.other, Kind::Fact, "deploy target is easypanel");
        let (mine, _) = f.add(&f.app, Kind::Fact, "deploy target is easypanel server");
        let hits = f.search(&f.params("deploy target easypanel")).hits;
        assert_eq!(hits[0].id, mine, "current project first");
        f.update(&mine, memories::UpdateAction::Archive);
        let hits = f.search(&f.params("deploy target easypanel")).hits;
        assert_ne!(hits[0].id, mine, "archived drops below active");
        assert_eq!(
            hits.iter().find(|h| h.id == mine).unwrap().status,
            Some(Status::Archived)
        );
    }

    #[test]
    fn trashed_memories_never_appear_and_filters_apply() {
        let f = fx();
        let (gone, _) = f.add(&f.app, Kind::Note, "secret plan alpha");
        f.add(&f.app, Kind::Decision, "alpha decision recorded");
        f.update(&gone, memories::UpdateAction::Forget);
        let all = f.search(&f.params("alpha"));
        assert!(all.hits.iter().all(|h| h.id != gone));
        assert_eq!(all.hits.len(), 1);
        let out = f.search(&SearchParams {
            kinds: vec![Kind::Decision],
            ..f.params("alpha")
        });
        assert!(out.hits.iter().all(|h| h.id != gone));
        assert!(out.hits.iter().all(|h| h.kind == Some(Kind::Decision)));
        let restricted = f.search(&SearchParams {
            project_filter_id: Some(f.other.id.clone()),
            ..f.params("alpha")
        });
        assert!(restricted.hits.is_empty());
    }

    #[test]
    fn topic_and_date_filters_apply() {
        let f = fx();
        let (tagged, _) = f.add_input(
            &f.app,
            memories::NewMemoryInput {
                content: "gamma rollout plan".into(),
                topics: vec!["SQLite".into()],
                created_at: Some("2026-01-05T10:00:00.000Z".into()),
                ..Default::default()
            },
        );
        let (plain, _) = f.add_input(
            &f.app,
            memories::NewMemoryInput {
                content: "gamma rollout notes".into(),
                created_at: Some("2026-03-05T10:00:00.000Z".into()),
                ..Default::default()
            },
        );
        let ids = |p: SearchParams| -> Vec<String> {
            f.search(&p).hits.into_iter().map(|h| h.id).collect()
        };
        assert_eq!(
            ids(SearchParams {
                topics: vec![" sqlite ".into()],
                ..f.params("gamma")
            }),
            vec![tagged.clone()],
            "topics are compared as normalised tags"
        );
        assert_eq!(
            ids(SearchParams {
                since: Some("2026-02-01".into()),
                ..f.params("gamma")
            }),
            vec![plain.clone()]
        );
        assert_eq!(
            ids(SearchParams {
                before: Some("2026-02-01".into()),
                ..f.params("gamma")
            }),
            vec![tagged]
        );
        assert!(
            ids(SearchParams {
                since: Some("2026-03-05T10:00:00.001Z".into()),
                ..f.params("gamma")
            })
            .is_empty()
        );
    }

    #[test]
    fn raw_fallback_when_knowledge_is_thin() {
        let f = fx();
        f.chunk(&f.app, "s1", "we discussed the frobnicator calibration");
        let res = f.search(&f.params("frobnicator"));
        assert!(res.raw_fallback);
        assert_eq!(res.hits[0].tier, Tier::Raw);
        assert_eq!(res.hits[0].project, "app");
        assert!(res.memory_seqs.is_empty());
        let no_fallback = f.search(&SearchParams {
            raw_min_results: 0,
            ..f.params("frobnicator")
        });
        assert!(no_fallback.hits.is_empty());
        assert!(!no_fallback.raw_fallback);
    }

    #[test]
    fn raw_results_follow_knowledge_results_in_the_fallback() {
        let f = fx();
        let (m, _) = f.add(&f.app, Kind::Fact, "the frobnicator lives in rack 4");
        f.chunk(
            &f.app,
            "s1",
            "frobnicator frobnicator frobnicator calibration log",
        );
        let res = f.search(&f.params("frobnicator"));
        assert!(res.raw_fallback);
        assert_eq!(res.hits[0].id, m);
        assert_eq!(res.hits[1].tier, Tier::Raw);
        // Enough knowledge results: no fallback.
        let res = f.search(&SearchParams {
            raw_min_results: 1,
            ..f.params("frobnicator")
        });
        assert!(!res.raw_fallback);
        assert_eq!(res.hits.len(), 1);
    }

    #[test]
    fn include_raw_ranks_both_tiers_together() {
        let f = fx();
        let (m, seq) = f.add(&f.app, Kind::Fact, "frobnicator calibration value is 7");
        f.chunk(&f.app, "s1", "frobnicator calibration chat");
        let res = f.search(&SearchParams {
            include_raw: true,
            ..f.params("frobnicator calibration")
        });
        assert!(!res.raw_fallback, "raw was asked for, not a fallback");
        assert_eq!(res.hits.len(), 2);
        assert_eq!(res.hits[0].id, m, "raw is weighted 0.6");
        assert_eq!(res.hits[1].tier, Tier::Raw);
        assert_eq!(res.memory_seqs, vec![seq]);
        // A project filter applies to raw chunks too.
        let res = f.search(&SearchParams {
            include_raw: true,
            project_filter_id: Some(f.other.id.clone()),
            ..f.params("frobnicator")
        });
        assert!(res.hits.is_empty());
    }

    #[test]
    fn hostile_and_empty_queries() {
        let f = fx();
        f.add(&f.app, Kind::Note, "anything");
        for q in [
            "\"", "NEAR(", "AI", "😀", "a:b*", "*", "-", ":", "(", "NEAR", "AND", "x\"y\"", "\u{0}",
        ] {
            assert!(
                run(
                    &f.c,
                    Some(&f.idx),
                    &f.e,
                    &Weights::default(),
                    &f.params(q),
                    chrono::Utc::now()
                )
                .is_ok(),
                "{q}"
            );
        }
        for q in ["   ", "", "\n\t"] {
            match run(
                &f.c,
                Some(&f.idx),
                &f.e,
                &Weights::default(),
                &f.params(q),
                chrono::Utc::now(),
            ) {
                Err(AliceError::Invalid(msg)) => assert!(msg.contains("query"), "{msg}"),
                other => panic!("{q:?}: expected Invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn fts_operators_in_queries_are_literal_words() {
        let f = fx();
        let (near, _) = f.add(&f.app, Kind::Note, "NEAR the AND gate");
        for q in [
            "NEAR",
            "AND",
            "-gate",
            "NEAR(gate",
            "\"gate",
            "gate*",
            "col:gate",
        ] {
            let out = f.search(&f.params(q));
            assert_eq!(
                out.hits.first().map(|h| h.id.as_str()),
                Some(near.as_str()),
                "{q}"
            );
        }
    }

    #[test]
    fn short_and_thai_queries_return_results_or_nothing_and_vector_still_runs() {
        let f = fx();
        let (thai, _) = f.add(&f.app, Kind::Fact, "ระบบความจำสำหรับเอเจนต์ภาษาไทย");
        f.add(&f.app, Kind::Fact, "AI agents share one memory");
        for q in ["AI", "ไทย", "ความจำ", "😀", "จำ", "ก"] {
            let out = f.search(&f.params(q));
            assert!(out.hits.len() <= 10, "{q}");
        }
        assert_eq!(f.search(&f.params("ไทย")).hits[0].id, thai);
        assert_eq!(f.search(&f.params("ความจำ")).hits[0].id, thai);
        assert_eq!(
            f.search(&f.params("จำ")).hits[0].id,
            thai,
            "LIKE fallback below trigram size"
        );
        assert!(f.search(&f.params("AI")).hits[0].title.contains("AI"));
        // Nothing lexical matches an emoji, but the vector search still runs: with the floor
        // lowered, its neighbours are returned and labelled as vector matches.
        let out = f.search(&SearchParams {
            relevance_floor: -1.0,
            ..f.params("😀")
        });
        assert_eq!(out.hits.len(), 2);
        assert!(out.hits.iter().all(|h| h.matched == vec![MatchVia::Vector]));
        assert!(f.search(&f.params("😀")).hits.is_empty());
    }

    #[test]
    fn works_without_a_vector_index_or_a_working_embedder() {
        struct Broken;
        impl Embedder for Broken {
            fn model_id(&self) -> &str {
                "broken"
            }
            fn dim(&self) -> usize {
                384
            }
            fn embed_passages(&self, _: &[String]) -> Result<Vec<Vec<f32>>> {
                Err(AliceError::Other("model missing".into()))
            }
            fn embed_query(&self, _: &str) -> Result<Vec<f32>> {
                Err(AliceError::Other("model missing".into()))
            }
        }
        let f = fx();
        let (id, _) = f.add(&f.app, Kind::Fact, "writer lease timeout");
        let lexical_only = run(
            &f.c,
            None,
            &f.e,
            &Weights::default(),
            &f.params("lease"),
            chrono::Utc::now(),
        )
        .unwrap();
        assert_eq!(lexical_only.hits[0].id, id);
        assert_eq!(lexical_only.hits[0].matched, vec![MatchVia::Words]);
        let broken = run(
            &f.c,
            Some(&f.idx),
            &Broken,
            &Weights::default(),
            &f.params("lease"),
            chrono::Utc::now(),
        )
        .unwrap();
        assert_eq!(broken.hits[0].id, id);
        let wrong_dim = FakeEmbedder::new(8);
        let mismatched = run(
            &f.c,
            Some(&f.idx),
            &wrong_dim,
            &Weights::default(),
            &f.params("lease"),
            chrono::Utc::now(),
        )
        .unwrap();
        assert_eq!(mismatched.hits[0].id, id);
    }

    #[test]
    fn limit_and_access_bookkeeping() {
        let f = fx();
        let mut seqs = Vec::new();
        for i in 0..8 {
            seqs.push(
                f.add(&f.app, Kind::Note, &format!("delta item number {i}"))
                    .1,
            );
        }
        let out = f.search(&SearchParams {
            limit: 3,
            ..f.params("delta")
        });
        assert_eq!(out.hits.len(), 3);
        assert_eq!(out.memory_seqs.len(), 3);
        for (h, s) in out.hits.iter().zip(&out.memory_seqs) {
            assert_eq!(memories::seq_of(&f.c, &h.id).unwrap(), Some(*s));
        }
        let capped = f.search(&SearchParams {
            limit: 500,
            ..f.params("delta")
        });
        assert_eq!(capped.hits.len(), 8);
        // Scores are positive and sorted best first.
        assert!(capped.hits.windows(2).all(|w| w[0].score >= w[1].score));
        assert!(capped.hits.iter().all(|h| h.score > 0.0));
    }

    #[test]
    fn related_follows_links() {
        let f = fx();
        let (old, _) = f.add(&f.app, Kind::Fact, "service listens on 8080");
        let m = memories::prepare(
            memories::NewMemoryInput {
                content: "api port is 9090".into(),
                kind: Some(Kind::Fact),
                supersedes: vec![old.clone()],
                ..Default::default()
            },
            &f.app,
            "h".into(),
        )
        .unwrap();
        memories::insert(&f.c, &m).unwrap();
        let out = f.search(&f.params("9090"));
        assert_eq!(out.hits.len(), 1);
        let rel = out
            .related
            .iter()
            .find(|r| r.id == old)
            .expect("old memory is related");
        assert_eq!(rel.via, "supersedes");
        assert_eq!(rel.kind, Kind::Fact);
        // A trashed neighbour is not offered.
        f.update(&old, memories::UpdateAction::Forget);
        assert!(f.search(&f.params("9090")).related.is_empty());
    }

    #[test]
    fn related_follows_shared_entities() {
        let f = fx();
        let (a, _) = f.add(&f.app, Kind::Fact, "deploys go to easypanel");
        let (b, _) = f.add(&f.app, Kind::Warning, "rotate the panel token monthly");
        f.c.execute(
            "INSERT INTO entities (id, name, created_at) VALUES ('E1', 'Easypanel', '2026-10-13T00:00:00.000Z')",
            [],
        )
        .unwrap();
        for id in [&a, &b] {
            f.c.execute(
                "INSERT INTO mentions (memory_id, entity_id) VALUES (?1, 'E1')",
                [id],
            )
            .unwrap();
        }
        let out = f.search(&f.params("deploys"));
        assert_eq!(out.hits[0].id, a);
        let rel = out
            .related
            .iter()
            .find(|r| r.id == b)
            .expect("shared entity");
        assert_eq!(rel.via, "entity:Easypanel");
        assert_eq!(rel.kind, Kind::Warning);
    }
}
