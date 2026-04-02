//! Shared test utilities for sandbox-local integration tests.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rap_protocol::{RapCallback, RapInvocation};
use sandbox_core::callback::PlainCallbackClient;
use sandbox_core::server::build_router;
use sandbox_local::backend::LocalBackend;
use sandbox_local::metadata::FileMetadataStore;

static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

#[allow(unused)]
/// Start the RAP server on an OS-assigned port, returning the base URL.
/// `metadata_dir` is where per-group JSON state files are stored.
pub async fn start_test_server(metadata_dir: &Path) -> String {
    std::fs::create_dir_all(metadata_dir).expect("create metadata dir");

    unsafe {
        std::env::set_var(
            "XDG_CONFIG_HOME",
            std::env::temp_dir().join("xdg-config-home"),
        );
    }

    let backend = LocalBackend::new(false);
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
    let invocation = RapInvocation {
        operation: operation.to_string(),
        arguments,
        id: format!(
            "call-{operation}-{}",
            CALL_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
        call_id: None,
        callback_url: callback_url.to_string(),
        group_id: group_id.to_string(),
        user_id: None,
        thread_ancestors,
    };

    reqwest::Client::new()
        .post(format!("{server_url}/invoke"))
        .json(&invocation)
        .send()
        .await
        .expect("send invoke request");

    let cb = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for callback")
        .expect("channel closed");

    match cb {
        RapCallback::ToolResult(r) => r.text,
        other => panic!("expected ToolResult, got: {other:?}"),
    }
}
