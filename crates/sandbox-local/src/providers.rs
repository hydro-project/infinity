//! Built-in mode providers for the local backend.
//!
//! The Jujutsu and Git sandbox modes are implemented as [`ModeProvider`]s and
//! registered on `LocalBackend` by default — fully symmetric with
//! externally-registered providers (see `LocalBackend::with_provider`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use async_trait::async_trait;

use sandbox_core::error::SandboxError;
use sandbox_core::git;
use sandbox_core::jj::{self, run_jj};
use sandbox_core::sandbox::{CloneContext, ModeInit, ModeProvider};
use sandbox_core::types::{ChangedFile, RepoState, SandboxMode};

/// Compute the sandboxes base directory for a given repo.
fn sandboxes_dir_for(remote_uri: &str) -> PathBuf {
    PathBuf::from(remote_uri)
        .join(".infinity")
        .join(".sandboxes")
}

/// Create a new temporary directory under `{remote_uri}/.infinity/.sandboxes/`.
fn make_tempdir(remote_uri: &str) -> std::io::Result<tempfile::TempDir> {
    let base = sandboxes_dir_for(remote_uri);
    std::fs::create_dir_all(&base)?;
    tempfile::tempdir_in(&base)
}

/// Check if a jj bookmark's commit is empty (no changes) using a synchronous command.
fn jj_bookmark_is_empty_sync(dir: &Path, bookmark: &str) -> bool {
    Command::new("jj")
        .args([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            bookmark,
            "-T",
            "empty",
        ])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Check if the top commit on the current branch has no file changes (sync, for Drop).
fn git_branch_top_is_empty_sync(dir: &Path) -> bool {
    Command::new("git")
        .args(["diff", "--quiet", "HEAD~1"])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Built-in provider for [`SandboxMode::Jj`]: each sandbox is a jj workspace
/// in a temp dir under `{repo}/.infinity/.sandboxes/`.
pub struct LocalJjProvider;

#[async_trait]
impl ModeProvider for LocalJjProvider {
    fn handles(&self, mode: &SandboxMode) -> bool {
        matches!(mode, SandboxMode::Jj { .. })
    }

    async fn detect(
        &self,
        repo_path: &Path,
        ctx: &CloneContext<'_>,
    ) -> Result<Option<ModeInit>, SandboxError> {
        jj::detect_mode(repo_path, ctx.base_bookmark).await
    }

    async fn create_sandbox(&self, state: &RepoState) -> Result<PathBuf, SandboxError> {
        let SandboxMode::Jj { base_revision } = &state.mode else {
            return Err(SandboxError::Other(
                "bug: jj provider invoked with a non-jj mode".to_owned(),
            ));
        };
        let tmp = make_tempdir(&state.remote_uri).map_err(SandboxError::Io)?;
        let sandbox_dir = tmp.keep();
        jj::jj_git_clone(
            &state.remote_uri,
            &sandbox_dir,
            &state.bookmark,
            base_revision,
        )
        .await?;
        Ok(sandbox_dir)
    }

    async fn refresh_sandbox(
        &self,
        sandbox_dir: &Path,
        _state: &RepoState,
    ) -> Result<(), SandboxError> {
        run_jj(sandbox_dir, &["workspace", "update-stale"]).await?;
        Ok(())
    }

    async fn describe(
        &self,
        sandbox_dir: &Path,
        _state: &RepoState,
        description: &str,
    ) -> Result<(), SandboxError> {
        jj::describe(sandbox_dir, description).await
    }

    async fn detect_external_change(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
    ) -> Option<String> {
        jj::detect_external_change(sandbox_dir, &state.bookmark).await
    }

    async fn push_sandbox(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
        _description: Option<&str>,
    ) -> Result<(), SandboxError> {
        jj::jj_push_working_copy(sandbox_dir, &state.bookmark).await?;
        // Keep colocated git refs in sync so git commands via
        // write-orig see the latest jj state.
        let orig = PathBuf::from(&state.remote_uri);
        if orig.join(".git").exists()
            && let Err(e) = run_jj(&orig, &["--ignore-working-copy", "git", "export"]).await
        {
            tracing::warn!(error = %e, "jj git export failed");
        }
        Ok(())
    }

    async fn squash(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
        from_bookmark: &str,
    ) -> Result<(), SandboxError> {
        jj::squash_from(sandbox_dir, from_bookmark).await?;
        self.push_sandbox(sandbox_dir, state, None).await
    }

    async fn diff_files(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
    ) -> Result<Vec<ChangedFile>, SandboxError> {
        jj::diff_files(sandbox_dir, &state.bookmark).await
    }

    async fn cleanup(&self, sandbox_dir: &Path, state: &RepoState) -> Result<(), SandboxError> {
        if jj::jj_bookmark_is_empty(sandbox_dir, &state.bookmark).await {
            let _ = run_jj(
                sandbox_dir,
                &["--ignore-working-copy", "abandon", &state.bookmark],
            )
            .await;
        }
        let _ = run_jj(
            sandbox_dir,
            &["--ignore-working-copy", "workspace", "forget"],
        )
        .await;
        if sandbox_dir.exists() {
            std::fs::remove_dir_all(sandbox_dir).map_err(SandboxError::Io)?;
        }
        Ok(())
    }

    fn cleanup_blocking(&self, sandbox_dir: &Path, state: &RepoState) {
        if jj_bookmark_is_empty_sync(sandbox_dir, &state.bookmark) {
            let _ = Command::new("jj")
                .args(["--ignore-working-copy", "abandon", &state.bookmark])
                .current_dir(sandbox_dir)
                .status();
        }
        let _ = Command::new("jj")
            .args(["--ignore-working-copy", "workspace", "forget"])
            .current_dir(sandbox_dir)
            .status();
        let _ = std::fs::remove_dir_all(sandbox_dir);
    }
}

/// Built-in provider for [`SandboxMode::Git`]: each sandbox is a git worktree
/// in a temp dir under `{repo}/.infinity/.sandboxes/`.
///
/// This is the fallback provider — its `detect` claims every repository not
/// claimed by an earlier provider (see `git::detect_mode`).
pub struct LocalGitProvider;

#[async_trait]
impl ModeProvider for LocalGitProvider {
    fn handles(&self, mode: &SandboxMode) -> bool {
        matches!(mode, SandboxMode::Git { .. })
    }

    async fn detect(
        &self,
        repo_path: &Path,
        ctx: &CloneContext<'_>,
    ) -> Result<Option<ModeInit>, SandboxError> {
        git::detect_mode(repo_path, ctx.base_bookmark).await
    }

    async fn create_sandbox(&self, state: &RepoState) -> Result<PathBuf, SandboxError> {
        let SandboxMode::Git { base_revision } = &state.mode else {
            return Err(SandboxError::Other(
                "bug: git provider invoked with a non-git mode".to_owned(),
            ));
        };
        let tmp = make_tempdir(&state.remote_uri).map_err(SandboxError::Io)?;
        let sandbox_dir = tmp.keep();
        git::git_worktree_add(
            &PathBuf::from(&state.remote_uri),
            &sandbox_dir,
            &state.bookmark,
            Some(base_revision),
        )
        .await?;
        Ok(sandbox_dir)
    }

    async fn push_sandbox(
        &self,
        sandbox_dir: &Path,
        _state: &RepoState,
        description: Option<&str>,
    ) -> Result<(), SandboxError> {
        git::git_amend_all(sandbox_dir, description).await
    }

    async fn squash(
        &self,
        sandbox_dir: &Path,
        _state: &RepoState,
        from_bookmark: &str,
    ) -> Result<(), SandboxError> {
        git::squash_from(sandbox_dir, from_bookmark).await
    }

    async fn diff_files(
        &self,
        _sandbox_dir: &Path,
        state: &RepoState,
    ) -> Result<Vec<ChangedFile>, SandboxError> {
        git::diff_files(Path::new(&state.remote_uri), &state.bookmark).await
    }

    async fn cleanup(&self, sandbox_dir: &Path, state: &RepoState) -> Result<(), SandboxError> {
        let original_repo = PathBuf::from(&state.remote_uri);
        let empty = git::git_branch_top_is_empty(sandbox_dir).await;
        let _ = git::git_worktree_remove(&original_repo, sandbox_dir).await;
        if empty {
            let _ = git::git_delete_branch(&original_repo, &state.bookmark).await;
        }
        Ok(())
    }

    fn cleanup_blocking(&self, sandbox_dir: &Path, state: &RepoState) {
        let original_repo = PathBuf::from(&state.remote_uri);
        let empty = git_branch_top_is_empty_sync(sandbox_dir);
        let _ = Command::new("git")
            .args([
                "worktree",
                "remove",
                "--force",
                &sandbox_dir.to_string_lossy(),
            ])
            .current_dir(&original_repo)
            .status();
        if empty {
            let _ = Command::new("git")
                .args(["branch", "-D", &state.bookmark])
                .current_dir(&original_repo)
                .status();
        }
    }
}
