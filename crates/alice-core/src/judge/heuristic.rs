//! The offline judge (spec §5.2): duplicates by cosine similarity in the same project,
//! "updates" suggestions by kind and near-identical titles, kinds by keyword rules, projects by
//! name mentions, predicates by string similarity. It never claims more certainty than the
//! rules justify: only duplicates (and identical predicates) reach the auto-apply threshold.

use super::text::{
    directions_differ, predicate_similarity, same_predicate, stem_predicate, title_similarity,
};
use super::{
    Choice, Distribution, Judge, JudgeMemory, JudgeResult, ProjectOption, Relation, Rubric, Score,
};
use crate::store::kg::normalize_predicate;

pub struct HeuristicJudge {
    pub duplicate_similarity: f32,
    pub title_similarity: f64,
}

impl Default for HeuristicJudge {
    fn default() -> Self {
        Self {
            duplicate_similarity: 0.97,
            title_similarity: 0.9,
        }
    }
}

/// Keyword cues per kind (lowercase; English and Thai).
const CUES: &[(&str, &[&str])] = &[
    (
        "bugfix",
        &[
            "fix",
            "bug",
            "crash",
            "error",
            "regression",
            "root cause",
            "symptom",
            "แก้",
            "บั๊ก",
            "พัง",
        ],
    ),
    (
        "decision",
        &[
            "decided",
            "decision",
            "we will",
            "chose",
            "choose",
            "instead of",
            "going with",
            "ตัดสินใจ",
            "เลือกใช้",
        ],
    ),
    (
        "howto",
        &["how to", "steps", "run `", "command", "วิธี", "ขั้นตอน"],
    ),
    (
        "preference",
        &["prefer", "i like", "always use", "please", "ชอบ", "อยากให้"],
    ),
    (
        "warning",
        &[
            "warning",
            "careful",
            "never ",
            "do not",
            "don't",
            "danger",
            "avoid",
            "ระวัง",
            "ห้าม",
        ],
    ),
    (
        "status",
        &[
            "status",
            "in progress",
            "currently",
            "blocked",
            "สถานะ",
            "กำลังทำ",
        ],
    ),
    (
        "handoff",
        &["handoff", "next steps", "open questions", "ส่งต่อ", "งานต่อไป"],
    ),
    ("summary", &["summary", "in short", "tl;dr", "สรุป"]),
    ("diary", &["today i", "this session", "diary", "บันทึก"]),
    (
        "fact",
        &[" is a ", "runs on", " uses ", "located", "version"],
    ),
];

/// P over kinds from keyword cues: 0.8 for a clear winner with two or more cues, 0.6 with one,
/// spread over the tied kinds otherwise; `note` when nothing matched.
pub fn kind_rules(title: &str, body: &str) -> Distribution {
    let text = format!(" {} \n {} ", title.to_lowercase(), body.to_lowercase());
    let mut hits: Vec<(&str, usize)> = CUES
        .iter()
        .map(|(k, cues)| (*k, cues.iter().filter(|c| text.contains(*c)).count()))
        .filter(|(_, n)| *n > 0)
        .collect();
    hits.sort_by_key(|h| std::cmp::Reverse(h.1));
    match hits.as_slice() {
        [] => spread("note", 0.4),
        [(k, n), rest @ ..] if rest.first().is_none_or(|(_, m)| m < n) => {
            spread(k, if *n >= 2 { 0.8 } else { 0.6 })
        }
        _ => {
            let top = hits[0].1;
            Distribution::from_pairs(
                hits.iter()
                    .filter(|(_, n)| *n == top)
                    .map(|(k, _)| (k.to_string(), 1.0))
                    .chain([("note".to_string(), 0.5)]),
            )
        }
    }
}

/// `p` on `winner`, the rest shared by the other kinds.
fn spread(winner: &str, p: f64) -> Distribution {
    let kinds: Vec<String> = super::kind_choices().into_iter().map(|c| c.id).collect();
    let share = (1.0 - p) / (kinds.len() - 1) as f64;
    Distribution::from_pairs(kinds.into_iter().map(|k| {
        let v = if k == winner { p } else { share };
        (k, v)
    }))
}

/// Whether two memories state the same text: equal title and body under the identity of an
/// idempotent write (`normalize::same_text`: NFC and collapsed whitespace; case and punctuation
/// count, so `retry > 3` and `retry < 3` differ). Only this is sure enough for the heuristic to
/// act on without asking (a vector rates "port 5432" and "port 5433" as near twins).
fn same_text(a: &JudgeMemory, b: &JudgeMemory) -> bool {
    crate::normalize::same_text(&a.title, &a.body, &b.title, &b.body)
}

