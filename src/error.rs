use std::path::PathBuf;

/// Unified error type for pgmcp.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum PgmcpError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Configuration file not found: {0}")]
    ConfigNotFound(PathBuf),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Database migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("File I/O error: {path}: {source}")]
    FileIo {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("File watcher error: {0}")]
    Watcher(#[from] notify::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("MCP error: {0}")]
    Mcp(String),

    #[error("Shutdown in progress")]
    ShuttingDown,

    #[error("Channel send error: {0}")]
    ChannelSend(String),

    #[error("Task panicked: {0}")]
    TaskPanic(String),

    #[error("{0}")]
    Other(String),
}

impl PgmcpError {
    pub fn file_io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::FileIo {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, PgmcpError>;
