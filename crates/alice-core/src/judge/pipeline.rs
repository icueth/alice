//! From relation probabilities to actions and review items (spec §5.2 thresholds).
//!
//! `decide` is pure: it never looks at the database or the network. A relation at or above the
//! auto-apply threshold becomes an action, one between the thresholds becomes a review item, and
//! anything below the review threshold is ignored. The heuristic's relations auto-apply only for
//! exact duplicates (`Relation::agreement == Some(true)`: the same text; its other duplicates
//! and its "updates" are similarity guesses, so they are suggestions at most). A heuristic that
//! stands in for the judge asked for, or answers because that judge failed, only suggests.
//!
//! A distilled newcomer (SP2 spec §6.6) never asks a person and never replaces an explicit
//! memory: one it duplicates or updates is what it duplicates (Task 11b). An explicit newcomer
//! replaces its older distilled duplicates. See `decide_distilled`.

use super::{JudgeMemory, Relation};
use crate::model::Origin;
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
    /// A `relates_to` link: what a distilled newcomer gets where an explicit one would be asked
    /// whether it updates an older distilled memory (SP2 spec §6.6).
    RelatesTo { other: String, p: f64 },
    /// The new memory, an explicit one, replaces its older `distilled` duplicate: the distilled
    /// one is trashed and points to it (SP2 spec §6.6).
    ReplaceDistilled { distilled: String, p: f64 },
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

/// [`merge_review`] the other way round: the new (explicit) memory survives its older
/// `distilled` duplicate.
fn replace_review(new_id: &str, distilled: &str, p: f64, provider: &str) -> NewReview {
    NewReview {
        payload: serde_json::json!({"survivor": new_id, "others": [distilled], "p": p, "provider": provider}),
        ..merge_review(new_id, distilled, p, provider)
    }
}

