//! Tests for the `/migrate` and `/migrate/import` endpoints.
//!
//! Sets up a source jj repo, makes edits via the sandbox server, then migrates
//! to a destination server backed by a separate clone of the same repo.

mod common;

use common::{invoke, jj_init_with_file};
use rap_client::callback_server::start_callback_channel;

use std::path::Path;
use std::process::Command;

/// Clone a jj repo via git clone + jj git init --colocate.
fn jj_clone(source: &Path) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dest = tmp.path();
    // Remove the empty dir so git clone can create it
    std::fs::remove_dir(dest).expect("remove empty dest dir");
    assert!(
        Command::new("git")
            .args([
                "clone",
                source.to_str().expect("source path"),
                dest.to_str().expect("dest path")
            ])
            .status()
            .expect("run git clone")
            .success()
    );
    assert!(
        Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(dest)
            .status()
            .expect("run jj git init on clone")
            .success()
    );
    tmp
}

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

/// Clone a plain git repo.
fn git_clone(source: &Path) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dest = tmp.path();
    std::fs::remove_dir(dest).expect("remove empty dest dir");
    assert!(
        Command::new("git")
            .args([
                "clone",
                source.to_str().expect("source path"),
                dest.to_str().expect("dest path")
            ])
            .status()
            .expect("run git clone")
            .success()
    );
    tmp
}

