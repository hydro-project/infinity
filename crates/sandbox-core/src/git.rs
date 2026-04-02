use std::path::Path;
use std::process::Stdio;

use crate::error::SandboxError;

/// Run a git command in the given directory, returning stdout on success.
#[tracing::instrument(level = "warn")]
pub async fn run_git(dir: &Path, args: &[&str]) -> Result<String, SandboxError> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            tracing::warn!("failed to spawn: {e}");
            SandboxError::CommandError(format!("failed to spawn git: {e}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(%stderr, "failed");
        return Err(SandboxError::CommandError(format!("git failed: {stderr}")));
    }

    tracing::debug!("success");
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Create a git worktree with a new branch.
/// If the branch already exists (e.g. from a previous session), prunes stale
/// worktrees and checks out the existing branch instead of creating a new one.
/// Detects the repo's configured user and configures the worktree identity.
pub async fn git_worktree_add(
    repo_dir: &Path,
    worktree_path: &Path,
    branch: &str,
    start_point: Option<&str>,
) -> Result<(), SandboxError> {
    let wt_str = worktree_path.to_string_lossy();

    // Check if the branch already exists
    let branch_exists = run_git(repo_dir, &["rev-parse", "--verify", branch])
        .await
        .is_ok();

    if branch_exists {
        // Prune stale worktree entries whose directories no longer exist
        let _ = run_git(repo_dir, &["worktree", "prune"]).await;
        // Check out the existing branch into the new worktree
        let args = vec!["worktree", "add", &wt_str, branch];
        run_git(repo_dir, &args).await?;
    } else {
        let mut args = vec!["worktree", "add", "-b", branch, &wt_str];
        if let Some(sp) = start_point {
            args.push(sp);
        }
        run_git(repo_dir, &args).await?;
        let (name, email) = git_configured_user(repo_dir).await.unwrap_or_else(|| {
            (
                crate::DEFAULT_SANDBOX_NAME.to_owned(),
                crate::DEFAULT_SANDBOX_EMAIL.to_owned(),
            )
        });

        // Configure the worktree-local user identity for a git worktree.
        // Enables `extensions.worktreeConfig` on the parent repo so that
        // `git config --worktree` writes to the worktree-specific config file.
        run_git(repo_dir, &["config", "extensions.worktreeConfig", "true"]).await?;
        run_git(worktree_path, &["config", "--worktree", "user.name", &name]).await?;
        run_git(
            worktree_path,
            &["config", "--worktree", "user.email", &email],
        )
        .await?;

        git_commit_all(worktree_path, "sandbox init").await?;
    }
    Ok(())
}

/// Remove a git worktree.
pub async fn git_worktree_remove(
    repo_dir: &Path,
    worktree_path: &Path,
) -> Result<(), SandboxError> {
    let wt = worktree_path.to_string_lossy();
    run_git(repo_dir, &["worktree", "remove", "--force", &wt]).await?;
    Ok(())
}

/// Stage all changes and create a commit.
pub async fn git_commit_all(dir: &Path, message: &str) -> Result<(), SandboxError> {
    run_git(dir, &["add", "-A"]).await?;
    run_git(
        dir,
        &["commit", "--allow-empty", "--no-verify", "-m", message],
    )
    .await?;
    Ok(())
}

/// Stage all changes and amend the current commit.
/// When `message` is `Some`, the commit message is replaced; when `None` the
/// existing message is kept (`--no-edit`).
pub async fn git_amend_all(dir: &Path, message: Option<&str>) -> Result<(), SandboxError> {
    run_git(dir, &["add", "-A"]).await?;
    let mut args = vec!["commit", "--amend", "--allow-empty", "--no-verify"];
    if let Some(msg) = message {
        args.extend(["-m", msg]);
    } else {
        args.push("--no-edit");
    }
    run_git(dir, &args).await?;
    Ok(())
}

/// Merge a branch into the current branch.
pub async fn git_merge_branch(dir: &Path, branch: &str) -> Result<(), SandboxError> {
    run_git(dir, &["merge", branch, "--no-edit"]).await?;
    Ok(())
}

/// Delete a branch.
pub async fn git_delete_branch(dir: &Path, branch: &str) -> Result<(), SandboxError> {
    run_git(dir, &["branch", "-D", branch]).await?;
    Ok(())
}

/// Check if a branch has commits beyond its fork point from HEAD.
pub async fn git_has_commits_on_branch(dir: &Path, branch: &str) -> Result<bool, SandboxError> {
    let range = format!("HEAD..{branch}");
    let output = run_git(dir, &["log", &range, "--oneline"]).await?;
    Ok(!output.trim().is_empty())
}

/// Check if the top commit on a branch has no file changes vs its parent.
pub async fn git_branch_top_is_empty(dir: &Path) -> bool {
    run_git(dir, &["diff", "--quiet", "HEAD~1"]).await.is_ok()
}

/// Read the configured git user from a repo directory.
/// Returns `Some((name, email))` if both are configured, `None` otherwise.
pub async fn git_configured_user(dir: &Path) -> Option<(String, String)> {
    let name = run_git(dir, &["config", "user.name"]).await.ok()?;
    let email = run_git(dir, &["config", "user.email"]).await.ok()?;
    Some((name.trim().to_owned(), email.trim().to_owned()))
}
