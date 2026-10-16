//! Alice core: domain model, storage and search.

pub mod api;
pub mod bq;
pub mod capture;
pub mod codetokens;
pub mod context;
pub mod embed;
pub mod error;
pub mod ids;
pub mod import;
pub mod judge;
pub mod model;
pub mod normalize;
pub mod project;
pub mod redact;
pub mod search;
pub mod settings;
pub mod store;
pub mod vector;

pub use error::{AliceError, Result};

/// Knowledge-graph types under a short path (used by `api`).
pub mod kg_types {
    pub use crate::store::kg::{AssertFact, Fact, FactAction, FactOutcome, FactsQuery, ObjectKind};
}
