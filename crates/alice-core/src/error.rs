//! Error type shared by every Alice crate.

/// Errors carry messages written for the agent or person who will read them:
/// what happened and what to do next.
#[derive(Debug, thiserror::Error)]
pub enum AliceError {
    /// The caller sent something invalid; the message says how to fix it.
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// Writes are refused until the cause is fixed.
    #[error("memory is read-only: {0}. Run \"alice doctor\".")]
    ReadOnly(String),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AliceError>;

impl AliceError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}
