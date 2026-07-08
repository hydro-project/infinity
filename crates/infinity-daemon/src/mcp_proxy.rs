//! In-process RAP server that proxies MCP servers (stdio or HTTP).
//!
//! Exposes `{name}_list_tools` and `{name}_invoke_tool` as RAP tools,
//! lazily connecting to the MCP server on first use.

use async_trait::async_trait;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rap_protocol::{DisplaySegment, RapCallback, RapInvocation, RapToolResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ── MCP JSON-RPC types ──

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[expect(dead_code, reason = "reserved for future use")]
    id: Option<u64>,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    message: String,
}

// ── Lazy stdio MCP client ──

struct StdioMcpClient {
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
    _child: Child,
}

impl StdioMcpClient {
    async fn new(command: &[String], env: &HashMap<String, String>) -> Result<Self, BoxError> {
        let (cmd, args) = command.split_first().ok_or("empty MCP command")?;
        let mut child = Command::new(cmd)
            .args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn MCP server: {e}"))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let reader = BufReader::new(stdout);

        let mut client = Self {
            stdin,
            reader,
            next_id: 0,
            _child: child,
        };

        // Initialize
        client
            .request(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "infinity-mcp-proxy", "version": "1.0.0"}
                })),
            )
            .await?;

        Ok(client)
    }

    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        self.next_id += 1;
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: self.next_id,
            method: method.to_owned(),
            params,
        };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        // Read lines until we get a JSON-RPC response with our id
        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf).await?;
            if n == 0 {
                return Err("MCP server closed stdout".into());
            }
            let buf = buf.trim();
            if buf.is_empty() {
                continue;
            }
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(buf) {
                if let Some(err) = resp.error {
                    return Err(format!("MCP error: {}", err.message).into());
                }
                return Ok(resp.result.unwrap_or(serde_json::Value::Null));
            }
            // Skip notifications / non-response messages
        }
    }
}

// ── MCP transport trait ──

#[async_trait]
#[doc(hidden)]
pub trait McpTransport: Send {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError>;
}

#[async_trait]
impl McpTransport for StdioMcpClient {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        self.request(method, params).await
    }
}

// ── HTTP MCP client ──

struct HttpMcpClient {
    url: String,
    headers: HashMap<String, String>,
    session_id: Option<String>,
    next_id: u64,
    http: reqwest::Client,
}

impl HttpMcpClient {
    async fn new(url: &str, headers: &HashMap<String, String>) -> Result<Self, BoxError> {
        let mut client = Self {
            url: url.to_owned(),
            headers: headers.clone(),
            session_id: None,
            next_id: 0,
            http: reqwest::Client::new(),
        };

        client
            .request(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "infinity-mcp-proxy", "version": "1.0.0"}
                })),
            )
            .await?;

        // Send initialized notification
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let mut req = client
            .http
            .post(&client.url)
            .header("content-type", "application/json");
        for (k, v) in &client.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(sid) = &client.session_id {
            req = req.header("mcp-session-id", sid.as_str());
        }
        let _ = req.body(notification.to_string()).send().await;

        Ok(client)
    }
}

#[async_trait]
impl McpTransport for HttpMcpClient {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params.unwrap_or(serde_json::Value::Null),
        });

        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(sid) = &self.session_id {
            req = req.header("mcp-session-id", sid.as_str());
        }

        let resp = req
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("HTTP MCP request failed: {e}"))?;

        if let Some(sid) = resp.headers().get("mcp-session-id") {
            self.session_id = sid.to_str().ok().map(String::from);
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP MCP error {status}: {text}").into());
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let text = resp.text().await?;

        if content_type.contains("text/event-stream") {
            // Parse SSE: look for data: lines
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(msg) = serde_json::from_str::<JsonRpcResponse>(data)
                {
                    if let Some(err) = msg.error {
                        return Err(format!("MCP error: {}", err.message).into());
                    }
                    return Ok(msg.result.unwrap_or(serde_json::Value::Null));
                }
            }
            return Err("No response in SSE stream".into());
        }

        let msg: JsonRpcResponse = serde_json::from_str(&text)?;
        if let Some(err) = msg.error {
            return Err(format!("MCP error: {}", err.message).into());
        }
        Ok(msg.result.unwrap_or(serde_json::Value::Null))
    }
}

