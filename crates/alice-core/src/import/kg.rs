//! MemPalace knowledge graph and tunnels: read from the snapshot copies, mapped to facts and
//! review items. Nothing here merges predicates; similar ones become review items only.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use chrono::FixedOffset;
use rusqlite::{Connection, OpenFlags};

use super::plan::{TextFilter, parse_time};
use crate::error::Result;
use crate::judge::text::predicate_similarity;
use crate::model::Author;
use crate::store::kg::{AssertFact, FactAction, ObjectKind, normalize_predicate};
use crate::store::reviews::{NewReview, ReviewType};

/// Predicate pairing is quadratic; above this many distinct predicates it is skipped.
const MAX_PAIRING_PREDICATES: usize = 5_000;
const SIMILARITY_FLOOR: f64 = 0.9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KgTriple {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub extracted_at: Option<String>,
    pub source_drawer_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Kg {
    pub triples: Vec<KgTriple>,
    /// Entities no triple mentions. Not imported: an entity without a fact carries no knowledge.
    pub isolated_entities: u64,
    pub warnings: Vec<String>,
}

fn columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = st.query_map([], |r| r.get::<_, String>(1))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Reads the copied knowledge graph read-only, resolving entity ids to names.
pub fn read_kg(path: &Path) -> Result<Kg> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let cols = columns(&conn, "triples")?;
    let col = |name: &str| {
        if cols.contains(name) {
            format!("t.{name}")
        } else {
            "NULL".to_string()
        }
    };
    let sql = format!(
        "SELECT t.id, t.subject, s.name, t.predicate, t.object, o.name, t.valid_from, t.valid_to, {}, {}
         FROM triples t
         LEFT JOIN entities s ON s.id = t.subject
         LEFT JOIN entities o ON o.id = t.object
         ORDER BY t.id",
        col("source_drawer_id"),
        col("extracted_at"),
    );
    let mut kg = Kg::default();
    let mut st = conn.prepare(&sql)?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let name = |idx: usize, role: &str, kg: &mut Kg| -> Result<String> {
            let ent: String = r.get(idx - 1)?;
            let resolved: Option<String> = r.get(idx)?;
            Ok(resolved.unwrap_or_else(|| {
                kg.warnings.push(format!(
                    "triple {id}: the {role} entity {ent} has no entity row; its id is used as the name"
                ));
                ent
            }))
        };
        let subject = name(2, "subject", &mut kg)?;
        let object = name(5, "object", &mut kg)?;
        kg.triples.push(KgTriple {
            id: id.clone(),
            subject,
            predicate: r.get(3)?,
            object,
            valid_from: r.get(6)?,
            valid_to: r.get(7)?,
            source_drawer_id: r.get(8)?,
            extracted_at: r.get(9)?,
        });
    }
    drop(rows);
    drop(st);
    let isolated: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entities e
         WHERE NOT EXISTS (SELECT 1 FROM triples t WHERE t.subject = e.id OR t.object = e.id)",
        [],
        |r| r.get(0),
    )?;
    kg.isolated_entities = isolated.max(0) as u64;
    Ok(kg)
}

