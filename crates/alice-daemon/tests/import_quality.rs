//! Search-quality comparison against MemPalace (spec section 12, method of section 10).
//!
//! The harness reads queries and relevance labels from a gold file (`GoldFile`), so the frozen
//! synthetic corpus (`tests/data/gold_corpus.json`, written by `data/gen_gold_corpus.py`) and a
//! later private gold file go through the same code. The corpus must not be edited after a
//! result has been seen: a corpus bug is fixed in its own commit that names the reason, and both
//! systems are re-run.
//!
//! Scoring conventions (frozen before the first result was seen):
//! * Exact-duplicate drawers (identical text) form a group. Both systems are scored on groups:
//!   a ranked list is mapped to groups, a group counts once at the rank of its first member (a
//!   hit on any member counts), and the relevant ids of a group are merged (its grade is the
//!   highest grade of its members). Alice collapses exact duplicates on import and MemPalace
//!   does not, so scoring raw drawer ids would reward one system for the other's behaviour.
//! * A hit of Alice maps to drawer ids through `legacy_ids` (every legacy id that points at the
//!   hit's memory or chunk) plus the memory's / chunk's own `legacy_id`.
//! * A query is `lexical_ok` when query and answer share plain words (for Thai: shared
//!   character n-grams). The CI test runs the fake embedder on those queries only; the others
//!   need the real model (the ignored test).
//! * nDCG@10 takes its ideal ranking from the query's own labels; recall@10 is the share of
//!   relevant groups (grade 1 or 2) found in the top 10.
//! * `GOLD_DEBUG=1` prints the missed queries and the per-query gaps (diagnostics only).
//! * The MemPalace results are recorded by `scripts/m3_mempalace_baseline.py`
//!   (`data/mempalace_baseline.json`); the same metric code scores them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::import::fixture::{DrawerSpec, build_sqlite_exact};
use alice_core::import::palace::DrawerMeta;
use alice_core::import::{ImportRequest, ImportState};
use alice_core::judge::metrics::ndcg_at;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn corpus() -> Value {
    serde_json::from_str(include_str!("data/gold_corpus.json")).expect("gold_corpus.json parses")
}

#[test]
fn the_gold_corpus_is_consistent() {
    let c = corpus();
    let drawers = c["drawers"].as_array().unwrap();
    let ids: HashSet<&str> = drawers.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), drawers.len(), "unique drawer ids");
    for d in drawers {
        assert!(
            !d["text"].as_str().unwrap().trim().is_empty(),
            "{} has no text",
            d["id"]
        );
        assert!(d["meta"]["wing"].is_string(), "{} has no wing", d["id"]);
    }
    let queries = c["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 48);
    let qids: HashSet<&str> = queries.iter().map(|q| q["id"].as_str().unwrap()).collect();
    assert_eq!(qids.len(), queries.len(), "unique query ids");
    for q in queries {
        let relevant = q["relevant"].as_object().unwrap();
        assert!(!relevant.is_empty(), "{} has no relevant drawer", q["id"]);
        assert!(
            relevant.values().any(|g| g.as_u64() == Some(2)),
            "{} has no grade-2 drawer",
            q["id"]
        );
        assert!(q["lexical_ok"].is_boolean(), "{} lexical_ok", q["id"]);
        for (d, g) in relevant {
            assert!(ids.contains(d.as_str()), "{d}");
            assert!(matches!(g.as_u64(), Some(1 | 2)));
        }
    }
    let cats: BTreeMap<String, usize> = queries.iter().fold(Default::default(), |mut m, q| {
        *m.entry(q["category"].as_str().unwrap().to_string())
            .or_default() += 1;
        m
    });
    assert_eq!(cats["fact"], 14);
    assert_eq!(cats["thai"], 8);
    assert_eq!(cats["mixed"], 6);
    assert_eq!(cats["identifier"], 8);
    assert_eq!(cats["paraphrase"], 6);
    assert_eq!(cats["cross_lingual"], 3);
    assert_eq!(cats["raw_only"], 3);
}

// ------------------------------------------------------------------------------- gold file

#[derive(Debug, Clone, Deserialize)]
struct GoldQuery {
    id: String,
    q: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    lexical_ok: bool,
    relevant: BTreeMap<String, u8>,
}

