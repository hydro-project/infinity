use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use async_trait::async_trait;

use sandbox_core::error::SandboxError;
use sandbox_core::git;
use sandbox_core::jj::{self, run_jj};
use sandbox_core::sandbox::{SandboxBackend, SpawnedCommand};
use sandbox_core::types::{RepoState, SandboxMode};

/// Cached sandbox entry: sandbox directory + the repo state that created it.
struct CachedSandbox {
    dir: PathBuf,
    state: RepoState,
}

/// Local sandbox backend.
/// The "remote" is just the original local git directory.
/// Each sandbox is a temp dir with a jj workspace pointing at that local path.
///
/// Sandbox directories are cached by group_id so that repeated
/// `create_sandbox` calls for the same group reuse the existing workspace
/// instead of running `jj workspace add` every time.
///
/// Sandbox temp directories are created under `{remote_uri}/.infinity/.sandboxes/`.
pub struct LocalBackend {
    /// group_id -> cached sandbox
    cache: Mutex<HashMap<String, CachedSandbox>>,
    /// Whether to use platform-specific sandboxing for command execution
    /// (macOS sandbox-exec or Linux bubblewrap).
    sandbox_enabled: bool,
}

/// Returns the list of additional writable paths for a sandbox.
/// Includes the temp dir and the sccache cache directory.
fn extra_writable_paths(_sandbox_dir: &Path, tmp_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(t) = tmp_dir {
        paths.push(t.to_path_buf());
    }

    // Allow sccache to write to its cache directory so that sandboxed
    // `cargo build` invocations (which use sccache as RUSTC_WRAPPER) can
    // store and retrieve cached artifacts.
    if let Some(dir) = sccache_cache_dir()
        && let Ok(resolved) = dir.canonicalize()
    {
        paths.push(resolved);
    }

    paths
}

/// Returns the sccache local cache directory, respecting `SCCACHE_DIR` and
/// platform-specific defaults.
fn sccache_cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SCCACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").ok()?;
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(format!(
            "{home}/Library/Caches/Mozilla.sccache"
        )))
    } else {
        Some(PathBuf::from(format!("{home}/.cache/sccache")))
    }
}

/// Ensure the sccache server is running. Called before each sandboxed
/// command so that a crashed server is restarted outside the sandbox
/// (where it has full write access).
fn ensure_sccache_server() {
    match Command::new("sccache")
        .arg("--start-server")
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            tracing::info!("started sccache server");
        }
        Ok(_) => {
            tracing::trace!("sccache server already running");
        }
        Err(_) => {} // sccache not available
    }
}

/// Returns `true` when the current platform supports sandboxed execution.
fn platform_sandbox_available() -> bool {
    cfg!(target_os = "macos") || cfg!(target_os = "linux")
}

impl LocalBackend {
    pub fn new(sandbox_enabled: bool) -> Self {
        if sandbox_enabled && !platform_sandbox_available() {
            tracing::warn!(
                "sandbox enabled but not supported on this platform; commands will run unsandboxed"
            );
        }

        Self {
            cache: Mutex::new(HashMap::new()),
            sandbox_enabled,
        }
    }

    /// Compute the sandboxes base directory for a given repo.
    fn sandboxes_dir_for(remote_uri: &str) -> PathBuf {
        PathBuf::from(remote_uri)
            .join(".infinity")
            .join(".sandboxes")
    }

    /// Create a new temporary directory under `{remote_uri}/.infinity/.sandboxes/`.
    fn make_tempdir(remote_uri: &str) -> std::io::Result<tempfile::TempDir> {
        let base = Self::sandboxes_dir_for(remote_uri);
        std::fs::create_dir_all(&base)?;
        tempfile::tempdir_in(&base)
    }

    /// Deterministic per-sandbox tmp dir path. Persists across commands and
    /// agent resumes; only cleaned up in `cleanup_sandbox_permanently`.
    fn tmp_dir_for(remote_uri: &str, group_id: &str) -> PathBuf {
        Self::sandboxes_dir_for(remote_uri).join(format!(".tmpdir-{group_id}"))
    }

    /// Look up the persistent tmp dir for a sandbox_dir by finding the
    /// matching cache entry. Creates the directory if it doesn't exist.
    fn get_or_create_tmp_dir(&self, sandbox_dir: &Path) -> Option<PathBuf> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let entry = cache.values().find(|e| e.dir == sandbox_dir)?;
        let tmp = Self::tmp_dir_for(&entry.state.remote_uri, &entry.state.group_id);
        drop(cache);
        let _ = std::fs::create_dir_all(&tmp);
        Some(tmp)
    }
}

