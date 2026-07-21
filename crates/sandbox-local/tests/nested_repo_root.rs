//! Tests that `clone_repo` detects the repository root when the provided
//! path is nested inside a repo (important for jj repos, where
//! subdirectories carry no `.jj`/`.git` marker), and reports the detected
//! root in the tool response.

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

/// clone_repo with a path nested inside a jj repo should resolve to the
/// repo root, use Jujutsu workspaces, and note the detected root.
#[tokio::test]
async fn jj_clone_from_nested_dir() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();
    let nested = repo.join("src").join("deep");
    std::fs::create_dir_all(&nested).expect("create nested dir");

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "jj-nested-clone";
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": nested.to_str().expect("nested path to str") }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("Jujutsu workspaces"),
        "expected jj mode for nested path inside jj repo, got: {text}"
    );
    let canonical_root = repo.canonicalize().expect("canonicalize repo root");
    assert!(
        text.contains(&format!("rooted at `{}`", canonical_root.display())),
        "expected note about detected repo root, got: {text}"
    );

    // Files should be read relative to the repo root, not the nested dir.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.md" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("hello"), "got: {text}");
}

/// clone_repo with a path nested inside a plain git repo should resolve to
/// the repo root, use Git worktrees, and note the detected root.
#[tokio::test]
async fn git_clone_from_nested_dir() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = git_init_with_file("README.md", "hello\n");
    let repo = tmp.path();
    let nested = repo.join("src").join("deep");
    std::fs::create_dir_all(&nested).expect("create nested dir");

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "git-nested-clone";
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": nested.to_str().expect("nested path to str") }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("Git worktrees"),
        "expected git mode for nested path inside git repo, got: {text}"
    );
    let canonical_root = repo.canonicalize().expect("canonicalize repo root");
    assert!(
        text.contains(&format!("rooted at `{}`", canonical_root.display())),
        "expected note about detected repo root, got: {text}"
    );

    // Files should be read relative to the repo root, not the nested dir.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "README.md" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("hello"), "got: {text}");
}

/// clone_repo with the repo root itself should not include a root note.
#[tokio::test]
async fn no_note_when_path_is_root() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let text = invoke(
        &server_url,
        &callback_url,
        "jj-root-clone",
        "clone_repo",
        serde_json::json!({ "repo": repo.to_str().expect("repo path to str") }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("Repository initialized (using Jujutsu workspaces)."),
        "got: {text}"
    );
    assert!(
        !text.contains("rooted at"),
        "unexpected root note for root path, got: {text}"
    );
}
