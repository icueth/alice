//! Importing from other memory systems (MemPalace first).

pub mod chroma;
pub mod palace;
pub mod plan;
pub mod wings;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;

pub use plan::{NoFilter, Planner, Survey, TextFilter, TierCounts, survey};

/// The machine's current UTC offset (MemPalace writes naive local times).
pub fn local_offset() -> chrono::FixedOffset {
    use chrono::Offset;
    chrono::Local::now().offset().fix()
}
