//! A suspended JJ sandbox must resume its actual working-copy change, not the
//! sandbox bookmark or its original starting revision.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::{invoke, jj_init_with_file, start_test_server_with_shutdown};
use rap_client::callback_server::start_callback_channel;
use sandbox_core::types::RepoState;

fn jj_output(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run jj");
    assert!(
        output.status.success(),
        "jj {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("jj output is UTF-8")
        .trim()
        .to_owned()
}

/// Simulates server suspension by forgetting and deleting the actual `.tmp…`
/// workspace and deleting the visible bookmark. The second server must restore
/// the WIP change recorded in file metadata, even though neither of those
/// workspace/bookmark identities remains.
#[tokio::test]
async fn jj_sandbox_resumes_moved_working_copy_after_server_restart() {
    let _ = tracing_subscriber::fmt::try_init();

    let repo = jj_init_with_file("README.md", "base\n");
    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let group_id = "resume-moved-wip";
    let (server_url, shutdown, server) = start_test_server_with_shutdown(metadata_dir.path()).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let clone = invoke(
        &server_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": repo.path() }),
        &mut rx,
        None,
    )
    .await;
    assert!(clone.contains("Jujutsu workspaces"), "got: {clone}");

    // `jj split` writes a file and moves @ to a new empty WIP change. It is a
    // sandbox-local mutation, so no write-orig permission is required.
    let split = invoke(
        &server_url,
        &callback_url,
        group_id,
        "execute_command",
        serde_json::json!({
            "command": "printf 'resumed content\\n' > resumed.txt && jj split -m split-wip resumed.txt"
        }),
        &mut rx,
        None,
    )
    .await;
    assert!(split.contains("exit code: 0"), "got: {split}");

    let metadata_path = metadata_dir.path().join(format!("{group_id}.json"));
    let persisted: RepoState =
        serde_json::from_str(&fs::read_to_string(&metadata_path).expect("read persisted metadata"))
            .expect("parse persisted metadata");
    let resume_revision = persisted
        .resume_revision
        .expect("successful sandbox mutation must persist its WIP change ID");
    assert_ne!(
        resume_revision,
        match persisted.mode {
            sandbox_core::types::SandboxMode::Jj { starting_revision } => starting_revision,
            _ => unreachable!("test repo must use JJ mode"),
        },
        "split should have moved the working copy away from its starting revision"
    );

    // `sandbox_path` is deliberately not persisted yet, so identify the one
    // temp workspace created beneath this repo. Forget it, delete it, and drop
    // the visible bookmark: resume must rely only on `resume_revision`.
    let sandboxes_dir = repo.path().join(".infinity/.sandboxes");
    let sandbox_path = fs::read_dir(&sandboxes_dir)
        .expect("list sandboxes")
        .map(|entry| entry.expect("read sandbox entry").path())
        .find(|path| path.is_dir())
        .expect("sandbox workspace exists");
    jj_output(
        &sandbox_path,
        &["--ignore-working-copy", "workspace", "forget"],
    );
    fs::remove_dir_all(&sandbox_path).expect("remove suspended workspace directory");
    jj_output(repo.path(), &["bookmark", "delete", &persisted.bookmark]);

    // Stop and await server one before restoring. This drops its LocalBackend
    // and proves server two cannot reuse an in-memory cached workspace.
    shutdown.send(()).expect("shut down initial server");
    server.await.expect("initial server exits cleanly");

    // A fresh backend has no cache. Reading the file materializes a new .tmp
    // workspace, which must check out the persisted WIP change and its files.
    let (resumed_server, resumed_shutdown, resumed_server_task) =
        start_test_server_with_shutdown(metadata_dir.path()).await;
    let resumed = invoke(
        &resumed_server,
        &callback_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "resumed.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert!(resumed.contains("resumed content"), "got: {resumed}");

    let resumed_sandbox = fs::read_dir(&sandboxes_dir)
        .expect("list resumed sandboxes")
        .map(|entry| entry.expect("read resumed sandbox entry").path())
        .find(|path| path.is_dir())
        .expect("resumed sandbox workspace exists");
    assert_ne!(resumed_sandbox, sandbox_path, "must create a new workspace");
    assert_eq!(
        jj_output(
            &resumed_sandbox,
            &[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "change_id ++ '/' ++ change_offset",
            ],
        ),
        resume_revision,
        "the new workspace must resume at the moved WIP change ID"
    );

    resumed_shutdown.send(()).expect("shut down resumed server");
    resumed_server_task
        .await
        .expect("resumed server exits cleanly");
}
