use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("repo not found for group_id: {0}")]
    RepoNotFound(String),

    #[error("jujutsu command failed: {0}")]
    JujutsuError(String),

    #[error("command execution failed: {0}")]
    CommandError(String),

    #[error("metadata error: {0}")]
    MetadataError(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}
