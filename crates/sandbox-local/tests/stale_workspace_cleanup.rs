//! Reproducer: closing a child sandbox fails to forget its workspace when the
//! parent has operated since the child's last operation, making the child stale.
//! Without `--ignore-working-copy`, `workspace forget` errors out and the
//! workspace lingers in jj.

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
async fn workspace_forget_succeeds_when_child_is_stale() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();
    let metadata_dir = tempfile::tempdir().expect("create metadata tempdir");

    let server_url = start_test_server(metadata_dir.path()).await;
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

    // 2. Edit in parent
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

    // 3. Clone repo for child based on parent
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

    // 4. Edit in child
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

    // 5. Parent does more work — this makes the child sandbox stale.
    let text = invoke(
        &server_url,
        &callback_url,
        parent_id,
        "create_file",
        serde_json::json!({ "path": "after.txt", "content": "more\n" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Created file"), "got: {text}");

    // 6. Close the child thread — workspace forget must succeed despite stale.
    let resp = reqwest::Client::new()
        .post(format!("{server_url}/close_thread"))
        .json(&serde_json::json!({ "thread_id": child_id }))
        .send()
        .await
        .expect("send close_thread");
    assert!(resp.status().is_success(), "close_thread failed");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 7. Only default + parent workspaces should remain.
    let output = jj_cmd(repo)
        .args(["--ignore-working-copy", "workspace", "list"])
        .output()
        .expect("jj workspace list");
    let list = String::from_utf8_lossy(&output.stdout).to_string();

    let workspace_count = list.lines().count();
    assert!(
        workspace_count == 2,
        "child workspace should have been forgotten, but {} workspaces remain:\n{list}",
        workspace_count
    );
}
