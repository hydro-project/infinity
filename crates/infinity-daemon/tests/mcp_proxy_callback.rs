//! Integration test: MCP proxy list_tools callback round-trips through the
//! RAP callback server without deserialization errors.

use async_trait::async_trait;
use infinity_daemon::mcp_proxy::{McpTransport, start_proxy_server};
use rap_client::callback_server::start_callback_channel;
use rap_protocol::{DisplaySegment, RapCallback, RapInvocation};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct MockMcpTransport;

#[async_trait]
impl McpTransport for MockMcpTransport {
    async fn request(
        &mut self,
        method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        match method {
            "tools/list" => Ok(serde_json::json!({
                "tools": [
                    {"name": "echo", "description": "Echoes input", "inputSchema": {"type": "object"}}
                ]
            })),
            _ => Err(format!("unexpected method: {method}").into()),
        }
    }
}

fn mock_factory() -> Box<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn McpTransport>, BoxError>> + Send>,
        > + Send
        + Sync,
> {
    Box::new(|| Box::pin(async { Ok(Box::new(MockMcpTransport) as Box<dyn McpTransport>) }))
}

#[tokio::test]
async fn list_tools_callback_deserializes() {
    let _ = tracing_subscriber::fmt::try_init();

    let port = start_proxy_server("mock".to_string(), mock_factory()).await.unwrap();
    let proxy_url = format!("http://127.0.0.1:{port}");

    let (callback_url, mut rx) = start_callback_channel().await.unwrap();

    let inv = RapInvocation {
        operation: "mock_list_tools".to_string(),
        arguments: serde_json::json!({}),
        id: "test-1".to_string(),
        call_id: None,
        callback_url,
        group_id: "g1".to_string(),
        user_id: None,
        thread_ancestors: None,
    };

    reqwest::Client::new()
        .post(&proxy_url)
        .json(&inv)
        .send()
        .await
        .unwrap();

    let cb = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out — callback was likely rejected (the original bug)")
        .expect("channel closed");

    match cb {
        RapCallback::ToolResult(tr) => {
            assert!(tr.text.contains("echo"), "tool list should mention 'echo': {}", tr.text);
            let segments = tr.display_as.expect("display_as should be Some");
            assert!(matches!(segments[0], DisplaySegment::Text(_)));
        }
        other => panic!("expected ToolResult, got: {other:?}"),
    }
}
