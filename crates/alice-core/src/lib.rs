//! Alice core: domain model, storage and search.

pub mod api;
pub mod codetokens;
pub mod embed;
pub mod error;
pub mod ids;
pub mod model;
pub mod normalize;
pub mod project;
pub mod search;
pub mod store;
pub mod vector;

pub use error::{AliceError, Result};

/// Knowledge-graph types under a short path (used by `api`).
pub mod kg_types {
    pub use crate::store::kg::{AssertFact, Fact, FactAction, FactOutcome, FactsQuery, ObjectKind};
}
