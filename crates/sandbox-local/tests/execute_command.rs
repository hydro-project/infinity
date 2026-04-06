//! Tests for `execute_command` in jj mode.
//!
//! Verifies that running a command preserves the commit description,
//! that the command string appears in the evolog, and snapshot-tests
//! the ViewUpdate diff callback.

mod common;

use common::{invoke, invoke_collecting_views, jj_init_with_file, start_test_server};
use rap_client::callback_server::start_callback_channel;

use std::path::Path;
use std::process::Command;

fn jj_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.arg("-R").arg(repo).current_dir(repo);
    cmd
}

#[tokio::test]
async fn jj_execute_command_preserves_description() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "test-thread";
    let repo_str = repo.to_str().expect("repo path to str");

    // Clone the repo.
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

    // Set a commit description.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "describe_overall_changes",
        serde_json::json!({ "message": "feat: my cool feature" }),
        &mut rx,
        None,
    )
    .await;
    assert_eq!(text, "Edits described.");

    // Run a command that creates a file.
    let (text, views) = invoke_collecting_views(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "echo new-content > new_file.txt" }),
        &mut rx,
        1,
    )
    .await;
    assert!(text.contains("exit code"), "got: {text}");

    // Snapshot the ViewUpdate diff.
    assert_eq!(views.len(), 1, "expected exactly one ViewUpdate");
    let diff = views[0].content["diff"]
        .as_str()
        .expect("diff should be a string");
    insta::assert_snapshot!(diff);

    // The commit description should still be the one we set, not the command.
    let desc = String::from_utf8_lossy(
        &jj_cmd(repo)
            .args([
                "log",
                "--no-graph",
                "-r",
                &format!("sandbox-{group_id}"),
                "-T",
                "description",
            ])
            .output()
            .expect("run jj log")
            .stdout,
    )
    .to_string();
    assert!(
        desc.contains("feat: my cool feature"),
        "expected original description preserved, got: {desc}"
    );
    assert!(
        !desc.contains("echo new-content"),
        "description should not contain the command, got: {desc}"
    );

    // The command should appear in the evolog.
    let evolog = String::from_utf8_lossy(
        &jj_cmd(repo)
            .args(["evolog", "-r", &format!("sandbox-{group_id}")])
            .output()
            .expect("run jj evolog")
            .stdout,
    )
    .to_string();
    assert!(
        evolog.contains("echo new-content > new_file.txt"),
        "expected command in evolog, got: {evolog}"
    );
}