/// What to do with `new` given how it relates to `relations` (decided by `provider`).
/// With `review_only` nothing is applied: every result at or above the review threshold becomes
/// a review item. A distilled newcomer follows [`decide_distilled`].
pub fn decide(
    new: &JudgeMemory,
    relations: &[Relation],
    t: &Thresholds,
    provider: &str,
    review_only: bool,
) -> Decided {
    if new.origin == Origin::Distill {
        return decide_distilled(relations, t, provider, review_only);
    }
    let heuristic = provider == "heuristic";
    // May this relation be applied without asking? The heuristic is sure of the same text only.
    let may_apply = |r: &Relation| !review_only && (!heuristic || r.agreement == Some(true));
    let confident = |r: &Relation| {
        strongest(r)
            .filter(|(k, p)| *k == "duplicate" && *p >= t.auto && may_apply(r))
            .map(|(_, p)| p)
    };
    let mut out = Decided::default();
    // A confident duplicate settles it: the new memory goes to the trash and nothing else about
    // it matters. An explicit memory is never lost to a distilled copy, so only an explicit
    // older memory survives it.
    if let Some((r, p)) = relations
        .iter()
        .filter(|r| r.existing_origin != Origin::Distill)
        .filter_map(|r| confident(r).map(|p| (r, p)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
    {
        out.actions.push(JudgeAction::DuplicateOf {
            survivor: r.existing_id.clone(),
            p,
        });
        return out;
    }
    // An older distilled duplicate is replaced by the new memory, which stays: the rest of its
    // relations still count.
    let mut replaced: Vec<&str> = Vec::new();
    for r in relations
        .iter()
        .filter(|r| r.existing_origin == Origin::Distill)
    {
        if let Some(p) = confident(r) {
            out.actions.push(JudgeAction::ReplaceDistilled {
                distilled: r.existing_id.clone(),
                p,
            });
            replaced.push(&r.existing_id);
        }
    }
    for r in relations {
        let Some((kind, p)) = strongest(r) else {
            continue;
        };
        if p < t.review || replaced.contains(&r.existing_id.as_str()) {
            continue;
        }
        let old = r.existing_id.clone();
        match kind {
            "updates" if p >= t.auto && !heuristic && !review_only => {
                out.actions.push(JudgeAction::Supersedes { old, p });
            }
            "updates" => out
                .reviews
                .push(supersede_review(&new.id, &old, p, provider)),
            "duplicate" if r.existing_origin == Origin::Distill => {
                out.reviews.push(replace_review(&new.id, &old, p, provider))
            }
            "duplicate" => out.reviews.push(merge_review(&new.id, &old, p, provider)),
            "contradicts" => {
                if !review_only {
                    out.actions.push(JudgeAction::Contradicts {
                        other: old.clone(),
                        p,
                    });
                }
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

/// [`decide`] for a distilled newcomer (SP2 spec §6.6): nothing is ever asked. Its best match at
/// the review threshold settles it (it goes to the trash as its duplicate): a duplicate, or an
/// explicit memory it updates (Task 11b: a near-copy of what an agent or a person saved, with
/// more detail or a newer value, never replaces it; the explicit memory is kept, and the
/// passage stays in the raw capture). An older distilled memory it updates is superseded
/// from the auto threshold and linked `relates_to` below it (the judge worker links one of
/// another project instead of superseding it); a contradiction is linked.
/// The heuristic applies the same text only (never an update), and a heuristic standing in for
/// the judge decides nothing: the memory waits for the judge's retry.
fn decide_distilled(
    relations: &[Relation],
    t: &Thresholds,
    provider: &str,
    review_only: bool,
) -> Decided {
    let mut out = Decided::default();
    if review_only {
        return out;
    }
    let heuristic = provider == "heuristic";
    // Whether the strongest relation `k` makes the newcomer a duplicate of `r`'s memory.
    let settles = |r: &Relation, k: &str| match k {
        "duplicate" => !heuristic || r.agreement == Some(true),
        "updates" => !heuristic && r.existing_origin != Origin::Distill,
        _ => false,
    };
    if let Some((r, p)) = relations
        .iter()
        .filter_map(|r| {
            strongest(r)
                .filter(|(k, p)| *p >= t.review && settles(r, k))
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
    if heuristic {
        return out;
    }
    for r in relations {
        let Some((kind, p)) = strongest(r) else {
            continue;
        };
        if p < t.review {
            continue;
        }
        let other = r.existing_id.clone();
        match kind {
            "updates" if p >= t.auto && r.existing_origin == Origin::Distill => {
                out.actions.push(JudgeAction::Supersedes { old: other, p });
            }
            "updates" => out.actions.push(JudgeAction::RelatesTo { other, p }),
            "contradicts" => out.actions.push(JudgeAction::Contradicts { other, p }),
            _ => {}
        }
    }
    out
}

/// The review item that asks about `action` instead of applying it (its target changed before
/// it could be applied). A distilled newcomer's actions are never asked about (the writer drops
/// them instead).
pub fn as_review(new_id: &str, action: &JudgeAction, provider: &str) -> Option<NewReview> {
    match action {
        JudgeAction::Supersedes { old, p } => Some(supersede_review(new_id, old, *p, provider)),
        JudgeAction::DuplicateOf { survivor, p } => {
            Some(merge_review(new_id, survivor, *p, provider))
        }
        JudgeAction::ReplaceDistilled { distilled, p } => {
            Some(replace_review(new_id, distilled, *p, provider))
        }
        // `decide` already asked about the contradiction; a link between a distilled memory and
        // another is never asked about.
        JudgeAction::Contradicts { .. } | JudgeAction::RelatesTo { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Distribution, JudgeMemory, Relation};
    use crate::model::{Kind, Origin};

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
            raw: false,
            status: None,
            origin: Origin::Agent,
        }
    }

    fn rel(id: &str, pairs: &[(&str, f64)]) -> Relation {
        Relation {
            existing_id: id.into(),
            probs: Distribution::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), *v))),
            agreement: None,
            existing_origin: Origin::Agent,
        }
    }

    /// A new memory distilled from a session (SP2 spec §6.5).
    fn distilled() -> JudgeMemory {
        JudgeMemory {
            origin: Origin::Distill,
            ..new()
        }
    }

    /// [`rel`] to an existing memory that was distilled too.
    fn rel_distilled(id: &str, pairs: &[(&str, f64)]) -> Relation {
        Relation {
            existing_origin: Origin::Distill,
            ..rel(id, pairs)
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
            false,
        );
        assert_eq!(
            d.actions,
            vec![JudgeAction::DuplicateOf {
                survivor: "B".into(),
                p: 0.97
            }]
        );
        assert!(d.reviews.is_empty());
        let exact = Relation {
            agreement: Some(true),
            ..rel("B", &[("duplicate", 0.98), ("unrelated", 0.02)])
        };
        let h = decide(&new(), std::slice::from_ref(&exact), &T, "heuristic", false);
        assert_eq!(h.actions.len(), 1, "the heuristic applies the same text");
    }

    #[test]
    fn the_heuristic_only_asks_about_a_near_miss_and_a_stand_in_never_applies() {
        let near = rel("B", &[("duplicate", 0.99), ("unrelated", 0.01)]);
        let h = decide(&new(), std::slice::from_ref(&near), &T, "heuristic", false);
        assert!(h.actions.is_empty(), "similar is not the same");
        assert_eq!(h.reviews.len(), 1);
        assert_eq!(h.reviews[0].kind, ReviewType::Merge);
        let exact = Relation {
            agreement: Some(true),
            ..near.clone()
        };
        let stand_in = decide(&new(), std::slice::from_ref(&exact), &T, "heuristic", true);
        assert!(
            stand_in.actions.is_empty(),
            "an outage fallback applies nothing"
        );
        assert_eq!(stand_in.reviews.len(), 1);
        let model_exact = decide(
            &new(),
            &[rel("B", &[("duplicate", 0.99)])],
            &T,
            "jev",
            false,
        );
        assert_eq!(model_exact.actions.len(), 1, "a model judge is unaffected");
    }

    #[test]
    fn updates_apply_for_a_model_judge_but_only_suggest_for_the_heuristic() {
        let r = [rel("OLD", &[("updates", 0.9), ("unrelated", 0.1)])];
        assert_eq!(
            decide(&new(), &r, &T, "jev", false).actions,
            vec![JudgeAction::Supersedes {
                old: "OLD".into(),
                p: 0.9
            }]
        );
        let h = decide(&new(), &r, &T, "heuristic", false);
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
            false,
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
            false,
        );
        assert!(mid.actions.is_empty());
        assert_eq!(mid.reviews[0].kind, ReviewType::Merge);
        assert_eq!(mid.reviews[0].payload["survivor"], "D");
        let low = decide(
            &new(),
            &[rel("E", &[("updates", 0.4), ("unrelated", 0.6)])],
            &T,
            "jev",
            false,
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

    /// SP2 spec §6.6: one case per row of the table "Distilled memories never ask a person".
    /// Whatever the judge says, a distilled newcomer gets no review item.
    #[test]
    fn distilled_newcomers_never_create_review_items() {
        let dup = |p: f64| JudgeAction::DuplicateOf {
            survivor: "OLD".into(),
            p,
        };
        let relates = |p: f64| JudgeAction::RelatesTo {
            other: "OLD".into(),
            p,
        };
        let cases: Vec<(&str, Relation, &str, bool, Vec<JudgeAction>)> = vec![
            (
                "duplicate >= auto: trashed as a duplicate, as today",
                rel("OLD", &[("duplicate", 0.95), ("unrelated", 0.05)]),
                "jev",
                false,
                vec![dup(0.95)],
            ),
            (
                "duplicate between review and auto: trashed, not asked",
                rel("OLD", &[("duplicate", 0.6), ("unrelated", 0.4)]),
                "jev",
                false,
                vec![dup(0.6)],
            ),
            (
                "updates >= auto of a distilled memory: supersedes it",
                rel_distilled("OLD", &[("updates", 0.9), ("unrelated", 0.1)]),
                "jev",
                false,
                vec![JudgeAction::Supersedes {
                    old: "OLD".into(),
                    p: 0.9,
                }],
            ),
            (
                "updates >= auto of an agent's memory: trashed as its duplicate (Task 11b)",
                rel("OLD", &[("updates", 0.9), ("unrelated", 0.1)]),
                "jev",
                false,
                vec![dup(0.9)],
            ),
            (
                "updates between review and auto of an agent's memory: trashed, not asked",
                rel("OLD", &[("updates", 0.6), ("unrelated", 0.4)]),
                "jev",
                false,
                vec![dup(0.6)],
            ),
            (
                "updates between review and auto of a distilled memory: linked too",
                rel_distilled("OLD", &[("updates", 0.6), ("unrelated", 0.4)]),
                "jev",
                false,
                vec![relates(0.6)],
            ),
            (
                "contradicts >= review: the link, without the review item",
                rel("OLD", &[("contradicts", 0.6), ("unrelated", 0.4)]),
                "jev",
                false,
                vec![JudgeAction::Contradicts {
                    other: "OLD".into(),
                    p: 0.6,
                }],
            ),
            (
                "contradicts >= auto: the link, without the review item",
                rel("OLD", &[("contradicts", 0.9), ("unrelated", 0.1)]),
                "jev",
                false,
                vec![JudgeAction::Contradicts {
                    other: "OLD".into(),
                    p: 0.9,
                }],
            ),
            (
                "below the review threshold: nothing",
                rel("OLD", &[("updates", 0.4), ("unrelated", 0.6)]),
                "jev",
                false,
                vec![],
            ),
            (
                "the heuristic applies the same text",
                Relation {
                    agreement: Some(true),
                    ..rel("OLD", &[("duplicate", 0.99), ("unrelated", 0.01)])
                },
                "heuristic",
                false,
                vec![dup(0.99)],
            ),
            (
                "the heuristic's near twin: nothing (no review)",
                rel("OLD", &[("duplicate", 0.99), ("unrelated", 0.01)]),
                "heuristic",
                false,
                vec![],
            ),
            (
                "the heuristic's updates: nothing (no link, no review)",
                rel("OLD", &[("updates", 0.7), ("unrelated", 0.3)]),
                "heuristic",
                false,
                vec![],
            ),
        ];
        // A heuristic standing in for the judge (an outage, a missing key) decides nothing at
        // all, not even the same text: the memory waits for the judge's retry.
        let stand_in: Vec<_> = cases
            .iter()
            .map(|(what, r, _, _, _)| (*what, r.clone(), "heuristic", true, Vec::new()))
            .chain([(
                "stand-in, the same text",
                Relation {
                    agreement: Some(true),
                    ..rel("OLD", &[("duplicate", 0.99), ("unrelated", 0.01)])
                },
                "heuristic",
                true,
                Vec::new(),
            )])
            .collect();
        for (what, r, provider, review_only, want) in cases.into_iter().chain(stand_in) {
            let d = decide(
                &distilled(),
                std::slice::from_ref(&r),
                &T,
                provider,
                review_only,
            );
            assert!(d.reviews.is_empty(), "{what}: {:?}", d.reviews);
            assert_eq!(d.actions, want, "{what} (stand-in: {review_only})");
        }
        // Several relations: the best duplicate settles it before anything else is applied.
        let d = decide(
            &distilled(),
            &[
                rel_distilled("A", &[("updates", 0.9), ("unrelated", 0.1)]),
                rel("B", &[("duplicate", 0.55), ("unrelated", 0.45)]),
                rel("C", &[("duplicate", 0.7), ("unrelated", 0.3)]),
                rel("D", &[("contradicts", 0.9), ("unrelated", 0.1)]),
            ],
            &T,
            "jev",
            false,
        );
        assert_eq!(
            d.actions,
            vec![JudgeAction::DuplicateOf {
                survivor: "C".into(),
                p: 0.7
            }]
        );
        assert!(d.reviews.is_empty());
    }

    /// Task 11b (spec §6.6): a distilled newcomer whose strongest relation to an explicit memory
    /// (an agent's or a person's) is `updates` at the review threshold or above is a near-copy of
    /// it: it goes to the trash as its duplicate, with no review item, and never replaces it
    /// (R19). Updates of a distilled memory and contradictions keep Task 11's rules, and the
    /// heuristic and a stand-in still apply no updates (R2).
    #[test]
    fn a_distilled_update_of_an_explicit_memory_is_its_duplicate() {
        let dup = |survivor: &str, p: f64| {
            vec![JudgeAction::DuplicateOf {
                survivor: survivor.into(),
                p,
            }]
        };
        let import = |id: &str, pairs: &[(&str, f64)]| Relation {
            existing_origin: Origin::Import,
            ..rel(id, pairs)
        };
        // (what, relations, provider, review_only, actions wanted)
        type Case = (
            &'static str,
            Vec<Relation>,
            &'static str,
            bool,
            Vec<JudgeAction>,
        );
        let cases: Vec<Case> = vec![
            (
                "updates 0.55 of an agent's memory",
                vec![rel("OLD", &[("updates", 0.55), ("unrelated", 0.45)])],
                "jev",
                false,
                dup("OLD", 0.55),
            ),
            (
                "updates 0.845 (just under auto) of an agent's memory",
                vec![rel("OLD", &[("updates", 0.845), ("unrelated", 0.155)])],
                "jev",
                false,
                dup("OLD", 0.845),
            ),
            (
                "updates at the review threshold exactly",
                vec![rel("OLD", &[("updates", 0.5), ("unrelated", 0.5)])],
                "jev",
                false,
                dup("OLD", 0.5),
            ),
            (
                "updates of an imported memory (saved by an agent or a person)",
                vec![import("OLD", &[("updates", 0.7), ("unrelated", 0.3)])],
                "jev",
                false,
                dup("OLD", 0.7),
            ),
            (
                "updates 0.4 of an agent's memory: nothing",
                vec![rel("OLD", &[("updates", 0.4), ("unrelated", 0.6)])],
                "jev",
                false,
                vec![],
            ),
            (
                "updates 0.6 of a distilled memory: Task 11's link",
                vec![rel_distilled(
                    "OLD",
                    &[("updates", 0.6), ("unrelated", 0.4)],
                )],
                "jev",
                false,
                vec![JudgeAction::RelatesTo {
                    other: "OLD".into(),
                    p: 0.6,
                }],
            ),
            (
                "updates 0.9 of a distilled memory: Task 11's supersede",
                vec![rel_distilled(
                    "OLD",
                    &[("updates", 0.9), ("unrelated", 0.1)],
                )],
                "jev",
                false,
                vec![JudgeAction::Supersedes {
                    old: "OLD".into(),
                    p: 0.9,
                }],
            ),
            (
                // Weights 0.6 and 0.8, normalised to 3/7 and 4/7.
                "the strongest relation is a contradiction: Task 11's link",
                vec![rel("OLD", &[("updates", 0.6), ("contradicts", 0.8)])],
                "jev",
                false,
                vec![JudgeAction::Contradicts {
                    other: "OLD".into(),
                    p: 0.8 / 1.4,
                }],
            ),
            (
                "the heuristic's updates of an agent's memory: nothing",
                vec![rel("OLD", &[("updates", 0.7), ("unrelated", 0.3)])],
                "heuristic",
                false,
                vec![],
            ),
            (
                "a stand-in's updates of an agent's memory: nothing",
                vec![rel("OLD", &[("updates", 0.9), ("unrelated", 0.1)])],
                "heuristic",
                true,
                vec![],
            ),
            (
                "the best match settles it: an explicit update over a distilled duplicate",
                vec![
                    rel_distilled("D", &[("duplicate", 0.7), ("unrelated", 0.3)]),
                    rel("A", &[("updates", 0.8), ("unrelated", 0.2)]),
                    rel("C", &[("contradicts", 0.9), ("unrelated", 0.1)]),
                ],
                "jev",
                false,
                dup("A", 0.8),
            ),
            (
                "the best match settles it: a duplicate over an explicit update",
                vec![
                    rel("A", &[("updates", 0.6), ("unrelated", 0.4)]),
                    rel("B", &[("duplicate", 0.9), ("unrelated", 0.1)]),
                ],
                "jev",
                false,
                dup("B", 0.9),
            ),
        ];
        for (what, relations, provider, review_only, want) in cases {
            let d = decide(&distilled(), &relations, &T, provider, review_only);
            assert!(d.reviews.is_empty(), "{what}: {:?}", d.reviews);
            assert_eq!(d.actions, want, "{what}");
        }
        // Only the strongest relation counts: with a lower review threshold an update over it
        // that a contradiction outweighs is still Task 11's contradiction link.
        let low = Thresholds {
            auto: 0.85,
            review: 0.3,
        };
        let d = decide(
            &distilled(),
            &[rel("OLD", &[("updates", 0.4), ("contradicts", 0.6)])],
            &low,
            "jev",
            false,
        );
        assert_eq!(
            d.actions,
            vec![JudgeAction::Contradicts {
                other: "OLD".into(),
                p: 0.6,
            }]
        );
        // An explicit newcomer's update is unchanged: it supersedes, or asks between the
        // thresholds.
        let r = [rel("OLD", &[("updates", 0.6), ("unrelated", 0.4)])];
        let d = decide(&new(), &r, &T, "jev", false);
        assert!(d.actions.is_empty());
        assert_eq!(d.reviews[0].kind, ReviewType::Supersede);
    }

    /// SP2 spec §6.6: an explicit newcomer that duplicates an older distilled memory survives,
    /// and the distilled one is merged into it.
    #[test]
    fn an_agent_memory_replaces_an_older_distilled_duplicate() {
        let d = decide(
            &new(),
            &[rel_distilled(
                "D",
                &[("duplicate", 0.95), ("unrelated", 0.05)],
            )],
            &T,
            "jev",
            false,
        );
        assert_eq!(
            d.actions,
            vec![JudgeAction::ReplaceDistilled {
                distilled: "D".into(),
                p: 0.95
            }]
        );
        assert!(d.reviews.is_empty());
        // The newcomer stays, so its other relations still count; every confident distilled
        // copy is replaced.
        let d = decide(
            &new(),
            &[
                rel_distilled("D1", &[("duplicate", 0.95), ("unrelated", 0.05)]),
                rel("OLD", &[("updates", 0.9), ("unrelated", 0.1)]),
                rel_distilled("D2", &[("duplicate", 0.9), ("unrelated", 0.1)]),
            ],
            &T,
            "jev",
            false,
        );
        assert_eq!(
            d.actions,
            vec![
                JudgeAction::ReplaceDistilled {
                    distilled: "D1".into(),
                    p: 0.95
                },
                JudgeAction::ReplaceDistilled {
                    distilled: "D2".into(),
                    p: 0.9
                },
                JudgeAction::Supersedes {
                    old: "OLD".into(),
                    p: 0.9
                },
            ]
        );
        // An older explicit duplicate still wins: the newcomer goes to it, whatever the
        // distilled copy scored.
        let d = decide(
            &new(),
            &[
                rel_distilled("D", &[("duplicate", 0.97), ("unrelated", 0.03)]),
                rel("A", &[("duplicate", 0.9), ("unrelated", 0.1)]),
            ],
            &T,
            "jev",
            false,
        );
        assert_eq!(
            d.actions,
            vec![JudgeAction::DuplicateOf {
                survivor: "A".into(),
                p: 0.9
            }]
        );
        // The heuristic replaces the same text only.
        let exact = Relation {
            agreement: Some(true),
            ..rel_distilled("D", &[("duplicate", 0.99), ("unrelated", 0.01)])
        };
        let h = decide(&new(), std::slice::from_ref(&exact), &T, "heuristic", false);
        assert_eq!(
            h.actions,
            vec![JudgeAction::ReplaceDistilled {
                distilled: "D".into(),
                p: 0.99
            }]
        );
        // A middle-band duplicate is asked as today, with the explicit newcomer as the
        // survivor; so is the heuristic's near twin and a stand-in's guess.
        for (r, provider, review_only) in [
            (
                rel_distilled("D", &[("duplicate", 0.6), ("unrelated", 0.4)]),
                "jev",
                false,
            ),
            (
                rel_distilled("D", &[("duplicate", 0.99), ("unrelated", 0.01)]),
                "heuristic",
                false,
            ),
            (exact.clone(), "heuristic", true),
        ] {
            let d = decide(&new(), std::slice::from_ref(&r), &T, provider, review_only);
            assert!(d.actions.is_empty(), "{r:?}");
            assert_eq!(d.reviews.len(), 1, "{r:?}");
            let m = &d.reviews[0];
            assert_eq!(m.kind, ReviewType::Merge);
            assert_eq!(m.payload["survivor"], "NEW", "{r:?}");
            assert_eq!(m.payload["others"], serde_json::json!(["D"]));
            assert_eq!(m.dedupe_key.as_deref(), Some("merge:D:NEW"));
        }
        // Its target edited before it is applied, a replacement becomes the same question; a
        // `relates_to` link is never asked about.
        let m = as_review(
            "NEW",
            &JudgeAction::ReplaceDistilled {
                distilled: "D".into(),
                p: 0.95,
            },
            "jev",
        )
        .unwrap();
        assert_eq!(m.kind, ReviewType::Merge);
        assert_eq!(
            (&m.payload["survivor"], &m.payload["others"]),
            (&serde_json::json!("NEW"), &serde_json::json!(["D"]))
        );
        assert_eq!(m.dedupe_key.as_deref(), Some("merge:D:NEW"));
        assert!(
            as_review(
                "NEW",
                &JudgeAction::RelatesTo {
                    other: "OLD".into(),
                    p: 0.9
                },
                "jev"
            )
            .is_none()
        );
    }
}
