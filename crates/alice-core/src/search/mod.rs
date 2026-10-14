//! Recall pipeline: candidates -> fusion -> filters and boosts -> raw fallback -> related.

pub mod fusion;
pub mod lexical;
pub mod pack;
pub mod stopwords;

use crate::api::{Hit, MatchVia, RelatedRef, Tier};
use crate::embed::Embedder;
use crate::error::{AliceError, Result};
use crate::model::{Kind, Memory, RAW_HALF_LIFE_DAYS, Status, limits};
use crate::normalize::{nfc, normalize_topic};
use crate::store::embeddings::VecKey;
use crate::store::projects::GLOBAL_ID;
use crate::store::sources::ChunkRow;
use crate::store::{ReaderPool, memories, sources};
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
/// Only the start of a longer query is searched and embedded (the e5 window is 512 tokens).
const MAX_QUERY_CHARS: usize = 2_000;
const MAX_RELATED: usize = 5;
/// Cosine similarity at which a vector-only match counts as relevant (spec §4.4 step 5), for
/// multilingual-e5-small. Measured on the gold set with the real model:
/// `crates/alice-embed/tests/real_search_quality.rs`.
pub const DEFAULT_RELEVANCE_FLOOR: f32 = 0.82;

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

/// Where the raw tier's chunk vector list reads the stored vectors it rescores with.
#[derive(Clone, Copy)]
pub enum ChunkReads<'a> {
    /// On the query's own connection, after the lexical lists.
    Inline,
    /// On a connection of this pool, on a thread of its own, while the lexical lists run on
    /// the query's connection: the raw tier then takes as long as the slower of the two
    /// instead of their sum. A query takes one connection of this pool at most, after its
    /// own; it never asks the pool its own connection came from for a second one, so queries
    /// cannot wait for each other in a cycle. Use a pool other than the query's.
    Pool(&'a ReaderPool),
}

/// The raw tier's chunk vector list, maybe still being computed.
enum ChunkVectors<'scope> {
    /// No index or no query vector.
    Nothing,
    /// To be computed on the query's connection.
    Inline,
    Running(std::thread::ScopedJoinHandle<'scope, Vec<(ItemRef, f32)>>),
}

/// The chunk vector list: the sign-bit shortlist, re-ranked in RAM and rescored with the stored
/// vectors (`VectorIndex::search_chunks`). A failure is logged and means no chunk vector
/// candidates: the raw tier keeps its word and code matches.
fn chunk_vector_list(
    index: &VectorIndex,
    conn: &Connection,
    v: &[f32],
    k: usize,
) -> Vec<(ItemRef, f32)> {
    index
        .search_chunks(conn, v, k)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "chunk vector search failed; using word matches only");
            Vec::new()
        })
        .into_iter()
        .filter_map(|(key, sim)| match key {
            VecKey::Chunk(s) => Some((ItemRef::Chunk(s), sim)),
            VecKey::Memory(_) => None,
        })
        .collect()
}

/// What every candidate list needs.
struct Query<'a> {
    conn: &'a Connection,
    index: Option<&'a VectorIndex>,
    vector: Option<Vec<f32>>,
    text: &'a str,
    k: usize,
    reads: ChunkReads<'a>,
}

