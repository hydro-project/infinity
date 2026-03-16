use std::path::Path;
use std::process::Stdio;

use crate::error::SandboxError;

/// Run a git command in the given directory, returning stdout on success.
pub async fn run_git(dir: &Path, args: &[&str]) -> Result<String, SandboxError> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SandboxError::CommandError(format!("failed to spawn git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SandboxError::CommandError(format!("git failed: {stderr}")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Create a git worktree with a new branch.
pub async fn git_worktree_add(
    repo_dir: &Path,
    worktree_path: &Path,
    branch: &str,
    start_point: Option<&str>,
) -> Result<(), SandboxError> {
    let wt_str = worktree_path.to_string_lossy();
    let mut args = vec!["worktree", "add", "-b", branch, &wt_str];
    tracing::trace!("Worktree command: {:?}", &args);
    if let Some(sp) = start_point {
        args.push(sp);
    }
    run_git(repo_dir, &args).await?;
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
    run_git(dir, &["commit", "--allow-empty", "-m", message]).await?;
    Ok(())
}

/// Stage all changes and amend the current commit.
pub async fn git_amend_all(dir: &Path, message: &str) -> Result<(), SandboxError> {
    run_git(dir, &["add", "-A"]).await?;
    run_git(dir, &["commit", "--amend", "--allow-empty", "-m", message]).await?;
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
