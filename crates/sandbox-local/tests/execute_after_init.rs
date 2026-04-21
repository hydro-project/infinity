//! Tests that `execute_command` works immediately after `clone_repo`
//! without hanging or errors — for both jj and git modes.

mod common;

use common::{invoke, jj_init_with_file, start_test_server};
use rap_client::callback_server::start_callback_channel;

use std::process::Command;

/// Create a plain git repo (no jj) with an initial commit.
fn git_init_with_file(filename: &str, content: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let path = tmp.path();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(["-c", "init.defaultBranch=main"])
                .args(["-c", "user.name=test"])
                .args(["-c", "user.email=test@test"])
                .args(args)
                .current_dir(path)
                .status()
                .expect("run git command")
                .success()
        );
    };
    git(&["init"]);
    std::fs::write(path.join(filename), content).expect("write file");
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);
    tmp
}

/// Running a command immediately after clone_repo in jj mode should
/// complete without hanging or errors.
#[tokio::test]
async fn jj_execute_immediately_after_clone() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "jj-immediate-exec";
    let repo_str = repo.to_str().expect("repo path to str");

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
    assert!(text.contains("Repository initialized"), "got: {text}");

    // Execute a command immediately — no intermediate operations.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "echo ok" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("ok"), "expected 'ok' in output, got: {text}");
    assert!(
        text.contains("exit code"),
        "expected exit code in output, got: {text}"
    );
}

/// Running a command immediately after clone_repo in git (non-jj) mode should
/// complete without hanging or errors.
#[tokio::test]
async fn git_execute_immediately_after_clone() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = git_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "git-immediate-exec";
    let repo_str = repo.to_str().expect("repo path to str");

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
    assert!(text.contains("Repository initialized"), "got: {text}");

    // Execute a command immediately — no intermediate operations.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "echo ok" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("ok"), "expected 'ok' in output, got: {text}");
    assert!(
        text.contains("exit code"),
        "expected exit code in output, got: {text}"
    );
}