#[doc(hidden)]
pub type McpClientFactory = Box<
    dyn Fn() -> std::pin::Pin<
            Box<dyn Future<Output = Result<Box<dyn McpTransport>, BoxError>> + Send>,
        > + Send
        + Sync,
>;

struct ProxyState {
    name: String,
    client_factory: McpClientFactory,
    client: Mutex<Option<Box<dyn McpTransport>>>,
    port: u16,
}

impl ProxyState {
    async fn ensure_client(&self) -> Result<(), BoxError> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some((self.client_factory)().await?);
        }
        Ok(())
    }

    async fn list_tools(&self) -> Result<(String, Option<Vec<DisplaySegment>>), BoxError> {
        self.ensure_client().await?;
        let mut guard = self.client.lock().await;
        let client = guard
            .as_mut()
            .expect("bug: client missing after ensure_client");
        let result = client
            .request("tools/list", Some(serde_json::json!({})))
            .await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        if tools.is_empty() {
            return Ok(("No tools available from this MCP server.".to_owned(), None));
        }
        let mut out = format!("Available tools ({}):\n\n", tools.len());
        for tool in &tools {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let desc = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("No description");
            out.push_str(&format!("**{name}**\n{desc}\n"));
            if let Some(schema) = tool.get("inputSchema") {
                out.push_str(&format!(
                    "Parameters: {}\n",
                    serde_json::to_string_pretty(schema).unwrap_or_default()
                ));
            }
            out.push('\n');
        }
        Ok((
            out,
            Some(vec![DisplaySegment::Text(format!(
                "Loaded {} tools",
                tools.len()
            ))]),
        ))
    }

    async fn invoke_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, BoxError> {
        self.ensure_client().await?;
        let mut guard = self.client.lock().await;
        let client = guard
            .as_mut()
            .expect("bug: client missing after ensure_client");
        let result = client
            .request(
                "tools/call",
                Some(serde_json::json!({"name": tool_name, "arguments": arguments})),
            )
            .await?;

        let mut out = format!("Tool \"{tool_name}\" completed.\n\n");
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            for item in content {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            out.push_str(text);
                            out.push('\n');
                        }
                    }
                    Some("image") => {
                        let mime = item
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown");
                        out.push_str(&format!("[Image: {mime}]\n"));
                    }
                    Some("resource") => {
                        let uri = item
                            .pointer("/resource/uri")
                            .and_then(|u| u.as_str())
                            .unwrap_or("unknown");
                        out.push_str(&format!("[Resource: {uri}]\n"));
                    }
                    _ => {}
                }
            }
        }
        if result
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false)
        {
            out = format!("Tool \"{tool_name}\" failed: {out}");
        }
        Ok(out)
    }
}

// ── HTTP server ──

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

