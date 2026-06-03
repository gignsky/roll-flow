use thiserror::Error;

#[derive(Debug, Error)]
pub enum RfError {
    #[error("git error: {0}")]
    Git(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("branch not found: {0}")]
    BranchNotFound(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
