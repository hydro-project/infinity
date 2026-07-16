use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use sandbox_core::error::SandboxError;
use sandbox_core::sandbox::{CloneContext, ModeInit, ModeProvider, SandboxBackend, SpawnedCommand};
use sandbox_core::types::{ChangedFile, RepoState, SandboxMode};

use crate::providers::{LocalGitProvider, LocalJjProvider};

/// Cached sandbox entry: sandbox directory + the repo state that created it.
struct CachedSandbox {
    dir: PathBuf,
    state: RepoState,
    /// Per-sandbox tmp dir. Persists across commands within the same session;
    /// cleaned up automatically on Drop (best-effort, matches `.gitignore`
    /// lifecycle semantics).
    tmp_dir: tempfile::TempDir,
}

/// Local sandbox backend.
/// The "remote" is just the original local git directory.
/// Each sandbox is a temp dir created by the mode's [`ModeProvider`]
/// (a jj workspace or git worktree pointing at that local path).
///
/// Sandbox directories are cached by group_id so that repeated
/// `create_sandbox` calls for the same group reuse the existing workspace
/// instead of recreating it every time.
///
/// Sandbox temp directories are created under `{remote_uri}/.infinity/.sandboxes/`.
pub struct LocalBackend {
    /// group_id -> cached sandbox
    cache: Mutex<HashMap<String, CachedSandbox>>,
    /// Whether to use platform-specific sandboxing for command execution
    /// (macOS sandbox-exec or Linux bubblewrap).
    sandbox_enabled: bool,
    /// Mode providers, consulted in order during `clone_repo` detection and
    /// used to dispatch all mode-specific behavior. The built-in jj and git
    /// providers are registered by default (git last, as the fallback);
    /// external providers are prepended via [`LocalBackend::with_provider`].
    providers: Vec<Arc<dyn ModeProvider>>,
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
            providers: vec![Arc::new(LocalJjProvider), Arc::new(LocalGitProvider)],
        }
    }

    /// Register an additional [`ModeProvider`], consulted before the built-in
    /// jj and git providers during `clone_repo` detection.
    #[must_use]
    pub fn with_provider(mut self, provider: Arc<dyn ModeProvider>) -> Self {
        self.providers.insert(0, provider);
        self
    }

    /// Find the provider that handles the given mode.
    fn provider_for(&self, mode: &SandboxMode) -> Option<&Arc<dyn ModeProvider>> {
        self.providers.iter().find(|p| p.handles(mode))
    }

    /// Find the provider that handles the given mode, or error.
    fn require_provider(&self, mode: &SandboxMode) -> Result<&Arc<dyn ModeProvider>, SandboxError> {
        self.provider_for(mode).ok_or_else(|| {
            SandboxError::Other("no mode provider registered for this sandbox mode".to_owned())
        })
    }

    /// Resolve a requested local path to a repository root.
    ///
    /// Validates that the path is absolute and exists, canonicalizes it, and
    /// walks up to the repository root so paths nested inside a repo (e.g. a
    /// cwd in a subdirectory of a jj repo, which has no `.jj`/`.git` marker
    /// of its own) resolve to the repo itself. If no root is found, falls
    /// back to the path as given so callers produce their usual "not a
    /// repository" errors (or, for Direct mode, operate on non-VCS dirs).
    fn resolve_repo_root(repo: &str) -> Result<PathBuf, SandboxError> {
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
        Ok(sandbox_core::find_repo_root(&abs).unwrap_or(abs))
    }

    /// Get the canonicalized tmp dir path for a sandbox from the cache.
    fn tmp_dir_for_sandbox(&self, sandbox_dir: &Path) -> Result<PathBuf, SandboxError> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let entry = cache
            .values()
            .find(|e| e.dir == sandbox_dir)
            .ok_or_else(|| SandboxError::Other("no cached sandbox for tmp dir".to_owned()))?;
        entry
            .tmp_dir
            .path()
            .canonicalize()
            .map_err(SandboxError::Io)
    }

    /// Extra writable paths granted by the sandbox's mode provider.
    fn provider_writable_paths(&self, sandbox_dir: &Path) -> Vec<PathBuf> {
        let mode = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .values()
                .find(|e| e.dir == sandbox_dir)
                .map(|e| e.state.mode.clone())
        };
        mode.and_then(|mode| self.provider_for(&mode))
            .map(|p| p.extra_writable_paths(sandbox_dir))
            .unwrap_or_default()
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        let cache = self.cache.get_mut().unwrap_or_else(|e| e.into_inner());
        for (group_id, entry) in cache.drain() {
            let dir = &entry.dir;
            tracing::info!(group_id = %group_id, dir = %dir.display(), "dropping cached sandbox");
            match &entry.state.mode {
                SandboxMode::Direct => {}
                mode => {
                    if let Some(provider) = self.providers.iter().find(|p| p.handles(mode)) {
                        provider.cleanup_blocking(dir, &entry.state);
                    } else {
                        tracing::warn!(
                            group_id = %group_id,
                            dir = %dir.display(),
                            "no mode provider registered for cached sandbox; \
                             skipping cleanup (temp directory and repo state may be left behind)"
                        );
                    }
                }
            }
        }
    }
}

