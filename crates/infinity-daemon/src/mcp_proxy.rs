//! In-process RAP server that exposes an MCP client over RAP.
//!
//! The bridge owns the adapter contract (tool names, descriptions, schemas,
//! and dispatch); this module only serves the RAP manifest and delivers
//! results to the invocation's callback URL.

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use infinity_mcp_bridge::{BoxError, McpClient, McpTransportFactory};
use rap_protocol::{RapCallback, RapInvocation, RapToolResult, ToolsetManifest};
use std::collections::HashMap;
use std::convert::Infallible;
use tokio::net::TcpListener;

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("bug: failed to build HTTP response")
}

fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("bug: failed to build HTTP response")
}

async fn handle(req: Request<Incoming>, client: McpClient, port: u16) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    if method == hyper::Method::GET && path.contains(".well-known/rap-toolset") {
        let descriptor = client.describe_toolset();
        let manifest = ToolsetManifest {
            name: descriptor.name,
            description: Some(descriptor.description),
            endpoint: format!("http://127.0.0.1:{port}"),
            tools: descriptor.tools.into_iter().map(Into::into).collect(),
            needs_migration: false,
        };
        let manifest =
            serde_json::to_string(&manifest).expect("bug: failed to serialize RAP manifest");
        return json_response(StatusCode::OK, &manifest);
    }

    if method != hyper::Method::POST {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }
    let body = match req.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(error) => {
            return text_response(StatusCode::BAD_REQUEST, &format!("bad body: {error}"));
        }
    };
    let invocation: RapInvocation = match serde_json::from_slice(&body) {
        Ok(invocation) => invocation,
        Err(error) => return text_response(StatusCode::BAD_REQUEST, &format!("bad json: {error}")),
    };

    tokio::spawn(rap_protocol::log_panic("mcp_proxy_invoke", async move {
        let (text, display_as) = client
            .dispatch(&invocation.operation, &invocation.arguments)
            .await;
        let callback = serde_json::to_string(&RapCallback::ToolResult(RapToolResult {
            group_id: invocation.group_id,
            id: invocation.id,
            call_id: invocation.call_id,
            text: Some(text),
            content: None,
            display_as,
            subscription: None,
        }))
        .expect("bug: failed to serialize RAP callback");

        match reqwest::Client::new()
            .post(&invocation.callback_url)
            .header("content-type", "application/json")
            .body(callback)
            .send()
            .await
        {
            Ok(response) if !response.status().is_success() => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                tracing::error!("Callback rejected MCP result (HTTP {status}): {body}");
            }
            Err(error) => tracing::warn!("Failed to send MCP result to callback: {error}"),
            _ => {}
        }
    }));

    text_response(StatusCode::OK, "OK")
}

/// Start an MCP proxy RAP server for a stdio subprocess.
pub async fn start_mcp_proxy(
    name: String,
    command: Vec<String>,
    env: HashMap<String, String>,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    start_client_proxy(McpClient::stdio(name, command, env)).await
}

/// Start an MCP proxy RAP server for a Streamable HTTP server.
pub async fn start_http_mcp_proxy(
    name: String,
    url: String,
    headers: HashMap<String, String>,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    start_client_proxy(McpClient::http(name, url, headers)).await
}

#[doc(hidden)]
/// Start a proxy from a custom MCP transport factory.
pub async fn start_proxy_server(
    name: String,
    factory: McpTransportFactory,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    start_client_proxy(McpClient::new(name, factory)).await
}

async fn start_client_proxy(
    client: McpClient,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(rap_protocol::log_panic(
        "mcp_proxy_accept_loop",
        async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::warn!("MCP proxy accept error: {error}");
                        continue;
                    }
                };
                let client = client.clone();
                tokio::spawn(rap_protocol::log_panic(
                    "mcp_proxy_connection",
                    async move {
                        let service = hyper::service::service_fn(move |request| {
                            let client = client.clone();
                            async move { Ok::<_, Infallible>(handle(request, client, port).await) }
                        });
                        if let Err(error) = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await
                        {
                            tracing::warn!("MCP proxy connection error: {error}");
                        }
                    },
                ));
            }
        },
    ));
    tracing::info!("MCP proxy RAP server listening on port {port}");
    Ok((port, task))
}
