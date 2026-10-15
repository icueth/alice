//! Reciprocal rank fusion and ranking boosts. Pure functions.

use crate::model::{Kind, Status};
use std::collections::HashMap;

/// Default `search.rrf_k`: the constant of reciprocal rank fusion (a smaller k lets a list's
/// first places count for more).
pub const DEFAULT_RRF_K: f64 = 60.0;
/// Default `search.boost_exponent`: the weight of the priors in the final score.
pub const DEFAULT_BOOST_EXPONENT: f64 = 1.0;

/// How the candidate lists and the priors are combined: `score = rrf_k(item) × boost(item)^λ`.
/// The `search.rrf_k` and `search.boost_exponent` settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fusion {
    /// The k of reciprocal rank fusion (1 to 200).
    pub rrf_k: f64,
    /// λ, the exponent of the boost product (0 to 1): 1 multiplies the boosts in whole, 0 ignores them.
    pub boost_exponent: f64,
}

impl Default for Fusion {
    fn default() -> Self {
        Self {
            rrf_k: DEFAULT_RRF_K,
            boost_exponent: DEFAULT_BOOST_EXPONENT,
        }
    }
}

impl Fusion {
    /// The final score of an item: its fused rank score times its boosts to the power λ.
    ///
    /// ```
    /// use alice_core::search::fusion::Fusion;
    /// let f = Fusion { rrf_k: 60.0, boost_exponent: 0.5 };
    /// assert!((f.combine(0.02, 4.0) - 0.04).abs() < 1e-12);
    /// ```
    pub fn combine(&self, rrf_score: f64, boost: f64) -> f64 {
        rrf_score * boost.powf(self.boost_exponent)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ItemRef {
    Memory(i64),
    Chunk(i64),
}

/// Sum of 1 / (k + rank) over every list an item appears in (rank starts at 1).
pub fn rrf(lists: &[Vec<ItemRef>], k: f64) -> HashMap<ItemRef, f64> {
    let mut scores = HashMap::new();
    for list in lists {
        for (i, item) in list.iter().enumerate() {
            *scores.entry(*item).or_insert(0.0) += 1.0 / (k + (i + 1) as f64);
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
    /// Days for `note` memories (0 = no decay, the default).
    pub note: f64,
    /// Days for `fact` memories (0 = no decay, the default).
    pub fact: f64,
    /// Days for `howto` memories (0 = no decay, the default).
    pub howto: f64,
    /// Days for `bugfix` memories (0 = no decay, the default).
    pub bugfix: f64,
    /// Days for `summary` memories (0 = no decay, the default).
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
            note: 0.0,
            fact: 0.0,
            howto: 0.0,
            bugfix: 0.0,
            summary: 0.0,
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
        let s = rrf(&[vec![a, b], vec![a]], 60.0);
        assert!(s[&a] > s[&b]);
        assert!((s[&b] - 1.0 / 62.0).abs() < 1e-9);
        assert!(rrf(&[vec![], vec![]], 60.0).is_empty());
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

    /// The fused score of an item that sits at `ranks` (1-based) in lists, times its boost.
    fn final_score(f: &Fusion, ranks: &[usize], boost: f64) -> f64 {
        let rrf: f64 = ranks.iter().map(|r| 1.0 / (f.rrf_k + *r as f64)).sum();
        f.combine(rrf, boost)
    }

    /// Task 19, ruling 2: relevance leads and the priors break ties. An item first in two lists
    /// (a raw chunk from another project, 0.6, aged to 0.73) outranks one 20th in both lists
    /// that is in the current project (1.3) at the defaults, and does not at the old k = 60,
    /// λ = 1 (so the two settings are what changed the order).
    #[test]
    fn relevance_outranks_priors_at_the_defaults_and_not_at_the_old_ones() {
        let (first, twentieth) = (0.6 * 0.73, 1.3);
        let d = Fusion::default();
        assert!(
            final_score(&d, &[1, 1], first) > final_score(&d, &[20, 20], twentieth),
            "defaults {d:?}"
        );
        let old = Fusion {
            rrf_k: 60.0,
            boost_exponent: 1.0,
        };
        assert!(final_score(&old, &[1, 1], first) < final_score(&old, &[20, 20], twentieth));
    }

    #[test]
    fn the_exponent_scales_the_priors_in_the_log_domain() {
        let f = Fusion {
            rrf_k: 60.0,
            boost_exponent: 0.0,
        };
        assert_eq!(f.combine(0.03, 123.0), 0.03, "λ = 0 ignores the boosts");
        let f = Fusion {
            rrf_k: 60.0,
            boost_exponent: 1.0,
        };
        assert!(
            (f.combine(0.03, 2.0) - 0.06).abs() < 1e-12,
            "λ = 1 multiplies"
        );
    }

    /// The published defaults are the module's constants, in range of the settings' rules.
    #[test]
    fn default_fusion_is_in_the_valid_range() {
        let d = Fusion::default();
        assert_eq!(
            (d.rrf_k, d.boost_exponent),
            (DEFAULT_RRF_K, DEFAULT_BOOST_EXPONENT)
        );
        assert!((1.0..=200.0).contains(&d.rrf_k) && (0.0..=1.0).contains(&d.boost_exponent));
    }
}