async fn handle(req: Request<Incoming>, state: Arc<ProxyState>) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    // Discovery endpoint
    if method == hyper::Method::GET && path.contains(".well-known/rap-toolset") {
        let manifest = serde_json::json!({
            "name": format!("{}-mcp", state.name),
            "description": format!("MCP server proxy for {}. Use {}_list_tools to discover available tools, then {}_invoke_tool to call them.", state.name, state.name, state.name),
            "endpoint": format!("http://127.0.0.1:{}", state.port),
            "tools": [
                {
                    "name": format!("{}_list_tools", state.name),
                    "description": format!("List all available tools from the {} MCP server.", state.name),
                    "inputSchema": {"type": "object", "properties": {}, "required": []}
                },
                {
                    "name": format!("{}_invoke_tool", state.name),
                    "description": format!("Invoke a tool from the {} MCP server. Use {}_list_tools first to see available tools.", state.name, state.name),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "tool_name": {"type": "string", "description": "Name of the tool to invoke."},
                            "arguments": {"type": "object", "description": "Arguments to pass to the tool."}
                        },
                        "required": ["tool_name"]
                    }
                }
            ]
        });
        return json_response(StatusCode::OK, &manifest.to_string());
    }

    if method != hyper::Method::POST {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }

    // Parse invocation
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "bad body"),
    };
    let inv: RapInvocation = match serde_json::from_slice(&body) {
        Ok(i) => i,
        Err(e) => return text_response(StatusCode::BAD_REQUEST, &format!("bad json: {e}")),
    };

    // Return immediately, process async
    let state = state.clone();
    tokio::spawn(rap_protocol::log_panic("mcp_proxy_invoke", async move {
        let res = if inv.operation.ends_with("_list_tools") {
            state.list_tools().await
        } else if inv.operation.ends_with("_invoke_tool") {
            let tool_name = inv
                .arguments
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let args = inv
                .arguments
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            state.invoke_tool(&tool_name, args).await.map(|r| (r, None))
        } else {
            Err(format!("unknown operation: {}", inv.operation).into())
        };

        let (text, display_as) = match res {
            Ok(t) => t,
            Err(e) => (format!("MCP tool error: {e}"), None),
        };

        let callback_body = serde_json::to_string(&RapCallback::ToolResult(RapToolResult {
            group_id: inv.group_id,
            id: inv.id,
            call_id: inv.call_id,
            text: Some(text),
            content: None,
            display_as,
            subscription: None,
        }))
        .expect("bug: serialize RapCallback");

        match reqwest::Client::new()
            .post(&inv.callback_url)
            .header("content-type", "application/json")
            .body(callback_body)
            .send()
            .await
        {
            Ok(resp) if !resp.status().is_success() => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::error!("Callback rejected MCP result (HTTP {status}): {body}");
            }
            Err(e) => {
                tracing::warn!("Failed to send MCP result to callback: {e}");
            }
            _ => {}
        }
    }));

    text_response(StatusCode::OK, "OK")
}

/// Start an MCP proxy RAP server for a stdio subprocess. Returns the port.
/// The MCP subprocess is spawned lazily on first request.
pub async fn start_mcp_proxy(
    name: String,
    command: Vec<String>,
    env: HashMap<String, String>,
) -> Result<u16, BoxError> {
    let factory: McpClientFactory = {
        let command = command.clone();
        let env = env.clone();
        Box::new(move || {
            let command = command.clone();
            let env = env.clone();
            Box::pin(async move {
                let client = StdioMcpClient::new(&command, &env).await?;
                Ok(Box::new(client) as Box<dyn McpTransport>)
            })
        })
    };
    start_proxy_server(name, factory).await
}

/// Start an MCP proxy RAP server for a remote HTTP MCP server. Returns the port.
/// The MCP connection is established lazily on first request.
pub async fn start_http_mcp_proxy(
    name: String,
    url: String,
    headers: HashMap<String, String>,
) -> Result<u16, BoxError> {
    let factory: McpClientFactory = {
        let url = url.clone();
        let headers = headers.clone();
        Box::new(move || {
            let url = url.clone();
            let headers = headers.clone();
            Box::pin(async move {
                let client = HttpMcpClient::new(&url, &headers).await?;
                Ok(Box::new(client) as Box<dyn McpTransport>)
            })
        })
    };
    start_proxy_server(name, factory).await
}

#[doc(hidden)]
pub async fn start_proxy_server(name: String, factory: McpClientFactory) -> Result<u16, BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let state = Arc::new(ProxyState {
        name,
        client_factory: factory,
        client: Mutex::new(None),
        port,
    });

    tokio::spawn(rap_protocol::log_panic(
        "mcp_proxy_accept_loop",
        async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("MCP proxy accept error: {e}");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(rap_protocol::log_panic(
                    "mcp_proxy_connection",
                    async move {
                        let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                            let state = state.clone();
                            async move { Ok::<_, Infallible>(handle(req, state).await) }
                        });
                        if let Err(e) = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), svc)
                            .await
                        {
                            tracing::warn!("MCP proxy connection error: {e}");
                        }
                    },
                ));
            }
        },
    ));

    tracing::info!("MCP proxy RAP server listening on port {port}");
    Ok(port)
}
