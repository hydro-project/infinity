//! Test that a sub-agent's changes survive thread closure even when the parent
//! never calls `squash_sandbox`. The `sandbox-{child}` bookmark and its
//! changeset must remain in jj so the work is not lost.

mod common;

use common::{invoke, jj_init_with_file, start_test_server};
use rap_client::callback_server::start_callback_channel;

use std::path::Path;
use std::process::Command;

fn jj_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.arg("-R").arg(repo).current_dir(repo);
    cmd
}

#[tokio::test]
async fn child_changes_preserved_after_close_without_squash() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let parent_id = "parent-thread";
    let child_id = "child-thread";
    let repo_str = repo.to_str().expect("repo path to str");

    // 1. Clone repo for parent
    let text = invoke(
        &server_url,
        &callback_url,
        parent_id,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    // 2. Edit a file on the parent so its bookmark exists
    let text = invoke(
        &server_url,
        &callback_url,
        parent_id,
        "edit_file",
        serde_json::json!({ "path": "README.md", "old_str": "hello\n", "new_str": "hello world\n" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Replaced"), "got: {text}");

    // 3. Clone repo for child, based on parent's bookmark
    let text = invoke(
        &server_url,
        &callback_url,
        child_id,
        "clone_repo",
        serde_json::json!({ "repo": repo_str, "base_thread_id": parent_id }),
        &mut rx,
        Some(vec![parent_id.to_owned()]),
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    // 4. Create a file in the child sandbox
    let text = invoke(
        &server_url,
        &callback_url,
        child_id,
        "create_file",
        serde_json::json!({ "path": "child.txt", "content": "from child\n" }),
        &mut rx,
        Some(vec![parent_id.to_owned()]),
    )
    .await;
    assert!(text.contains("Created file"), "got: {text}");

    // 5. Close the child thread WITHOUT squashing into parent
    let resp = reqwest::Client::new()
        .post(format!("{server_url}/close_thread"))
        .json(&serde_json::json!({ "thread_id": child_id }))
        .send()
        .await
        .expect("send close_thread");
    assert!(resp.status().is_success(), "close_thread failed");

    // Give the background cleanup task time to run
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 6. Verify the child's bookmark still exists and contains the change
    let output = jj_cmd(repo)
        .args([
            "log",
            "--no-graph",
            "-r",
            &format!("sandbox-{child_id}"),
            "-T",
            "description",
        ])
        .output()
        .expect("run jj log");
    assert!(
        output.status.success(),
        "sandbox-{child_id} bookmark should still exist after close_thread.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 7. Verify the child's file is reachable through the bookmark
    let output = jj_cmd(repo)
        .args([
            "diff",
            "--summary",
            "--from",
            "@-",
            "--to",
            &format!("sandbox-{child_id}"),
        ])
        .output()
        .expect("run jj diff");
    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        diff.contains("child.txt"),
        "child.txt should be in the child's changeset.\ndiff:\n{diff}"
    );

    // 8. Verify the parent does NOT have the child's file
    let text = invoke(
        &server_url,
        &callback_url,
        parent_id,
        "execute_command",
        serde_json::json!({ "command": "test -f child.txt && echo EXISTS || echo MISSING" }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("MISSING"),
        "parent sandbox should not contain child.txt, got: {text}"
    );
}