/// Check if a jj bookmark's commit is empty (no changes) using a synchronous command.
fn jj_bookmark_is_empty(dir: &Path, bookmark: &str) -> bool {
    Command::new("jj")
        .args(["log", "--no-graph", "-r", bookmark, "-T", "empty"])
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

impl Drop for LocalBackend {
    fn drop(&mut self) {
        let cache = self.cache.get_mut().unwrap_or_else(|e| e.into_inner());
        for (group_id, entry) in cache.drain() {
            let dir = &entry.dir;
            let branch = format!("sandbox-{group_id}");
            tracing::info!(group_id = %group_id, dir = %dir.display(), "dropping cached sandbox");
            match &entry.state.mode {
                SandboxMode::Jj { .. } => {
                    if jj_bookmark_is_empty(dir, &branch) {
                        let _ = Command::new("jj")
                            .args(["abandon", &branch])
                            .current_dir(dir)
                            .status();
                    }
                    let _ = Command::new("jj")
                        .args(["workspace", "forget"])
                        .current_dir(dir)
                        .status();
                    let _ = std::fs::remove_dir_all(dir);
                }
                SandboxMode::Git { .. } => {
                    let original_repo = PathBuf::from(&entry.state.remote_uri);
                    let empty = git_branch_top_is_empty_sync(dir);
                    let _ = Command::new("git")
                        .args(["worktree", "remove", "--force", &dir.to_string_lossy()])
                        .current_dir(&original_repo)
                        .status();
                    if empty {
                        let _ = Command::new("git")
                            .args(["branch", "-D", &branch])
                            .current_dir(&original_repo)
                            .status();
                    }
                }
                SandboxMode::Direct => {}
            }
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
        if !path.is_absolute() {
            return Err(SandboxError::Other(format!(
                "repo path must be absolute, got: {repo}"
            )));
        }
        if !path.exists() {
            return Err(SandboxError::Other(format!(
                "local repo path does not exist: {repo}"
            )));
        }
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
                cache.get(&state.group_id).map(|e| e.dir.clone())
            };

            if let Some(dir) = maybe_dir
                && dir.exists()
            {
                tracing::info!(group_id = %state.group_id, "reusing cached sandbox");
                match &state.mode {
                    SandboxMode::Jj { .. } => {
                        run_jj(&dir, &["workspace", "update-stale"]).await?;
                    }
                    SandboxMode::Git { .. } | SandboxMode::Direct => {}
                }
                return Ok(dir);
            } else {
                tracing::trace!(group_id = %state.group_id, "no cached sandbox to reuse");
            }
        }

        let sandbox_dir = match &state.mode {
            SandboxMode::Direct => PathBuf::from(&state.remote_uri),
            SandboxMode::Jj { base_revision } => {
                let tmp = Self::make_tempdir(&state.remote_uri).map_err(SandboxError::Io)?;
                let sandbox_dir = tmp.keep();
                jj::jj_git_clone(
                    &state.remote_uri,
                    &sandbox_dir,
                    &state.bookmark,
                    base_revision,
                )
                .await?;
                sandbox_dir
            }
            SandboxMode::Git { base_revision } => {
                let tmp = Self::make_tempdir(&state.remote_uri).map_err(SandboxError::Io)?;
                let sandbox_dir = tmp.keep();
                git::git_worktree_add(
                    &PathBuf::from(&state.remote_uri),
                    &sandbox_dir,
                    &state.bookmark,
                    Some(base_revision),
                )
                .await?;
                sandbox_dir
            }
        };

        // Store in cache for future reuse.
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(
                state.group_id.clone(),
                CachedSandbox {
                    dir: sandbox_dir.clone(),
                    state: state.clone(),
                },
            );
        }

        Ok(sandbox_dir)
    }

    /// Spawn a command in the sandbox, returning the child process handle.
    ///
    /// `argv` is the raw argument vector: `argv[0]` is the program name and
    /// `argv[1..]` are its arguments.  The caller is responsible for any
    /// shell wrapping (e.g. `&["bash", "-c", cmd]` for user shell commands).
    ///
    /// When sandboxing is enabled, uses `sandbox-exec` on macOS or `bwrap`
    /// on Linux to restrict filesystem write access to only the sandbox
    /// directory. On other platforms, runs the command directly.
    /// The temp directory for `TMPDIR` is stored in the returned
    /// `SpawnedCommand` so it outlives the child process.
    async fn spawn_command(
        &self,
        sandbox_dir: &Path,
        argv: &[&str],
        extra_writable: &[&Path],
        sandbox_writable: bool,
    ) -> Result<SpawnedCommand, SandboxError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| SandboxError::Other("argv must not be empty".to_owned()))?;

        // Re-check that the sccache server is alive before each command so
        // that a crashed server is restarted outside the sandbox (where it
        // has full filesystem access).
        if self.sandbox_enabled {
            ensure_sccache_server();
        }

        // Prevent git from discovering the outer repo when sandboxes live
        // inside the repository (e.g. {repo}/.infinity/.sandboxes/).
        let abs_sandbox = sandbox_dir.canonicalize().map_err(SandboxError::Io)?;
        let git_ceiling = abs_sandbox
            .parent()
            .unwrap_or(&abs_sandbox)
            .as_os_str()
            .to_owned();

        if cfg!(target_os = "macos") && self.sandbox_enabled {
            let sandbox_dir_str = abs_sandbox.to_string_lossy();

            let tmp_dir = self
                .get_or_create_tmp_dir(sandbox_dir)
                .ok_or_else(|| SandboxError::Other("no cached sandbox for tmp dir".to_owned()))?;
            let abs_tmp = tmp_dir.canonicalize().map_err(SandboxError::Io)?;

            let mut writable = extra_writable_paths(sandbox_dir, Some(&abs_tmp));
            for p in extra_writable {
                if let Ok(resolved) = p.canonicalize() {
                    writable.push(resolved);
                } else {
                    writable.push(p.to_path_buf());
                }
            }
            let writable_iter: Box<dyn Iterator<Item = String>> = if sandbox_writable {
                Box::new(
                    std::iter::once(sandbox_dir_str.to_string())
                        .chain(writable.iter().map(|p| p.to_string_lossy().to_string())),
                )
            } else {
                Box::new(writable.iter().map(|p| p.to_string_lossy().to_string()))
            };
            let subpath_rules: String = writable_iter
                .map(|p| format!("\n       (subpath \"{p}\")"))
                .collect();
            let profile = if subpath_rules.is_empty() {
                "(version 1)\n\
                 (debug deny)\n\
                 (allow default)\n\
                 (deny file-write*)\n\
                 (allow file-write-data\n\
                     (require-all\n\
                         (path \"/dev/null\")\n\
                         (vnode-type CHARACTER-DEVICE)))"
                    .to_owned()
            } else {
                format!(
                    "(version 1)\n\
                     (debug deny)\n\
                     (allow default)\n\
                     (deny file-write*)\n\
                     (allow file-write*{subpath_rules})\n\
                     (allow file-write-data\n\
                         (require-all\n\
                             (path \"/dev/null\")\n\
                             (vnode-type CHARACTER-DEVICE)))"
                )
            };
            let mut cmd = tokio::process::Command::new("sandbox-exec");
            cmd.args(["-p", &profile])
                .arg(program)
                .args(args)
                .env("TMPDIR", abs_tmp.as_os_str())
                .env("GIT_CEILING_DIRECTORIES", &git_ceiling)
                .current_dir(sandbox_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);

            let child = cmd
                .spawn()
                .map_err(|e| SandboxError::CommandError(format!("failed to spawn command: {e}")))?;

            Ok(SpawnedCommand { child })
        } else if cfg!(target_os = "linux") && self.sandbox_enabled {
            let sandbox_dir_str = abs_sandbox.to_string_lossy();

            let tmp_dir = self
                .get_or_create_tmp_dir(sandbox_dir)
                .ok_or_else(|| SandboxError::Other("no cached sandbox for tmp dir".to_owned()))?;
            let abs_tmp = tmp_dir.canonicalize().map_err(SandboxError::Io)?;
            let tmp_str = abs_tmp.to_string_lossy();

            let mut writable = extra_writable_paths(sandbox_dir, Some(&abs_tmp));
            for p in extra_writable {
                if let Ok(resolved) = p.canonicalize() {
                    writable.push(resolved);
                } else {
                    writable.push(p.to_path_buf());
                }
            }
            let mut bwrap_args = vec![
                "--ro-bind",
                "/",
                "/",
                "--bind",
                &tmp_str,
                &tmp_str,
                "--dev",
                "/dev",
                "--proc",
                "/proc",
            ];
            if sandbox_writable {
                bwrap_args.extend(["--bind", &sandbox_dir_str, &sandbox_dir_str]);
            }
            let writable_strs: Vec<String> = writable
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            for p in &writable_strs {
                bwrap_args.extend(["--bind", p.as_str(), p.as_str()]);
            }
            bwrap_args.push("--");
            let mut cmd = tokio::process::Command::new("bwrap");
            cmd.args(&bwrap_args)
                .arg(program)
                .args(args)
                .env("TMPDIR", abs_tmp.as_os_str())
                .env("GIT_CEILING_DIRECTORIES", &git_ceiling)
                .current_dir(sandbox_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);

            let child = cmd
                .spawn()
                .map_err(|e| SandboxError::CommandError(format!("failed to spawn command: {e}")))?;

            Ok(SpawnedCommand { child })
        } else {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args)
                .env("GIT_CEILING_DIRECTORIES", &git_ceiling)
                .current_dir(sandbox_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);

            let child = cmd
                .spawn()
                .map_err(|e| SandboxError::CommandError(format!("failed to spawn command: {e}")))?;

            Ok(SpawnedCommand { child })
        }
    }

    /// Push the sandbox's working copy back to the local git remote.
    async fn push_sandbox(
        &self,
        sandbox_dir: &Path,
        group_id: &str,
        description: Option<&str>,
    ) -> Result<(), SandboxError> {
        let state = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.get(group_id).map(|e| e.state.clone())
        };
        match state.as_ref().map(|s| &s.mode) {
            Some(SandboxMode::Direct) => Ok(()),
            Some(SandboxMode::Git { .. }) => git::git_amend_all(sandbox_dir, description).await,
            Some(SandboxMode::Jj { .. }) => {
                let bookmark = format!("sandbox-{group_id}");
                jj::jj_push_working_copy(sandbox_dir, &bookmark).await?;
                // Keep colocated git refs in sync so git commands via
                // write-orig see the latest jj state.
                let orig = PathBuf::from(
                    &state
                        .as_ref()
                        .expect("bug: state should be Some")
                        .remote_uri,
                );
                if orig.join(".git").exists()
                    && let Err(e) = run_jj(&orig, &["git", "export"]).await
                {
                    tracing::warn!(error = %e, "jj git export failed");
                }
                Ok(())
            }
            None => Err(SandboxError::Other(format!(
                "no cached sandbox state for group_id: {group_id}"
            ))),
        }
    }

    /// No-op for the local backend — sandboxes are cached and cleaned up
    /// when the `LocalBackend` is dropped.
    async fn cleanup_sandbox(&self, _sandbox_dir: &Path) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Permanently clean up the cached sandbox for the given group_id.
    ///
    /// For jj mode: runs `jj workspace forget` and deletes the sandbox directory.
    async fn cleanup_sandbox_permanently(&self, group_id: &str) -> Result<(), SandboxError> {
        let entry = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.remove(group_id)
        };

        let Some(entry) = entry else {
            tracing::info!(group_id = %group_id, "no cached sandbox to clean up");
            return Ok(());
        };

        let dir = &entry.dir;
        let branch = format!("sandbox-{group_id}");
        tracing::info!(group_id = %group_id, dir = %dir.display(), "permanently cleaning up sandbox");

        match &entry.state.mode {
            SandboxMode::Jj { .. } => {
                if jj::jj_bookmark_is_empty(dir, &branch).await {
                    let _ = run_jj(dir, &["abandon", &branch]).await;
                }
                let _ = run_jj(dir, &["workspace", "forget"]).await;
                if dir.exists() {
                    std::fs::remove_dir_all(dir).map_err(SandboxError::Io)?;
                }
            }
            SandboxMode::Git { .. } => {
                let original_repo = PathBuf::from(&entry.state.remote_uri);
                let empty = git::git_branch_top_is_empty(dir).await;
                let _ = git::git_worktree_remove(&original_repo, dir).await;
                if empty {
                    let _ = git::git_delete_branch(&original_repo, &branch).await;
                }
            }
            SandboxMode::Direct => {
                tracing::info!(group_id = %group_id, "direct mode — nothing to clean up");
            }
        }

        // Clean up the persistent tmp dir for this sandbox.
        let tmp_dir = Self::tmp_dir_for(&entry.state.remote_uri, group_id);
        if tmp_dir.exists() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }

        Ok(())
    }
}
