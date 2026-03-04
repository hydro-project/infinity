use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use async_trait::async_trait;

use sandbox_core::error::SandboxError;
use sandbox_core::jj::{self, run_jj};
use sandbox_core::sandbox::{ExecResult, SandboxBackend};
use sandbox_core::types::RepoState;

/// Local sandbox backend.
/// The "remote" is just the original local git directory.
/// Each sandbox is a temp dir with a jj workspace pointing at that local path.
///
/// Sandbox directories are cached by group_id so that repeated
/// `create_sandbox` calls for the same group reuse the existing workspace
/// instead of running `jj workspace add` every time.
pub struct LocalBackend {
    /// group_id -> cached sandbox directory
    cache: Mutex<HashMap<String, PathBuf>>,
    /// Whether to use macOS sandbox-exec for command execution.
    sandbox_enabled: bool,
}

impl LocalBackend {
    pub fn new(sandbox_enabled: bool) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            sandbox_enabled,
        }
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        let cache = self.cache.get_mut().unwrap_or_else(|e| e.into_inner());
        for (group_id, dir) in cache.drain() {
            tracing::info!(group_id = %group_id, dir = %dir.display(), "dropping cached sandbox");
            // Best-effort: forget the jj workspace then remove the directory.
            let _ = Command::new("jj")
                .args(["workspace", "forget"])
                .current_dir(&dir)
                .status();
            let _ = std::fs::remove_dir_all(&dir);
        }
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
        let abs = path.canonicalize().map_err(SandboxError::Io)?;
        Ok(abs.to_string_lossy().to_string())
    }

    /// Create a sandbox for the given group. If a cached directory already
    /// exists for this group_id it is returned directly; otherwise a new
    /// temp dir is created and `jj workspace add` is run.
    async fn create_sandbox(&self, state: &RepoState) -> Result<PathBuf, SandboxError> {
        // Fast path: return cached dir if we already have one.
        {
            let maybe_dir = {
                let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.get(&state.group_id).cloned()
            };

            if let Some(dir) = maybe_dir && dir.exists() {
                tracing::info!(group_id = %state.group_id, "reusing cached sandbox");
                run_jj(&dir, &["workspace", "update-stale"]).await?;
                return Ok(dir);
            }
        }

        let tmp = tempfile::tempdir().map_err(SandboxError::Io)?;
        let sandbox_dir = tmp.keep();

        let bookmark = format!("sandbox-{}", &state.group_id);
        let res = jj::jj_git_clone(
            &state.remote_uri,
            &sandbox_dir,
            &bookmark,
            state.bookmark.is_none(),
        )
        .await;

        if res.as_ref().is_err_and(|e| match e {
            SandboxError::JujutsuError(e) if e.contains("It looks like this is a git repo.") => {
                true
            }
            _ => false,
        }) {
            run_jj(&PathBuf::from(&state.remote_uri), &["git", "init"]).await?;
            jj::jj_git_clone(
                &state.remote_uri,
                &sandbox_dir,
                &bookmark,
                state.bookmark.is_none(),
            )
            .await?;
        } else {
            res?;
        }

        // Store in cache for future reuse.
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(state.group_id.clone(), sandbox_dir.clone());
        }

        Ok(sandbox_dir)
    }

    /// Execute a command in the sandbox.
    ///
    /// On macOS, uses `sandbox-exec` to restrict filesystem write access to
    /// only the sandbox directory. On other platforms, falls back to plain bash.
    async fn execute_command(
        &self,
        sandbox_dir: &Path,
        command: &str,
    ) -> Result<ExecResult, SandboxError> {
        let output = if cfg!(target_os = "macos") && self.sandbox_enabled {
            let abs_sandbox = sandbox_dir.canonicalize().map_err(SandboxError::Io)?;
            let sandbox_dir_str = abs_sandbox.to_string_lossy();

            let tmp = tempfile::tempdir().map_err(SandboxError::Io)?;
            let abs_tmp = tmp.path().canonicalize().map_err(SandboxError::Io)?;
            let tmp_str = abs_tmp.to_string_lossy();

            let profile = format!(
                "(version 1)\n\
                 (debug deny)\n\
                 (allow default)\n\
                 (deny file-write*)\n\
                 (allow file-write*\n\
                     (subpath \"{sandbox_dir_str}\")\n\
                     (subpath \"{tmp_str}\"))\n\
                 (allow file-write-data\n\
                     (require-all\n\
                         (path \"/dev/null\")\n\
                         (vnode-type CHARACTER-DEVICE)))"
            );
            let result = tokio::process::Command::new("sandbox-exec")
                .args(["-p", &profile, "bash", "-c", command])
                .env("TMPDIR", abs_tmp.as_os_str())
                .current_dir(sandbox_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| SandboxError::CommandError(format!("failed to run command: {e}")))?;

            // Clean up the scratch tmpdir (best-effort).
            drop(tmp);
            result
        } else {
            tokio::process::Command::new("bash")
                .args(["-c", command])
                .current_dir(sandbox_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| SandboxError::CommandError(format!("failed to run command: {e}")))?
        };

        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    /// Push the sandbox's working copy back to the local git remote.
    async fn push_sandbox(&self, sandbox_dir: &Path, group_id: &str) -> Result<(), SandboxError> {
        let bookmark = format!("sandbox-{group_id}");
        jj::jj_push_working_copy(sandbox_dir, &bookmark).await
    }

    /// No-op for the local backend — sandboxes are cached and cleaned up
    /// when the `LocalBackend` is dropped.
    async fn cleanup_sandbox(&self, _sandbox_dir: &Path) -> Result<(), SandboxError> {
        Ok(())
    }
}
