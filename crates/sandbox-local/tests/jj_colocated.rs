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

    // Run `git rev-parse --git-dir` inside the sandbox.
    // Without GIT_CEILING_DIRECTORIES this would succeed and print the
    // outer repo's .git path. With the fix it should fail (exit code 128).
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "git rev-parse --git-dir" }),
        &mut rx,
        None,
    )
    .await;
    assert!(
        text.contains("exit code: 128"),
        "expected git to fail with exit code 128 (no repo found), got: {text}"
    );
}
