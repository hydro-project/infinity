//! Tests that git commands inside a jj-colocated sandbox don't resolve
//! the outer repo as the git root.
//!
//! When sandboxes live under `{repo}/.infinity/.sandboxes/`, git would
//! normally walk up and find the outer `.git`. We set `GIT_CEILING_DIRECTORIES`
//! to prevent this.

mod common;

use common::{invoke, jj_init_with_file, start_test_server};
use rap_client::callback_server::start_callback_channel;

#[tokio::test]
async fn git_does_not_resolve_outer_repo() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "colocated-test";
    let repo_str = repo.to_str().expect("repo path to str");

    // Clone the repo (creates sandbox under {repo}/.infinity/.sandboxes/).
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

    // Run `git rev-parse --show-toplevel` inside the sandbox. Without
    // GIT_CEILING_DIRECTORIES, git would walk up and report the outer repo
    // as its working tree. Depending on the jj version, the correct outcomes
    // are:
    // * older jj: the workspace has no `.git`, so git finds no repo (128);
    // * newer jj: `jj workspace add` in a colocated repo also creates a git
    //   worktree, so git reports the sandbox itself as the toplevel.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "git rev-parse --show-toplevel" }),
        &mut rx,
        None,
    )
    .await;
    if text.contains("exit code: 128") {
        return;
    }

    assert!(
        text.contains("exit code: 0"),
        "expected git to either find no repo (exit code 128) or the sandbox worktree, got: {text}"
    );
    let canonical_repo = repo
        .canonicalize()
        .expect("canonicalize repo path")
        .join(".infinity")
        .join(".sandboxes");
    let toplevel = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('/'))
        .unwrap_or_else(|| panic!("expected an absolute toplevel path, got: {text}"));
    let toplevel = std::path::Path::new(toplevel)
        .canonicalize()
        .expect("canonicalize git toplevel");
    assert!(
        toplevel.starts_with(&canonical_repo),
        "git resolved a working tree outside the sandbox ({}), expected one under {}",
        toplevel.display(),
        canonical_repo.display()
    );
}
