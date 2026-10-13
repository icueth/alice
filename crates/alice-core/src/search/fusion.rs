//! Reciprocal rank fusion and ranking boosts. Pure functions.

use crate::model::Status;
use std::collections::HashMap;

pub const RRF_K: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ItemRef {
    Memory(i64),
    Chunk(i64),
}

/// Sum of 1 / (k + rank) over every list an item appears in (rank starts at 1).
pub fn rrf(lists: &[Vec<ItemRef>]) -> HashMap<ItemRef, f64> {
    let mut scores = HashMap::new();
    for list in lists {
        for (i, item) in list.iter().enumerate() {
            *scores.entry(*item).or_insert(0.0) += 1.0 / (RRF_K + (i + 1) as f64);
        }
    }
    scores
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Weights {
    pub knowledge: f64,
    pub raw: f64,
    pub current_project: f64,
    pub global_project: f64,
    pub superseded: f64,
    pub archived: f64,
    /// Maximum boost from frequent use (0.10 = +10 %).
    pub usage_max: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            knowledge: 1.0,
            raw: 0.6,
            current_project: 1.3,
            global_project: 1.1,
            superseded: 0.3,
            archived: 0.5,
            usage_max: 0.10,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BoostInput {
    pub raw: bool,
    pub same_project: bool,
    pub is_global: bool,
    pub age_days: f64,
    pub half_life_days: Option<f64>,
    pub importance: u8,
    pub access_count: i64,
    pub status: Status,
}

/// Product of the spec §4.4 boosts: tier, project, recency, importance, usage and status.
pub fn boost(w: &Weights, b: &BoostInput) -> f64 {
    let tier = if b.raw { w.raw } else { w.knowledge };
    let project = if b.same_project {
        w.current_project
    } else if b.is_global {
        w.global_project
    } else {
        1.0
    };
    let recency = match b.half_life_days {
        Some(h) if h > 0.0 => 0.7 + 0.3 * (-b.age_days.max(0.0) / h).exp(),
        _ => 1.0,
    };
    let importance = 0.9 + 0.05 * f64::from(b.importance);
    // Full usage boost at 1000 accesses.
    let usage_frac = ((1.0 + b.access_count.max(0) as f64).ln() / 1001f64.ln()).min(1.0);
    let usage = 1.0 + w.usage_max * usage_frac;
    let status = match b.status {
        Status::Superseded => w.superseded,
        Status::Archived => w.archived,
        _ => 1.0,
    };
    tier * project * recency * importance * usage * status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    #[test]
    fn rrf_rewards_items_found_by_several_lists() {
        let a = ItemRef::Memory(1);
        let b = ItemRef::Memory(2);
        let s = rrf(&[vec![a, b], vec![a]]);
        assert!(s[&a] > s[&b]);
        assert!((s[&b] - 1.0 / 62.0).abs() < 1e-9);
        assert!(rrf(&[vec![], vec![]]).is_empty());
    }

    fn base() -> BoostInput {
        BoostInput {
            raw: false,
            same_project: false,
            is_global: false,
            age_days: 0.0,
            half_life_days: None,
            importance: 3,
            access_count: 0,
            status: Status::Active,
        }
    }

    #[test]
    fn boosts_follow_the_spec() {
        let w = Weights::default();
        let b0 = boost(&w, &base());
        assert!((b0 - 1.05).abs() < 1e-9, "importance 3 => 1.05, got {b0}");
        assert!(
            (boost(
                &w,
                &BoostInput {
                    raw: true,
                    ..base()
                }
            ) / b0
                - 0.6)
                .abs()
                < 1e-9
        );
        assert!(
            (boost(
                &w,
                &BoostInput {
                    same_project: true,
                    ..base()
                }
            ) / b0
                - 1.3)
                .abs()
                < 1e-9
        );
        assert!(
            (boost(
                &w,
                &BoostInput {
                    is_global: true,
                    ..base()
                }
            ) / b0
                - 1.1)
                .abs()
                < 1e-9
        );
        assert!(
            (boost(
                &w,
                &BoostInput {
                    status: Status::Superseded,
                    ..base()
                }
            ) / b0
                - 0.3)
                .abs()
                < 1e-9
        );
        assert!(
            (boost(
                &w,
                &BoostInput {
                    status: Status::Archived,
                    ..base()
                }
            ) / b0
                - 0.5)
                .abs()
                < 1e-9
        );
        let old = boost(
            &w,
            &BoostInput {
                age_days: 7.0,
                half_life_days: Some(7.0),
                ..base()
            },
        ) / b0;
        assert!((old - (0.7 + 0.3 * (-1.0f64).exp())).abs() < 1e-9);
        let hot = boost(
            &w,
            &BoostInput {
                access_count: 1000,
                ..base()
            },
        ) / b0;
        assert!((hot - 1.10).abs() < 1e-6);
        let critical = boost(
            &w,
            &BoostInput {
                importance: 5,
                ..base()
            },
        );
        assert!((critical - 1.15).abs() < 1e-9);
    }

    #[test]
    fn boosts_stay_bounded_on_odd_inputs() {
        let w = Weights::default();
        // A future date (clock skew) is not a bonus; a huge access count is capped.
        let future = boost(
            &w,
            &BoostInput {
                age_days: -30.0,
                half_life_days: Some(7.0),
                ..base()
            },
        );
        assert!((future - 1.05).abs() < 1e-9);
        let capped = boost(
            &w,
            &BoostInput {
                access_count: i64::MAX,
                ..base()
            },
        );
        assert!((capped - 1.05 * 1.10).abs() < 1e-9);
        let negative = boost(
            &w,
            &BoostInput {
                access_count: -5,
                ..base()
            },
        );
        assert!((negative - 1.05).abs() < 1e-9);
    }
}
