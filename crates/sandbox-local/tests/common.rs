//! Shared test utilities for sandbox-local integration tests.
#![allow(dead_code, reason = "not all test binaries use every helper")]
#![allow(
    clippy::allow_attributes,
    reason = "need allow(dead_code) for shared test helpers"
)]

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rap_protocol::{RapCallback, RapInvocation, RapToolResult, RapViewUpdate};
use sandbox_core::callback::PlainCallbackClient;
use sandbox_core::server::build_router;
use sandbox_local::backend::LocalBackend;
use sandbox_local::metadata::FileMetadataStore;

static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Start the RAP server on an OS-assigned port, returning the base URL.
/// `metadata_dir` is where per-group JSON state files are stored.
pub async fn start_test_server(metadata_dir: &Path) -> String {
    start_test_server_sandboxed(metadata_dir, false).await
}

/// Start the RAP server with platform sandboxing (bwrap/sandbox-exec) enabled.
pub async fn start_test_server_sandboxed(metadata_dir: &Path, sandbox_enabled: bool) -> String {
    std::fs::create_dir_all(metadata_dir).expect("create metadata dir");

    unsafe {
        std::env::set_var(
            "XDG_CONFIG_HOME",
            std::env::temp_dir().join("xdg-config-home"),
        );
    }

    let backend = LocalBackend::new(sandbox_enabled);
    let metadata = FileMetadataStore::new(metadata_dir.to_path_buf());
    let (app, _tracker) = build_router(backend, metadata, PlainCallbackClient::new(), false, None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let port = listener.local_addr().expect("get local addr").port();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve test server") });

    format!("http://127.0.0.1:{port}")
}

/// POST a RapInvocation and wait for the callback result text.
pub async fn invoke(
    server_url: &str,
    callback_url: &str,
    group_id: &str,
    operation: &str,
    arguments: serde_json::Value,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RapCallback>,
    thread_ancestors: Option<Vec<String>>,
) -> String {
    invoke_raw(
        server_url,
        callback_url,
        group_id,
        operation,
        arguments,
        rx,
        thread_ancestors,
    )
    .await
    .text
    .unwrap_or_default()
}

/// Like [`invoke`] but returns the full [`RapToolResult`] including `display_as`.
pub async fn invoke_raw(
    server_url: &str,
    callback_url: &str,
    group_id: &str,
    operation: &str,
    arguments: serde_json::Value,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RapCallback>,
    thread_ancestors: Option<Vec<String>>,
) -> RapToolResult {
    let invocation = RapInvocation {
        operation: operation.to_owned(),
        arguments,
        id: format!(
            "call-{operation}-{}",
            CALL_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
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

    loop {
        let cb = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for callback")
            .expect("channel closed");

        match cb {
            RapCallback::ToolResult(r) => return r,
            RapCallback::ViewUpdate(_) => continue,
            other => panic!("expected ToolResult, got: {other:?}"),
        }
    }
}

/// Create a colocated jj+git repo with an initial commit containing one file.
pub fn jj_init_with_file(filename: &str, content: &str) -> tempfile::TempDir {
    jj_init_with_binary_file(filename, content.as_bytes())
}

/// Like [`jj_init_with_file`] but for arbitrary (e.g. binary) file content.
pub fn jj_init_with_binary_file(filename: &str, content: &[u8]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let path = tmp.path();
    assert!(
        Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init"])
            .current_dir(path)
            .status()
            .expect("run git init")
            .success()
    );
    assert!(
        Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(path)
            .status()
            .expect("run jj git init")
            .success()
    );
    std::fs::write(path.join(filename), content).expect("write file");
    assert!(
        Command::new("jj")
            .args(["commit", "-m", "initial"])
            .current_dir(path)
            .status()
            .expect("run jj commit")
            .success()
    );
    tmp
}

/// POST a RapInvocation and collect ViewUpdate callbacks alongside the
/// final result text. `expected_views` is the number of ViewUpdates to
/// wait for after the result. Handles both fast (ToolResult) and streaming
/// (ToolResult + SubscriptionEvents) responses.
pub async fn invoke_collecting_views(
    server_url: &str,
    callback_url: &str,
    group_id: &str,
    operation: &str,
    arguments: serde_json::Value,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<RapCallback>,
    expected_views: usize,
) -> (String, Vec<RapViewUpdate>) {
    let invocation = RapInvocation {
        operation: operation.to_owned(),
        arguments,
        id: format!(
            "call-{operation}-{}",
            CALL_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
        call_id: None,
        callback_url: callback_url.to_owned(),
        group_id: group_id.to_owned(),
        user_id: None,
        thread_ancestors: None,
    };

    reqwest::Client::new()
        .post(format!("{server_url}/invoke"))
        .json(&invocation)
        .send()
        .await
        .expect("send invoke request");

    let mut views = Vec::new();
    let mut result_text = None;
    loop {
        let cb = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for callback")
            .expect("channel closed");

        match cb {
            RapCallback::ToolResult(r) => {
                if r.subscription == Some(true) {
                    result_text = Some(r.text.unwrap_or_default());
                    continue;
                }
                result_text = Some(r.text.unwrap_or_default());
                if views.len() >= expected_views {
                    return (result_text.expect("bug: just set"), views);
                }
            }
            RapCallback::ViewUpdate(v) => {
                views.push(v);
                if let Some(ref text) = result_text
                    && views.len() >= expected_views
                {
                    return (text.clone(), views);
                }
            }
            RapCallback::SubscriptionEvent(e) => {
                if e.r#final == Some(true) {
                    let initial = result_text.unwrap_or_default();
                    result_text = Some(format!("{initial}\n{}", e.text));
                    if views.len() >= expected_views {
                        return (result_text.expect("bug: just set"), views);
                    }
                }
            }
            _ => continue,
        }
    }
}
