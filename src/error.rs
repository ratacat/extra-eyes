use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, EyesError>;

#[derive(Debug, Error)]
pub enum EyesError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("toml serialization error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),

    #[error("project already has a running eyes daemon")]
    AlreadyRunning,

    #[error("no eyes daemon is running for this project")]
    NotRunning,

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("state error: {0}")]
    State(String),
}

impl EyesError {
    pub fn io_context(source: std::io::Error, context: impl Into<String>) -> Self {
        EyesError::Io(std::io::Error::new(source.kind(), context.into()))
    }
}
