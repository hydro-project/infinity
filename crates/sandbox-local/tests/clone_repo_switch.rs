//! Tests for clone_repo re-initialization behavior.
//!
//! - Re-initializing a non-Direct repo is rejected with an error.
//! - Upgrading from Direct mode to a sandboxed (non-Direct) repo is allowed.

mod common;

use common::{invoke, start_test_server};
use rap_client::callback_server::start_callback_channel;

fn jj_init_with_file(filename: &str, content: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let path = tmp.path();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init"])
            .current_dir(path)
            .status()
            .expect("run git init")
            .success()
    );
    assert!(
        std::process::Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(path)
            .status()
            .expect("run jj git init")
            .success()
    );
    std::fs::write(path.join(filename), content).expect("write marker file");
    // Commit the file so it appears in the base revision
    assert!(
        std::process::Command::new("jj")
            .args(["commit", "-m", "add marker file"])
            .current_dir(path)
            .status()
            .expect("run jj commit")
            .success()
    );
    tmp
}

/// Calling clone_repo twice with different repos should return an error.
#[tokio::test]
async fn clone_repo_rejects_reinit() {
    let _ = tracing_subscriber::fmt::try_init();

    let repo_a = jj_init_with_file("REPO_A.txt", "this is repo A\n");
    let repo_b = jj_init_with_file("REPO_B.txt", "this is repo B\n");

    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let server_url = start_test_server(metadata_dir.path()).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "reinit-test";

    // First clone succeeds
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo_a.path().to_str().expect("repo_a path to str") }),
        &mut rx,
        None,
    )
    .await;
    insta::assert_snapshot!(text, @"Repository initialized (using Jujutsu workspaces).");

    // Second clone with a different repo should be rejected
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo_b.path().to_str().expect("repo_b path to str") }),
        &mut rx,
        None,
    )
    .await;
    insta::assert_snapshot!(text, @"Error: A repository has already been initialized for this thread. Re-initializing with a different repository is not supported.");
}

/// Upgrading from open_sandbox_direct to clone_repo should be allowed.
#[tokio::test]
async fn clone_repo_allows_upgrade_from_direct() {
    let _ = tracing_subscriber::fmt::try_init();

    let repo = jj_init_with_file("README.txt", "hello\n");

    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let server_url = start_test_server(metadata_dir.path()).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "upgrade-test";
    let repo_str = repo.path().to_str().expect("repo path to str");

    // Start in Direct mode
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "open_sandbox_direct",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;
    insta::assert_snapshot!(text, @"Repository initialized (Direct mode — file edits require approval, commands run without file write access unless write-orig is granted).");

    // Materialize the Direct cache before upgrading.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("hello"),
        "should read Direct file, got: {text}"
    );

    // Upgrade to sandboxed mode via clone_repo
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;
    insta::assert_snapshot!(text, @"Repository initialized (using Jujutsu workspaces).");

    // Verify the sandbox works
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("hello"),
        "should read file from sandbox, got: {text}"
    );

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({
            "command": "printf upgraded > upgrade-marker.txt; cat upgrade-marker.txt"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("upgraded"),
        "should write in upgraded sandbox, got: {text}"
    );
    assert!(
        !repo.path().join("upgrade-marker.txt").exists(),
        "upgraded sandbox must not write to the original repository"
    );
}

