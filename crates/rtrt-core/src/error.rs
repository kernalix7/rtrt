use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("plugin error: {0}")]
    Plugin(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("memory error: {0}")]
    Memory(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}
