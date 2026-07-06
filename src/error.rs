use thiserror::Error;

#[derive(Debug, Error)]
pub enum RfError {
    #[error("git error: {0}")]
    Git(String),

    #[error("config error: {0}")]
    Config(String),

    // Constructed by upcoming epics (promotion/verification paths); kept in the
    // public error surface so callers can match on them as they land.
    #[allow(dead_code)]
    #[error("branch not found: {0}")]
    BranchNotFound(String),

    #[allow(dead_code)]
    #[error("parse error: {0}")]
    Parse(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
