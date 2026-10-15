//! The Judge (spec §5.2): decisions as probabilities over named choices, from a provider that
//! may be offline (`heuristic`) or a model (`jev`). Callers apply a decision automatically at
//! `judge.auto_apply_threshold`, ask in the review inbox at `judge.review_threshold`, and
//! otherwise do nothing; a failing judge never fails a write.

pub mod heuristic;
pub mod jev;
pub mod metrics;
pub mod pipeline;
pub mod text;
pub mod verbal;

use crate::model::Kind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Duplicate,
    Updates,
    Contradicts,
    Unrelated,
}

impl RelationKind {
    pub const ALL: [RelationKind; 4] = [
        Self::Duplicate,
        Self::Updates,
        Self::Contradicts,
        Self::Unrelated,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Updates => "updates",
            Self::Contradicts => "contradicts",
            Self::Unrelated => "unrelated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub description: String,
}

impl Choice {
    pub fn new(id: &str, description: &str) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
        }
    }
}

/// Probabilities over choice ids, normalised to sum to 1 (empty when nothing had mass).
/// Zero-mass ids are dropped. Deserialising normalises too, so a stored or foreign map is
/// always a valid distribution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "BTreeMap<String, f64>")]
pub struct Distribution(pub BTreeMap<String, f64>);

impl From<BTreeMap<String, f64>> for Distribution {
    fn from(m: BTreeMap<String, f64>) -> Self {
        Self::from_pairs(m)
    }
}

impl Distribution {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, f64)>) -> Self {
        let items: Vec<(String, f64)> = pairs
            .into_iter()
            .map(|(k, v)| (k, if v.is_finite() { v.max(0.0) } else { 0.0 }))
            .collect();
        let mut total: f64 = items.iter().map(|(_, v)| v).sum();
        // Huge weights would overflow the sum: scale by the largest first.
        let scale = if total.is_finite() {
            1.0
        } else {
            items.iter().map(|(_, v)| *v).fold(0.0, f64::max)
        };
        let mut m: BTreeMap<String, f64> = BTreeMap::new();
        if scale != 1.0 {
            total = items.iter().map(|(_, v)| v / scale).sum();
        }
        if total > 0.0 {
            for (k, v) in items {
                let p = v / scale / total;
                if p > 0.0 {
                    *m.entry(k).or_default() += p;
                }
            }
        }
        Self(m)
    }

    pub fn certain(id: &str) -> Self {
        Self::from_pairs([(id.to_string(), 1.0)])
    }

    pub fn uniform(ids: &[&str]) -> Self {
        Self::from_pairs(ids.iter().map(|i| (i.to_string(), 1.0)))
    }

    pub fn p(&self, id: &str) -> f64 {
        self.0.get(id).copied().unwrap_or(0.0)
    }

    /// The most likely id with positive mass; ties go to the first id in order, so the result
    /// is deterministic. `None` when no id has mass.
    pub fn best(&self) -> Option<(String, f64)> {
        self.0
            .iter()
            .filter(|(_, v)| **v > 0.0)
            .fold(None, |acc: Option<(&String, f64)>, (k, &v)| match acc {
                Some((_, b)) if b >= v => acc,
                _ => Some((k, v)),
            })
            .map(|(k, v)| (k.clone(), v))
    }

    /// The mean of two distributions. When one side is empty the result is the other side
    /// unchanged (an empty side carries no information, so it does not dilute the answer).
    pub fn average(a: &Self, b: &Self) -> Self {
        Self::from_pairs(
            a.0.iter()
                .chain(b.0.iter())
                .map(|(k, v)| (k.clone(), v / 2.0)),
        )
    }
}

/// Levels of a graded answer, worst first; a score is the expected level in 0..=1.
#[derive(Debug, Clone, PartialEq)]
pub struct Rubric {
    pub question: String,
    pub levels: Vec<Choice>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub value: f64,
    pub confidence: f64,
}

