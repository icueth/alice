//! Alice core: domain model, storage and search.

pub mod codetokens;
pub mod error;
pub mod ids;
pub mod model;
pub mod normalize;
pub mod project;
pub mod store;

pub use error::{AliceError, Result};
