use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::SandboxError;
use crate::types::RepoState;

/// Result of executing a command in a sandbox.
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Trait for the sandbox backend.
/// Handles creating sandboxes (temp dirs with jj clones) and
/// managing the git remote (local path vs s3).
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Called on clone_repo. Sets up the remote if needed (e.g. push to s3)
    /// and returns the remote URI to store in metadata.
    async fn init_repo(&self, repo: &str, group_id: &str) -> Result<String, SandboxError>;

    /// Spin up a sandbox temp dir with a jj clone pointing at the remote,
    /// checked out to the given working copy commit.
    async fn create_sandbox(&self, state: &RepoState) -> Result<PathBuf, SandboxError>;

    /// Execute a command inside the sandbox directory.
    async fn execute_command(
        &self,
        sandbox_dir: &Path,
        command: &str,
    ) -> Result<ExecResult, SandboxError>;

    /// Push the updated working copy from the sandbox back to the remote.
    async fn push_sandbox(&self, sandbox_dir: &Path, group_id: &str) -> Result<(), SandboxError>;

    /// Clean up the sandbox temp dir.
    async fn cleanup_sandbox(&self, sandbox_dir: &Path) -> Result<(), SandboxError>;
}
