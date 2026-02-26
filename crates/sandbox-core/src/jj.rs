use std::path::Path;
use std::process::Stdio;

use crate::error::SandboxError;

/// Run a jj command in the given directory and return stdout.
async fn run_jj(dir: &Path, args: &[&str]) -> Result<String, SandboxError> {
    let output = tokio::process::Command::new("jj")
        .args(args)
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join(".jj"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SandboxError::JujutsuError(format!("failed to spawn jj: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SandboxError::JujutsuError(format!(
            "jj {} failed: {stderr}",
            args.join(" ")
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Initialize a jj repo by cloning from a git remote into `dest`.
/// Uses --depth=1 to only fetch the most recent commit on the default branch.
pub async fn jj_git_clone(remote: &str, dest: &Path) -> Result<(), SandboxError> {
    let output = tokio::process::Command::new("jj")
        .args(["git", "clone", "--depth=1", remote, dest.to_str().unwrap()])
        .env("XDG_CONFIG_HOME", dest.join(".jj"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SandboxError::JujutsuError(format!("failed to spawn jj git clone: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SandboxError::JujutsuError(format!(
            "jj git clone failed: {stderr}"
        )));
    }
    Ok(())
}
/// Configure the jj repo with a default author so all commits have valid metadata.
pub async fn jj_configure_author(dir: &Path) -> Result<(), SandboxError> {
    run_jj(
        dir,
        &["config", "set", "--repo", "user.name", "RAP Sandbox"],
    )
    .await?;
    run_jj(
        dir,
        &["config", "set", "--repo", "user.email", "sandbox@rap"],
    )
    .await?;
    Ok(())
}

/// Get the current working copy commit id.
pub async fn get_working_copy_commit(dir: &Path) -> Result<String, SandboxError> {
    run_jj(dir, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]).await
}

/// Fetch from the remote.
pub async fn jj_git_fetch(dir: &Path) -> Result<(), SandboxError> {
    run_jj(dir, &["git", "fetch"]).await?;
    Ok(())
}

/// Push the current working copy to the remote.
/// We describe the working copy first (jj won't push commits with no description),
/// then create a bookmark and push it.
pub async fn jj_push_working_copy(dir: &Path, bookmark_name: &str) -> Result<(), SandboxError> {
    // Describe the working copy so jj considers it a complete commit
    run_jj(dir, &["describe", "-m", "sandbox snapshot"]).await?;
    // Track the remote bookmark if it exists (needed on subsequent pushes
    // so jj links the local bookmark to the remote one)
    let remote_ref = format!("{bookmark_name}@origin");
    let _ = run_jj(dir, &["bookmark", "track", &remote_ref]).await;
    // Set a bookmark on the working copy
    run_jj(dir, &["bookmark", "set", bookmark_name, "-r", "@"]).await?;
    // Push the bookmark (--allow-new for first push when bookmark doesn't exist on remote)
    run_jj(
        dir,
        &["git", "push", "--allow-new", "--bookmark", bookmark_name],
    )
    .await?;
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
