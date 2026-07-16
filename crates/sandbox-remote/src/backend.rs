use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;

use sandbox_core::error::SandboxError;
use sandbox_core::jj::{self, run_jj};
use sandbox_core::sandbox::{CloneContext, ModeInit, SandboxBackend, SpawnedCommand};
use sandbox_core::types::{ChangedFile, RepoState, SandboxMode};

/// EFS-backed sandbox backend for remote (Lambda) mode.
///
/// The EFS mount point contains bare git repos as remotes, named by a
/// normalized form of the original git remote URI. Multiple group_ids
/// sharing the same source repo will share the same bare repo on EFS.
/// Sandboxes are temp dirs (in /tmp on Lambda) with jj clones pointing
/// at the EFS bare repo.
pub struct EfsBackend {
    /// Path to the EFS mount point (e.g. "/mnt/efs/sandbox-repos").
    efs_mount: PathBuf,
}

impl EfsBackend {
    pub fn new(efs_mount: PathBuf) -> Self {
        Self { efs_mount }
    }

    /// Derive a filesystem-safe directory name from a git remote URI.
    /// e.g. "https://github.com/acme/api.git" -> "https___github.com_acme_api.git"
    fn normalize_remote(uri: &str) -> String {
        uri.chars()
            .map(|c| match c {
                '/' | ':' | '@' | '?' | '#' | ' ' => '_',
                c if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' => c,
                _ => '_',
            })
            .collect()
    }

    /// Path to the bare git repo for a given remote URI.
    fn bare_repo_path(&self, repo_uri: &str) -> PathBuf {
        let name = Self::normalize_remote(repo_uri);
        self.efs_mount.join(name)
    }

    /// Run `git fetch` inside a bare repo to pull latest changes from the upstream remote.
    async fn git_fetch_upstream(bare_path: &PathBuf) -> Result<(), SandboxError> {
        let output = tokio::process::Command::new("jj")
            .args(["git", "fetch"])
            .current_dir(bare_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| SandboxError::CommandError(format!("git fetch origin failed: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SandboxError::CommandError(format!(
                "git fetch origin failed: {stderr}"
            )));
        }
        Ok(())
    }
    /// Mirror the source repo into a bare repo on EFS, named by the normalized URI.
    /// If the bare repo already exists, fetch latest changes from the upstream remote.
    async fn mirror_repo(&self, repo: &str, group_id: &str) -> Result<PathBuf, SandboxError> {
        let bare_path = self.bare_repo_path(repo);

        if bare_path.exists() {
            // Repo already on EFS — pull in any new upstream changes
            tracing::info!(
                group_id = %group_id,
                path = %bare_path.display(),
                "bare repo exists, fetching latest changes"
            );
            Self::git_fetch_upstream(&bare_path).await?;
            return Ok(bare_path);
        }

        // Create a bare clone on EFS
        let output = tokio::process::Command::new("jj")
            .args([
                "git",
                "clone",
                "--no-colocate",
                repo,
                bare_path
                    .to_str()
                    .expect("bug: bare repo path is not valid UTF-8"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| SandboxError::CommandError(format!("jj git clone failed: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SandboxError::CommandError(format!(
                "jj git clone failed: {stderr}"
            )));
        }

        tracing::info!(
            group_id = %group_id,
            path = %bare_path.display(),
            "created jj repo on EFS"
        );
        Ok(bare_path)
    }
}

#[async_trait]
impl SandboxBackend for EfsBackend {
    /// Create a temp dir and jj git clone from the EFS bare repo.
    /// If we have a previous bookmark, fetch and create a new working copy on top of it.
    async fn create_sandbox(&self, state: &RepoState) -> Result<PathBuf, SandboxError> {
        let base_revision = match &state.mode {
            SandboxMode::Jj { base_revision } => base_revision.as_str(),
            _ => {
                return Err(SandboxError::Other(
                    "EFS backend only supports Jj mode".to_owned(),
                ));
            }
        };

        let tmp = tempfile::tempdir().map_err(SandboxError::Io)?;
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

    /// Spawn a command in the sandbox, returning the child process handle.
    /// `argv[0]` is the program and `argv[1..]` are its arguments — no
    /// shell wrapping is applied.
    async fn spawn_command(
        &self,
        sandbox_dir: &Path,
        argv: &[&str],
        _extra_writable: &[&Path],
        _sandbox_writable: bool,
    ) -> Result<SpawnedCommand, SandboxError> {
        let child = tokio::process::Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(sandbox_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SandboxError::CommandError(format!("failed to spawn command: {e}")))?;

        Ok(SpawnedCommand { child })
    }

    /// Push the sandbox's working copy back to the EFS bare repo.
    async fn push_sandbox(
        &self,
        sandbox_dir: &Path,
        group_id: &str,
        _description: Option<&str>,
    ) -> Result<(), SandboxError> {
        let bookmark = format!("sandbox-{group_id}");
        jj::jj_push_working_copy(sandbox_dir, &bookmark).await
    }

    /// Remove the temp sandbox directory.
    async fn cleanup_sandbox(&self, sandbox_dir: &Path) -> Result<(), SandboxError> {
        run_jj(sandbox_dir, &["workspace", "forget"]).await?;
        tokio::fs::remove_dir_all(sandbox_dir)
            .await
            .map_err(SandboxError::Io)
    }

    /// The EFS backend only supports remote git URIs and Jujutsu mode: the
    /// requested URI is mirrored into a bare jj repo on EFS, and detection
    /// runs against that mirror (which always has a `.jj` directory).
    async fn detect_mode(&self, ctx: &CloneContext<'_>) -> Result<Option<ModeInit>, SandboxError> {
        let mirror = self.mirror_repo(ctx.requested_path, ctx.group_id).await?;
        jj::detect_mode(&mirror, ctx.base_bookmark).await
    }

    async fn describe_sandbox(
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

    async fn squash_sandbox(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
        from_bookmark: &str,
    ) -> Result<(), SandboxError> {
        jj::squash_from(sandbox_dir, from_bookmark).await?;
        jj::jj_push_working_copy(sandbox_dir, &state.bookmark).await
    }

    async fn diff_files(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
    ) -> Result<Vec<ChangedFile>, SandboxError> {
        jj::diff_files(sandbox_dir, &state.bookmark).await
    }
}