/// Queries and labels only: a private gold file has no drawers, the synthetic corpus has both.
#[derive(Debug, Clone, Deserialize)]
struct GoldFile {
    queries: Vec<GoldQuery>,
}

impl GoldFile {
    fn load(path: impl AsRef<Path>) -> GoldFile {
        let text = std::fs::read_to_string(path.as_ref())
            .unwrap_or_else(|e| panic!("{}: {e}", path.as_ref().display()));
        Self::parse(&text)
    }
    fn parse(text: &str) -> GoldFile {
        serde_json::from_str(text).expect("a gold file with a `queries` array")
    }
}

fn corpus_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/gold_corpus.json")
}

const CATEGORIES: [&str; 7] = [
    "fact",
    "thai",
    "mixed",
    "identifier",
    "paraphrase",
    "cross_lingual",
    "raw_only",
];

// ----------------------------------------------------------------------------------- metrics

/// Share of `relevant_ids` found among the first `k` of `ranked_ids`.
fn recall_at_k(ranked_ids: &[String], relevant_ids: &BTreeSet<String>, k: usize) -> f64 {
    if relevant_ids.is_empty() {
        return 0.0;
    }
    let found = ranked_ids
        .iter()
        .take(k)
        .filter(|i| relevant_ids.contains(*i))
        .count();
    found as f64 / relevant_ids.len() as f64
}

/// Drawer id -> the id of its exact-duplicate group (the smallest id of the identical texts).
/// An id the corpus does not know is its own group.
#[derive(Default)]
struct Groups(HashMap<String, String>);

