//! Error type shared by every Alice crate.

/// What a read-only error tells the reader to do; it ends every `ReadOnly` message.
pub const READ_ONLY_HINT: &str =
    "See \"alice status\" and the daemon log (\"alice daemon logs\") for the cause.";

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
    #[error("memory is read-only: {0}. {hint}", hint = READ_ONLY_HINT)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding m-5: M1 has no `alice doctor`; the read-only error points to what exists.
    #[test]
    fn the_read_only_error_names_the_commands_that_exist() {
        let m = AliceError::ReadOnly("the disk is full".into()).to_string();
        assert!(
            m.starts_with("memory is read-only: the disk is full. "),
            "{m}"
        );
        assert!(
            m.contains("alice status") && m.contains("alice daemon logs"),
            "{m}"
        );
        assert!(!m.contains("doctor"), "{m}");
    }
}
