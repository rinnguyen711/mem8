use thiserror::Error;

#[derive(Debug, Error)]
pub enum Mem8Error {
    #[error("memory not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("store error: {0}")]
    Store(String),

    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },

    #[error("could not determine project scope: {0}")]
    Scope(String),

    #[error("database schema version {found} is newer than this binary supports ({expected}); upgrade mem8")]
    Migration { found: i32, expected: i32 },
}

pub type Result<T> = std::result::Result<T, Mem8Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_message_includes_id() {
        let e = Mem8Error::NotFound("abc-123".into());
        assert!(e.to_string().contains("abc-123"));
    }

    #[test]
    fn migration_error_names_both_versions() {
        let e = Mem8Error::Migration { found: 3, expected: 2 };
        let msg = e.to_string();
        assert!(msg.contains('3') && msg.contains('2'));
    }
}
