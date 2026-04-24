//! Test: what happens when a user externally moves a sandbox's bookmark.
//!
//! The sandbox tracks its working copy via jj's internal change_id, not the
//! bookmark. On every modifying operation, `push_sandbox` calls
//! `jj bookmark set sandbox-{group_id}` which silently overwrites any external
//! bookmark change. This test verifies that behavior and documents it.

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

/// Helper: get the commit_id that a bookmark points to.
fn bookmark_commit(repo: &Path, bookmark: &str) -> String {
    let output = jj_cmd(repo)
        .args(["log", "--no-graph", "-r", bookmark, "-T", "commit_id"])
        .output()
        .expect("run jj log");
    assert!(output.status.success(), "bookmark lookup failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Moving a bookmark externally between sandbox operations is silently
/// overwritten by the next modifying operation (edit_file, create_file, etc.).
#[tokio::test]
async fn external_bookmark_move_is_overwritten_by_next_edit() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group = "bm-move-test";
    let repo_str = repo.to_str().expect("repo path to str");
    let bookmark = format!("sandbox-{group}");

    // 1. Clone repo and make an initial edit so the bookmark has content
    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "README.md",
            "old_str": "hello\n",
            "new_str": "hello world\n"
        }),
        &mut rx,
        None,
    )
    .await;

    // Record the bookmark's commit after the edit
    let commit_after_edit = bookmark_commit(repo, &bookmark);
    assert!(!commit_after_edit.is_empty());

    // 2. Externally move the bookmark to a different revision (the initial commit)
    let output = jj_cmd(repo)
        .args(["bookmark", "set", &bookmark, "-r", "@-", "-B"])
        .output()
        .expect("move bookmark externally");
    assert!(
        output.status.success(),
        "external bookmark move failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let commit_after_external_move = bookmark_commit(repo, &bookmark);
    assert_ne!(
        commit_after_edit, commit_after_external_move,
        "bookmark should have moved to a different commit"
    );

    // 3. Make another edit through the sandbox
    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "README.md",
            "old_str": "hello world\n",
            "new_str": "hello world updated\n"
        }),
        &mut rx,
        None,
    )
    .await;
    // The tool result should contain a user-facing warning about the external move
    insta::assert_snapshot!(text, @r#"
    Warning: bookmark 'sandbox-bm-move-test' was moved externally; overwriting with sandbox working copy.
    Replaced text in README.md
    "#);

    // 4. The bookmark should now point back to the sandbox's working copy,
    //    NOT the externally-moved revision. The external move was silently lost.
    let commit_after_second_edit = bookmark_commit(repo, &bookmark);
    assert_ne!(
        commit_after_second_edit, commit_after_external_move,
        "bookmark should have been overwritten back to the sandbox's working copy"
    );

    // 5. Verify the sandbox still has both edits
    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "README.md" }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("hello world updated"),
        "sandbox should still have the latest edit, got: {text}"
    );
}

/// Moving a bookmark externally does NOT affect read-only operations (read_file)
/// since those don't call push_sandbox. The sandbox workspace still sees its own
/// working copy regardless of where the bookmark points.
#[tokio::test]
async fn external_bookmark_move_does_not_affect_reads() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "original\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group = "bm-read-test";
    let repo_str = repo.to_str().expect("repo path to str");
    let bookmark = format!("sandbox-{group}");

    // 1. Clone and edit
    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "README.md",
            "old_str": "original\n",
            "new_str": "modified\n"
        }),
        &mut rx,
        None,
    )
    .await;

    // 2. Externally move bookmark away
    let output = jj_cmd(repo)
        .args(["bookmark", "set", &bookmark, "-r", "@-", "-B"])
        .output()
        .expect("move bookmark externally");
    assert!(output.status.success());

    // 3. Read file — should still see the sandbox's working copy content
    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "README.md" }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("modified"),
        "read should see sandbox working copy, not the moved bookmark target. got: {text}"
    );
}

/// Deleting a bookmark externally doesn't crash the sandbox — the next
/// modifying operation recreates it.
#[tokio::test]
async fn external_bookmark_delete_is_recovered() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group = "bm-delete-test";
    let repo_str = repo.to_str().expect("repo path to str");
    let bookmark = format!("sandbox-{group}");

    // 1. Clone and edit
    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "README.md",
            "old_str": "hello\n",
            "new_str": "hello world\n"
        }),
        &mut rx,
        None,
    )
    .await;

    // 2. Delete the bookmark externally
    let output = jj_cmd(repo)
        .args(["bookmark", "delete", &bookmark])
        .output()
        .expect("delete bookmark externally");
    assert!(output.status.success(), "external bookmark delete failed");

    // 3. Make another edit — should not crash, and should recreate the bookmark
    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "README.md",
            "old_str": "hello world\n",
            "new_str": "hello world recovered\n"
        }),
        &mut rx,
        None,
    )
    .await;
    // No "moved externally" warning when bookmark was deleted (not moved)
    insta::assert_snapshot!(text, @"Replaced text in README.md");

    // 4. Verify bookmark was recreated
    let output = jj_cmd(repo)
        .args(["log", "--no-graph", "-r", &bookmark, "-T", "description"])
        .output()
        .expect("check bookmark exists");
    assert!(
        output.status.success(),
        "bookmark should have been recreated. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 5. Verify content is correct
    let text = invoke(
        &server_url,
        &callback_url,
        group,
        "read_file",
        serde_json::json!({ "path": "README.md" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("hello world recovered"), "got: {text}");
}

/// Moving a bookmark externally does not affect the diff view in practice,
/// since push_sandbox always restores the bookmark before push_diff_view runs.
/// This test verifies that the diff view still works correctly after an
/// external bookmark move.
#[tokio::test]
async fn external_bookmark_move_affects_diff_view_temporarily() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group = "bm-diff-test";
    let repo_str = repo.to_str().expect("repo path to str");
    let bookmark = format!("sandbox-{group}");

    // 1. Clone and edit
    invoke(
        &server_url,
        &callback_url,
        group,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
        None,
    )
    .await;

    let (text, views) = invoke_collecting_views(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "README.md",
            "old_str": "hello\n",
            "new_str": "hello world\n"
        }),
        &mut rx,
        1,
    )
    .await;
    assert!(text.contains("Replaced"), "got: {text}");
    assert_eq!(views.len(), 1, "should get a diff view");

    // 2. Move bookmark externally
    let output = jj_cmd(repo)
        .args(["bookmark", "set", &bookmark, "-r", "@-", "-B"])
        .output()
        .expect("move bookmark externally");
    assert!(output.status.success());

    // 3. Make another edit — the diff view should still work (not crash)
    //    and after push_sandbox restores the bookmark, the diff should be correct.
    let (text, views) = invoke_collecting_views(
        &server_url,
        &callback_url,
        group,
        "edit_file",
        serde_json::json!({
            "path": "README.md",
            "old_str": "hello world\n",
            "new_str": "hello world v2\n"
        }),
        &mut rx,
        1,
    )
    .await;
    insta::assert_snapshot!(text, @r#"
    Warning: bookmark 'sandbox-bm-diff-test' was moved externally; overwriting with sandbox working copy.
    Replaced text in README.md
    "#);
    // The diff view should contain our changes (push_sandbox runs before push_diff_view)
    assert_eq!(views.len(), 1, "should get exactly one diff view");
    let files = views[0].content["files"]
        .as_array()
        .expect("files should be an array");
    insta::assert_json_snapshot!(files);
}