#[tokio::test]
async fn open_sandbox_direct_rejects_managed_sandbox_without_changing_it() {
    let repo = jj_init_with_file("README.txt", "managed\n");
    let direct = tempfile::tempdir().expect("create Direct dir");
    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let server_url = start_test_server(metadata_dir.path()).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group_id = "managed-to-direct";

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    let before = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "pwd; printf before > pending.txt" }),
        &mut rx,
        None,
    )
    .await;
    let cwd = before
        .lines()
        .find(|line| line.starts_with('/'))
        .expect("sandbox cwd in command output");
    let metadata_path = metadata_dir.path().join(format!("{group_id}.json"));
    let metadata_before = std::fs::read(&metadata_path).expect("read managed metadata");

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "open_sandbox_direct",
        serde_json::json!({ "repo": direct.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.starts_with("Error:") && text.contains("already been initialized"),
        "managed sandbox must reject Direct re-initialization, got: {text}"
    );
    assert_eq!(
        std::fs::read(&metadata_path).expect("read unchanged managed metadata"),
        metadata_before
    );

    let after = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({
            "command": "pwd; printf after >> pending.txt; cat pending.txt"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        after.lines().any(|line| line == cwd),
        "managed sandbox cwd changed: {after}"
    );
    assert!(after.contains("beforeafter"), "sandbox lost edits: {after}");
    assert!(!repo.path().join("pending.txt").exists());
    assert!(!direct.path().join("pending.txt").exists());
}

#[tokio::test]
async fn open_sandbox_direct_rejects_reinit_without_changing_it() {
    let direct = tempfile::tempdir().expect("create Direct dir");
    let other = tempfile::tempdir().expect("create other Direct dir");
    let missing = other.path().join("missing");
    std::fs::write(direct.path().join("README.txt"), "original Direct\n")
        .expect("write Direct file");
    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let server_url = start_test_server(metadata_dir.path()).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group_id = "direct-reinit";

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "open_sandbox_direct",
        serde_json::json!({ "repo": direct.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("original Direct"), "got: {text}");
    let metadata_path = metadata_dir.path().join(format!("{group_id}.json"));
    let metadata_before = std::fs::read(&metadata_path).expect("read Direct metadata");

    for path in [direct.path(), other.path(), missing.as_path()] {
        let text = invoke(
            &server_url,
            &callback_url,
            group_id,
            "open_sandbox_direct",
            serde_json::json!({ "repo": path }),
            &mut rx,
            None,
        )
        .await;
        assert!(
            text.starts_with("Error:") && text.contains("already been initialized"),
            "Direct sandbox must reject re-initialization, got: {text}"
        );
        assert_eq!(
            std::fs::read(&metadata_path).expect("read unchanged Direct metadata"),
            metadata_before
        );
    }
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("original Direct"), "got: {text}");
}

#[tokio::test]
async fn clone_repo_upgrades_cached_non_vcs_direct_directory() {
    let direct = tempfile::tempdir().expect("create non-VCS Direct dir");
    std::fs::write(direct.path().join("README.txt"), "Direct only\n").expect("write Direct file");
    let repo = jj_init_with_file("README.txt", "sandbox only\n");
    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let server_url = start_test_server(metadata_dir.path()).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group_id = "non-vcs-direct-upgrade";

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "open_sandbox_direct",
        serde_json::json!({ "repo": direct.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Direct only"), "got: {text}");

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("sandbox only"), "got: {text}");

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({
            "command": "pwd; printf upgraded > upgrade-marker.txt; cat upgrade-marker.txt"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("upgraded"), "got: {text}");
    let cwd = text
        .lines()
        .find(|line| line.starts_with('/'))
        .expect("upgraded sandbox cwd in command output");
    let cwd = std::path::Path::new(cwd)
        .canonicalize()
        .expect("canonicalize sandbox cwd");
    assert_ne!(cwd, direct.path().canonicalize().expect("Direct path"));
    assert_ne!(cwd, repo.path().canonicalize().expect("repo path"));
    assert!(!direct.path().join("upgrade-marker.txt").exists());
    assert!(!repo.path().join("upgrade-marker.txt").exists());
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "upgrade-marker.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("upgraded"), "got: {text}");
}

#[tokio::test]
async fn clone_repo_failure_preserves_cached_direct_sandbox() {
    let direct = tempfile::tempdir().expect("create Direct dir");
    std::fs::write(direct.path().join("README.txt"), "still Direct\n").expect("write Direct file");
    let invalid_repo = tempfile::tempdir().expect("create non-repository dir");
    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let server_url = start_test_server(metadata_dir.path()).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");
    let group_id = "failed-direct-upgrade";

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "open_sandbox_direct",
        serde_json::json!({ "repo": direct.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("still Direct"), "got: {text}");
    let metadata_path = metadata_dir.path().join(format!("{group_id}.json"));
    let metadata_before = std::fs::read(&metadata_path).expect("read Direct metadata");

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": invalid_repo.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.starts_with("Error:"), "clone should fail, got: {text}");
    assert_eq!(
        std::fs::read(&metadata_path).expect("read unchanged Direct metadata"),
        metadata_before
    );
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("still Direct"), "got: {text}");
    assert_eq!(
        std::fs::read_to_string(direct.path().join("README.txt")).expect("read original file"),
        "still Direct\n"
    );
}