impl Groups {
    fn from_corpus(c: &Value) -> Groups {
        let mut by_text: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for d in c["drawers"].as_array().unwrap() {
            let h: String = Sha256::digest(d["text"].as_str().unwrap().as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            by_text
                .entry(h)
                .or_default()
                .push(d["id"].as_str().unwrap().to_string());
        }
        let mut m = HashMap::new();
        for ids in by_text.values() {
            let rep = ids.iter().min().unwrap().clone();
            for i in ids {
                m.insert(i.clone(), rep.clone());
            }
        }
        Groups(m)
    }
    fn of(&self, id: &str) -> String {
        self.0.get(id).cloned().unwrap_or_else(|| id.to_string())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Agg {
    ndcg_at_10: f64,
    recall_at_10: f64,
    n: usize,
}

#[derive(Debug, Default)]
struct Scores {
    overall: Agg,
    by_cat: BTreeMap<String, Agg>,
}

/// Per query, the ranked hits, each mapped to the drawer ids it stands for.
type Ranked = BTreeMap<String, Vec<Vec<String>>>;

fn score_query(q: &GoldQuery, groups: &Groups, ranked: &[Vec<String>]) -> (f64, f64) {
    // Relevant groups with their merged (highest) grade.
    let mut grade: BTreeMap<String, u8> = BTreeMap::new();
    for (d, g) in &q.relevant {
        let e = grade.entry(groups.of(d)).or_default();
        *e = (*e).max(*g);
    }
    // Groups in rank order, each once.
    let mut seen = BTreeSet::new();
    let mut order: Vec<String> = Vec::new();
    for hit in ranked {
        let mut gs: Vec<String> = hit.iter().map(|d| groups.of(d)).collect();
        gs.sort();
        gs.dedup();
        for g in gs {
            if seen.insert(g.clone()) {
                order.push(g);
            }
        }
    }
    let grades: Vec<f64> = order
        .iter()
        .map(|g| *grade.get(g).unwrap_or(&0) as f64)
        .collect();
    let pool: Vec<f64> = grade.values().map(|g| *g as f64).collect();
    let relevant: BTreeSet<String> = grade.keys().cloned().collect();
    (
        ndcg_at(&grades, &pool, 10),
        recall_at_k(&order, &relevant, 10),
    )
}

fn score_ranked(
    gold: &GoldFile,
    groups: &Groups,
    ranked: &Ranked,
    keep: impl Fn(&GoldQuery) -> bool,
) -> Scores {
    let mut s = Scores::default();
    let mut sums: BTreeMap<String, (f64, f64, usize)> = BTreeMap::new();
    let mut all = (0.0, 0.0, 0usize);
    for q in gold.queries.iter().filter(|q| keep(q)) {
        let Some(r) = ranked.get(&q.id) else { continue };
        let (n, rc) = score_query(q, groups, r);
        let cat = q.category.clone().unwrap_or_else(|| "uncategorised".into());
        let e = sums.entry(cat).or_default();
        *e = (e.0 + n, e.1 + rc, e.2 + 1);
        all = (all.0 + n, all.1 + rc, all.2 + 1);
    }
    let mean = |t: (f64, f64, usize)| Agg {
        ndcg_at_10: t.0 / t.2.max(1) as f64,
        recall_at_10: t.1 / t.2.max(1) as f64,
        n: t.2,
    };
    s.overall = mean(all);
    s.by_cat = sums.into_iter().map(|(k, v)| (k, mean(v))).collect();
    s
}

fn comparison_table(alice: &Scores, base: &Scores) -> String {
    let mut o = String::from(
        "| category | queries | nDCG@10 Alice | nDCG@10 MemPalace | recall@10 Alice | recall@10 MemPalace |\n|---|---|---|---|---|---|\n",
    );
    let mut row = |name: &str, a: &Agg, b: &Agg| {
        o.push_str(&format!(
            "| {name} | {} | {:.3} | {:.3} | {:.3} | {:.3} |\n",
            a.n, a.ndcg_at_10, b.ndcg_at_10, a.recall_at_10, b.recall_at_10
        ));
    };
    row("**overall**", &alice.overall, &base.overall);
    for (c, a) in &alice.by_cat {
        row(c, a, base.by_cat.get(c).unwrap_or(&Agg::default()));
    }
    o
}

// ---------------------------------------------------------------------------- MemPalace side

struct MemPalaceBaseline {
    queries: BTreeMap<String, Vec<String>>,
}

impl MemPalaceBaseline {
    fn parse(text: &str) -> MemPalaceBaseline {
        let v: Value = serde_json::from_str(text).expect("mempalace_baseline.json parses");
        let queries = v["queries"]
            .as_object()
            .expect("queries object")
            .iter()
            .map(|(k, l)| {
                let ids = l
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i.as_str().unwrap().to_string());
                (k.clone(), ids.collect())
            })
            .collect();
        MemPalaceBaseline { queries }
    }
    fn load() -> MemPalaceBaseline {
        match std::env::var("ALICE_GOLD_BASELINE") {
            Ok(p) => Self::parse(&std::fs::read_to_string(p).unwrap()),
            Err(_) => Self::parse(include_str!("data/mempalace_baseline.json")),
        }
    }
    fn ranked(&self) -> Ranked {
        self.queries
            .iter()
            .map(|(k, l)| (k.clone(), l.iter().map(|d| vec![d.clone()]).collect()))
            .collect()
    }
}

// ----------------------------------------------------------------------------------- the flow

struct Flow {
    _root: tempfile::TempDir,
    _palace_root: tempfile::TempDir,
    home: AliceHome,
    svc: Arc<Service>,
}

fn spec_of(d: &Value) -> DrawerSpec {
    let m = &d["meta"];
    let s = |k: &str| m[k].as_str().map(str::to_string);
    let source = s("source_file");
    let mode = match m["kind"].as_str() {
        Some("transcript") => Some("convos".to_string()),
        _ if source.is_some() => Some("projects".to_string()),
        _ => None,
    };
    DrawerSpec {
        id: d["id"].as_str().unwrap().to_string(),
        text: d["text"].as_str().unwrap().to_string(),
        meta: DrawerMeta {
            wing: s("wing").unwrap(),
            room: s("room").unwrap(),
            added_by: s("added_by"),
            source_file: source,
            ingest_mode: mode,
            chunk_index: m["chunk_index"].as_i64(),
            filed_at: s("created_at").map(|t| t.trim_end_matches('Z').to_string()),
            authored_at: s("created_at").map(|t| t.trim_end_matches('Z').to_string()),
            ..Default::default()
        },
    }
}

impl Flow {
    /// The synthetic corpus as a MemPalace sqlite_exact palace, imported, with the fake embedder.
    async fn from_gold_corpus() -> Flow {
        let specs = corpus_specs();
        Self::import(&specs, None, || {
            Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>)
        })
        .await
    }

    /// The same with the local e5 model (linked from `~/.alice/models`), waiting until every
    /// chunk and memory is embedded. `ALICE_GOLD_PALACE=<folder>` names an existing palace
    /// folder (a COPY) to import instead of the synthetic corpus.
    async fn from_gold_corpus_real_model() -> Flow {
        let user_models =
            AliceHome::new(PathBuf::from(std::env::var("HOME").unwrap()).join(".alice"))
                .models_dir();
        let palace = std::env::var("ALICE_GOLD_PALACE").ok().map(PathBuf::from);
        let specs = if palace.is_some() {
            Vec::new()
        } else {
            corpus_specs()
        };
        let f = Self::import(&specs, Some(user_models), || unreachable!()).await;
        f.wait_embedded().await;
        f
    }

    async fn import(
        specs: &[DrawerSpec],
        real_models: Option<PathBuf>,
        fake: impl Fn() -> alice_core::Result<Arc<dyn Embedder>> + Send + Sync + 'static,
    ) -> Flow {
        let root = tempfile::tempdir().unwrap();
        let palace_root = tempfile::tempdir().unwrap();
        let home = AliceHome::new(root.path().join("alice"));
        let palace = match std::env::var("ALICE_GOLD_PALACE") {
            Ok(p) if real_models.is_some() => PathBuf::from(p),
            _ => {
                let p = palace_root.path().join("palace");
                std::fs::create_dir_all(&p).unwrap();
                build_sqlite_exact(&p, specs);
                p
            }
        };
        let svc = match &real_models {
            None => Service::start(home.clone(), &fake, ServiceOptions::default()).unwrap(),
            Some(user_models) => {
                home.ensure_dirs().unwrap();
                let dest = alice_embed::model_dir(&home.models_dir());
                let src = alice_embed::model_dir(user_models);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&src, &dest).unwrap();
                #[cfg(not(unix))]
                copy_dir_all(&src, &dest);
                assert!(
                    alice_embed::is_model_present(&home.models_dir()),
                    "the e5 model is not in ~/.alice/models"
                );
                let models = home.models_dir();
                Service::start(
                    home.clone(),
                    &move || {
                        Ok(Arc::new(alice_embed::OnnxEmbedder::new(
                            &models,
                            alice_embed::OnnxOptions::default(),
                        )?) as Arc<dyn Embedder>)
                    },
                    ServiceOptions::default(),
                )
                .unwrap()
            }
        };
        let f = Flow {
            _root: root,
            _palace_root: palace_root,
            home,
            svc,
        };
        f.svc
            .start_import(ImportRequest {
                palace: Some(palace.display().to_string()),
                ..Default::default()
            })
            .unwrap();
        let t = Instant::now();
        loop {
            let st = f.svc.import_status();
            if st.state != ImportState::Running {
                assert_eq!(st.state, ImportState::Done, "{st:?}");
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(300), "import did not end");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if real_models.is_none() {
            f.wait_embedded().await;
        }
        f
    }

    async fn wait_embedded(&self) {
        let t = Instant::now();
        while self.svc.health().await.unwrap().embed_backlog > 0 {
            assert!(
                t.elapsed() < Duration::from_secs(600),
                "embedding did not drain"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Every drawer id a hit stands for: the legacy ids that point at it plus its own.
    fn drawers_of(&self, hit_id: &str) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.home.db()).unwrap();
        let mut out: BTreeSet<String> = BTreeSet::new();
        let mut add = |sql: &str| {
            let mut st = conn.prepare(sql).unwrap();
            let rows = st
                .query_map([hit_id], |r| r.get::<_, Option<String>>(0))
                .unwrap();
            out.extend(rows.flat_map(|r| r.unwrap()));
        };
        add("SELECT legacy_id FROM legacy_ids WHERE target_id = ?1");
        add("SELECT legacy_id FROM memories WHERE id = ?1");
        add("SELECT legacy_id FROM chunks WHERE id = ?1");
        out.into_iter().collect()
    }

    async fn ranked(&self, gold: &GoldFile) -> Ranked {
        let mut out = Ranked::new();
        for q in &gold.queries {
            let r = self
                .svc
                .recall(alice_core::api::RecallRequest {
                    query: q.q.clone(),
                    include_raw: true,
                    limit: Some(10),
                    format: alice_core::api::OutputFormat::Json,
                    max_tokens: Some(200_000),
                    ..Default::default()
                })
                .await
                .unwrap();
            out.insert(
                q.id.clone(),
                r.results.iter().map(|h| self.drawers_of(&h.id)).collect(),
            );
        }
        out
    }

    /// (queries with a grade-2 group in the top 10, queries kept) over the queries `keep` selects.
    async fn recall_at_10(
        &self,
        gold: &GoldFile,
        keep: impl Fn(&GoldQuery) -> bool,
    ) -> (usize, usize) {
        let groups = Groups::from_corpus(&corpus());
        let ranked = self.ranked(gold).await;
        let mut hits = 0;
        let mut total = 0;
        for q in gold.queries.iter().filter(|q| keep(q)) {
            total += 1;
            let top: BTreeSet<String> = ranked[&q.id]
                .iter()
                .flatten()
                .map(|d| groups.of(d))
                .collect();
            if q.relevant
                .iter()
                .any(|(d, g)| *g == 2 && top.contains(&groups.of(d)))
            {
                hits += 1;
            } else if std::env::var("GOLD_DEBUG").is_ok() {
                eprintln!(
                    "MISS {} [{:?}] {:?} rel={:?} got={:?}",
                    q.id, q.category, q.q, q.relevant, ranked[&q.id]
                );
            }
        }
        (hits, total)
    }

    async fn score_where(&self, gold: &GoldFile, keep: impl Fn(&GoldQuery) -> bool) -> Scores {
        let groups = Groups::from_corpus(&corpus());
        score_ranked(gold, &groups, &self.ranked(gold).await, keep)
    }
}

fn corpus_specs() -> Vec<DrawerSpec> {
    corpus()["drawers"]
        .as_array()
        .unwrap()
        .iter()
        .map(spec_of)
        .collect()
}

// -------------------------------------------------------------------------------------- tests

#[tokio::test]
async fn gold_queries_with_a_plain_word_match_are_found_after_an_import() {
    // CI: fake embedder, so only the queries whose answer shares plain words (Thai: n-grams).
    let gold = GoldFile::load(corpus_path());
    let f = Flow::from_gold_corpus().await;
    let (hits, total) = f.recall_at_10(&gold, |q| q.lexical_ok).await;
    let s = f.score_where(&gold, |q| q.lexical_ok).await;
    if std::env::var("GOLD_DEBUG").is_ok() {
        eprintln!(
            "CI lexical_ok: {hits}/{total}, {:?}, by category {:?}",
            s.overall, s.by_cat
        );
    }
    assert!(hits as f64 / total as f64 >= 0.90, "{hits}/{total}");
    assert!(s.overall.ndcg_at_10 >= 0.80, "{:?}", s.overall);
}

#[test]
fn recall_at_k_matches_a_hand_computed_example() {
    let ranked: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
    let relevant: BTreeSet<String> = ["b", "d", "x"].iter().map(|s| s.to_string()).collect();
    // Within the first 3 only b is relevant: 1 of 3.
    assert!((recall_at_k(&ranked, &relevant, 3) - 1.0 / 3.0).abs() < 1e-12);
    // All of the first 4: b and d, 2 of 3.
    assert!((recall_at_k(&ranked, &relevant, 4) - 2.0 / 3.0).abs() < 1e-12);
    assert_eq!(recall_at_k(&ranked, &BTreeSet::new(), 3), 0.0);
}

#[test]
fn duplicate_groups_count_once_and_merge_their_labels() {
    let c = serde_json::json!({"drawers": [
        {"id": "a", "text": "same"}, {"id": "b", "text": "same"}, {"id": "c", "text": "other"}]});
    let groups = Groups::from_corpus(&c);
    let gold = GoldFile::parse(
        r#"{"queries":[{"id":"q","q":"x","category":"fact","lexical_ok":true,"relevant":{"a":1,"b":2,"c":1}}]}"#,
    );
    // Both copies are hit (twice in a row, then the other drawer): the group is one rank,
    // graded 2 (the merged label), and the ranking is ideal.
    let ranked: Ranked = [(
        "q".to_string(),
        vec![
            vec!["b".to_string()],
            vec!["a".to_string()],
            vec!["c".to_string()],
        ],
    )]
    .into();
    let s = score_ranked(&gold, &groups, &ranked, |_| true);
    assert!(
        (s.overall.ndcg_at_10 - 1.0).abs() < 1e-12,
        "{:?}",
        s.overall
    );
    assert!((s.overall.recall_at_10 - 1.0).abs() < 1e-12);
}

#[test]
fn the_baseline_file_matches_the_corpus() {
    let c = corpus();
    let drawers: HashSet<&str> = c["drawers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    let qids: HashSet<&str> = c["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap())
        .collect();
    let base = MemPalaceBaseline::parse(include_str!("data/mempalace_baseline.json"));
    assert_eq!(base.queries.len(), 48);
    for (q, list) in &base.queries {
        assert!(
            qids.contains(q.as_str()),
            "baseline query {q} is not in the corpus"
        );
        assert!(
            !list.is_empty() && list.len() <= 10,
            "{q}: {} results",
            list.len()
        );
        for d in list {
            assert!(drawers.contains(d.as_str()), "{q}: unknown drawer {d}");
        }
    }
    for q in &qids {
        assert!(base.queries.contains_key(*q), "no baseline for {q}");
    }
}

#[test]
fn a_gold_file_without_drawers_loads_too() {
    let g = GoldFile::parse(r#"{"queries":[{"id":"p1","q":"hello","relevant":{"d1":2}}]}"#);
    assert_eq!(g.queries.len(), 1);
    assert_eq!(g.queries[0].category, None);
    assert!(!g.queries[0].lexical_ok);
    assert_eq!(g.queries[0].relevant["d1"], 2);
}

#[tokio::test]
#[ignore = "real e5 model (linked from ~/.alice/models); allowed by the controller - local, no cloud"]
async fn alice_beats_mempalace_on_the_gold_set() {
    let gold = GoldFile::load(
        std::env::var("ALICE_GOLD_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| corpus_path()),
    );
    let f = Flow::from_gold_corpus_real_model().await;
    let groups = Groups::from_corpus(&corpus());
    let alice = score_ranked(&gold, &groups, &f.ranked(&gold).await, |_| true);
    let base = score_ranked(&gold, &groups, &MemPalaceBaseline::load().ranked(), |_| {
        true
    });
    eprintln!("{}", comparison_table(&alice, &base));
    if std::env::var("GOLD_DEBUG").is_ok() {
        let ar = f.ranked(&gold).await;
        let br = MemPalaceBaseline::load().ranked();
        for q in &gold.queries {
            let (an, _) = score_query(q, &groups, &ar[&q.id]);
            let (bn, _) = score_query(q, &groups, &br[&q.id]);
            if an + 0.2 < bn {
                eprintln!(
                    "LOW {} {:?} {:?} a={:.2} b={:.2} rel={:?} alice={:?}",
                    q.id, q.category, q.q, an, bn, q.relevant, ar[&q.id]
                );
            }
        }
        let c = rusqlite::Connection::open(f.home.db()).unwrap();
        let n: i64 = c
            .query_row("select count(*) from embeddings", [], |r| r.get(0))
            .unwrap_or(-1);
        eprintln!("DBG embeddings {n}");
        eprintln!("DBG health {:?}", f.svc.health().await.unwrap());
    }
    assert!(
        alice.overall.ndcg_at_10 > base.overall.ndcg_at_10,
        "{} vs {}",
        alice.overall.ndcg_at_10,
        base.overall.ndcg_at_10
    );
    assert!(alice.overall.recall_at_10 >= base.overall.recall_at_10);
    for c in CATEGORIES {
        let (a, b) = (alice.by_cat.get(c), base.by_cat.get(c));
        if let (Some(a), Some(b)) = (a, b) {
            assert!(a.ndcg_at_10 + 0.05 >= b.ndcg_at_10, "{c} regressed");
        }
    }
}

/// Windows has no unprivileged symlinks: stand the model directory in by copying it.
#[cfg(not(unix))]
fn copy_dir_all(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dest.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_all(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}
