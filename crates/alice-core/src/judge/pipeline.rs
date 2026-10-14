//! From relation probabilities to actions and review items (spec §5.2 thresholds).
//!
//! `decide` is pure: it never looks at the database or the network. A relation at or above the
//! auto-apply threshold becomes an action, one between the thresholds becomes a review item, and
//! anything below the review threshold is ignored. The heuristic's relations auto-apply only for
//! duplicates (its "updates" are a title-similarity guess, so they are suggestions at most).

use super::{JudgeMemory, Relation};
use crate::store::reviews::{NewReview, ReviewType};

/// The two thresholds of `judge.*`: apply from `auto`, ask from `review`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub auto: f64,
    pub review: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JudgeAction {
    /// The new memory duplicates `survivor`: it is trashed and points to it.
    DuplicateOf { survivor: String, p: f64 },
    /// The new memory supersedes `old`.
    Supersedes { old: String, p: f64 },
    /// A `contradicts` link (a review item asks which is right).
    Contradicts { other: String, p: f64 },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decided {
    pub actions: Vec<JudgeAction>,
    pub reviews: Vec<NewReview>,
}

/// The strongest non-`unrelated` relation and its probability.
fn strongest(r: &Relation) -> Option<(&'static str, f64)> {
    ["duplicate", "updates", "contradicts"]
        .into_iter()
        .map(|k| (k, r.probs.p(k)))
        .filter(|(_, p)| *p > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

fn supersede_review(new_id: &str, old: &str, p: f64, provider: &str) -> NewReview {
    NewReview {
        kind: ReviewType::Supersede,
        payload: serde_json::json!({"new_id": new_id, "old_id": old, "p": p, "provider": provider}),
        confidence: Some(p),
        dedupe_key: Some(format!("supersede:{new_id}:{old}")),
    }
}

fn merge_review(new_id: &str, survivor: &str, p: f64, provider: &str) -> NewReview {
    let mut ids = [new_id.to_string(), survivor.to_string()];
    ids.sort();
    NewReview {
        kind: ReviewType::Merge,
        payload: serde_json::json!({"survivor": survivor, "others": [new_id], "p": p, "provider": provider}),
        confidence: Some(p),
        dedupe_key: Some(format!("merge:{}", ids.join(":"))),
    }
}

/// What to do with `new` given how it relates to `relations` (decided by `provider`).
pub fn decide(
    new: &JudgeMemory,
    relations: &[Relation],
    t: &Thresholds,
    provider: &str,
) -> Decided {
    let heuristic = provider == "heuristic";
    let mut out = Decided::default();
    // A confident duplicate settles it: the new memory goes to the trash and nothing else about
    // it matters.
    if let Some((r, p)) = relations
        .iter()
        .filter_map(|r| {
            strongest(r)
                .filter(|(k, p)| *k == "duplicate" && *p >= t.auto)
                .map(|(_, p)| (r, p))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
    {
        out.actions.push(JudgeAction::DuplicateOf {
            survivor: r.existing_id.clone(),
            p,
        });
        return out;
    }
    for r in relations {
        let Some((kind, p)) = strongest(r) else {
            continue;
        };
        if p < t.review {
            continue;
        }
        let old = r.existing_id.clone();
        match kind {
            "updates" if p >= t.auto && !heuristic => {
                out.actions.push(JudgeAction::Supersedes { old, p });
            }
            "updates" => out
                .reviews
                .push(supersede_review(&new.id, &old, p, provider)),
            "duplicate" => out.reviews.push(merge_review(&new.id, &old, p, provider)),
            "contradicts" => {
                out.actions.push(JudgeAction::Contradicts {
                    other: old.clone(),
                    p,
                });
                out.reviews.push(NewReview {
                    kind: ReviewType::Contradiction,
                    payload: serde_json::json!({"new_id": new.id, "old_id": old, "p": p, "provider": provider}),
                    confidence: Some(p),
                    dedupe_key: Some(format!("contradiction:{}:{old}", new.id)),
                });
            }
            _ => {}
        }
    }
    out
}

/// The review item that asks about `action` instead of applying it (its target changed before
/// it could be applied).
pub fn as_review(new_id: &str, action: &JudgeAction, provider: &str) -> Option<NewReview> {
    match action {
        JudgeAction::Supersedes { old, p } => Some(supersede_review(new_id, old, *p, provider)),
        JudgeAction::DuplicateOf { survivor, p } => {
            Some(merge_review(new_id, survivor, *p, provider))
        }
        // `decide` already asked about the contradiction.
        JudgeAction::Contradicts { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Distribution, JudgeMemory, Relation};
    use crate::model::Kind;

    fn new() -> JudgeMemory {
        JudgeMemory {
            id: "NEW".into(),
            project: "app".into(),
            kind: Kind::Fact,
            title: "t".into(),
            body: "b".into(),
            importance: 3,
            created_at: "2026-10-13T00:00:00.000Z".into(),
            similarity: None,
        }
    }

    fn rel(id: &str, pairs: &[(&str, f64)]) -> Relation {
        Relation {
            existing_id: id.into(),
            probs: Distribution::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), *v))),
            agreement: None,
        }
    }

    const T: Thresholds = Thresholds {
        auto: 0.85,
        review: 0.5,
    };

    #[test]
    fn a_confident_duplicate_wins_and_stops_everything_else() {
        let d = decide(
            &new(),
            &[
                rel("A", &[("updates", 0.9), ("unrelated", 0.1)]),
                rel("B", &[("duplicate", 0.97), ("unrelated", 0.03)]),
            ],
            &T,
            "jev",
        );
        assert_eq!(
            d.actions,
            vec![JudgeAction::DuplicateOf {
                survivor: "B".into(),
                p: 0.97
            }]
        );
        assert!(d.reviews.is_empty());
        let h = decide(
            &new(),
            &[rel("B", &[("duplicate", 0.98), ("unrelated", 0.02)])],
            &T,
            "heuristic",
        );
        assert_eq!(
            h.actions.len(),
            1,
            "duplicates auto-apply for the heuristic too"
        );
    }

    #[test]
    fn updates_apply_for_a_model_judge_but_only_suggest_for_the_heuristic() {
        let r = [rel("OLD", &[("updates", 0.9), ("unrelated", 0.1)])];
        assert_eq!(
            decide(&new(), &r, &T, "jev").actions,
            vec![JudgeAction::Supersedes {
                old: "OLD".into(),
                p: 0.9
            }]
        );
        let h = decide(&new(), &r, &T, "heuristic");
        assert!(h.actions.is_empty());
        assert_eq!(h.reviews.len(), 1);
        assert_eq!(h.reviews[0].kind, ReviewType::Supersede);
        assert_eq!(
            h.reviews[0].dedupe_key.as_deref(),
            Some("supersede:NEW:OLD")
        );
    }

    #[test]
    fn contradictions_link_and_ask_middle_band_asks_low_band_does_nothing() {
        let d = decide(
            &new(),
            &[rel("C", &[("contradicts", 0.8), ("unrelated", 0.2)])],
            &T,
            "jev",
        );
        assert_eq!(
            d.actions,
            vec![JudgeAction::Contradicts {
                other: "C".into(),
                p: 0.8
            }]
        );
        assert_eq!(d.reviews[0].kind, ReviewType::Contradiction);
        let mid = decide(
            &new(),
            &[rel("D", &[("duplicate", 0.6), ("unrelated", 0.4)])],
            &T,
            "jev",
        );
        assert!(mid.actions.is_empty());
        assert_eq!(mid.reviews[0].kind, ReviewType::Merge);
        assert_eq!(mid.reviews[0].payload["survivor"], "D");
        let low = decide(
            &new(),
            &[rel("E", &[("updates", 0.4), ("unrelated", 0.6)])],
            &T,
            "jev",
        );
        assert_eq!((low.actions.len(), low.reviews.len()), (0, 0));
    }

    #[test]
    fn an_action_whose_target_changed_becomes_the_matching_review_item() {
        let s = as_review(
            "NEW",
            &JudgeAction::Supersedes {
                old: "OLD".into(),
                p: 0.9,
            },
            "jev",
        )
        .unwrap();
        assert_eq!(s.kind, ReviewType::Supersede);
        assert_eq!(s.dedupe_key.as_deref(), Some("supersede:NEW:OLD"));
        let m = as_review(
            "NEW",
            &JudgeAction::DuplicateOf {
                survivor: "OLD".into(),
                p: 0.95,
            },
            "jev",
        )
        .unwrap();
        assert_eq!(m.kind, ReviewType::Merge);
        assert_eq!(m.dedupe_key.as_deref(), Some("merge:NEW:OLD"));
        assert_eq!(m.payload["survivor"], "OLD");
        assert!(
            as_review(
                "NEW",
                &JudgeAction::Contradicts {
                    other: "C".into(),
                    p: 0.9
                },
                "jev"
            )
            .is_none()
        );
    }
}
