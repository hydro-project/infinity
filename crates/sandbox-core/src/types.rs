use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// The version-control mode used by a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SandboxMode {
    /// Jujutsu workspace (the default for repos with `.jj`).
    Jj {
        /// Absolute jj change_id the workspace is based on.
        base_revision: String,
    },
    /// Plain git worktree.
    Git {
        /// Absolute git commit hash the worktree is based on.
        base_revision: String,
    },
    /// Externally-provided custom mode, keyed by a string id with opaque JSON data.
    ///
    /// The core server and backend treat this as opaque: all mode-specific
    /// behavior is delegated to a registered [`crate::sandbox::ModeProvider`],
    /// exactly like the built-in [`SandboxMode::Jj`] and [`SandboxMode::Git`]
    /// modes.
    Custom {
        /// Provider-defined identifier for the custom mode.
        id: String,
        /// Provider-defined opaque payload.
        data: serde_json::Value,
    },
    /// Direct mode: operates on the original repo directory with no worktrees.
    /// File edits require user approval; commands run without file write access unless write-orig is granted.
    Direct,
}

/// Status of a changed file in a sandbox diff.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileChangeStatus {
    Added,
    Deleted,
    Modified,
}

/// A single changed file with its old and new contents, used to render diffs.
#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileChangeStatus,
    #[serde(rename = "oldContents")]
    pub old_contents: String,
    #[serde(rename = "newContents")]
    pub new_contents: String,
}

/// Parse `git diff --name-status` or `jj diff --summary` output into (status, path) pairs.
pub(crate) fn parse_changed_files(output: &str) -> Vec<(FileChangeStatus, &str)> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // jj format: "M path" or "A path" or "D path"
            // git format: "M\tpath" or "A\tpath" or "D\tpath"
            let (status, path) =
                if let Some(rest) = line.strip_prefix("M\t").or(line.strip_prefix("M ")) {
                    (FileChangeStatus::Modified, rest)
                } else if let Some(rest) = line.strip_prefix("A\t").or(line.strip_prefix("A ")) {
                    (FileChangeStatus::Added, rest)
                } else {
                    let rest = line.strip_prefix("D\t").or(line.strip_prefix("D "))?;
                    (FileChangeStatus::Deleted, rest)
                };
            Some((status, path.trim()))
        })
        .collect()
}

/// Metadata stored per group_id tracking the repo state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    /// The group_id this state belongs to.
    pub group_id: String,
    /// The git remote URI (local path or s3 URI).
    pub remote_uri: String,
    /// The bookmark name used to track state.
    pub bookmark: String,
    /// The version-control mode and its associated data.
    pub mode: SandboxMode,
    /// Path to the created sandbox workspace.
    #[serde(default)]
    pub sandbox_path: Option<String>,
    /// Whether write-orig permission has been granted for the session.
    #[serde(default, alias = "direct_write_granted")]
    pub write_orig_granted: bool,
    /// Absolute paths granted write permission via `write:/path`.
    #[serde(default)]
    pub write_path_grants: HashSet<String>,
    /// The root thread (session) this sandbox belongs to.
    #[serde(default)]
    pub root_thread_id: Option<String>,
}

/// Input for the clone_repo tool.
#[derive(Debug, Deserialize)]
pub struct CloneRepoArgs {
    /// Local path to a git repo, or a git remote URI.
    pub repo: String,
    /// Optional thread ID to base this sandbox on top of.
    pub base_thread_id: Option<String>,
}

/// Input for the open_sandbox_direct tool.
#[derive(Debug, Deserialize)]
pub struct OpenSandboxDirectArgs {
    /// Local path to a git repo.
    pub repo: String,
}

/// Input for the execute_command tool.
#[derive(Debug, Deserialize)]
pub struct ExecuteCommandArgs {
    /// The bash command to execute in the sandbox.
    pub command: String,
    /// Optional additional permissions for this command.
    /// Supported values: `"write-orig"` (allow writing to the original repo directory),
    /// `"write:/path"` (allow writing to a specific path).
    #[serde(default)]
    pub additional_permissions: Option<Vec<String>>,
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

/// Input for the create_file tool.
#[derive(Debug, Deserialize)]
pub struct CreateFileArgs {
    /// Path for the new file, relative to the repository root.
    pub path: String,
    /// The content to write to the new file.
    pub content: String,
}

/// Input for the describe_overall_changes tool.
#[derive(Debug, Deserialize)]
pub struct DescribeOverallChangesArgs {
    /// A description of the edits that were made.
    pub message: String,
}

/// Input for the squash_sandbox tool.
#[derive(Debug, Deserialize)]
pub struct SquashSandboxArgs {
    /// The thread ID of the child sandbox to squash from.
    pub from_thread_id: String,
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
