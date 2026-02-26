use std::path::PathBuf;

use async_trait::async_trait;

use sandbox_core::error::SandboxError;
use sandbox_core::jj;
use sandbox_core::sandbox::SandboxBackend;
use sandbox_core::types::RepoState;

/// Local sandbox backend.
/// The "remote" is just the original local git directory.
/// Each sandbox is a temp dir with a jj clone pointing at that local path.
pub struct LocalBackend;

impl LocalBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxBackend for LocalBackend {
    /// For local mode, the repo arg is a path to an existing git dir.
    /// We don't need to do anything special — just validate it exists
    /// and return the path as the remote URI.
    async fn init_repo(&self, repo: &str, _group_id: &str) -> Result<String, SandboxError> {
        let path = PathBuf::from(repo);
        if !path.exists() {
            return Err(SandboxError::Other(format!(
                "local repo path does not exist: {repo}"
            )));
        }
        // Return the absolute path as the remote URI
        let abs = path.canonicalize().map_err(|e| SandboxError::Io(e))?;
        Ok(abs.to_string_lossy().to_string())
    }

    /// Create a temp dir and jj git clone from the local remote into it.
    /// If this group has been used before, fetch and restore to the bookmark.
    async fn create_sandbox(&self, state: &RepoState) -> Result<PathBuf, SandboxError> {
        let tmp = tempfile::tempdir().map_err(SandboxError::Io)?;
        let sandbox_dir = tmp.keep();

        jj::jj_git_clone(&state.remote_uri, &sandbox_dir).await?;

        // Configure author so all commits in this sandbox have valid metadata
        jj::jj_configure_author(&sandbox_dir).await?;

        // If we have a previous bookmark, fetch and edit to it
        if state.bookmark.is_some() {
            let bookmark = format!("sandbox-{}@origin", state.group_id);
            jj::jj_git_fetch(&sandbox_dir).await?;
            jj::jj_new(&sandbox_dir, &bookmark).await?;
        }

        Ok(sandbox_dir)
    }

    /// Push the sandbox's working copy back to the local git remote.
    async fn push_sandbox(
        &self,
        sandbox_dir: &PathBuf,
        group_id: &str,
    ) -> Result<(), SandboxError> {
        let bookmark = format!("sandbox-{group_id}");
        jj::jj_push_working_copy(sandbox_dir, &bookmark).await
    }

    /// Remove the temp sandbox directory.
    async fn cleanup_sandbox(&self, sandbox_dir: &PathBuf) -> Result<(), SandboxError> {
        tokio::fs::remove_dir_all(sandbox_dir)
            .await
            .map_err(SandboxError::Io)
    }
}