#[async_trait]
impl SandboxBackend for LocalBackend {
    /// Create a sandbox for the given group. If a cached directory already
    /// exists for this group_id it is refreshed and returned directly;
    /// otherwise the mode's provider creates a new one.
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
                if !matches!(state.mode, SandboxMode::Direct) {
                    self.require_provider(&state.mode)?
                        .refresh_sandbox(&dir, state)
                        .await?;
                }
                return Ok(dir);
            } else {
                tracing::trace!(group_id = %state.group_id, "no cached sandbox to reuse");
            }
        }

        let sandbox_dir = match &state.mode {
            SandboxMode::Direct => PathBuf::from(&state.remote_uri),
            mode => self.require_provider(mode)?.create_sandbox(state).await?,
        };

        // Store in cache for future reuse.
        {
            let tmp_dir = tempfile::tempdir().map_err(SandboxError::Io)?;
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(
                state.group_id.clone(),
                CachedSandbox {
                    dir: sandbox_dir.clone(),
                    state: state.clone(),
                    tmp_dir,
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

            let abs_tmp = self.tmp_dir_for_sandbox(sandbox_dir)?;

            let mut writable = extra_writable_paths(sandbox_dir, Some(&abs_tmp));
            for p in extra_writable {
                if let Ok(resolved) = p.canonicalize() {
                    writable.push(resolved);
                } else {
                    writable.push(p.to_path_buf());
                }
            }
            writable.extend(self.provider_writable_paths(sandbox_dir));
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

            let abs_tmp = self.tmp_dir_for_sandbox(sandbox_dir)?;
            let tmp_str = abs_tmp.to_string_lossy();

            let mut writable = extra_writable_paths(sandbox_dir, Some(&abs_tmp));
            for p in extra_writable {
                if let Ok(resolved) = p.canonicalize() {
                    writable.push(resolved);
                } else {
                    writable.push(p.to_path_buf());
                }
            }
            writable.extend(self.provider_writable_paths(sandbox_dir));
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
        match state {
            Some(state) => match &state.mode {
                SandboxMode::Direct => Ok(()),
                mode => {
                    self.require_provider(mode)?
                        .push_sandbox(sandbox_dir, &state, description)
                        .await
                }
            },
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
    /// Delegates to the mode's provider (e.g. `jj workspace forget` and
    /// deleting the sandbox directory for jj mode).
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
        tracing::info!(group_id = %group_id, dir = %dir.display(), "permanently cleaning up sandbox");

        match &entry.state.mode {
            SandboxMode::Direct => {
                tracing::info!(group_id = %group_id, "direct mode — nothing to clean up");
            }
            mode => {
                self.require_provider(mode)?
                    .cleanup(dir, &entry.state)
                    .await?;
            }
        }

        // tmp_dir is cleaned up automatically when `entry` is dropped.

        Ok(())
    }

    /// For local mode, the requested path points at an existing repo on
    /// disk: resolve it to the repository root, then let the providers
    /// claim it.
    async fn detect_mode(&self, ctx: &CloneContext<'_>) -> Result<Option<ModeInit>, SandboxError> {
        let root = Self::resolve_repo_root(ctx.requested_path)?;
        for provider in &self.providers {
            if let Some(init) = provider.detect(&root, ctx).await? {
                return Ok(Some(init));
            }
        }
        Ok(None)
    }

    /// Direct mode operates on the resolved repository root itself (falling
    /// back to the requested directory when it has no VCS markers at all).
    async fn init_direct(&self, repo: &str) -> Result<String, SandboxError> {
        Ok(Self::resolve_repo_root(repo)?.to_string_lossy().to_string())
    }

    async fn describe_sandbox(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
        description: &str,
    ) -> Result<(), SandboxError> {
        match &state.mode {
            SandboxMode::Direct => Ok(()),
            mode => {
                self.require_provider(mode)?
                    .describe(sandbox_dir, state, description)
                    .await
            }
        }
    }

    async fn detect_external_change(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
    ) -> Option<String> {
        self.provider_for(&state.mode)?
            .detect_external_change(sandbox_dir, state)
            .await
    }

    async fn squash_sandbox(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
        from_bookmark: &str,
    ) -> Result<(), SandboxError> {
        self.require_provider(&state.mode)?
            .squash(sandbox_dir, state, from_bookmark)
            .await
    }

    async fn diff_files(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
    ) -> Result<Vec<ChangedFile>, SandboxError> {
        match self.provider_for(&state.mode) {
            Some(provider) => provider.diff_files(sandbox_dir, state).await,
            None => Ok(Vec::new()),
        }
    }
}