/// Whether `name` appears in `text` as a word. Boundaries are ASCII-aware: a Thai letter next
/// to an ASCII name (`ใช้pg-aiกับ`) does not hide it, but `pg-aid` does not match `pg-ai`.
fn mentions(text: &str, name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    let name = name.to_lowercase();
    let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
    [name.clone(), name.replace('-', " "), name.replace('-', "_")]
        .iter()
        .any(|n| {
            text.match_indices(n.as_str()).any(|(i, _)| {
                boundary(text[..i].chars().next_back())
                    && boundary(text[i + n.len()..].chars().next())
            })
        })
}

/// Whether `a` was created strictly after `b`: by parsed time, string order only when neither
/// parses.
fn newer(a: &str, b: &str) -> bool {
    match (super::verbal::parse_time(a), super::verbal::parse_time(b)) {
        (Some(x), Some(y)) => x > y,
        (None, None) => a > b,
        _ => false,
    }
}

impl Judge for HeuristicJudge {
    fn provider(&self) -> &str {
        "heuristic"
    }

    /// The heuristic cannot read arbitrary questions: no information.
    fn classify(&self, _state: &str, options: &[Choice]) -> JudgeResult<Distribution> {
        Ok(Distribution::from_pairs(
            options.iter().map(|c| (c.id.clone(), 1.0)),
        ))
    }

    fn score(&self, _state: &str, _rubric: &Rubric) -> JudgeResult<Score> {
        Ok(Score {
            value: 0.5,
            confidence: 0.0,
        })
    }

    fn compare(&self, new: &JudgeMemory, existing: &[JudgeMemory]) -> JudgeResult<Vec<Relation>> {
        Ok(existing
            .iter()
            .map(|e| {
                let same_project = e.project == new.project;
                let sim = e.similarity.unwrap_or(0.0);
                let mut exact = None;
                let probs = if same_project && sim >= self.duplicate_similarity {
                    let p = f64::from(sim).min(0.99);
                    // `agreement` marks the duplicates that are the same text: the only
                    // heuristic result `pipeline::decide` applies without asking.
                    exact = Some(same_text(new, e));
                    Distribution::from_pairs([
                        ("duplicate".to_string(), p),
                        ("unrelated".to_string(), 1.0 - p),
                    ])
                } else if same_project
                    && e.kind == new.kind
                    && newer(&new.created_at, &e.created_at)
                    && title_similarity(&new.title, &e.title) >= self.title_similarity
                {
                    Distribution::from_pairs([
                        ("updates".to_string(), 0.7),
                        ("unrelated".to_string(), 0.3),
                    ])
                } else {
                    Distribution::certain("unrelated")
                };
                Relation {
                    existing_id: e.id.clone(),
                    probs,
                    agreement: exact,
                }
            })
            .collect())
    }

    fn classify_kind(&self, m: &JudgeMemory) -> JudgeResult<Distribution> {
        Ok(kind_rules(&m.title, &m.body))
    }

    fn route_project(
        &self,
        m: &JudgeMemory,
        projects: &[ProjectOption],
    ) -> JudgeResult<Distribution> {
        let text = format!("{}\n{}", m.title, m.body);
        let hits: Vec<&ProjectOption> = projects
            .iter()
            .filter(|p| mentions(&text, &p.slug) || mentions(&text, &p.name))
            .collect();
        Ok(match hits.as_slice() {
            [one] => {
                Distribution::from_pairs([(one.slug.clone(), 0.7), ("global".to_string(), 0.3)])
            }
            _ => Distribution::certain("global"),
        })
    }

