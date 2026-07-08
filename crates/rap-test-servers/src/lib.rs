//! In-process stub RAP tool servers for integration tests.
//!
//! This crate is test support shared by the TUI and web e2e suites (a
//! dev-dependency of `infinity-agent-cli` and `infinity-daemon`); it is
//! never published.
//!
//! Servers bind an OS-assigned loopback port and serve the RAP toolset
//! discovery + invoke endpoints, delivering results through the standard
//! callback URL, so tests exercise the same pipeline as production RAP
//! servers. Point a session at one by dropping a config into the session's
//! working directory (see [`write_rap_config`]).

use std::path::Path;

use axum::routing::{get, post};
use axum::{Json, Router};
use rap_protocol::{
    DisplaySegment, ImageContent, RapCallback, RapInvocation, RapToolResult, RapToolResultContent,
};

/// A 96×64 solid-indigo PNG used as the stub image tool's "image file".
pub const STUB_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAGAAAABACAIAAABqVuVZAAAAaUlEQVR42u3QQQkAAAgEsOsq2N8GNvDtY7AES/VwiAJBggQJEiRIkCAECRIkSJAgQYIQJEiQIEGCBAlCkCBBggQJEiRIEIIECRIkSJAgQQgSJEiQIEGCBCFIkCBBggQJEiQIQYIECfpnAbKIcmjVgp/0AAAAAElFTkSuQmCC";

/// Write a cwd-local RAP config (`{cwd}/.infinity/rap.json`) pointing
/// sessions created in `cwd` at the toolset server on `port`.
pub fn write_rap_config(cwd: &Path, port: u16) -> std::io::Result<()> {
    let rap_dir = cwd.join(".infinity");
    std::fs::create_dir_all(&rap_dir)?;
    std::fs::write(
        rap_dir.join("rap.json"),
        format!(
            r#"{{"tool_sets":[{{"type":"toolset_server","server_url":"http://127.0.0.1:{port}"}}]}}"#
        ),
    )
}

/// Start a minimal RAP tool server exposing a single `read_image` tool.
///
/// Invocations are answered (via the invocation's callback URL) with the
/// fixed [`STUB_PNG_BASE64`] image: multimodal tool-result `content`
/// (text + image) plus an image display segment with a text summary
/// fallback. Returns the server's port.
///
/// Must be called from within a tokio runtime; the server task runs until
/// the runtime shuts down.
pub async fn start_stub_image_server() -> std::io::Result<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();

    let manifest = serde_json::json!({
        "name": "stub-images",
        "endpoint": format!("http://127.0.0.1:{port}/invoke"),
        "tools": [{
            "name": "read_image",
            "description": "Read an image file and attach it to the conversation.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        }]
    });

    let app = Router::new()
        .route(
            "/.well-known/rap-toolset",
            get(move || {
                let manifest = manifest.clone();
                async move { Json(manifest) }
            }),
        )
        .route(
            "/invoke",
            post(|Json(inv): Json<RapInvocation>| async move {
                tokio::spawn(async move {
                    let path = inv
                        .arguments
                        .get("path")
                        .and_then(|p| p.as_str())
                        .unwrap_or("?")
                        .to_owned();
                    let text =
                        format!("Read image file \"{path}\" (image/png). The image is attached.");
                    let callback = RapCallback::ToolResult(RapToolResult {
                        group_id: inv.group_id,
                        id: inv.id,
                        call_id: inv.call_id,
                        // Emit structured `content` only (no top-level `text`);
                        // the description lives in the leading text item.
                        text: None,
                        content: Some(vec![
                            RapToolResultContent::Text { text },
                            RapToolResultContent::Image {
                                data: STUB_PNG_BASE64.to_owned(),
                                media_type: "image/png".to_owned(),
                            },
                        ]),
                        display_as: Some(vec![
                            DisplaySegment::Image(ImageContent {
                                data: STUB_PNG_BASE64.to_owned(),
                                media_type: "image/png".to_owned(),
                            }),
                            DisplaySegment::Text(format!("Read image {path} (image/png)")),
                        ]),
                        subscription: None,
                    });
                    let body = serde_json::to_string(&callback).expect("serialize callback");
                    if let Err(e) = reqwest::Client::new()
                        .post(&inv.callback_url)
                        .header("content-type", "application/json")
                        .body(body)
                        .send()
                        .await
                    {
                        panic!("stub failed to deliver tool result: {e}");
                    }
                });
                axum::http::StatusCode::OK
            }),
        );

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve stub RAP server");
    });
    Ok(port)
}
