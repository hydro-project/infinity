use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::error::SandboxError;

/// Build a jj command in the given directory.
fn jj_command(dir: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("jj");
    cmd.args(args).current_dir(dir);
    cmd
}

/// Run a jj command and return stdout.
#[tracing::instrument(level = "warn")]
pub async fn run_jj(dir: &Path, args: &[&str]) -> Result<String, SandboxError> {
    let output = jj_command(dir, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| {
            tracing::warn!("failed to spawn: {e}");
            SandboxError::JujutsuError(format!("failed to spawn jj: {e}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(%stderr, "failed");
        return Err(SandboxError::JujutsuError(format!("jj failed: {stderr}")));
    }

    tracing::debug!("success");
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Resolve a jj revision to an absolute change_id.
pub async fn jj_resolve_revision(dir: &Path, rev: &str) -> Result<String, SandboxError> {
    run_jj(
        dir,
        &[
            "log",
            "--no-graph",
            "-r",
            rev,
            "-T",
            "change_id ++ '/' ++ change_offset",
        ],
    )
    .await
}

/// Create a jj workspace at `dest` based on `revision`, set and edit the bookmark.
/// Detects the repo's configured user and configures the workspace identity.
pub async fn jj_git_clone(
    remote: &str,
    dest: &Path,
    bookmark_name: &str,
    revision: &str,
) -> Result<(), SandboxError> {
    let (name, email) = jj_configured_user(Path::new(remote))
        .await
        .unwrap_or_else(|| {
            (
                crate::DEFAULT_SANDBOX_NAME.to_owned(),
                crate::DEFAULT_SANDBOX_EMAIL.to_owned(),
            )
        });

    let _ = run_jj(&PathBuf::from(remote), &["workspace", "update-stale"]).await;

    // workspace add runs against the parent repo before the workspace exists,
    // so we inject the identity via --config flags for this command.
    let mut cmd = jj_command(
        Path::new(remote),
        &[
            "workspace",
            "add",
            "-r",
            "root()",
            dest.to_str()
                .expect("bug: sandbox dest path is not valid UTF-8"),
        ],
    );
    cmd.arg("--config")
        .arg(format!("user.name={name}"))
        .arg("--config")
        .arg(format!("user.email={email}"));
    let output = cmd
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

    // Configure workspace-local user identity for all subsequent workspace commands.
    run_jj(dest, &["config", "set", "--workspace", "user.name", &name]).await?;
    run_jj(
        dest,
        &["config", "set", "--workspace", "user.email", &email],
    )
    .await?;

    // Edit (checkout) the bookmark, creating it if it doesn't yet exist.
    tracing::info!(bookmark_name, "Edit bookmark");
    if run_jj(dest, &["edit", bookmark_name]).await.is_err() {
        tracing::info!(bookmark_name, revision, "Bookmark not found, creating new");
        run_jj(dest, &["new", revision]).await?;
        run_jj(dest, &["bookmark", "set", bookmark_name]).await?;
    }

    Ok(())
}
/// Squash a stacked commit (created by `jj new`) back into its parent,
/// preserving the parent's description. The stacked commit should already
/// be described so it remains visible in `jj evolog`.
pub async fn jj_squash_stacked(dir: &Path) -> Result<(), SandboxError> {
    run_jj(dir, &["workspace", "update-stale"]).await?;
    run_jj(
        dir,
        &["squash", "--into", "@-", "--use-destination-message"],
    )
    .await?;
    run_jj(dir, &["edit", "@-"]).await?;
    Ok(())
}

/// Push the current working copy to the remote.
pub async fn jj_push_working_copy(dir: &Path, bookmark_name: &str) -> Result<(), SandboxError> {
    run_jj(dir, &["bookmark", "set", bookmark_name]).await?;
    Ok(())
}

/// Check if a bookmark has been moved externally (i.e. no longer points to `@`).
pub async fn jj_detect_external_bookmark_move(dir: &Path, bookmark_name: &str) -> bool {
    let Some(bookmark_commit) = run_jj(
        dir,
        &["log", "--no-graph", "-r", bookmark_name, "-T", "commit_id"],
    )
    .await
    .ok() else {
        return false;
    };
    let Some(working_copy_commit) =
        run_jj(dir, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
            .await
            .ok()
    else {
        return false;
    };

    if bookmark_commit != working_copy_commit {
        tracing::warn!(
            bookmark = %bookmark_name,
            bookmark_target = %bookmark_commit,
            working_copy = %working_copy_commit,
            "bookmark was moved externally; overwriting with sandbox working copy"
        );
        true
    } else {
        false
    }
}

/// Check if a bookmark's commit is empty (no file changes).
pub async fn jj_bookmark_is_empty(dir: &Path, bookmark: &str) -> bool {
    run_jj(dir, &["log", "--no-graph", "-r", bookmark, "-T", "empty"])
        .await
        .map(|s| s == "true")
        .unwrap_or(false)
}

/// Read the configured jj user from a repo directory.
/// Returns `Some((name, email))` if both are configured in the repo, `None` otherwise.
pub async fn jj_configured_user(dir: &Path) -> Option<(String, String)> {
    let name = tokio::process::Command::new("jj")
        .args(["config", "get", "user.name"])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;
    if !name.status.success() {
        return None;
    }

    let email = tokio::process::Command::new("jj")
        .args(["config", "get", "user.email"])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;
    if !email.status.success() {
        return None;
    }

    let name = String::from_utf8_lossy(&name.stdout).trim().to_owned();
    let email = String::from_utf8_lossy(&email.stdout).trim().to_owned();
    if name.is_empty() || email.is_empty() {
        return None;
    }
    Some((name, email))
}