    fn match_predicate(&self, new: &str, existing: &[String]) -> JudgeResult<Distribution> {
        let key = normalize_predicate(new);
        let canonical = stem_predicate(new);
        // Best candidate: exact normalised match, then higher similarity, then a matching canonical stem, then the shorter
        // predicate, then lexicographic order, so the answer does not depend on input order.
        let mut ranked: Vec<(&String, f64, bool, bool)> = existing
            .iter()
            .map(|p| {
                (
                    p,
                    predicate_similarity(new, p),
                    normalize_predicate(p) == key,
                    stem_predicate(p) == canonical,
                )
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then(b.1.total_cmp(&a.1))
                .then(b.3.cmp(&a.3))
                .then(a.0.chars().count().cmp(&b.0.chars().count()))
                .then(a.0.cmp(b.0))
        });
        Ok(match ranked.first() {
            // The same words read the other way round (`depends_on`, `depended_on_by`) are a
            // question for a person, never a sure match.
            Some((p, _, _, _)) if same_predicate(new, p) && !directions_differ(new, p) => {
                Distribution::from_pairs([((*p).clone(), 0.95), ("none".to_string(), 0.05)])
            }
            Some((p, s, _, _)) if *s >= self.title_similarity => {
                Distribution::from_pairs([((*p).clone(), 0.7), ("none".to_string(), 0.3)])
            }
            _ => Distribution::certain("none"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Judge, JudgeMemory, ProjectOption};
    use crate::model::Kind;

    fn mem(
        id: &str,
        project: &str,
        kind: Kind,
        title: &str,
        created: &str,
        sim: Option<f32>,
    ) -> JudgeMemory {
        JudgeMemory {
            id: id.into(),
            project: project.into(),
            kind,
            title: title.into(),
            body: title.into(),
            importance: 3,
            created_at: created.into(),
            similarity: sim,
        }
    }

    #[test]
    fn compare_finds_duplicates_in_the_project_and_suggests_updates() {
        let j = HeuristicJudge::default();
        let new = mem(
            "N",
            "app",
            Kind::Decision,
            "Use WAL mode for the hub DB",
            "2026-10-13T00:00:00.000Z",
            None,
        );
        let out = j
            .compare(
                &new,
                &[
                    mem(
                        "D",
                        "app",
                        Kind::Decision,
                        "Use WAL mode",
                        "2026-10-01T00:00:00.000Z",
                        Some(0.985),
                    ),
                    mem(
                        "X",
                        "other",
                        Kind::Decision,
                        "Use WAL mode",
                        "2026-10-01T00:00:00.000Z",
                        Some(0.99),
                    ),
                    mem(
                        "U",
                        "app",
                        Kind::Decision,
                        "Use WAL mode for the hub database",
                        "2026-09-01T00:00:00.000Z",
                        Some(0.90),
                    ),
                    mem(
                        "Z",
                        "app",
                        Kind::Fact,
                        "Tom yum recipe",
                        "2026-09-01T00:00:00.000Z",
                        Some(0.40),
                    ),
                ],
            )
            .unwrap();
        let by = |id: &str| out.iter().find(|r| r.existing_id == id).unwrap();
        assert_eq!(by("D").probs.best().unwrap().0, "duplicate");
        assert!(by("D").probs.p("duplicate") >= 0.97);
        assert_eq!(
            by("X").probs.best().unwrap().0,
            "unrelated",
            "never across projects"
        );
        assert_eq!(by("U").probs.best().unwrap().0, "updates");
        assert!(
            by("U").probs.p("updates") < 0.85,
            "a heuristic update is only ever a suggestion"
        );
        assert_eq!(by("Z").probs.best().unwrap().0, "unrelated");
    }

    #[test]
    fn only_text_identical_under_an_idempotent_write_is_marked_as_exact() {
        let j = HeuristicJudge::default();
        let at = "2026-10-13T00:00:00.000Z";
        let one = |title: &str, kind: Kind| mem("N", "app", kind, title, at, None);
        let twin = |title: &str, kind: Kind| mem("T", "app", kind, title, at, Some(0.999));
        let exact = |new: &str, old: &str, kind: Kind| {
            j.compare(&one(new, Kind::Note), &[twin(old, kind)])
                .unwrap()[0]
                .agreement
        };
        for (a, b) in [
            ("if retry > 3 then stop", "if retry < 3 then stop"),
            ("offset -5", "offset 5"),
            ("uses C++", "uses C"),
            ("if !enabled", "if enabled"),
            ("ratio 1.5", "ratio 1,5"),
            (
                "Postgres listens on port 5433",
                "Postgres listens on port 5432",
            ),
            ("Use WAL", "use wal"),
            ("Use WAL.", "Use WAL"),
        ] {
            assert_eq!(exact(a, b, Kind::Note), Some(false), "{a:?} vs {b:?}");
        }
        assert_eq!(
            exact("Use  WAL\n mode", "Use WAL mode", Kind::Note),
            Some(true),
            "whitespace only"
        );
        assert_eq!(
            exact("Use WAL mode", "Use WAL mode", Kind::Decision),
            Some(true),
            "the same text under another kind"
        );
        let far = mem("F", "app", Kind::Note, "Tom yum", at, Some(0.3));
        assert_eq!(
            j.compare(&one("Use WAL", Kind::Note), &[far]).unwrap()[0].agreement,
            None
        );
    }

    #[test]
    fn kind_rules_read_english_and_thai_cues() {
        assert_eq!(
            kind_rules(
                "Fixed the crash",
                "Root cause: a null pointer. Fix: check first."
            )
            .best()
            .unwrap()
            .0,
            "bugfix"
        );
        assert_eq!(
            kind_rules("ตัดสินใจเลือกใช้ SQLite", "เพราะไม่ต้องมี server")
                .best()
                .unwrap()
                .0,
            "decision"
        );
        assert_eq!(
            kind_rules(
                "Never run migrations on prod by hand",
                "careful: it locks tables"
            )
            .best()
            .unwrap()
            .0,
            "warning"
        );
        let vague = kind_rules("Meeting", "we talked");
        assert!(vague.best().unwrap().1 < 0.5);
    }

    #[test]
    fn thai_glued_names_route_and_timestamps_compare_as_times() {
        let j = HeuristicJudge::default();
        let m = mem(
            "N",
            "global",
            Kind::Note,
            "ใช้pg-aiกับเซิร์ฟเวอร์",
            "2026-10-13T00:00:00.000Z",
            None,
        );
        let projects = vec![
            ProjectOption {
                slug: "pg-ai".into(),
                name: "pg-ai".into(),
                hint: String::new(),
            },
            ProjectOption {
                slug: String::new(),
                name: String::new(),
                hint: String::new(),
            },
        ];
        assert_eq!(
            j.route_project(&m, &projects).unwrap().best().unwrap().0,
            "pg-ai"
        );
        let m2 = mem(
            "N",
            "global",
            Kind::Note,
            "pg-aid is different",
            "2026-10-13T00:00:00.000Z",
            None,
        );
        assert_eq!(
            j.route_project(&m2, &projects).unwrap().best().unwrap().0,
            "global"
        );
        // +07:00 offset: 2026-10-13T05:00+07:00 is earlier than 2026-10-13T00:00Z? No, it is
        // 2026-10-12T22:00Z, so it is older although it sorts later as a string.
        assert!(newer(
            "2026-10-13T00:00:00.000Z",
            "2026-10-13T05:00:00+07:00"
        ));
        assert!(!newer(
            "2026-10-13T05:00:00+07:00",
            "2026-10-13T00:00:00.000Z"
        ));
        assert!(!newer("bad", "2026-10-13T00:00:00.000Z"));
    }

    #[test]
    fn routing_and_predicates() {
        let j = HeuristicJudge::default();
        let m = mem(
            "N",
            "global",
            Kind::Note,
            "pg-ai v4 frames must be framed protobuf",
            "2026-10-13T00:00:00.000Z",
            None,
        );
        let projects = vec![
            ProjectOption {
                slug: "pg-ai".into(),
                name: "pg-ai".into(),
                hint: String::new(),
            },
            ProjectOption {
                slug: "alice".into(),
                name: "alice".into(),
                hint: String::new(),
            },
        ];
        let d = j.route_project(&m, &projects).unwrap();
        assert_eq!(d.best().unwrap().0, "pg-ai");
        assert!(d.best().unwrap().1 < 0.85);
        let p = j
            .match_predicate("uses", &["use".into(), "deployed_on".into()])
            .unwrap();
        assert_eq!(p.best().unwrap(), ("use".to_string(), 0.95));
        for set in [["uses", "use"], ["use", "uses"]] {
            let d = j.match_predicate("uses", &set.map(String::from)).unwrap();
            assert_eq!(d.best().unwrap(), ("uses".to_string(), 0.95), "{set:?}");
        }
        let us = j
            .match_predicate("uses", &["us".into(), "use".into()])
            .unwrap();
        assert_eq!(us.best().unwrap().0, "use");
        let none = j.match_predicate("owns", &["deployed_on".into()]).unwrap();
        assert_eq!(none.best().unwrap().0, "none");
        // Opposite directions are similar, never sure (F3): at most a review item.
        for (new, old) in [("depended_on_by", "depends_on"), ("used_by", "uses")] {
            assert!(directions_differ(new, old));
            let d = j.match_predicate(new, &[old.to_string()]).unwrap();
            let p = d.p(old);
            assert!(p < 0.85, "{new} -> {old} is at most a suggestion, got {p}");
        }
    }
}
