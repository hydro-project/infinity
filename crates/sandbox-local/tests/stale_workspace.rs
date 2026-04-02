//! Reproducer for stale working copy bug.
//!
//! When a parent thread modifies the jj repo (e.g. `describe_overall_changes`)
//! while a child thread has a command in flight, the child's `jj squash --into @-`
//! fails because its workspace is stale. This leaves an unsquashed stacked commit.

mod common;

use common::{invoke, jj_init_with_file, start_test_server};
use rap_client::callback_server::start_callback_channel;
use rap_protocol::{RapCallback, RapInvocation};

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static LOCAL_COUNTER: AtomicU64 = AtomicU64::new(1000);

fn jj_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.arg("-R").arg(repo).current_dir(repo);
    cmd
}

/// Fire an invoke without waiting for the result. Returns the tool_call_id.
async fn invoke_fire(
    server_url: &str,
    callback_url: &str,
    group_id: &str,
    operation: &str,
    arguments: serde_json::Value,
    thread_ancestors: Option<Vec<String>>,
) -> String {
    let id = format!(
        "call-{operation}-{}",
        LOCAL_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let invocation = RapInvocation {
        operation: operation.to_owned(),
        arguments,
        id: id.clone(),
        call_id: None,
        callback_url: callback_url.to_owned(),
        group_id: group_id.to_owned(),
        user_id: None,
        thread_ancestors,
    };

    reqwest::Client::new()
        .post(format!("{server_url}/invoke"))
        .json(&invocation)
        .send()
        .await
        .expect("send invoke request");

    id
}

/// Wait for the initial ToolResult with subscription=true (command still running).
async fn wait_for_subscription_result(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RapCallback>,
) -> String {
    loop {
        let cb = tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
            .await
            .expect("timed out waiting for subscription result")
            .expect("channel closed");

        match cb {
            RapCallback::ToolResult(r) if r.subscription == Some(true) => return r.text,
            RapCallback::ViewUpdate(_) => continue,
            other => panic!("expected subscription ToolResult, got: {other:?}"),
        }
    }
}

/// Drain all remaining callbacks until the final subscription event.
async fn wait_for_final_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<RapCallback>) {
    loop {
        let cb = tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
            .await
            .expect("timed out waiting for final event")
            .expect("channel closed");

        match cb {
            RapCallback::SubscriptionEvent(e) if e.r#final == Some(true) => return,
            _ => continue,
        }
    }
}

#[tokio::test]
async fn stale_workspace_after_parent_describe_during_child_command() {
    let _ = tracing_subscriber::fmt::try_init();

    let tmp = jj_init_with_file("README.md", "hello\n");
    let repo = tmp.path();

    let server_url = start_test_server(&repo.join(".test-metadata")).await;
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

    // 2. Edit a file on the parent so its bookmark exists
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

    // 3. Clone repo for child, stacked on parent
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

    // 4. Read a file on the child to force workspace creation
    let text = invoke(
        &server_url,
        &callback_url,
        child_id,
        "read_file",
        serde_json::json!({ "path": "README.md" }),
        &mut rx,
        Some(vec![parent_id.to_owned()]),
    )
    .await;
    assert!(text.contains("hello"), "got: {text}");

    // 5. Fire a long-running command on the child (don't wait)
    let tool_call_id = invoke_fire(
        &server_url,
        &callback_url,
        child_id,
        "execute_command",
        serde_json::json!({ "command": "sleep 300" }),
        Some(vec![parent_id.to_owned()]),
    )
    .await;

    // 6. Wait for "command is still running" subscription result
    let text = wait_for_subscription_result(&mut rx).await;
    assert!(
        text.contains("still running"),
        "expected streaming result, got: {text}"
    );

    // 7. While child command is in flight, edit a file on the parent
    //    This creates a new jj operation that makes the child workspace stale.
    let text = invoke(
        &server_url,
        &callback_url,
        parent_id,
        "edit_file",
        serde_json::json!({ "path": "README.md", "old_str": "hello world\n", "new_str": "hello world updated\n" }),
        &mut rx,
        None,
    )
    .await;
    assert!(text.contains("Replaced"), "got: {text}");

    // 8. Cancel the child command — this triggers jj_squash_stacked
    reqwest::Client::new()
        .post(format!("{server_url}/cancel_tool_call"))
        .json(&serde_json::json!({
            "tool_call_id": tool_call_id,
            "thread_id": child_id,
        }))
        .send()
        .await
        .expect("send cancel request");

    // 9. Wait for the final subscription event
    wait_for_final_event(&mut rx).await;

    // 10. Verify: the child's stacked commit should have been squashed.
    //     If the bug is present, `jj squash --into @-` failed with stale workspace,
    //     so "sleep 300" won't appear in the bookmark's evolog (it was never squashed in).
    //     If the fix works, the evolog will contain the "sleep 300" entry.
    let output = jj_cmd(repo)
        .args(["evolog", "--no-graph", "-r", &format!("sandbox-{child_id}")])
        .output()
        .expect("run jj evolog");
    let evolog = String::from_utf8_lossy(&output.stdout).to_string();
    eprintln!("child bookmark evolog:\n{evolog}");

    assert!(
        evolog.contains("sleep 300"),
        "stacked commit was not squashed into bookmark — stale workspace bug!\nevolog:\n{evolog}"
    );
}
