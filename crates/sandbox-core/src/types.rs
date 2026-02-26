use serde::{Deserialize, Serialize};

/// Metadata stored per group_id tracking the repo state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    /// The group_id this state belongs to.
    pub group_id: String,
    /// The git remote URI (local path or s3 URI).
    pub remote_uri: String,
    /// The bookmark name used to track state, set after first push.
    pub bookmark: Option<String>,
}

/// Input for the clone_repo tool.
#[derive(Debug, Deserialize)]
pub struct CloneRepoArgs {
    /// Local path to a git repo, or a git remote URI.
    pub repo: String,
}

/// Input for the execute_command tool.
#[derive(Debug, Deserialize)]
pub struct ExecuteCommandArgs {
    /// The bash command to execute in the sandbox.
    pub command: String,
}