/// Start a test server with `needs_migration: true` and an optional repo root.
async fn start_migration_server(
    metadata_dir: &Path,
    repo_root: Option<std::path::PathBuf>,
) -> String {
    std::fs::create_dir_all(metadata_dir).expect("create metadata dir");

    unsafe {
        std::env::set_var(
            "XDG_CONFIG_HOME",
            std::env::temp_dir().join("xdg-config-home"),
        );
    }

    let backend = sandbox_local::backend::LocalBackend::new(false);
    let metadata = sandbox_local::metadata::FileMetadataStore::new(metadata_dir.to_path_buf());
    let (app, _tracker) = sandbox_core::server::build_router(
        backend,
        metadata,
        sandbox_core::callback::PlainCallbackClient::new(),
        true,
        repo_root,
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let port = listener.local_addr().expect("get local addr").port();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve test server") });

    format!("http://127.0.0.1:{port}")
}

/// Verify the toolset manifest advertises `needsMigration: true`.
#[tokio::test]
async fn manifest_advertises_needs_migration() {
    let _ = tracing_subscriber::fmt::try_init();

    let metadata_dir = tempfile::tempdir().expect("create metadata dir");
    let server_url = start_migration_server(metadata_dir.path(), None).await;

    let resp: serde_json::Value = reqwest::Client::new()
        .get(format!("{server_url}/.well-known/rap-toolset"))
        .send()
        .await
        .expect("fetch manifest")
        .json()
        .await
        .expect("parse manifest");

    assert_eq!(resp["needsMigration"], true);
}

/// Full migration flow: create edits on source, migrate to destination, verify
/// the bookmark and file changes arrived.
#[tokio::test]
async fn migrate_jj_repo_to_destination() {
    let _ = tracing_subscriber::fmt::try_init();

    // Set up source repo with a file
    let source_repo = jj_init_with_file("README.md", "hello world\n");
    let source_path = source_repo.path();

    // Clone it to create the destination repo
    let dest_repo = jj_clone(source_path);
    let dest_path = dest_repo.path();

    // Start source server
    let source_meta = tempfile::tempdir().expect("source metadata dir");
    let source_url = start_migration_server(source_meta.path(), None).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    // Start destination server
    let dest_meta = tempfile::tempdir().expect("dest metadata dir");
    let dest_url = start_migration_server(dest_meta.path(), Some(dest_path.to_path_buf())).await;

    // Clone repo on source server
    let group_id = "migrate-thread";
    let text = invoke(
        &source_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": source_path.to_str().expect("path") }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    // Create a file via the sandbox
    let text = invoke(
        &source_url,
        &callback_url,
        group_id,
        "create_file",
        serde_json::json!({ "path": "new_file.txt", "content": "migrated content\n" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Created file"), "got: {text}");

    // Describe changes so the bookmark has a meaningful commit
    let text = invoke(
        &source_url,
        &callback_url,
        group_id,
        "describe_overall_changes",
        serde_json::json!({ "message": "feat: add new_file.txt" }),
        &mut rx,
        None,
    )
    .await;
    assert_eq!(text, "Edits described.");

    // Trigger migration from source to destination
    let resp = reqwest::Client::new()
        .post(format!("{source_url}/migrate"))
        .json(&serde_json::json!({
            "session_id": "migrate-thread",
            "destination_url": dest_url,
        }))
        .send()
        .await
        .expect("send migrate request");
    assert!(
        resp.status().is_success(),
        "migrate failed: {} {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Verify: the bookmark should exist in the destination repo
    let bookmark = format!("sandbox-{group_id}");
    let output = Command::new("jj")
        .args(["log", "--no-graph", "-r", &bookmark, "-T", "description"])
        .current_dir(dest_path)
        .output()
        .expect("run jj log on dest");
    assert!(
        output.status.success(),
        "bookmark not found in dest: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let description = String::from_utf8_lossy(&output.stdout);
    assert!(
        description.contains("feat: add new_file.txt"),
        "expected commit message in dest, got: {description}"
    );

    // Verify: the destination server can read the migrated file via its sandbox
    let (dest_cb_url, mut dest_rx) = start_callback_channel()
        .await
        .expect("start dest callback channel");

    // The import wrote metadata, so the dest server should know about the sandbox.
    // We need to clone_repo on the dest server first to set up the sandbox workspace.
    // Actually, the metadata was written by the import, so let's read the file directly.
    let text = invoke(
        &dest_url,
        &dest_cb_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "new_file.txt" }),
        &mut dest_rx,
        None,
    )
    .await;
    assert!(
        text.contains("migrated content"),
        "expected migrated file content, got: {text}"
    );
}

/// Migration with multiple threads: two group_ids on the same repo should both
/// transfer successfully.
#[tokio::test]
async fn migrate_multiple_threads() {
    let _ = tracing_subscriber::fmt::try_init();

    let source_repo = jj_init_with_file("README.md", "base\n");
    let source_path = source_repo.path();
    let dest_repo = jj_clone(source_path);
    let dest_path = dest_repo.path();

    let source_meta = tempfile::tempdir().expect("source metadata dir");
    let source_url = start_migration_server(source_meta.path(), None).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let dest_meta = tempfile::tempdir().expect("dest metadata dir");
    let dest_url = start_migration_server(dest_meta.path(), Some(dest_path.to_path_buf())).await;

    // Set up two threads on the source
    for (group_id, filename) in [("thread-a", "a.txt"), ("thread-b", "b.txt")] {
        let text = invoke(
            &source_url,
            &callback_url,
            group_id,
            "clone_repo",
            serde_json::json!({ "repo": source_path.to_str().expect("path") }),
            &mut rx,
            Some(vec!["session-root".to_string()]),
        )
        .await;
        assert!(text.contains("Repository initialized"), "got: {text}");

        let text = invoke(
            &source_url,
            &callback_url,
            group_id,
            "create_file",
            serde_json::json!({ "path": filename, "content": format!("from {group_id}\n") }),
            &mut rx,
            None,
        )
        .await;
        assert!(text.contains("Created file"), "got: {text}");
    }

    // Migrate
    let resp = reqwest::Client::new()
        .post(format!("{source_url}/migrate"))
        .json(&serde_json::json!({
            "session_id": "session-root",
            "destination_url": dest_url,
        }))
        .send()
        .await
        .expect("send migrate request");
    assert!(resp.status().is_success(), "migrate failed");

    // Verify both bookmarks exist in destination
    for (group_id, filename, expected) in [
        ("thread-a", "a.txt", "from thread-a"),
        ("thread-b", "b.txt", "from thread-b"),
    ] {
        let bookmark = format!("sandbox-{group_id}");
        let output = Command::new("jj")
            .args(["log", "--no-graph", "-r", &bookmark, "-T", "description"])
            .current_dir(dest_path)
            .output()
            .expect("run jj log on dest");
        assert!(
            output.status.success(),
            "bookmark {bookmark} not found in dest: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Read the file via dest server
        let (dest_cb_url, mut dest_rx) = start_callback_channel()
            .await
            .expect("start dest callback channel");
        let text = invoke(
            &dest_url,
            &dest_cb_url,
            group_id,
            "read_file",
            serde_json::json!({ "path": filename }),
            &mut dest_rx,
            None,
        )
        .await;
        assert!(
            text.contains(expected),
            "expected '{expected}' in {filename}, got: {text}"
        );
    }
}

/// Migration should fail when a Direct mode sandbox exists.
#[tokio::test]
async fn migrate_rejects_direct_mode() {
    let _ = tracing_subscriber::fmt::try_init();

    let source_repo = jj_init_with_file("README.md", "hello\n");
    let source_path = source_repo.path();

    let source_meta = tempfile::tempdir().expect("source metadata dir");
    let source_url = start_migration_server(source_meta.path(), None).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    // Open in Direct mode
    let text = invoke(
        &source_url,
        &callback_url,
        "direct-thread",
        "open_sandbox_direct",
        serde_json::json!({ "repo": source_path.to_str().expect("path") }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Direct mode"), "got: {text}");

    // Migration should fail
    let resp = reqwest::Client::new()
        .post(format!("{source_url}/migrate"))
        .json(&serde_json::json!({
            "session_id": "direct-thread",
            "destination_url": "http://127.0.0.1:9999",
        }))
        .send()
        .await
        .expect("send migrate request");
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("Direct mode"),
        "expected Direct mode error, got: {body}"
    );
}

/// Migration with no sandboxes should return an error.
#[tokio::test]
async fn migrate_rejects_empty() {
    let _ = tracing_subscriber::fmt::try_init();

    let source_meta = tempfile::tempdir().expect("source metadata dir");
    let source_url = start_migration_server(source_meta.path(), None).await;

    let resp = reqwest::Client::new()
        .post(format!("{source_url}/migrate"))
        .json(&serde_json::json!({
            "session_id": "test-session",
            "destination_url": "http://127.0.0.1:9999",
        }))
        .send()
        .await
        .expect("send migrate request");
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("nothing to migrate"),
        "expected empty error, got: {body}"
    );
}

/// Full migration flow for a plain git (non-jj) repo using git worktrees.
#[tokio::test]
async fn migrate_git_worktree_repo() {
    let _ = tracing_subscriber::fmt::try_init();

    // Prevent git from using a globally-configured user.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    }

    let source_repo = git_init_with_file("README.md", "hello git\n");
    let source_path = source_repo.path();
    let dest_repo = git_clone(source_path);
    let dest_path = dest_repo.path();

    let source_meta = tempfile::tempdir().expect("source metadata dir");
    let source_url = start_migration_server(source_meta.path(), None).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let dest_meta = tempfile::tempdir().expect("dest metadata dir");
    let dest_url = start_migration_server(dest_meta.path(), Some(dest_path.to_path_buf())).await;

    // Clone repo on source
    let group_id = "git-migrate";
    let text = invoke(
        &source_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": source_path.to_str().expect("path") }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    // Create a file
    let text = invoke(
        &source_url,
        &callback_url,
        group_id,
        "create_file",
        serde_json::json!({ "path": "git_file.txt", "content": "git migration works\n" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Created file"), "got: {text}");

    // Migrate
    let resp = reqwest::Client::new()
        .post(format!("{source_url}/migrate"))
        .json(&serde_json::json!({
            "session_id": "git-migrate",
            "destination_url": dest_url,
        }))
        .send()
        .await
        .expect("send migrate request");
    assert!(
        resp.status().is_success(),
        "migrate failed: {} {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Verify bookmark exists in destination git repo
    let bookmark = format!("sandbox-{group_id}");
    let output = Command::new("git")
        .args(["log", "-1", "--format=%s", &bookmark])
        .current_dir(dest_path)
        .output()
        .expect("run git log on dest");
    assert!(
        output.status.success(),
        "branch not found in dest: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the file is readable via the dest server
    let (dest_cb_url, mut dest_rx) = start_callback_channel()
        .await
        .expect("start dest callback channel");
    let text = invoke(
        &dest_url,
        &dest_cb_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "git_file.txt" }),
        &mut dest_rx,
        None,
    )
    .await;
    assert!(
        text.contains("git migration works"),
        "expected migrated file content, got: {text}"
    );
}

/// Migration works when the destination is behind (missing parent commits of
/// the sandbox bookmark). The git bundle is self-contained and includes all
/// ancestor objects.
#[tokio::test]
async fn migrate_destination_behind() {
    let _ = tracing_subscriber::fmt::try_init();

    // Create source with initial commit
    let source_repo = jj_init_with_file("README.md", "v1\n");
    let source_path = source_repo.path();

    // Clone destination NOW — it only has the initial commit
    let dest_repo = jj_clone(source_path);
    let dest_path = dest_repo.path();

    // Add more commits to source that dest doesn't have
    std::fs::write(source_path.join("extra.txt"), "new stuff\n").expect("write extra.txt");
    assert!(
        Command::new("jj")
            .args(["commit", "-m", "second commit"])
            .current_dir(source_path)
            .status()
            .expect("run jj commit")
            .success()
    );

    // Start servers
    let source_meta = tempfile::tempdir().expect("source metadata dir");
    let source_url = start_migration_server(source_meta.path(), None).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let dest_meta = tempfile::tempdir().expect("dest metadata dir");
    let dest_url = start_migration_server(dest_meta.path(), Some(dest_path.to_path_buf())).await;

    // Clone repo on source — this bases the sandbox on the LATEST source state
    // (which includes "second commit" that dest doesn't have)
    let group_id = "behind-test";
    let text = invoke(
        &source_url,
        &callback_url,
        group_id,
        "clone_repo",
        serde_json::json!({ "repo": source_path.to_str().expect("path") }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Repository initialized"), "got: {text}");

    // Create a file on top of the new base
    let text = invoke(
        &source_url,
        &callback_url,
        group_id,
        "create_file",
        serde_json::json!({ "path": "sandbox.txt", "content": "on top of missing parent\n" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Created file"), "got: {text}");

    // Migrate — dest is missing the parent commits
    let resp = reqwest::Client::new()
        .post(format!("{source_url}/migrate"))
        .json(&serde_json::json!({
            "session_id": "behind-test",
            "destination_url": dest_url,
        }))
        .send()
        .await
        .expect("send migrate request");
    assert!(
        resp.status().is_success(),
        "migrate failed: {} {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Verify the file is readable on the destination
    let (dest_cb_url, mut dest_rx) = start_callback_channel()
        .await
        .expect("start dest callback channel");
    let text = invoke(
        &dest_url,
        &dest_cb_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "sandbox.txt" }),
        &mut dest_rx,
        None,
    )
    .await;
    assert!(
        text.contains("on top of missing parent"),
        "expected migrated content, got: {text}"
    );

    // Also verify the parent file made it
    let text = invoke(
        &dest_url,
        &dest_cb_url,
        group_id,
        "read_file",
        serde_json::json!({ "path": "extra.txt" }),
        &mut dest_rx,
        None,
    )
    .await;
    assert!(
        text.contains("new stuff"),
        "expected parent commit's file, got: {text}"
    );
}

/// Migration succeeds after a child thread's sandbox has been squashed into its
/// parent. Previously the squashed child's metadata was left behind, causing
/// `migrate` to try bundling a deleted bookmark.
#[tokio::test]
async fn migrate_after_squash_child() {
    let _ = tracing_subscriber::fmt::try_init();

    let source_repo = jj_init_with_file("README.md", "base\n");
    let source_path = source_repo.path();
    let dest_repo = jj_clone(source_path);
    let dest_path = dest_repo.path();

    let source_meta = tempfile::tempdir().expect("source metadata dir");
    let source_url = start_migration_server(source_meta.path(), None).await;
    let (callback_url, mut rx) = start_callback_channel()
        .await
        .expect("start callback channel");

    let dest_meta = tempfile::tempdir().expect("dest metadata dir");
    let dest_url = start_migration_server(dest_meta.path(), Some(dest_path.to_path_buf())).await;

    // Set up parent and child threads (both under session-root)
    let parent_id = "parent-thread";
    let child_id = "child-thread";
    for gid in [parent_id, child_id] {
        let text = invoke(
            &source_url,
            &callback_url,
            gid,
            "clone_repo",
            serde_json::json!({ "repo": source_path.to_str().expect("path") }),
            &mut rx,
            Some(vec!["session-root".to_string()]),
        )
        .await;
        assert!(text.contains("Repository initialized"), "got: {text}");
    }

    // Create a file in the child
    let text = invoke(
        &source_url,
        &callback_url,
        child_id,
        "create_file",
        serde_json::json!({ "path": "child.txt", "content": "from child\n" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Created file"), "got: {text}");

    // Squash child into parent — this deletes the child's bookmark
    let text = invoke(
        &source_url,
        &callback_url,
        parent_id,
        "squash_sandbox",
        serde_json::json!({ "from_thread_id": child_id }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Squashed"), "got: {text}");

    // Migrate — should succeed even though child was squashed
    let resp = reqwest::Client::new()
        .post(format!("{source_url}/migrate"))
        .json(&serde_json::json!({
            "session_id": "session-root",
            "destination_url": dest_url,
        }))
        .send()
        .await
        .expect("send migrate request");
    assert!(
        resp.status().is_success(),
        "migrate failed: {} {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Verify the parent's content (including squashed child file) arrived
    let (dest_cb_url, mut dest_rx) = start_callback_channel()
        .await
        .expect("start dest callback channel");
    let text = invoke(
        &dest_url,
        &dest_cb_url,
        parent_id,
        "read_file",
        serde_json::json!({ "path": "child.txt" }),
        &mut dest_rx,
        None,
    )
    .await;
    assert!(
        text.contains("from child"),
        "expected squashed child content, got: {text}"
    );
}
