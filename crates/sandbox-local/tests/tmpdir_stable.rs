//! Test that TMPDIR remains stable across multiple command executions
//! within the same sandbox.

mod common;

use common::{invoke, jj_init_with_file, start_test_server_sandboxed};
use rap_client::callback_server::start_callback_channel;

/// TMPDIR should be the same path across multiple commands in the same sandbox.
/// This reproduces the bug where a new tempdir is created per command.
#[tokio::test]
async fn sandboxed_tmpdir_stable_across_commands() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server_sandboxed(&repo.join(".test-metadata"), true).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "tmpdir-stable";
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

    // Run first command to get TMPDIR
    let text1 = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "echo $TMPDIR" }),
        &mut rx,
        None,
    )
    .await;
    let tmpdir1 = text1
        .lines()
        .find(|l| l.starts_with('/'))
        .expect("TMPDIR path not found in first command output");

    // Run second command to get TMPDIR
    let text2 = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "echo $TMPDIR" }),
        &mut rx,
        None,
    )
    .await;
    let tmpdir2 = text2
        .lines()
        .find(|l| l.starts_with('/'))
        .expect("TMPDIR path not found in second command output");

    assert_eq!(
        tmpdir1, tmpdir2,
        "TMPDIR changed between commands: first={tmpdir1}, second={tmpdir2}"
    );
}

/// Files written to TMPDIR in one command should be readable in the next.
#[tokio::test]
async fn sandboxed_tmpdir_persists_files_across_commands() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server_sandboxed(&repo.join(".test-metadata"), true).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "tmpdir-persist";
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

    // Write a file to TMPDIR
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "echo 'persistent-data' > $TMPDIR/test-file.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("exit code 0"), "write failed: {text}");

    // Read the file back in a subsequent command
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "cat $TMPDIR/test-file.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("persistent-data"),
        "file written in previous command not found in TMPDIR: {text}"
    );
}
