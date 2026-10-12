//! `create_file` and `edit_file` must never write outside the sandbox, even
//! when the repository contains symlinks (committed by an earlier command)
//! that point outside it, or when the model passes absolute / `..` paths.
//!
//! Reads are intentionally unrestricted (matching `execute_command`), so
//! `read_file` is not covered here.

#![cfg(unix)]

mod common;

use common::{invoke, start_test_server};
use rap_client::callback_server::start_callback_channel;

use std::path::Path;
use std::process::Command;

/// Layout:
///
/// ```text
/// <tmp>/
///   outside/
///     target.txt          "secret\n"
///   repo/                 (colocated jj+git, one commit)
///     README.md
///     link-dir   -> <tmp>/outside
///     link-file  -> <tmp>/outside/target.txt
///     dangling   -> <tmp>/outside/not-yet-there
///     rel-link   -> ../outside            (relative target)
///     inner      -> real                  (legit, stays inside)
///     real/
///       f.txt
/// ```
fn setup() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let outside = tmp.path().join("outside");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    std::fs::write(outside.join("target.txt"), "secret\n").expect("write target");

    assert!(
        Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init"])
            .current_dir(&repo)
            .status()
            .expect("run git init")
            .success()
    );
    assert!(
        Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(&repo)
            .status()
            .expect("run jj git init")
            .success()
    );

    std::fs::write(repo.join("README.md"), "hello\n").expect("write README");
    std::fs::create_dir_all(repo.join("real")).expect("mkdir real");
    std::fs::write(repo.join("real/f.txt"), "inside\n").expect("write f.txt");
    std::os::unix::fs::symlink(&outside, repo.join("link-dir")).expect("symlink link-dir");
    std::os::unix::fs::symlink(outside.join("target.txt"), repo.join("link-file"))
        .expect("symlink link-file");
    std::os::unix::fs::symlink(outside.join("not-yet-there"), repo.join("dangling"))
        .expect("symlink dangling");
    std::os::unix::fs::symlink("../outside", repo.join("rel-link")).expect("symlink rel-link");
    std::os::unix::fs::symlink("real", repo.join("inner")).expect("symlink inner");

    assert!(
        Command::new("jj")
            .args(["commit", "-m", "initial with symlinks"])
            .current_dir(&repo)
            .status()
            .expect("run jj commit")
            .success()
    );
    tmp
}

struct Harness {
    server_url: String,
    callback_url: String,
    rx: tokio::sync::mpsc::UnboundedReceiver<rap_protocol::RapCallback>,
    group: &'static str,
}

impl Harness {
    async fn new(repo: &Path, group: &'static str) -> Self {
        Self::new_with(repo, group, "clone_repo", "Repository initialized").await
    }

    async fn new_direct(repo: &Path, group: &'static str) -> Self {
        Self::new_with(repo, group, "open_sandbox_direct", "Direct mode").await
    }

    async fn new_with(repo: &Path, group: &'static str, init_tool: &str, expect: &str) -> Self {
        let server_url = start_test_server(&repo.join(".test-metadata")).await;
        let (callback_url, mut rx) = start_callback_channel()
            .await
            .expect("start callback channel");
        let repo_str = repo.to_str().expect("repo path to str");
        let text = invoke(
            &server_url,
            &callback_url,
            group,
            init_tool,
            serde_json::json!({ "repo": repo_str }),
            &mut rx,
            None,
        )
        .await;
        assert!(text.contains(expect), "got: {text}");
        Self {
            server_url,
            callback_url,
            rx,
            group,
        }
    }

    /// Fire an invocation without waiting for its result.
    async fn fire(&self, tool: &str, args: serde_json::Value) {
        let invocation = rap_protocol::RapInvocation {
            operation: tool.to_owned(),
            arguments: args,
            id: format!("direct-{tool}-{}", rand_suffix()).into(),
            call_id: None,
            callback_url: self.callback_url.clone(),
            group_id: self.group.into(),
            user_id: None,
            thread_ancestors: None,
        };
        reqwest::Client::new()
            .post(format!("{}/invoke", self.server_url))
            .json(&invocation)
            .send()
            .await
            .expect("send invoke request");
    }

    /// Wait for the next callback, skipping ViewUpdates.
    async fn next_callback(&mut self) -> rap_protocol::RapCallback {
        loop {
            let cb = tokio::time::timeout(std::time::Duration::from_secs(10), self.rx.recv())
                .await
                .expect("timed out waiting for callback")
                .expect("channel closed");
            if !matches!(cb, rap_protocol::RapCallback::ViewUpdate(_)) {
                return cb;
            }
        }
    }

