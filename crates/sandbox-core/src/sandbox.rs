use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::SandboxError;
use crate::types::{ChangedFile, RepoState, SandboxMode};

/// A spawned command with its child process handle.
pub struct SpawnedCommand {
    pub child: tokio::process::Child,
}

/// Context passed to backends and [`ModeProvider`]s during `clone_repo` so
/// they can set up the repository and build a [`RepoState`].
pub struct CloneContext<'a> {
    /// The group_id (thread id) initializing the repo.
    pub group_id: &'a str,
    /// The path (or remote URI) as originally requested by the caller,
    /// before any backend resolution. Providers that use their own root
    /// markers should walk up from here (and return the root they find via
    /// [`ModeInit::repo_root`]).
    pub requested_path: &'a str,
    /// The bookmark name for this sandbox (`sandbox-<group_id>`).
    pub bookmark: &'a str,
    /// Base bookmark to resolve against (e.g. `sandbox-<id>`) when the caller
    /// passed a `base_thread_id`; `None` means base off the current tip.
    pub base_bookmark: Option<&'a str>,
}

/// Result of a [`ModeProvider`] claiming a repo during `clone_repo`.
pub struct ModeInit {
    /// The mode to store in the repo's [`RepoState`].
    pub mode: SandboxMode,
    /// The repository root as resolved by the provider (canonicalized).
    /// The server stores this as the repo's `remote_uri` instead of the
    /// backend's jj/git-based resolution, and notes the detected root in the
    /// response if the requested path was nested inside it.
    pub repo_root: PathBuf,
    /// Human-facing message returned from `clone_repo`.
    pub message: String,
    /// If `true`, the server eagerly calls `create_sandbox` right after
    /// `clone_repo` (e.g. to front-load expensive setup).
    pub precreate: bool,
}

/// Extension point for sandbox modes.
///
/// Providers are registered on a backend (see the local backend's
/// `with_provider`) and are consulted for all mode-specific behavior. The
/// built-in Jujutsu and Git modes are themselves implemented as providers
/// registered by default, so external providers are fully symmetric with
/// them. The core server treats every non-[`SandboxMode::Direct`] mode as
/// opaque and delegates here, so mode-specific concepts never leak into the
/// core crate.
#[async_trait]
pub trait ModeProvider: Send + Sync {
    /// Whether this provider handles the given mode. Used to route all
    /// per-sandbox operations to the right provider.
    fn handles(&self, mode: &SandboxMode) -> bool;

    /// Inspect the repo during `clone_repo`; return `Some(..)` to claim it,
    /// or `None` to let later-registered providers try. Providers are
    /// consulted in registration order (externally-registered providers
    /// first, then jj, then git).
    ///
    /// `repo_path` is the repository as set up by the backend (the resolved
    /// local root, or a local mirror of a remote); the originally requested
    /// path is available as [`CloneContext::requested_path`].
    async fn detect(
        &self,
        repo_path: &Path,
        ctx: &CloneContext<'_>,
    ) -> Result<Option<ModeInit>, SandboxError>;

    /// Create (or otherwise materialize) the sandbox working directory for a
    /// [`RepoState`] in this provider's mode.
    async fn create_sandbox(&self, state: &RepoState) -> Result<PathBuf, SandboxError>;

    /// Refresh a cached sandbox directory before it is reused (e.g. `jj
    /// workspace update-stale`).
    async fn refresh_sandbox(
        &self,
        _sandbox_dir: &Path,
        _state: &RepoState,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Record a change description on the sandbox before an operation runs
    /// (e.g. `jj describe`). Providers that apply descriptions at push time
    /// can keep the default no-op.
    async fn describe(
        &self,
        _sandbox_dir: &Path,
        _state: &RepoState,
        _description: &str,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Detect whether the sandbox's target (e.g. its bookmark) was moved
    /// externally since the last push. Returns a human-facing warning to
    /// prepend to the tool result, or `None`.
    async fn detect_external_change(
        &self,
        _sandbox_dir: &Path,
        _state: &RepoState,
    ) -> Option<String> {
        None
    }

    /// Push the sandbox working copy back to its source.
    async fn push_sandbox(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
        description: Option<&str>,
    ) -> Result<(), SandboxError>;

    /// Squash the changes from `from_bookmark` into the sandbox, including
    /// any push needed to make the result visible at the sandbox's bookmark.
    async fn squash(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
        from_bookmark: &str,
    ) -> Result<(), SandboxError>;

    /// Compute the set of changed files for the diff view.
    async fn diff_files(
        &self,
        sandbox_dir: &Path,
        state: &RepoState,
    ) -> Result<Vec<ChangedFile>, SandboxError>;

    /// Permanently clean up the sandbox.
    async fn cleanup(&self, sandbox_dir: &Path, state: &RepoState) -> Result<(), SandboxError>;

    /// Extra writable paths to grant when spawning commands in this sandbox.
    fn extra_writable_paths(&self, _sandbox_dir: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Best-effort SYNCHRONOUS cleanup, used from `Drop` where no async runtime
    /// is available.
    fn cleanup_blocking(&self, _sandbox_dir: &Path, _state: &RepoState) {}
}

/// Trait for the sandbox backend.
/// Handles creating sandboxes (temp dirs managed by the mode's
/// [`ModeProvider`], e.g. jj workspaces or git worktrees) and managing the
/// remote (local path vs s3).
#[async_trait]
pub trait SandboxBackend: Send + Sync {
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

    /// Called on `clone_repo`: the single entry point for repository setup
    /// and mode detection.
    ///
    /// The backend first sets up the repository from
    /// [`CloneContext::requested_path`] (e.g. the local backend resolves and
    /// canonicalizes the repo root; the remote backend mirrors the git URL
    /// onto shared storage), then consults its [`ModeProvider`]s in order and
    /// returns the first claim. [`ModeInit::repo_root`] becomes the repo's
    /// `remote_uri`. Returns `Ok(None)` when no provider claims the repo.
    async fn detect_mode(&self, ctx: &CloneContext<'_>) -> Result<Option<ModeInit>, SandboxError>;

    /// Called on `open_sandbox_direct`: resolve the repository root for
    /// Direct mode, which operates on the original directory and bypasses
    /// mode providers. Returns the root to store as the repo's `remote_uri`.
    async fn init_direct(&self, _repo: &str) -> Result<String, SandboxError> {
        Err(SandboxError::Other(
            "Direct mode is not supported by this backend".to_owned(),
        ))
    }

    /// Record a change description on the sandbox before an operation runs.
    async fn describe_sandbox(
        &self,
        _sandbox_dir: &Path,
        _state: &RepoState,
        _description: &str,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Detect whether the sandbox's target was moved externally since the
    /// last push. Returns a human-facing warning, or `None`.
    async fn detect_external_change(
        &self,
        _sandbox_dir: &Path,
        _state: &RepoState,
    ) -> Option<String> {
        None
    }

    /// Squash `from_bookmark` into the sandbox via the mode's provider.
    async fn squash_sandbox(
        &self,
        _sandbox_dir: &Path,
        _state: &RepoState,
        _from_bookmark: &str,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::Other(
            "squash is not supported by this backend".to_owned(),
        ))
    }

    /// Compute changed files for the sandbox diff via the mode's provider.
    async fn diff_files(
        &self,
        _sandbox_dir: &Path,
        _state: &RepoState,
    ) -> Result<Vec<ChangedFile>, SandboxError> {
        Ok(Vec::new())
    }
}
