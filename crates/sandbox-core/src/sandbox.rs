use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::SandboxError;
use crate::types::RepoState;

/// A spawned command with its child process handle and any resources
/// that must stay alive until the process exits.
pub struct SpawnedCommand {
    pub child: tokio::process::Child,
    /// Temp resources (e.g. sandbox tmpdir) that must outlive the child process.
    pub _keepalive: Option<Box<dyn std::any::Any + Send>>,
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

    /// Spawn a command inside the sandbox directory, returning the child process.
    /// stdout and stderr MUST be piped so the caller can stream output.
    ///
    /// `argv` is the raw argument vector: `argv[0]` is the program name and
    /// `argv[1..]` are its arguments.  No shell wrapping is performed.
    ///
    /// `extra_writable` grants write access to additional paths beyond the
    /// sandbox directory (e.g. the original repo for `git push`).
    ///
    /// `sandbox_writable` controls whether the sandbox directory itself is
    /// writable. Set to `false` for Direct mode where no file writes are allowed.
    async fn spawn_command(
        &self,
        sandbox_dir: &Path,
        argv: &[&str],
        extra_writable: &[&Path],
        sandbox_writable: bool,
    ) -> Result<SpawnedCommand, SandboxError>;

    /// Push the updated working copy from the sandbox back to the remote.
    /// `description` is an optional commit message; when `None` the backend
    /// uses a default like `sandbox-{group_id}`.
    async fn push_sandbox(
        &self,
        sandbox_dir: &Path,
        group_id: &str,
        description: Option<&str>,
    ) -> Result<(), SandboxError>;

    /// Clean up the sandbox temp dir.
    async fn cleanup_sandbox(&self, sandbox_dir: &Path) -> Result<(), SandboxError>;

    /// Permanently clean up a sandbox associated with the given group_id.
    ///
    /// Called when a thread is closed — the sandbox is no longer needed.
    /// On the local backend this runs `jj workspace forget` and deletes the
    /// cached directory. On the remote backend this is a no-op since sandboxes
    /// are ephemeral temp dirs cleaned up after each invocation.
    async fn cleanup_sandbox_permanently(&self, _group_id: &str) -> Result<(), SandboxError> {
        Ok(())
    }
}