/// The start of the title of a raw transcript chunk (`raw · <source>`).
pub const RAW_TITLE_PREFIX: &str = "raw · ";

/// A memory as a judge sees it. (A raw chunk is shown as a `note` whose title starts with
/// [`RAW_TITLE_PREFIX`]: the item header takes a memory kind, and there is no other one.)
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeMemory {
    pub id: String,
    /// Project slug.
    pub project: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub importance: u8,
    pub created_at: String,
    /// Cosine similarity to the memory it is compared with, when known.
    pub similarity: Option<f32>,
}

impl JudgeMemory {
    /// True for a raw transcript chunk, which a recall grading shows up to
    /// [`verbal::RAW_BODY_CHARS`] characters of.
    pub fn is_raw_chunk(&self) -> bool {
        self.title.starts_with(RAW_TITLE_PREFIX)
    }
}

/// How a new memory relates to one existing memory: P over `RelationKind` names.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub existing_id: String,
    pub probs: Distribution,
    /// For a model: forward and reversed questions picked the same answer. For the heuristic
    /// (which asks once): `Some(true)` marks a duplicate that is the same text, the only
    /// heuristic result `pipeline::decide` applies without asking, and `Some(false)` a near
    /// twin by vector only.
    pub agreement: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectOption {
    pub slug: String,
    pub name: String,
    /// A few recent titles or topics, to tell projects apart.
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JudgeError {
    #[error("the judge's daily budget is used up ({0})")]
    Budget(String),
    #[error("this project may not be sent to a cloud judge")]
    Denied,
    #[error("the judge is unavailable: {0}")]
    Unavailable(String),
    #[error("the judge failed: {0}")]
    Failed(String),
}

pub type JudgeResult<T> = std::result::Result<T, JudgeError>;

pub const RELEVANCE_ANSWERS: &str = "answers";
pub const RELEVANCE_PARTIAL: &str = "partially";

pub fn relation_choices() -> Vec<Choice> {
    vec![
        Choice::new(
            "duplicate",
            "both say the same thing; neither adds anything",
        ),
        Choice::new(
            "updates",
            "the new memory replaces the existing one with newer or corrected information about the same thing",
        ),
        Choice::new(
            "contradicts",
            "they cannot both be true, and the new one does not clearly replace the existing one",
        ),
        Choice::new(
            "unrelated",
            "different subjects, or compatible information that should both be kept",
        ),
    ]
}

/// The same question asked from the existing memory's side (`outdated_by` maps to `updates`).
pub fn reverse_relation_choices() -> Vec<Choice> {
    vec![
        Choice::new(
            "duplicate",
            "both say the same thing; neither adds anything",
        ),
        Choice::new(
            "outdated_by",
            "the existing memory is replaced by the new one: newer or corrected information about the same thing",
        ),
        Choice::new(
            "contradicts",
            "they cannot both be true, and the new one does not clearly replace the existing one",
        ),
        Choice::new(
            "unrelated",
            "different subjects, or compatible information that should both be kept",
        ),
    ]
}

pub fn kind_choices() -> Vec<Choice> {
    vec![
        Choice::new("fact", "a fact about code, a system or the world"),
        Choice::new(
            "decision",
            "a decision that was made, ideally with its reason",
        ),
        Choice::new("bugfix", "a bug: its symptom, cause and fix"),
        Choice::new("howto", "steps or commands that achieve something"),
        Choice::new("preference", "how the user wants things done"),
        Choice::new("warning", "a danger, or something to avoid"),
        Choice::new("status", "the current state of some work"),
        Choice::new("note", "anything else worth keeping"),
        Choice::new("summary", "a summary of a longer discussion or document"),
        Choice::new("diary", "a journal entry about what happened in a session"),
        Choice::new(
            "handoff",
            "what is done, what is next and open questions, for the next session",
        ),
    ]
}

/// The question "which existing predicate means the same?": the state and the options (the
/// existing names, and `none`).
pub fn predicate_question(new: &str, existing: &[String]) -> (String, Vec<Choice>) {
    let options: Vec<Choice> = existing
        .iter()
        .map(|p| Choice::new(p, &p.replace('_', " ")))
        .chain([Choice::new("none", "none of these means the same")])
        .collect();
    (
        format!(
            "A knowledge-graph relation is named \"{}\". Which existing relation name means the same? Names that express opposite directions (for example `belongs_to` and `owns`) are different relations.",
            new.replace('_', " ")
        ),
        options,
    )
}

pub fn relevance_choices() -> Vec<Choice> {
    vec![
        Choice::new(RELEVANCE_ANSWERS, "the result answers the query"),
        Choice::new(
            RELEVANCE_PARTIAL,
            "the result is about the query's subject but does not answer it",
        ),
        Choice::new("unrelated", "the result is not about the query"),
    ]
}

pub trait Judge: Send + Sync {
    fn provider(&self) -> &str;

    /// A condition worth showing in health (e.g. Jev paused because its spend cannot be read
    /// or the day's budget is used up).
    fn warning(&self) -> Option<String> {
        None
    }

    /// P over `options` for `state`. A provider that sends text away (Jev) refuses this entry
    /// point with [`JudgeError::Denied`]: it cannot know which project the text belongs to. A
    /// caller with project text uses the scoped methods; one whose text carries none uses
    /// [`classify_constant`](Self::classify_constant).
    fn classify(&self, state: &str, options: &[Choice]) -> JudgeResult<Distribution>;

    /// [`classify`](Self::classify) for a text that is a constant of the program and so holds
    /// no project content (a connection test). `'static` is the proof, for the text and for the
    /// options (id, description): nothing built at run time fits.
    fn classify_constant(
        &self,
        state: &'static str,
        options: &'static [(&'static str, &'static str)],
    ) -> JudgeResult<Distribution> {
        let options: Vec<Choice> = options.iter().map(|(id, d)| Choice::new(id, d)).collect();
        self.classify(state, &options)
    }

    /// The expected level of `rubric` (0 = worst, 1 = best) and how sure the judge is.
    fn score(&self, state: &str, rubric: &Rubric) -> JudgeResult<Score>;

    /// Per existing memory: P(duplicate | updates | contradicts | unrelated).
    fn compare(&self, new: &JudgeMemory, existing: &[JudgeMemory]) -> JudgeResult<Vec<Relation>>;

    fn score_many(&self, items: &[(String, Rubric)]) -> JudgeResult<Vec<Score>> {
        items.iter().map(|(s, r)| self.score(s, r)).collect()
    }

    fn classify_kind(&self, m: &JudgeMemory) -> JudgeResult<Distribution> {
        let now = chrono::Utc::now();
        self.classify(&verbal::kind_state(m, now), &kind_choices())
    }

    fn route_project(
        &self,
        m: &JudgeMemory,
        projects: &[ProjectOption],
    ) -> JudgeResult<Distribution> {
        let now = chrono::Utc::now();
        let options: Vec<Choice> = projects
            .iter()
            .map(|p| Choice::new(&p.slug, &format!("{}: {}", p.name, p.hint)))
            .chain([Choice::new("global", "not about any one project")])
            .collect();
        self.classify(&verbal::memory_block("Memory", m, now), &options)
    }

    fn match_predicate(&self, new: &str, existing: &[String]) -> JudgeResult<Distribution> {
        let (state, options) = predicate_question(new, existing);
        self.classify(&state, &options)
    }

    /// [`match_predicate`](Self::match_predicate) for names that belong to the knowledge graph
    /// and to `projects` (the projects of the memory a fact cites, if any). A provider that
    /// sends text away (Jev) needs the scope; the others ignore it.
    fn match_predicate_in(
        &self,
        _projects: &[&str],
        new: &str,
        existing: &[String],
    ) -> JudgeResult<Distribution> {
        self.match_predicate(new, existing)
    }

    /// `query_project` is the project the query was asked from: a provider that sends text
    /// away needs it besides the projects of `results`.
    fn grade_relevance(
        &self,
        _query_project: &str,
        query: &str,
        results: &[JudgeMemory],
    ) -> JudgeResult<Vec<Distribution>> {
        let now = chrono::Utc::now();
        results
            .iter()
            .map(|r| {
                self.classify(
                    &format!(
                        "Query: {query}\n\n{}",
                        verbal::memory_block("Result", r, now)
                    ),
                    &relevance_choices(),
                )
            })
            .collect()
    }

    /// [`grade_relevance`](Self::grade_relevance) for a recall: one attempt, no retries, and
    /// the whole call ends within `budget` (a provider that waits on a network sets its
    /// request timeout to it). The default is the plain call.
    fn grade_relevance_within(
        &self,
        query_project: &str,
        query: &str,
        results: &[JudgeMemory],
        _budget: std::time::Duration,
    ) -> JudgeResult<Vec<Distribution>> {
        self.grade_relevance(query_project, query, results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distributions_normalise_and_pick_deterministically() {
        let d = Distribution::from_pairs([
            ("a".to_string(), 2.0),
            ("b".to_string(), 2.0),
            ("c".to_string(), -1.0),
        ]);
        assert!((d.p("a") - 0.5).abs() < 1e-9 && d.p("c") == 0.0 && d.p("zzz") == 0.0);
        assert_eq!(
            d.best(),
            Some(("a".to_string(), 0.5)),
            "ties go to the first id"
        );
        let avg = Distribution::average(&Distribution::certain("a"), &Distribution::certain("b"));
        assert_eq!((avg.p("a"), avg.p("b")), (0.5, 0.5));
        assert_eq!(Distribution::uniform(&["x", "y", "z", "w"]).p("w"), 0.25);
        assert_eq!(
            Distribution::from_pairs(Vec::<(String, f64)>::new()).best(),
            None
        );
    }

    #[test]
    fn huge_weights_do_not_overflow_and_deserialising_normalises() {
        let d =
            Distribution::from_pairs([("a".to_string(), f64::MAX), ("b".to_string(), f64::MAX)]);
        assert_eq!((d.p("a"), d.p("b")), (0.5, 0.5));
        let z = Distribution::from_pairs([("a".to_string(), 0.0), ("b".to_string(), f64::NAN)]);
        assert!(
            z.0.is_empty() && z.best().is_none(),
            "zero-mass ids are dropped"
        );
        let d: Distribution = serde_json::from_str(r#"{"a":3,"b":1,"c":0}"#).unwrap();
        assert_eq!((d.p("a"), d.p("b")), (0.75, 0.25));
        assert!(!d.0.contains_key("c"));
        let d: Distribution = serde_json::from_str(r#"{"a":-1,"b":2}"#).unwrap();
        assert_eq!((d.p("a"), d.p("b")), (0.0, 1.0));
        assert!(!d.0.contains_key("a"));
        let zeros = Distribution(BTreeMap::from([("a".to_string(), 0.0)]));
        assert_eq!(zeros.best(), None);
        let one_side = Distribution::average(&Distribution::default(), &Distribution::certain("a"));
        assert_eq!(one_side, Distribution::certain("a"));
    }

    #[test]
    fn relation_kinds_round_trip_and_choices_are_described() {
        for k in RelationKind::ALL {
            assert_eq!(RelationKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(relation_choices().len(), 4);
        assert!(
            reverse_relation_choices()
                .iter()
                .any(|c| c.id == "outdated_by")
        );
        assert_eq!(kind_choices().len(), 11);
        assert!(kind_choices().iter().all(|c| !c.description.is_empty()));
    }
}