    /// Expect a Direct-mode approval prompt and answer it by picking the
    /// choice whose label equals `choice_label`. Returns the prompt text.
    async fn answer_prompt(&mut self, choice_label: &str) -> String {
        let cb = self.next_callback().await;
        let rap_protocol::RapCallback::UserChoice(uc) = cb else {
            panic!("expected UserChoice, got: {cb:?}");
        };
        let selected = uc
            .choices
            .iter()
            .position(|c| c == choice_label)
            .unwrap_or_else(|| panic!("no choice {choice_label:?} in {:?}", uc.choices));
        // The test server never learns its own base URL (that happens via the
        // toolset handshake in production), so `uc.response_url` falls back to
        // `http://localhost/...`. Post to the known server URL instead.
        reqwest::Client::new()
            .post(format!("{}/user_choice_response", self.server_url))
            .json(&serde_json::json!({ "id": uc.id, "selected": selected }))
            .send()
            .await
            .expect("send user_choice_response");
        uc.prompt
    }

    /// Expect the final ToolResult.
    async fn result(&mut self) -> String {
        let cb = self.next_callback().await;
        let rap_protocol::RapCallback::ToolResult(r) = cb else {
            panic!("expected ToolResult, got: {cb:?}");
        };
        r.text.unwrap_or_default()
    }
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

impl Harness {
    async fn call(&mut self, tool: &str, args: serde_json::Value) -> String {
        invoke(
            &self.server_url,
            &self.callback_url,
            self.group,
            tool,
            args,
            &mut self.rx,
            None,
        )
        .await
    }

    async fn create(&mut self, path: &str) -> String {
        self.call(
            "create_file",
            serde_json::json!({ "path": path, "content": "pwned\n" }),
        )
        .await
    }

    async fn edit(&mut self, path: &str) -> String {
        self.call(
            "edit_file",
            serde_json::json!({ "path": path, "old_str": "secret", "new_str": "pwned" }),
        )
        .await
    }
}

fn assert_rejected(text: &str) {
    assert!(
        text.contains("outside the sandbox") || text.contains("failed to resolve"),
        "expected rejection, got: {text}"
    );
}

#[tokio::test]
async fn create_file_cannot_escape_sandbox() {
    let _ = tracing_subscriber::fmt::try_init();
    let tmp = setup();
    let outside = tmp.path().join("outside");
    let repo = tmp.path().join("repo");
    let mut h = Harness::new(&repo, "create-escape").await;

    // Through a committed symlinked directory pointing outside.
    assert_rejected(&h.create("link-dir/evil.txt").await);
    assert_rejected(&h.create("link-dir/a/b/evil.txt").await);
    assert!(!outside.join("evil.txt").exists());
    assert!(!outside.join("a").exists());

    // Through a relative symlink that climbs out.
    assert_rejected(&h.create("rel-link/evil2.txt").await);
    assert!(!outside.join("evil2.txt").exists());

    // Onto a dangling symlink: `exists()` says false, but O_EXCL / lstat
    // must still refuse to create the target through it.
    let text = h.create("dangling").await;
    assert!(
        text.contains("outside the sandbox")
            || text.contains("failed to resolve")
            || text.contains("already exists"),
        "got: {text}"
    );
    assert!(!outside.join("not-yet-there").exists());

    // Absolute path outside.
    let abs = outside.join("abs.txt");
    assert_rejected(&h.create(abs.to_str().expect("utf8")).await);
    assert!(!abs.exists());

    // `..` escapes, including after a nonexistent component.
    assert_rejected(&h.create("../outside/dotdot.txt").await);
    assert_rejected(&h.create("nope/../../outside/dotdot2.txt").await);
    assert!(!outside.join("dotdot.txt").exists());
    assert!(!outside.join("dotdot2.txt").exists());

    // Nothing leaked: `outside/` still only has the original file.
    let entries: Vec<_> = std::fs::read_dir(&outside)
        .expect("read outside")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("target.txt")]);

    // Legit writes still work: plain, nested, and through an *inside* symlink.
    let text = h.create("new.txt").await;
    assert!(text.contains("Created file"), "got: {text}");
    let text = h.create("deep/er/new.txt").await;
    assert!(text.contains("Created file"), "got: {text}");
    let text = h.create("inner/via-link.txt").await;
    assert!(text.contains("Created file"), "got: {text}");
    let text = h
        .call(
            "read_file",
            serde_json::json!({ "path": "real/via-link.txt" }),
        )
        .await;
    assert!(text.contains("pwned"), "got: {text}");

    // And the existing-file guard still fires.
    let text = h.create("README.md").await;
    assert!(text.contains("already exists"), "got: {text}");
}

