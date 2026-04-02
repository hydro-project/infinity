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
}
