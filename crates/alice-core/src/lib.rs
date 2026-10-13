//! Alice core: domain model, storage and search.

pub mod codetokens;
pub mod embed;
pub mod error;
pub mod ids;
pub mod model;
pub mod normalize;
pub mod project;
pub mod store;
pub mod vector;

pub use error::{AliceError, Result};
