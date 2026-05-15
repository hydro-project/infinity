//! Tests for `execute_command`.
//!
//! - Verifies that running a command preserves the commit description
//!   and snapshot-tests the ViewUpdate diff callback (jj mode, unsandboxed).
//! - Verifies that the sccache cache directory is writable from inside
//!   a platform sandbox (bwrap / sandbox-exec).

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
    let files = views[0].content["files"]
        .as_array()
        .expect("files should be an array");
    insta::assert_json_snapshot!(files);

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
}

/// Verify that TMPDIR inside a sandboxed command does not point inside the
/// sandbox working directory. If TMPDIR were inside the sandbox worktree,
/// temp files would be committed by jj snapshots and pollute the working copy.
///
/// TMPDIR is allowed to be inside the original repo (under .infinity/.sandboxes/)
/// since that path is not tracked by jj.
///
/// Only tests the platform-sandboxed path (bwrap/sandbox-exec) since the
/// unsandboxed path inherits the parent's TMPDIR without managing it.
#[tokio::test]
async fn sandboxed_tmpdir_is_not_inside_sandbox_workdir() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = common::start_test_server_sandboxed(&repo.join(".test-metadata"), true).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "tmpdir-test";
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

    // Print TMPDIR and PWD from inside the sandbox.
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": "echo TMPDIR=$TMPDIR; echo PWD=$PWD" }),
        &mut rx,
        None,
    )
    .await;

    // Parse the TMPDIR and PWD values from the output.
    let tmpdir_val = text
        .lines()
        .find_map(|l| l.strip_prefix("TMPDIR="))
        .expect("TMPDIR line not found in output");
    let pwd_val = text
        .lines()
        .find_map(|l| l.strip_prefix("PWD="))
        .expect("PWD line not found in output");

    let tmpdir = Path::new(tmpdir_val);
    let pwd = Path::new(pwd_val);

    // TMPDIR must not be inside the sandbox working dir (where jj snapshots).
    assert!(
        !tmpdir.starts_with(pwd),
        "TMPDIR ({tmpdir_val}) must not be inside the sandbox workdir ({pwd_val})"
    );
    // The sandbox working dir must not be inside TMPDIR either.
    assert!(
        !pwd.starts_with(tmpdir),
        "Sandbox dir ({pwd_val}) must not be inside TMPDIR ({tmpdir_val})"
    );
}

/// Verify that the sccache cache directory is writable from inside a
/// bwrap sandbox. This exercises the `extra_writable_paths` fix that
/// adds the sccache cache dir to the sandbox's writable mounts.
#[tokio::test]
async fn sandboxed_command_can_write_sccache_cache_dir() {
    let _ = tracing_subscriber::fmt::try_init();

    // Point SCCACHE_DIR at a temp dir so we don't touch the real cache.
    let sccache_tmp = tempfile::tempdir().expect("create sccache temp dir");
    unsafe {
        std::env::set_var("SCCACHE_DIR", sccache_tmp.path());
    }

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = common::start_test_server_sandboxed(&repo.join(".test-metadata"), true).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let group_id = "sccache-test";
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

    // Try to write a file inside the sccache cache directory.
    // Under bwrap this only succeeds if the dir is in the writable mounts.
    let sccache_dir = sccache_tmp.path().to_str().expect("sccache dir as str");
    let cmd = format!("touch {sccache_dir}/sandbox-write-test");
    let text = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({ "command": cmd }),
        &mut rx,
        None,
    )
    .await;
    insta::assert_snapshot!(text, @"Command completed with exit code 0");

    // Clean up env var.
    unsafe {
        std::env::remove_var("SCCACHE_DIR");
    }
}
