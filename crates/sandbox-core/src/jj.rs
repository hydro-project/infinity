use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::error::SandboxError;

/// Run a jj command in the given directory and return stdout.
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

/// Initialize a jj repo by cloning from a git remote into `dest`.
/// Uses --depth=1 to only fetch the most recent commit on the default branch.
pub async fn jj_git_clone(
    remote: &str,
    dest: &Path,
    bookmark_name: &str,
    first_clone: bool,
) -> Result<(), SandboxError> {
    run_jj(&PathBuf::from(remote), &["workspace", "update-stale"]).await?;

    let output = tokio::process::Command::new("jj")
        .args(["--config", "user.name=RAP Sandbox"])
        .args(["--config", "user.email=sandbox@rap"])
        .args(["workspace", "add", "-r", "@", dest.to_str().unwrap()])
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

    if first_clone {
        tracing::info!("This is the first clone of the repo");
        tracing::info!("Pushing initial bookmark, path {:?}", dest);
        let _ = run_jj(dest, &["bookmark", "set", bookmark_name]).await;
        // on error, bookmark might already be there, attempt to inherit it
    }

    run_jj(dest, &["edit", bookmark_name]).await?;

    Ok(())
}
/// Push the current working copy to the remote.
/// We describe the working copy first (jj won't push commits with no description),
/// then create a bookmark and push it.
pub async fn jj_push_working_copy(dir: &Path, bookmark_name: &str) -> Result<(), SandboxError> {
    run_jj(dir, &["bookmark", "set", bookmark_name]).await?;

    Ok(())
}

/// Edit a specific commit (set working copy to it).
pub async fn jj_edit(dir: &Path, revision: &str) -> Result<(), SandboxError> {
    run_jj(dir, &["edit", revision]).await?;
    Ok(())
}

/// Create a new mutable working copy on top of a revision.
/// Use this instead of `jj_edit` when the target is immutable (e.g. remote-tracking bookmarks).
pub async fn jj_new(dir: &Path, revision: &str) -> Result<(), SandboxError> {
    run_jj(dir, &["new", revision]).await?;
    Ok(())
}
