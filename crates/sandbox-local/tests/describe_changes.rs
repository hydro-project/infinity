//! Tests for `describe_overall_changes` commit authorship.
//!
//! Four variants: jj and git, each with and without a repo-level user configured.
//! Both currently produce "RAP Sandbox" as the author. Once the sandbox
//! forwards the repo's configured identity, the `with_repo_user` snapshots
//! will diverge to show "Test User".

mod common;

use common::{invoke, start_test_server};
use rap_client::callback_server::start_callback_channel;

use std::path::Path;

fn redact_jj_log(text: &str) -> String {
    use regex::Regex;
    let text = Regex::new(r"Commit ID: [0-9a-f]+")
        .unwrap()
        .replace_all(text, "Commit ID: [COMMIT_ID]");
    let text = Regex::new(r"Change ID: [a-z]+")
        .unwrap()
        .replace_all(&text, "Change ID: [CHANGE_ID]");
    Regex::new(r"\(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\)")
        .unwrap()
        .replace_all(&text, "([TIMESTAMP])")
        .to_string()
}

/// Create a colocated jj+git repo in a new temp directory, returning the `TempDir` handle.
fn jj_init() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init"])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
    let mut cmd = std::process::Command::new("jj");
    assert!(
        cmd.args(["git", "init", "--colocate"])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
    tmp
}

/// Run `jj` against an existing repo with sandboxed config.
fn jj_cmd(repo: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("jj");
    cmd.arg("-R").arg(repo).current_dir(repo);
    cmd
}

/// Clone repo, describe changes, return the redacted jj log of the sandbox bookmark.
async fn describe_and_read_log(repo: &Path) -> String {
    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel().await.unwrap();

    let group_id = "test-thread";
    let repo_str = repo.to_str().unwrap();

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    let text = invoke(&server_url, &callback_url, group_id, "describe_overall_changes",
        serde_json::json!({ "message": "feat: add widget support\n\nAdded the new widget module." }),
        &mut rx).await;
    assert_eq!(text, "Edits described.");

    let output = jj_cmd(repo)
        .args([
            "log",
            "--no-graph",
            "-r",
            &format!("sandbox-{group_id}"),
            "-T",
            "builtin_log_detailed",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "jj log failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    redact_jj_log(&String::from_utf8(output.stdout).unwrap())
}

#[tokio::test]
async fn jj_describe_with_repo_user() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init();
    let repo = tmp.path();

    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(
        repo.join(".jj/repo/config.toml"),
        "user.name = \"Test User\"\nuser.email = \"test@example.com\"\n",
    )
    .unwrap();

    insta::assert_snapshot!(describe_and_read_log(repo).await);
}

#[tokio::test]
async fn jj_describe_without_user() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init();
    let repo = tmp.path();

    std::fs::write(repo.join("README.md"), "hello\n").unwrap();

    insta::assert_snapshot!(describe_and_read_log(repo).await);
}

// ── Git-only variants ──

/// Initialise a plain git repo (no jj) with one commit in a new temp directory,
/// returning the `TempDir` handle.
/// The global config provides the default sandbox identity for sandbox operations,
/// but no repo-level user is configured.
fn git_init() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();

    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main"])
                .args(["-c", "user.name=init"])
                .args(["-c", "user.email=init@init"])
                .args(args)
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
    };

    git(&["init"]);
    std::fs::write(path.join("README.md"), "hello\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);
    tmp
}

fn redact_git_log(text: &str) -> String {
    use regex::Regex;
    let text = Regex::new(r"(?m)^commit [0-9a-f]+")
        .unwrap()
        .replace_all(text, "commit [COMMIT_ID]");
    Regex::new(r"(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun) \w+ \d+ \d{2}:\d{2}:\d{2} \d{4} [+-]\d{4}")
        .unwrap()
        .replace_all(&text, "[TIMESTAMP]")
        .to_string()
}

/// Clone a git repo, describe changes, return the redacted git log of the sandbox branch.
async fn git_describe_and_read_log(repo: &Path) -> String {
    // Prevent git from using a globally-configured user.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    }

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
    let (callback_url, mut rx) = start_callback_channel().await.unwrap();

    let group_id = "test-thread";
    let repo_str = repo.to_str().unwrap();

    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo_str }),
        &mut rx,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    let text = invoke(&server_url, &callback_url, group_id, "describe_overall_changes",
        serde_json::json!({ "message": "feat: add widget support\n\nAdded the new widget module." }),
        &mut rx).await;
    assert_eq!(text, "Edits described.");

    let output = std::process::Command::new("git")
        .args(["log", "-1", "sandbox-test-thread", "--pretty=fuller"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git log failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    redact_git_log(&String::from_utf8(output.stdout).unwrap())
}

#[tokio::test]
async fn git_describe_with_repo_user() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = git_init();
    let repo = tmp.path();
    // Repo-level config overrides the global fallback.
    assert!(
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );

    insta::assert_snapshot!(git_describe_and_read_log(repo).await);
}

#[tokio::test]
async fn git_describe_without_user() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = git_init();
    let repo = tmp.path();

    insta::assert_snapshot!(git_describe_and_read_log(repo).await);
}