#[derive(Debug, Clone)]
pub struct PlannedFact {
    pub legacy_id: String,
    pub add: AssertFact,
    pub end_at: Option<String>,
    pub source_drawer_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct KgPlan {
    pub facts: Vec<PlannedFact>,
    pub reviews: Vec<NewReview>,
    /// Detector names per blocked triple.
    pub blocked: Vec<Vec<String>>,
    pub warnings: Vec<String>,
}

fn filtered(
    filter: &dyn TextFilter,
    field: &str,
    text: &str,
) -> std::result::Result<String, Vec<String>> {
    filter.filter(field, text)
}

/// `existing`: (predicate name, how many relations use it) already in Alice's database.
pub fn plan_kg(
    kg: &Kg,
    existing: &[(String, u64)],
    assume: FixedOffset,
    filter: &dyn TextFilter,
) -> KgPlan {
    let mut plan = KgPlan::default();
    let mut order: Vec<&KgTriple> = kg.triples.iter().collect();
    order.sort_by(|a, b| {
        (&a.subject, &a.predicate, &a.valid_from, &a.id).cmp(&(
            &b.subject,
            &b.predicate,
            &b.valid_from,
            &b.id,
        ))
    });
    for t in order {
        let mut detectors = Vec::new();
        let mut field = |name: &str, text: &str| match filtered(filter, name, text) {
            Ok(s) => s,
            Err(d) => {
                detectors.extend(d);
                String::new()
            }
        };
        let subject = field("kg.subject", &t.subject);
        let predicate = field("kg.predicate", &t.predicate);
        let object = field("kg.object", &t.object);
        if !detectors.is_empty() {
            detectors.sort();
            detectors.dedup();
            plan.blocked.push(detectors);
            continue;
        }
        let from = t
            .valid_from
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(t.extracted_at.as_deref())
            .and_then(|s| parse_time(s, assume));
        let end_at = t.valid_to.as_deref().and_then(|s| parse_time(s, assume));
        plan.facts.push(PlannedFact {
            legacy_id: format!("kg:{}", t.id),
            add: AssertFact {
                subject,
                predicate,
                object,
                object_kind: ObjectKind::Auto,
                valid_from: from,
                action: FactAction::Add,
                source_memory_id: None,
                author: Author {
                    agent: Some("mempalace-import".into()),
                    ..Author::default()
                },
            },
            end_at,
            source_drawer_id: t.source_drawer_id.clone(),
        });
    }
    plan.warnings = kg.warnings.clone();
    plan_predicate_reviews(&mut plan, existing);
    plan
}

fn plan_predicate_reviews(plan: &mut KgPlan, existing: &[(String, u64)]) {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut from_kg: BTreeSet<String> = BTreeSet::new();
    for f in &plan.facts {
        let n = normalize_predicate(&f.add.predicate);
        from_kg.insert(n.clone());
        *counts.entry(n).or_default() += 1;
    }
    let mut in_alice: BTreeSet<String> = BTreeSet::new();
    for (name, n) in existing {
        let n_name = normalize_predicate(name);
        in_alice.insert(n_name.clone());
        *counts.entry(n_name).or_default() += n;
    }
    if counts.len() > MAX_PAIRING_PREDICATES {
        plan.warnings.push(format!(
            "{} distinct predicates: similar-predicate review items were skipped",
            counts.len()
        ));
        return;
    }
    let names: Vec<&String> = counts.keys().collect();
    // other -> (similarity, canonical): the best partner per predicate (one open item per key).
    let mut best: BTreeMap<String, (f64, String)> = BTreeMap::new();
    for (i, a) in names.iter().enumerate() {
        for b in &names[i + 1..] {
            if !from_kg.contains(*a) && !from_kg.contains(*b) {
                continue;
            }
            let sim = predicate_similarity(a, b);
            if sim < SIMILARITY_FLOOR {
                continue;
            }
            let (ca, cb) = (counts[*a], counts[*b]);
            let canonical_is_a = if ca != cb {
                ca > cb
            } else {
                match (in_alice.contains(*a), in_alice.contains(*b)) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => a < b,
                }
            };
            let (canonical, other) = if canonical_is_a { (a, b) } else { (b, a) };
            let slot = best
                .entry((*other).clone())
                .or_insert((sim, (*canonical).clone()));
            if sim > slot.0 {
                *slot = (sim, (*canonical).clone());
            }
        }
    }
    for (other, (sim, canonical)) in best {
        plan.reviews.push(NewReview {
            kind: ReviewType::Predicate,
            payload: serde_json::json!({
                "predicate": other,
                "canonical": canonical,
                "p": sim,
                "provider": "mempalace-import",
            }),
            confidence: Some(sim),
            dedupe_key: Some(format!("predicate:{other}")),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnel {
    pub id: Option<String>,
    pub label: Option<String>,
    pub kind: Option<String>,
    pub source_wing: String,
    pub source_room: String,
    pub target_wing: String,
    pub target_room: String,
    pub source_drawer_id: Option<String>,
    pub target_drawer_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Tunnels {
    pub explicit: Vec<Tunnel>,
    pub dropped_topic: u64,
    pub dropped_malformed: u64,
}

fn str_of(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// (wing, room, drawer_id) from a nested endpoint object, or from the flat `<side>_wing` form.
fn endpoint(v: &serde_json::Value, side: &str) -> Option<(String, String, Option<String>)> {
    if let Some(e) = v.get(side).filter(|e| e.is_object()) {
        return Some((
            str_of(e, "wing")?,
            str_of(e, "room")?,
            str_of(e, "drawer_id"),
        ));
    }
    Some((
        str_of(v, &format!("{side}_wing"))?,
        str_of(v, &format!("{side}_room"))?,
        str_of(v, &format!("{side}_drawer_id")),
    ))
}

/// A missing or corrupt file gives an empty result, never an error.
pub fn read_tunnels(path: &Path) -> Tunnels {
    let mut out = Tunnels::default();
    let Ok(bytes) = std::fs::read(path) else {
        return out;
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return out;
    };
    let items = match &doc {
        serde_json::Value::Array(a) => a.as_slice(),
        serde_json::Value::Object(o) => match o.get("tunnels") {
            Some(serde_json::Value::Array(a)) => a.as_slice(),
            _ => return out,
        },
        _ => return out,
    };
    for v in items {
        let kind = str_of(v, "kind");
        if kind.as_deref() == Some("topic") {
            out.dropped_topic += 1;
            continue;
        }
        let (Some(s), Some(t)) = (endpoint(v, "source"), endpoint(v, "target")) else {
            out.dropped_malformed += 1;
            continue;
        };
        out.explicit.push(Tunnel {
            id: str_of(v, "id"),
            label: str_of(v, "label"),
            kind,
            source_wing: s.0,
            source_room: s.1,
            source_drawer_id: s.2,
            target_wing: t.0,
            target_room: t.1,
            target_drawer_id: t.2,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::fixture::{TripleSpec, build_kg, build_tunnels};
    use crate::import::plan::NoFilter;

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn t(id: &str, s: &str, p: &str, o: &str) -> TripleSpec {
        TripleSpec {
            id: id.into(),
            subject: s.into(),
            predicate: p.into(),
            object: o.into(),
            ..TripleSpec::default()
        }
    }

    fn fact(id: &str, p: &str) -> KgTriple {
        KgTriple {
            id: id.into(),
            subject: "alice".into(),
            predicate: p.into(),
            object: format!("o{id}"),
            valid_from: None,
            valid_to: None,
            extracted_at: None,
            source_drawer_id: None,
        }
    }

    fn kg_of(preds: &[(&str, usize)]) -> Kg {
        let mut kg = Kg::default();
        for (p, n) in preds {
            for i in 0..*n {
                kg.triples.push(fact(&format!("{p}{i}"), p));
            }
        }
        kg
    }

    struct Blocker;
    impl TextFilter for Blocker {
        fn filter(&self, _f: &str, text: &str) -> std::result::Result<String, Vec<String>> {
            if text.contains("BLOCKME") {
                Err(vec!["test-secret".into()])
            } else {
                Ok(text.to_string())
            }
        }
    }

    #[test]
    fn triples_read_with_entity_names_and_missing_entities_are_warned_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut t2 = t("t2", "e1", "uses", "e3");
        t2.valid_to = Some("2026-03-01".into());
        let path = build_kg(
            dir.path(),
            &[
                ("e1", "alice"),
                ("e2", "sqlite"),
                ("e3", "postgres"),
                ("e4", "lonely"),
            ],
            &[t("t1", "e1", "uses", "e2"), t2, t("t3", "e9", "uses", "e2")],
            false,
        );
        let kg = read_kg(&path).unwrap();
        assert_eq!(kg.triples.len(), 3);
        assert_eq!(kg.triples[0].subject, "alice");
        assert_eq!(kg.triples[0].object, "sqlite");
        assert_eq!(kg.triples[1].object, "postgres");
        assert_eq!(kg.triples[1].valid_to.as_deref(), Some("2026-03-01"));
        assert_eq!(kg.triples[2].subject, "e9");
        assert_eq!(kg.triples[2].object, "sqlite");
        assert_eq!(kg.warnings.len(), 1);
        assert!(kg.warnings[0].contains("t3") && kg.warnings[0].contains("e9"));
        assert_eq!(kg.isolated_entities, 1);
    }

    #[test]
    fn an_ended_triple_plans_an_add_and_an_end_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut t1 = t("t1", "e1", "uses", "e2");
        t1.valid_from = Some("2026-01-05".into());
        let mut t2 = t("t2", "e1", "uses", "e3");
        t2.valid_from = Some("2026-02-01".into());
        t2.valid_to = Some("2026-03-01".into());
        let mut t3 = t("t3", "e1", "likes", "e2");
        t3.extracted_at = Some("2026-04-02T10:00:00+00:00".into());
        let path = build_kg(
            dir.path(),
            &[("e1", "alice"), ("e2", "sqlite"), ("e3", "postgres")],
            &[t1, t2, t3],
            false,
        );
        let plan = plan_kg(&read_kg(&path).unwrap(), &[], utc(), &NoFilter);
        assert_eq!(plan.facts.len(), 3);
        let by = |id: &str| plan.facts.iter().find(|f| f.legacy_id == id).unwrap();
        assert_eq!(by("kg:t1").end_at, None);
        assert_eq!(
            by("kg:t1").add.valid_from.as_deref(),
            Some("2026-01-05T00:00:00.000Z")
        );
        assert_eq!(
            by("kg:t2").end_at.as_deref(),
            Some("2026-03-01T00:00:00.000Z")
        );
        assert_eq!(
            by("kg:t2").add.valid_from.as_deref(),
            Some("2026-02-01T00:00:00.000Z")
        );
        assert_eq!(
            by("kg:t3").add.valid_from.as_deref(),
            Some("2026-04-02T10:00:00.000Z")
        );
        assert_eq!(by("kg:t2").add.action, FactAction::Add);
        assert_eq!(by("kg:t2").add.object_kind, ObjectKind::Auto);
        assert_eq!(
            by("kg:t2").add.author.agent.as_deref(),
            Some("mempalace-import")
        );
        // Sorted by (subject, predicate, valid_from, id): "likes" before "uses".
        let order: Vec<&str> = plan.facts.iter().map(|f| f.legacy_id.as_str()).collect();
        assert_eq!(order, ["kg:t3", "kg:t1", "kg:t2"]);
    }

    #[test]
    fn similar_predicates_become_review_items_in_the_m2_shape_never_merges() {
        let kg = kg_of(&[
            ("has_bug", 3),
            ("had_bug", 1),
            ("had_critical_bug", 1),
            ("located_at", 2),
        ]);
        let plan = plan_kg(&kg, &[], utc(), &NoFilter);
        assert_eq!(plan.facts.len(), 7);
        let r = plan
            .reviews
            .iter()
            .find(|r| r.dedupe_key.as_deref() == Some("predicate:had_bug"))
            .expect("a review for had_bug");
        assert_eq!(r.kind, ReviewType::Predicate);
        assert_eq!(r.payload["predicate"], "had_bug");
        assert_eq!(r.payload["canonical"], "has_bug");
        assert_eq!(r.payload["provider"], "mempalace-import");
        let p = r.payload["p"].as_f64().unwrap();
        assert!((0.9..=1.0).contains(&p));
        assert_eq!(r.confidence, Some(p));
        for r in &plan.reviews {
            assert_eq!(r.kind, ReviewType::Predicate);
            assert!(!r.payload.to_string().contains("located_at"));
        }
        let mut keys: Vec<_> = plan
            .reviews
            .iter()
            .filter_map(|r| r.dedupe_key.clone())
            .collect();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n);
        // Facts keep the ORIGINAL predicate text.
        assert!(plan.facts.iter().any(|f| f.add.predicate == "had_bug"));
        assert_eq!(
            plan.facts
                .iter()
                .filter(|f| f.add.predicate == "has_bug")
                .count(),
            3
        );
    }

    #[test]
    fn predicate_canonical_prefers_count_then_existing_vocabulary_then_lexicographic() {
        // Existing usage outweighs the KG's own count.
        let kg = kg_of(&[("has_bug", 1), ("had_bug", 1)]);
        let plan = plan_kg(&kg, &[("had_bug".into(), 5)], utc(), &NoFilter);
        let r = &plan.reviews[0];
        assert_eq!(r.payload["predicate"], "has_bug");
        assert_eq!(r.payload["canonical"], "had_bug");
        // Equal counts: the one already in Alice's vocabulary wins (the other is only in the KG).
        let kg = kg_of(&[("has_bug", 2), ("had_bug", 1)]);
        let plan = plan_kg(&kg, &[("had_bug".into(), 1)], utc(), &NoFilter);
        assert_eq!(plan.reviews[0].payload["canonical"], "had_bug");
        assert_eq!(plan.reviews[0].payload["predicate"], "has_bug");
        // Equal counts, neither existing: lexicographic order.
        let kg = kg_of(&[("has_bug", 1), ("had_bug", 1)]);
        let plan = plan_kg(&kg, &[], utc(), &NoFilter);
        assert_eq!(plan.reviews.len(), 1);
        assert_eq!(plan.reviews[0].payload["canonical"], "had_bug");
        assert_eq!(plan.reviews[0].payload["predicate"], "has_bug");
    }

    #[test]
    fn secrets_in_a_kg_object_block_that_triple_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = build_kg(
            dir.path(),
            &[
                ("e1", "alice"),
                ("e2", "sqlite"),
                ("e3", "token BLOCKME"),
                ("e4", "redis"),
            ],
            &[
                t("t1", "e1", "uses", "e2"),
                t("t2", "e1", "holds", "e3"),
                t("t3", "e1", "uses", "e4"),
            ],
            false,
        );
        let plan = plan_kg(&read_kg(&path).unwrap(), &[], utc(), &Blocker);
        assert_eq!(plan.blocked, vec![vec!["test-secret".to_string()]]);
        let ids: Vec<&str> = plan.facts.iter().map(|f| f.legacy_id.as_str()).collect();
        assert_eq!(ids, ["kg:t1", "kg:t3"]);
        assert!(!plan.facts.iter().any(|f| f.add.object.contains("BLOCKME")));
    }

    #[test]
    fn an_old_palace_kg_without_source_drawer_id_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut t1 = t("t1", "e1", "uses", "e2");
        t1.valid_from = Some("2026-01-05".into());
        let path = build_kg(
            dir.path(),
            &[("e1", "alice"), ("e2", "sqlite")],
            &[t1],
            true,
        );
        let kg = read_kg(&path).unwrap();
        assert_eq!(kg.triples.len(), 1);
        assert_eq!(kg.triples[0].source_drawer_id, None);
        assert_eq!(kg.triples[0].extracted_at, None);
        assert_eq!(kg.triples[0].subject, "alice");
        assert!(kg.warnings.is_empty());
        assert_eq!(kg.isolated_entities, 0);
    }

    #[test]
    fn tunnels_parse_mempalaces_real_shape_and_survive_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let real = r#"[
            {"id":"t1","source":{"wing":"pg_ai","room":"decisions","drawer_id":"d1"},
             "target":{"wing":"devspace","room":"src","drawer_id":"d2"},"label":"x","kind":"explicit"},
            {"id":"t2","source":{"wing":"a","room":"r"},"target":{"wing":"b","room":"r"},"kind":"topic"},
            {"id":"t3"}]"#;
        let tn = read_tunnels(&build_tunnels(dir.path(), real));
        assert_eq!(tn.explicit.len(), 1);
        assert_eq!(tn.dropped_topic, 1);
        assert_eq!(tn.dropped_malformed, 1);
        let e = &tn.explicit[0];
        assert_eq!(e.id.as_deref(), Some("t1"));
        assert_eq!(e.label.as_deref(), Some("x"));
        assert_eq!(e.kind.as_deref(), Some("explicit"));
        assert_eq!(
            (e.source_wing.as_str(), e.source_room.as_str()),
            ("pg_ai", "decisions")
        );
        assert_eq!(
            (e.target_wing.as_str(), e.target_room.as_str()),
            ("devspace", "src")
        );
        assert_eq!(e.source_drawer_id.as_deref(), Some("d1"));
        assert_eq!(e.target_drawer_id.as_deref(), Some("d2"));

        let wrapped = format!(r#"{{"tunnels":{real}}}"#);
        let tn = read_tunnels(&build_tunnels(dir.path(), &wrapped));
        assert_eq!(
            (tn.explicit.len(), tn.dropped_topic, tn.dropped_malformed),
            (1, 1, 1)
        );

        let flat =
            r#"[{"source_wing":"a","source_room":"r1","target_wing":"b","target_room":"r2"}]"#;
        let tn = read_tunnels(&build_tunnels(dir.path(), flat));
        assert_eq!(tn.explicit.len(), 1);
        assert_eq!(tn.explicit[0].target_room, "r2");
        assert_eq!(tn.explicit[0].id, None);

        let tn = read_tunnels(&build_tunnels(dir.path(), "{not json"));
        assert_eq!(
            (tn.explicit.len(), tn.dropped_topic, tn.dropped_malformed),
            (0, 0, 0)
        );
        let tn = read_tunnels(&dir.path().join("absent.json"));
        assert_eq!(
            (tn.explicit.len(), tn.dropped_topic, tn.dropped_malformed),
            (0, 0, 0)
        );
    }
}