#[tokio::test]
async fn edit_file_cannot_escape_sandbox() {
    let _ = tracing_subscriber::fmt::try_init();
    let tmp = setup();
    let outside = tmp.path().join("outside");
    let repo = tmp.path().join("repo");
    let mut h = Harness::new(&repo, "edit-escape").await;

    // Through a committed symlink to a file outside.
    assert_rejected(&h.edit("link-file").await);
    // Through a symlinked directory.
    assert_rejected(&h.edit("link-dir/target.txt").await);
    assert_rejected(&h.edit("rel-link/target.txt").await);
    // Absolute path.
    let abs = outside.join("target.txt");
    assert_rejected(&h.edit(abs.to_str().expect("utf8")).await);
    // `..`
    assert_rejected(&h.edit("../outside/target.txt").await);
    assert_rejected(&h.edit("nope/../../outside/target.txt").await);

    assert_eq!(
        std::fs::read_to_string(outside.join("target.txt")).expect("read target"),
        "secret\n",
        "file outside the sandbox must be untouched"
    );

    // Legit edit through an inside symlink still works.
    let text = h
        .call(
            "edit_file",
            serde_json::json!({ "path": "inner/f.txt", "old_str": "inside", "new_str": "edited" }),
        )
        .await;
    assert!(text.contains("Replaced"), "got: {text}");
    let text = h
        .call("read_file", serde_json::json!({ "path": "real/f.txt" }))
        .await;
    assert!(text.contains("edited"), "got: {text}");
}

/// In Direct mode the approval prompt must show the *resolved* repo-relative
/// target, so a symlink or `..` in the argument cannot disguise what the user
/// is approving. Writes that resolve outside the repo are rejected before any
/// prompt is shown.
#[tokio::test]
async fn direct_mode_prompt_shows_resolved_target() {
    let _ = tracing_subscriber::fmt::try_init();
    let tmp = setup();
    let outside = tmp.path().join("outside");
    let repo = tmp.path().join("repo");
    let mut h = Harness::new_direct(&repo, "direct-resolved").await;

    // create_file through an inside symlink: prompt shows `real/new.txt`,
    // not `inner/new.txt`, and the file lands at the real location.
    h.fire(
        "create_file",
        serde_json::json!({ "path": "inner/new.txt", "content": "via link\n" }),
    )
    .await;
    let prompt = h.answer_prompt("Yes once").await;
    assert!(
        prompt.contains("real/new.txt") && !prompt.contains("inner/"),
        "prompt should show resolved target, got: {prompt:?}"
    );
    let text = h.result().await;
    assert!(text.contains("Created file"), "got: {text}");
    assert_eq!(
        std::fs::read_to_string(repo.join("real/new.txt")).expect("read"),
        "via link\n"
    );

    // edit_file with `..` that stays inside: prompt shows the normalized path.
    h.fire(
        "edit_file",
        serde_json::json!({ "path": "inner/../real/f.txt", "old_str": "inside", "new_str": "edited" }),
    )
    .await;
    let prompt = h.answer_prompt("Yes once").await;
    assert!(
        prompt.contains("real/f.txt") && !prompt.contains(".."),
        "prompt should show normalized target, got: {prompt:?}"
    );
    let text = h.result().await;
    assert!(text.contains("Replaced"), "got: {text}");
    assert_eq!(
        std::fs::read_to_string(repo.join("real/f.txt")).expect("read"),
        "edited\n"
    );

    // Denying the prompt leaves the file untouched.
    h.fire(
        "edit_file",
        serde_json::json!({ "path": "real/f.txt", "old_str": "edited", "new_str": "denied" }),
    )
    .await;
    h.answer_prompt("No").await;
    let text = h.result().await;
    assert!(text.contains("denied by user"), "got: {text}");
    assert_eq!(
        std::fs::read_to_string(repo.join("real/f.txt")).expect("read"),
        "edited\n"
    );

    // Writes resolving outside the repo are rejected with NO prompt: the very
    // next callback is the ToolResult, not a UserChoice.
    h.fire(
        "create_file",
        serde_json::json!({ "path": "link-dir/evil.txt", "content": "pwned\n" }),
    )
    .await;
    assert_rejected(&h.result().await);
    assert!(!outside.join("evil.txt").exists());

    h.fire(
        "edit_file",
        serde_json::json!({ "path": "link-file", "old_str": "secret", "new_str": "pwned" }),
    )
    .await;
    assert_rejected(&h.result().await);
    assert_eq!(
        std::fs::read_to_string(outside.join("target.txt")).expect("read"),
        "secret\n"
    );
}
