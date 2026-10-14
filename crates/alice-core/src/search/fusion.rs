//! Reciprocal rank fusion and ranking boosts. Pure functions.

use crate::model::{Kind, Status};
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

/// Ranking multipliers (spec §4.4); the `[search.weights]` settings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    /// Multiplier for knowledge memories.
    pub knowledge: f64,
    /// Multiplier for raw transcript chunks.
    pub raw: f64,
    /// Multiplier for results from the current project.
    pub current_project: f64,
    /// Multiplier for results from the global project.
    pub global_project: f64,
    /// Multiplier for superseded memories.
    pub superseded: f64,
    /// Multiplier for archived memories.
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

/// Recency half-lives in days per kind (spec §4.4); 0 means no decay. The `[search.half_life]`
/// settings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HalfLives {
    /// Days for `status` memories (0 = no decay).
    pub status: f64,
    /// Days for `handoff` memories (0 = no decay).
    pub handoff: f64,
    /// Days for `diary` memories (0 = no decay).
    pub diary: f64,
    /// Days for `note` memories (0 = no decay).
    pub note: f64,
    /// Days for `fact` memories (0 = no decay).
    pub fact: f64,
    /// Days for `howto` memories (0 = no decay).
    pub howto: f64,
    /// Days for `bugfix` memories (0 = no decay).
    pub bugfix: f64,
    /// Days for `summary` memories (0 = no decay).
    pub summary: f64,
    /// Days for `decision` memories (0 = no decay).
    pub decision: f64,
    /// Days for `preference` memories (0 = no decay).
    pub preference: f64,
    /// Days for `warning` memories (0 = no decay).
    pub warning: f64,
    /// Days for raw transcript chunks (0 = no decay).
    pub raw: f64,
}

impl Default for HalfLives {
    fn default() -> Self {
        Self {
            status: 7.0,
            handoff: 14.0,
            diary: 30.0,
            note: 90.0,
            fact: 180.0,
            howto: 180.0,
            bugfix: 180.0,
            summary: 180.0,
            decision: 0.0,
            preference: 0.0,
            warning: 0.0,
            raw: crate::model::RAW_HALF_LIFE_DAYS,
        }
    }
}

impl HalfLives {
    /// The half-life for `k`, or `None` when it does not decay.
    pub fn days(&self, k: Kind) -> Option<f64> {
        let d = match k {
            Kind::Status => self.status,
            Kind::Handoff => self.handoff,
            Kind::Diary => self.diary,
            Kind::Note => self.note,
            Kind::Fact => self.fact,
            Kind::Howto => self.howto,
            Kind::Bugfix => self.bugfix,
            Kind::Summary => self.summary,
            Kind::Decision => self.decision,
            Kind::Preference => self.preference,
            Kind::Warning => self.warning,
        };
        (d > 0.0).then_some(d)
    }

    /// The half-life for raw chunks, or `None` when they do not decay.
    pub fn raw_days(&self) -> Option<f64> {
        (self.raw > 0.0).then_some(self.raw)
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

    /// The published schema says `additionalProperties: false` for both groups alike.
    #[test]
    fn weights_and_half_lives_both_refuse_unknown_fields() {
        let typo = serde_json::json!({"knowledgee": 2.0});
        assert!(serde_json::from_value::<Weights>(typo.clone()).is_err());
        assert!(serde_json::from_value::<HalfLives>(serde_json::json!({"statuss": 1.0})).is_err());
        assert!(serde_json::from_value::<Weights>(serde_json::json!({"knowledge": 2.0})).is_ok());
    }

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

    /// The settings' half-lives start as the spec's (spec §4.4), so ranking is unchanged.
    #[test]
    fn default_half_lives_are_the_kinds_own() {
        let h = HalfLives::default();
        for k in crate::model::Kind::ALL {
            assert_eq!(h.days(k), k.half_life_days(), "{k:?}");
        }
        assert_eq!(h.raw_days(), Some(crate::model::RAW_HALF_LIFE_DAYS));
        let no_decay = HalfLives {
            status: 0.0,
            raw: 0.0,
            ..HalfLives::default()
        };
        assert_eq!(no_decay.days(crate::model::Kind::Status), None);
        assert_eq!(no_decay.raw_days(), None);
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
