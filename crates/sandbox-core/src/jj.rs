use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::error::SandboxError;

/// Run a jj command in the given directory (status only).
pub async fn run_jj(dir: &Path, args: &[&str]) -> Result<(), SandboxError> {
    let status = tokio::process::Command::new("jj")
        .args(["--config", "user.name=RAP Sandbox"])
        .args(["--config", "user.email=sandbox@rap"])
        .args(args)
        .current_dir(dir)
        .status()
        .await
        .map_err(|e| SandboxError::JujutsuError(format!("failed to spawn jj: {e}")))?;

    if !status.success() {
        return Err(SandboxError::JujutsuError("jj failed".to_string()));
    }

    Ok(())
}

/// Run a jj command and return stdout.
pub async fn run_jj_output(dir: &Path, args: &[&str]) -> Result<String, SandboxError> {
    let output = tokio::process::Command::new("jj")
        .args(["--config", "user.name=RAP Sandbox"])
        .args(["--config", "user.email=sandbox@rap"])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SandboxError::JujutsuError(format!("failed to spawn jj: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SandboxError::JujutsuError(format!("jj failed: {stderr}")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Resolve a jj revision to an absolute change_id.
pub async fn jj_resolve_revision(dir: &Path, rev: &str) -> Result<String, SandboxError> {
    run_jj_output(dir, &["log", "--no-graph", "-r", rev, "-T", "change_id"]).await
}

/// Create a jj workspace at `dest` based on `revision`, set and edit the bookmark.
pub async fn jj_git_clone(
    remote: &str,
    dest: &Path,
    bookmark_name: &str,
    revision: &str,
) -> Result<(), SandboxError> {
    let _ = run_jj(&PathBuf::from(remote), &["workspace", "update-stale"]).await;

    let output = tokio::process::Command::new("jj")
        .args(["--config", "user.name=RAP Sandbox"])
        .args(["--config", "user.email=sandbox@rap"])
        .args(["workspace", "add", "-r", "root()", dest.to_str().unwrap()])
        .current_dir(remote)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SandboxError::JujutsuError(format!("failed to spawn jj init: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SandboxError::JujutsuError(format!(
            "jj init failed: {stderr}"
        )));
    }

    if run_jj(dest, &["edit", bookmark_name]).await.is_err() {
        run_jj(dest, &["new", revision]).await?;
        run_jj(dest, &["bookmark", "set", bookmark_name]).await?;
    }

    Ok(())
}
/// Push the current working copy to the remote.
pub async fn jj_push_working_copy(dir: &Path, bookmark_name: &str) -> Result<(), SandboxError> {
    run_jj(dir, &["bookmark", "set", bookmark_name]).await?;
    Ok(())
}

/// Check if a bookmark's commit is empty (no file changes).
pub async fn jj_bookmark_is_empty(dir: &Path, bookmark: &str) -> bool {
    run_jj_output(dir, &["log", "--no-graph", "-r", bookmark, "-T", "empty"])
        .await
        .map(|s| s == "true")
        .unwrap_or(false)
}
