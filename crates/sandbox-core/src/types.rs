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

/// Input for the read_file tool.
#[derive(Debug, Deserialize)]
pub struct ReadFileArgs {
    /// Path to the file to read, relative to the repository root.
    pub path: String,
    /// Optional starting line number (1-indexed).
    pub start_line: Option<usize>,
    /// Optional ending line number (1-indexed, inclusive).
    pub end_line: Option<usize>,
}

/// Input for the edit_file tool.
#[derive(Debug, Deserialize)]
pub struct EditFileArgs {
    /// Path to the file to edit, relative to the repository root.
    pub path: String,
    /// The exact string to find in the file.
    pub old_str: String,
    /// The replacement string.
    pub new_str: String,
}

/// Input for the grep tool.
#[derive(Debug, Deserialize)]
pub struct GrepArgs {
    /// The regex pattern to search for.
    pub query: String,
    /// Optional glob pattern for files to include (e.g. "**/*.rs").
    #[serde(rename = "includePattern")]
    pub include_pattern: Option<String>,
    /// Optional glob pattern for files to exclude.
    #[serde(rename = "excludePattern")]
    pub exclude_pattern: Option<String>,
    /// Whether the search should be case sensitive. Defaults to false.
    #[serde(rename = "caseSensitive")]
    pub case_sensitive: Option<bool>,
}
