use thiserror::Error;

/// Error type for the orchestration core.
#[derive(Error, Debug)]
pub enum TalesError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("failed to spawn agent process: {0}")]
    Spawn(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, TalesError>;