impl Query<'_> {
    /// Start the raw tier's chunk vector list: on a thread of `scope` with a connection of its
    /// own pool, or (inline) when the raw lists are collected.
    fn chunk_vectors<'s>(&'s self, scope: &'s std::thread::Scope<'s, '_>) -> ChunkVectors<'s> {
        let (Some(index), Some(v)) = (self.index, self.vector.as_deref()) else {
            return ChunkVectors::Nothing;
        };
        let ChunkReads::Pool(pool) = self.reads else {
            return ChunkVectors::Inline;
        };
        let k = self.k;
        let job = move || {
            pool.with(|c| Ok(chunk_vector_list(index, c, v, k)))
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "no connection for the chunk vector search; using word matches only");
                    Vec::new()
                })
        };
        match std::thread::Builder::new()
            .name("alice-chunk-vectors".into())
            .spawn_scoped(scope, job)
        {
            Ok(handle) => ChunkVectors::Running(handle),
            Err(e) => {
                tracing::warn!(error = %e, "cannot start a thread for the chunk vector search; running it after the word lists");
                ChunkVectors::Inline
            }
        }
    }

    /// Nearest memories. A failure is logged and means no vector candidates.
    fn memory_vector_hits(&self) -> Vec<(VecKey, f32)> {
        let (Some(index), Some(v)) = (self.index, self.vector.as_deref()) else {
            return Vec::new();
        };
        index
            .search(v, self.k, KeyFilter::Memories)
            .unwrap_or_else(|e| {
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
            .memory_vector_hits()
            .into_iter()
            .filter_map(|(key, sim)| match key {
                VecKey::Memory(s) => Some((ItemRef::Memory(s), sim)),
                VecKey::Chunk(_) => None,
            })
            .collect();
        collect(lists, vector)
    }

    /// The raw tier's lists: word, code and trigram lists here, and the chunk vector list
    /// started by [`Self::chunk_vectors`] (joined last, so the two overlap).
    fn raw(&self, chunks: ChunkVectors<'_>) -> Candidates {
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
        // Sign-bit candidates rescored with the stored vectors: exact cosine similarities.
        let vector = match chunks {
            ChunkVectors::Nothing => Vec::new(),
            ChunkVectors::Inline => match (self.index, self.vector.as_deref()) {
                (Some(index), Some(v)) => chunk_vector_list(index, c, v, k),
                _ => Vec::new(),
            },
            ChunkVectors::Running(handle) => handle.join().unwrap_or_else(|_| {
                tracing::error!("the chunk vector search panicked; using word matches only");
                Vec::new()
            }),
        };
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

/// The text a query searches and embeds: NFC, trimmed, at most the first 2,000 characters.
/// Fails for an empty query.
pub fn query_text(query: &str) -> Result<String> {
    let mut normalized = nfc(query.trim());
    if normalized.is_empty() {
        return Err(AliceError::invalid(
            "query is empty; pass keywords or a question",
        ));
    }
    if let Some((cut, _)) = normalized.char_indices().nth(MAX_QUERY_CHARS) {
        normalized.truncate(cut);
    }
    Ok(normalized)
}

/// The query vector for `text` (from [`query_text`]), or `None` when there is no index (it is
/// being rebuilt) or the embedder fails: the search then uses the word and code lists only.
pub fn embed_query(
    index: Option<&VectorIndex>,
    embedder: &dyn Embedder,
    text: &str,
) -> Option<Vec<f32>> {
    index?;
    embedder
        .embed_query(text)
        .inspect_err(|e| {
            if embed_failure_level(e) == tracing::Level::DEBUG {
                tracing::debug!(error = %e, "query embedding skipped; using word matches only");
            } else {
                tracing::warn!(error = %e, "query embedding failed; using word matches only");
            }
        })
        .ok()
}

/// How a query embedding failure is logged: a model reloading after an idle period is
/// expected (debug; a reload that fails is logged where it fails), anything else warns.
fn embed_failure_level(e: &AliceError) -> tracing::Level {
    match e {
        AliceError::Other(m) if m.starts_with(crate::embed::MODEL_LOADING) => tracing::Level::DEBUG,
        _ => tracing::Level::WARN,
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
    let vector = embed_query(index, embedder, &query_text(&p.query)?);
    run_with_vector(conn, index, vector, weights, p, now)
}

/// [`run`] with the query already embedded ([`embed_query`]). The daemon embeds before it takes
/// a pooled connection, so a recall waiting for the model holds no connection.
pub fn run_with_vector(
    conn: &Connection,
    index: Option<&VectorIndex>,
    vector: Option<Vec<f32>>,
    weights: &Weights,
    p: &SearchParams,
    now: DateTime<Utc>,
) -> Result<SearchOutput> {
    run_with_chunk_reads(conn, index, vector, ChunkReads::Inline, weights, p, now)
}

/// [`run_with_vector`] with the raw tier's chunk vector list reading where `reads` says: with
/// [`ChunkReads::Pool`] it runs on a thread of its own, started as soon as the raw tier is
/// known to be needed (at once for `include_raw`, after the knowledge tier for the raw
/// fallback), while this connection runs the lexical lists.
pub fn run_with_chunk_reads(
    conn: &Connection,
    index: Option<&VectorIndex>,
    vector: Option<Vec<f32>>,
    reads: ChunkReads<'_>,
    weights: &Weights,
    p: &SearchParams,
    now: DateTime<Utc>,
) -> Result<SearchOutput> {
    let normalized = query_text(&p.query)?;
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
    let vector = index.and(vector);
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
        reads,
    };
    let filters = Filters {
        p,
        topics: &topics,
        window: &window,
        now,
        weights,
    };

    // Raw chunks have no kind or topics, so those filters leave the knowledge tier only.
    let raw_allowed = p.kinds.is_empty() && topics.is_empty();
    let (mut ranked, raw_fallback) = std::thread::scope(|scope| -> Result<_> {
        let with_raw = p.include_raw && raw_allowed;
        // Started first, so it also overlaps the knowledge tier.
        let chunks = if with_raw {
            query.chunk_vectors(scope)
        } else {
            ChunkVectors::Nothing
        };
        let (mut scores, mut signals) = query.knowledge();
        if with_raw {
            // Both tiers ranked together; the raw tier weight (0.6) keeps knowledge ahead on ties.
            let (s, sig) = query.raw(chunks);
            scores.extend(s);
            signals.extend(sig);
        }
        // Only relevant results are returned: a lexical match or vector similarity >=
        // relevance_floor. The lexical lists search content terms only (English stopwords are
        // left out whenever the query has other words), so a lexical match is a content-term
        // match.
        let mut ranked: Vec<Scored> = filters
            .score(conn, &scores, &signals)?
            .into_iter()
            .filter(|s| s.relevant)
            .collect();
        let mut raw_fallback = false;
        let knowledge_relevant = ranked.iter().filter(|s| s.is_knowledge()).count();
        if !p.include_raw && raw_allowed && knowledge_relevant < p.raw_min_results {
            let (s, sig) = query.raw(query.chunk_vectors(scope));
            let extra: Vec<Scored> = filters
                .score(conn, &s, &sig)?
                .into_iter()
                .filter(|s| s.relevant)
                .collect();
            raw_fallback = !extra.is_empty();
            // Spec §4.4 step 5: appended after the knowledge results.
            ranked.extend(extra);
        }
        Ok((ranked, raw_fallback))
    })?;
    ranked.truncate(limit);

    let mut terms = lexical::content_terms(&normalized);
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
            let when = c.authored_at.as_deref().unwrap_or(&c.created_at);
            if p.project_filter_id
                .as_deref()
                .is_some_and(|f| f != c.project_id)
                || !self.window.contains(when)
            {
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

    /// Review M8: a query that uses word matches because the model is reloading after an idle
    /// period is expected (debug); any other embedding failure is a warning.
    #[test]
    fn a_reloading_model_is_logged_at_debug_and_other_failures_warn() {
        use crate::embed::MODEL_LOADING;
        use tracing::Level;
        let loading = AliceError::Other(MODEL_LOADING.into());
        assert_eq!(embed_failure_level(&loading), Level::DEBUG);
        let failed_reload = AliceError::Other(format!(
            "{MODEL_LOADING} (reloading the embedding model failed: no such file)"
        ));
        assert_eq!(embed_failure_level(&failed_reload), Level::DEBUG);
        let broken = AliceError::Other("the embedding model failed: bad input".into());
        assert_eq!(embed_failure_level(&broken), Level::WARN);
    }
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

    /// Fix round 4 (R1): a rare Thai word (0.2% of chunks) next to a common one (40%) fills the
    /// fused top 10 of the raw lexical lists, whether the rare word has combining marks
    /// (`ตัดสินใจ`) or not (`ทะเลสาบ`): both Thai words are ranked by the trigram list only.
    #[test]
    fn a_rare_thai_word_fills_the_fused_raw_top_10_marked_or_not() {
        let n = 6_000;
        let mut rare_in_top = Vec::new();
        for rare in ["ตัดสินใจ", "ทะเลสาบ"] {
            let d = tempfile::tempdir().unwrap();
            let c = open_writer(&d.path().join("a.db")).unwrap();
            let chunks: Vec<sources::ChunkInput> = (0..n)
                .map(|i| {
                    let mut text = format!("note{i} filler{}", i % 97);
                    if i % 5 < 2 {
                        text.push_str(" ภาษาไทย");
                    }
                    if i % 500 == 250 {
                        text.push(' ');
                        text.push_str(rare);
                    }
                    sources::ChunkInput {
                        ord: i as i64,
                        text,
                        ..Default::default()
                    }
                })
                .collect();
            let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
            for (b, batch) in chunks.chunks(1_000).enumerate() {
                sources::ingest(
                    &c,
                    &p,
                    &sources::SourceInput {
                        uri: format!("s{b}"),
                        ..Default::default()
                    },
                    batch,
                )
                .unwrap();
            }
            let has_rare: HashSet<ItemRef> = c
                .prepare("SELECT rowid FROM chunks_tri WHERE chunks_tri MATCH ?1")
                .unwrap()
                .query_map([lexical::quote(rare)], |r| r.get(0))
                .unwrap()
                .map(|s| ItemRef::Chunk(s.unwrap()))
                .collect();
            assert_eq!(has_rare.len(), n / 500);
            let text = format!("ภาษาไทย {rare}");
            let query = Query {
                conn: &c,
                index: None,
                vector: None,
                text: &text,
                k: CANDIDATES,
                reads: ChunkReads::Inline,
            };
            let (scores, _) = query.raw(ChunkVectors::Nothing);
            let mut fused: Vec<(ItemRef, f64)> = scores.into_iter().collect();
            fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
            let top: Vec<ItemRef> = fused.iter().take(10).map(|(i, _)| *i).collect();
            rare_in_top.push((rare, top.iter().filter(|i| has_rare.contains(i)).count()));
        }
        assert_eq!(rare_in_top, vec![("ตัดสินใจ", 10), ("ทะเลสาบ", 10)]);
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
        // Raw chunks have no kind or topics: those filters keep them out, fallback included.
        f.chunk(&f.app, "s1", "alpha chatter from a session");
        assert!(f.search(&f.params("alpha")).raw_fallback);
        for p in [
            SearchParams {
                kinds: vec![Kind::Decision],
                ..f.params("alpha")
            },
            SearchParams {
                topics: vec!["x".into()],
                include_raw: true,
                ..f.params("alpha")
            },
        ] {
            let out = f.search(&p);
            assert!(!out.raw_fallback);
            assert!(out.hits.iter().all(|h| h.tier == Tier::Knowledge));
        }
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
    fn sharing_only_stopwords_is_not_relevant() {
        let f = fx();
        f.add(&f.app, Kind::Note, "what is the plan for the move");
        let out = f.search(&f.params("what is the frobnicator"));
        assert!(out.hits.is_empty(), "{:?}", out.hits);
        let (text, _) = pack::pack_text(&out, Some("app"), 1500);
        assert!(text.contains("No memories matched"), "{text}");
        f.chunk(&f.app, "s1", "frobnicator calibration notes");
        let out = f.search(&f.params("what is the frobnicator"));
        assert!(out.raw_fallback, "the raw fallback fires");
        assert!(!out.hits.is_empty() && out.hits.iter().all(|h| h.tier == Tier::Raw));
        // A query made only of stopwords still finds a memory that contains them.
        let out = f.search(&f.params("what is the"));
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].tier, Tier::Knowledge);
    }

    /// Review finding I-2: an apostrophe splits a contraction (`what's` -> `what`, `s`), and the
    /// fragment it leaves is no content word: "It's raining" shares only `s` with "what's the
    /// deploy target". With straight and curly apostrophes alike, such memories are not
    /// relevant and the raw fallback fires.
    #[test]
    fn contraction_fragments_are_not_a_content_match() {
        let f = fx();
        f.add(&f.app, Kind::Note, "It's raining in the garden today");
        f.add(&f.app, Kind::Note, "The user’s cat sleeps on the sofa");
        f.add(
            &f.app,
            Kind::Note,
            "We'll ship it; don't wait, they've said they'd call. I'm sure it isn't late",
        );
        f.chunk(
            &f.app,
            "s1",
            "notes: the deploy target is the staging cluster",
        );
        for q in [
            "what's the deploy target",
            "what’s the deploy target",
            "who'll own the deploy target",
            "where’d the deploy target go",
            "they've moved the deploy target, haven't they",
            "I'm told the deploy target isn't ready",
            "don't touch the deploy target",
        ] {
            let out = f.search(&f.params(q));
            assert!(
                out.hits.iter().all(|h| h.tier == Tier::Raw),
                "{q}: {:?}",
                out.hits
            );
            assert!(out.raw_fallback, "{q}: the raw fallback fires");
            assert!(!out.hits.is_empty(), "{q}");
        }
    }

    #[test]
    fn huge_queries_use_their_first_2000_characters() {
        let f = fx();
        let (id, _) = f.add(&f.app, Kind::Fact, "frobnicator calibration");
        let filler = "zz ".repeat(1_000);
        assert!(
            f.search(&f.params(&format!("{filler}frobnicator")))
                .hits
                .is_empty(),
            "words after 2,000 characters are not searched"
        );
        assert_eq!(
            f.search(&f.params(&format!("frobnicator {filler}"))).hits[0].id,
            id
        );
        let wall: String = (0..20_000).map(|i| format!("w{i}x ")).collect();
        let t = std::time::Instant::now();
        f.search(&f.params(&wall));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            t.elapsed()
        );
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

    /// M2 Task 1: the raw tier's vector list is the sign-bit chunk index rescored with the
    /// vectors stored for the index's model, so a chunk without a stored vector is no vector
    /// match even though the chunk index still holds it.
    #[test]
    fn raw_vector_matches_are_rescored_from_the_stored_vectors() {
        let mut f = fx();
        f.idx = VectorIndex::for_model("fake", 384).unwrap();
        let texts = [
            "the frobnicator calibration happens every tuesday",
            "the frobnicator calibration happened last tuesday",
        ];
        let mut seqs = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            f.chunk(&f.app, &format!("s{i}"), text);
            let seq: i64 =
                f.c.query_row("SELECT seq FROM chunks WHERE text = ?1", [text], |r| {
                    r.get(0)
                })
                .unwrap();
            let v = f.e.embed_passages(&[text.to_string()]).unwrap().remove(0);
            f.idx.upsert(VecKey::Chunk(seq), &v).unwrap();
            seqs.push((seq, v));
        }
        // Only the first chunk's vector is stored (the second row was deleted meanwhile).
        crate::store::embeddings::put(&f.c, VecKey::Chunk(seqs[0].0), "fake", &seqs[0].1).unwrap();
        let out = f.search(&SearchParams {
            include_raw: true,
            ..f.params("frobnicator calibration tuesday")
        });
        let raw: Vec<&Hit> = out.hits.iter().filter(|h| h.tier == Tier::Raw).collect();
        assert_eq!(raw.len(), 2, "{:?}", out.hits);
        let stored = raw.iter().find(|h| h.snippet.contains("every")).unwrap();
        assert!(stored.matched.contains(&MatchVia::Vector), "{stored:?}");
        let unstored = raw.iter().find(|h| h.snippet.contains("last")).unwrap();
        assert!(
            !unstored.matched.contains(&MatchVia::Vector),
            "{unstored:?}"
        );
    }

    /// Fix round 2: chunks with stored vectors, a memory, and the parameters of an
    /// `include_raw` recall and of one that takes the raw fallback.
    fn chunk_fixture() -> (Fx, Vec<SearchParams>) {
        let mut f = fx();
        f.idx = VectorIndex::for_model("fake", 384).unwrap();
        let texts = [
            "the frobnicator calibration happens every tuesday",
            "frobnicator calibration notes from the lab",
            "weekly lab sync about the tuesday frobnicator",
            "an unrelated tom yum soup recipe",
        ];
        for (i, text) in texts.iter().enumerate() {
            f.chunk(&f.app, &format!("s{i}"), text);
            let seq: i64 =
                f.c.query_row("SELECT seq FROM chunks WHERE text = ?1", [text], |r| {
                    r.get(0)
                })
                .unwrap();
            let v = f.e.embed_passages(&[text.to_string()]).unwrap().remove(0);
            crate::store::embeddings::put(&f.c, VecKey::Chunk(seq), "fake", &v).unwrap();
            f.idx.upsert(VecKey::Chunk(seq), &v).unwrap();
        }
        f.add(&f.app, Kind::Fact, "frobnicator calibration value is 7");
        let params = vec![
            SearchParams {
                include_raw: true,
                ..f.params("frobnicator calibration tuesday")
            },
            f.params("weekly lab sync tuesday"),
        ];
        (f, params)
    }

    fn summary(out: &SearchOutput) -> Vec<(String, Vec<MatchVia>, u64)> {
        out.hits
            .iter()
            .map(|h| (h.id.clone(), h.matched.clone(), h.score.to_bits()))
            .collect()
    }

    /// The chunk vector list can read on a pool of its own, at the same time as the lexical
    /// lists: the results are exactly those of the search on one connection.
    #[test]
    fn a_separate_chunk_read_pool_gives_the_same_results() {
        let (f, params) = chunk_fixture();
        let pool = crate::store::ReaderPool::new(f._d.path().join("a.db"), 1);
        let now = chrono::Utc::now();
        for p in &params {
            let v = embed_query(Some(&f.idx), &f.e, &query_text(&p.query).unwrap());
            let inline =
                run_with_vector(&f.c, Some(&f.idx), v.clone(), &Weights::default(), p, now)
                    .unwrap();
            let pooled = run_with_chunk_reads(
                &f.c,
                Some(&f.idx),
                v,
                ChunkReads::Pool(&pool),
                &Weights::default(),
                p,
                now,
            )
            .unwrap();
            assert_eq!(summary(&pooled), summary(&inline), "{}", p.query);
            assert_eq!(
                pooled.raw_fallback, !p.include_raw,
                "{}: include_raw, or the raw fallback",
                p.query
            );
            assert!(
                pooled
                    .hits
                    .iter()
                    .any(|h| h.tier == Tier::Raw && h.matched.contains(&MatchVia::Vector)),
                "{}: {:?}",
                p.query,
                pooled.hits
            );
        }
    }

    /// A chunk vector list that fails (here its pool cannot open the database) leaves the
    /// lexical lists: the raw tier is found by words, as without an index.
    #[test]
    fn a_failing_chunk_read_pool_leaves_the_word_matches() {
        let (f, params) = chunk_fixture();
        let broken = crate::store::ReaderPool::new(f._d.path().join("gone").join("a.db"), 1);
        for p in &params {
            let v = embed_query(Some(&f.idx), &f.e, &query_text(&p.query).unwrap());
            let out = run_with_chunk_reads(
                &f.c,
                Some(&f.idx),
                v,
                ChunkReads::Pool(&broken),
                &Weights::default(),
                p,
                chrono::Utc::now(),
            )
            .unwrap();
            let raw: Vec<&Hit> = out.hits.iter().filter(|h| h.tier == Tier::Raw).collect();
            assert!(!raw.is_empty(), "{}: {:?}", p.query, out.hits);
            assert!(
                raw.iter().all(|h| h.matched == vec![MatchVia::Words]),
                "{}: {raw:?}",
                p.query
            );
        }
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
