#![warn(missing_docs)]

//! Connect to MCP servers from Infinity runtime embeddings.
//!
//! [`McpClient`] owns a lazily initialized stdio or Streamable HTTP session
//! and provides canonical tool metadata and dispatch for protocol adapters.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use rap_protocol::DisplaySegment;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Error type returned by MCP transports and operations.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    message: String,
}

/// A transport capable of sending requests to one initialized MCP session.
#[async_trait]
pub trait McpTransport: Send {
    /// Send one JSON-RPC request and return its `result` value.
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError>;
}

struct StdioMcpTransport {
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
    _child: Child,
}

impl StdioMcpTransport {
    async fn new(command: &[String], env: &HashMap<String, String>) -> Result<Self, BoxError> {
        let (executable, args) = command.split_first().ok_or("empty MCP command")?;
        let mut child = Command::new(executable)
            .args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("failed to spawn MCP server: {error}"))?;
        let stdin = child.stdin.take().ok_or("MCP server has no stdin")?;
        let stdout = child.stdout.take().ok_or("MCP server has no stdout")?;
        let mut transport = Self {
            stdin,
            reader: BufReader::new(stdout),
            next_id: 0,
            _child: child,
        };
        initialize(&mut transport).await?;
        Ok(transport)
    }
}

#[async_trait]
impl McpTransport for StdioMcpTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        self.next_id += 1;
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: self.next_id,
            method: method.to_owned(),
            params,
        };
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                return Err("MCP server closed stdout".into());
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(line) {
                return response_result(response);
            }
        }
    }
}

struct HttpMcpTransport {
    url: String,
    headers: HashMap<String, String>,
    session_id: Option<String>,
    next_id: u64,
    http: reqwest::Client,
}

impl HttpMcpTransport {
    async fn new(url: String, headers: HashMap<String, String>) -> Result<Self, BoxError> {
        let mut transport = Self {
            url,
            headers,
            session_id: None,
            next_id: 0,
            http: reqwest::Client::new(),
        };
        initialize(&mut transport).await?;
        transport.notify_initialized().await?;
        Ok(transport)
    }

    async fn notify_initialized(&self) -> Result<(), BoxError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let mut request = self
            .http
            .post(&self.url)
            .header("content-type", "application/json");
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if let Some(session_id) = &self.session_id {
            request = request.header("mcp-session-id", session_id);
        }
        request
            .body(body.to_string())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[async_trait]
impl McpTransport for HttpMcpTransport {
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
        let mut request = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if let Some(session_id) = &self.session_id {
            request = request.header("mcp-session-id", session_id);
        }
        let response = request.body(body.to_string()).send().await?;
        if let Some(session_id) = response.headers().get("mcp-session-id") {
            self.session_id = session_id.to_str().ok().map(str::to_owned);
        }
        let response = response.error_for_status()?;
        let is_event_stream = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        let body = response.text().await?;
        if is_event_stream {
            for line in body.lines() {
                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(response) = serde_json::from_str::<JsonRpcResponse>(data)
                {
                    return response_result(response);
                }
            }
            return Err("MCP event stream contained no response".into());
        }
        response_result(serde_json::from_str(&body)?)
    }
}

fn response_result(response: JsonRpcResponse) -> Result<serde_json::Value, BoxError> {
    if let Some(error) = response.error {
        return Err(format!("MCP error: {}", error.message).into());
    }
    Ok(response.result.unwrap_or(serde_json::Value::Null))
}

async fn initialize(transport: &mut dyn McpTransport) -> Result<(), BoxError> {
    transport
        .request(
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "infinity-mcp-bridge", "version": env!("CARGO_PKG_VERSION")}
            })),
        )
        .await?;
    Ok(())
}

/// Factory for a lazily initialized MCP transport.
pub type McpTransportFactory = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn McpTransport>, BoxError>> + Send>>
        + Send
        + Sync,
>;

/// A lazily initialized, serialized MCP session.
#[derive(Clone)]
pub struct McpClient {
    name: Arc<str>,
    factory: McpTransportFactory,
    transport: Arc<Mutex<Option<Box<dyn McpTransport>>>>,
}

impl McpClient {
    /// Create a client backed by a custom transport factory.
    pub fn new(name: impl Into<String>, factory: McpTransportFactory) -> Self {
        Self {
            name: Arc::from(name.into()),
            factory,
            transport: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a lazy stdio MCP client.
    pub fn stdio(
        name: impl Into<String>,
        command: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self::new(
            name,
            Arc::new(move || {
                let command = command.clone();
                let env = env.clone();
                Box::pin(async move {
                    Ok(Box::new(StdioMcpTransport::new(&command, &env).await?)
                        as Box<dyn McpTransport>)
                })
            }),
        )
    }

    /// Create a lazy Streamable HTTP MCP client.
    pub fn http(
        name: impl Into<String>,
        url: impl Into<String>,
        headers: HashMap<String, String>,
    ) -> Self {
        let url = url.into();
        Self::new(
            name,
            Arc::new(move || {
                let url = url.clone();
                let headers = headers.clone();
                Box::pin(async move {
                    Ok(Box::new(HttpMcpTransport::new(url, headers).await?)
                        as Box<dyn McpTransport>)
                })
            }),
        )
    }

    /// Logical server name used to prefix the generated tool names.
    pub fn name(&self) -> &str {
        &self.name
    }

    async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        let mut transport = self.transport.lock().await;
        if transport.is_none() {
            *transport = Some((self.factory)().await?);
        }
        transport
            .as_mut()
            .expect("bug: MCP transport missing after initialization")
            .request(method, params)
            .await
    }

    /// List the server's tools as text suitable for a model and optional display segments.
    async fn list_tools(&self) -> Result<(String, Option<Vec<DisplaySegment>>), BoxError> {
        let result = self
            .request("tools/list", Some(serde_json::json!({})))
            .await?;
        let tools = result
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if tools.is_empty() {
            return Ok(("No tools available from this MCP server.".to_owned(), None));
        }
        let mut text = format!("Available tools ({}):\n\n", tools.len());
        for tool in &tools {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let description = tool
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("No description");
            text.push_str(&format!("**{name}**\n{description}\n"));
            if let Some(schema) = tool.get("inputSchema") {
                text.push_str(&format!(
                    "Parameters: {}\n",
                    serde_json::to_string_pretty(schema)?
                ));
            }
            text.push('\n');
        }
        Ok((
            text,
            Some(vec![DisplaySegment::Text(format!(
                "Loaded {} tools",
                tools.len()
            ))]),
        ))
    }

    /// Invoke one MCP tool and format its content for the model.
    async fn invoke_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, BoxError> {
        let result = self
            .request(
                "tools/call",
                Some(serde_json::json!({"name": tool_name, "arguments": arguments})),
            )
            .await?;
        let mut text = format!("Tool \"{tool_name}\" completed.\n\n");
        if let Some(content) = result.get("content").and_then(serde_json::Value::as_array) {
            for item in content {
                match item.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        if let Some(value) = item.get("text").and_then(serde_json::Value::as_str) {
                            text.push_str(value);
                            text.push('\n');
                        }
                    }
                    Some("image") => {
                        let mime = item
                            .get("mimeType")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        text.push_str(&format!("[Image: {mime}]\n"));
                    }
                    Some("resource") => {
                        let uri = item
                            .pointer("/resource/uri")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        text.push_str(&format!("[Resource: {uri}]\n"));
                    }
                    _ => {}
                }
            }
        }
        if result
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            text = format!("Tool \"{tool_name}\" failed: {text}");
        }
        Ok(text)
    }
}

/// One of the two operations the adapter exposes for each MCP server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpOperation {
    /// List the server's tools (`{server}_list_tools`).
    ListTools,
    /// Invoke one of the server's tools by name (`{server}_invoke_tool`).
    InvokeTool,
}

impl McpOperation {
    /// Recognize an adapter tool name by its operation suffix.
    ///
    /// Returns `None` for names that do not belong to this adapter.
    /// [`McpClient::dispatch`] reports unknown names in its result text.
    fn parse(tool_name: &str) -> Option<Self> {
        if tool_name.ends_with("_list_tools") {
            Some(Self::ListTools)
        } else if tool_name.ends_with("_invoke_tool") {
            Some(Self::InvokeTool)
        } else {
            None
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::ListTools => "list_tools",
            Self::InvokeTool => "invoke_tool",
        }
    }

    fn definition(self, server_name: &str) -> McpToolDefinition {
        McpToolDefinition {
            name: format!("{server_name}_{}", self.suffix()),
            description: match self {
                Self::ListTools => format!(
                    "List the tools available from the {server_name} MCP server before invoking one."
                ),
                Self::InvokeTool => format!(
                    "Invoke a tool from the {server_name} MCP server by name. Use {server_name}_list_tools first to see the available tools."
                ),
            },
            input_schema: match self {
                Self::ListTools => {
                    serde_json::json!({"type": "object", "properties": {}, "required": []})
                }
                Self::InvokeTool => serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tool_name": {"type": "string", "description": "Name of the MCP tool to invoke."},
                        "arguments": {"type": "object", "description": "Arguments to pass to the MCP tool."}
                    },
                    "required": ["tool_name"]
                }),
            },
            display_script: None,
        }
    }
}

/// The model-facing definition of one adapter tool.
///
/// Definitions are the single source for the adapter's tool names,
/// descriptions, and schemas. Protocol adapters such as a RAP proxy convert
/// them into their own manifest entries.
#[derive(Clone, Debug, PartialEq)]
pub struct McpToolDefinition {
    /// Tool name: `{server}_list_tools` or `{server}_invoke_tool`.
    pub name: String,
    /// Model-facing description of the tool.
    pub description: String,
    /// JSON Schema for the tool's argument object.
    pub input_schema: serde_json::Value,
    /// Optional Rhai display script for pretty-printing calls.
    pub display_script: Option<String>,
}

impl From<McpToolDefinition> for rap_protocol::ToolDef {
    fn from(definition: McpToolDefinition) -> Self {
        Self {
            name: definition.name,
            description: definition.description,
            input_schema: definition.input_schema,
            annotations: None,
            display_script: definition.display_script,
        }
    }
}

/// A protocol-agnostic summary of the adapter for one MCP server: a toolset
/// name, a usage description, and the tool definitions.
#[derive(Clone, Debug)]
pub struct McpToolsetDescriptor {
    /// Toolset name derived from the server name.
    pub name: String,
    /// Usage description referencing the adapter's tool names.
    pub description: String,
    /// The adapter's tool definitions, in dispatch order.
    pub tools: Vec<McpToolDefinition>,
}

impl McpClient {
    /// The adapter's tool definitions for this server, in dispatch order.
    ///
    /// Every surface that exposes this client must derive its tool metadata
    /// from these definitions so the model sees identical tools regardless of
    /// the transport in between.
    fn tool_definitions(&self) -> Vec<McpToolDefinition> {
        vec![
            McpOperation::ListTools.definition(self.name()),
            McpOperation::InvokeTool.definition(self.name()),
        ]
    }

    /// Describe the adapter as a toolset for protocol manifests.
    pub fn describe_toolset(&self) -> McpToolsetDescriptor {
        let name = self.name();
        McpToolsetDescriptor {
            name: format!("{name}-mcp"),
            description: format!(
                "MCP server proxy for {name}. Use {name}_list_tools to discover available tools, then {name}_invoke_tool to call them."
            ),
            tools: self.tool_definitions(),
        }
    }

    /// Execute one adapter tool call and format the outcome for the model.
    ///
    /// `tool_name` is an adapter tool name (`{server}_list_tools` or
    /// `{server}_invoke_tool`); `arguments` is the tool-call argument object.
    /// Failures, including unknown tool names, are formatted into the
    /// returned text so the model receives the same error message from every
    /// adapter surface.
    pub async fn dispatch(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> (String, Option<Vec<DisplaySegment>>) {
        let result = match McpOperation::parse(tool_name) {
            Some(McpOperation::ListTools) => self.list_tools().await,
            Some(McpOperation::InvokeTool) => {
                let target = arguments
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let target_arguments = arguments
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                self.invoke_tool(target, target_arguments)
                    .await
                    .map(|text| (text, None))
            }
            None => Err(format!("unknown operation: {tool_name}").into()),
        };
        result.unwrap_or_else(|error| (format!("MCP tool error: {error}"), None))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    type RecordedRequests = Arc<Mutex<Vec<(String, Option<serde_json::Value>)>>>;

    struct RecordingTransport {
        requests: RecordedRequests,
    }

    #[async_trait]
    impl McpTransport for RecordingTransport {
        async fn request(
            &mut self,
            method: &str,
            params: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, BoxError> {
            self.requests
                .lock()
                .await
                .push((method.to_owned(), params.clone()));
            match method {
                "tools/list" => Ok(serde_json::json!({
                    "tools": [{
                        "name": "echo",
                        "description": "Echo input",
                        "inputSchema": {"type": "object"}
                    }]
                })),
                "tools/call" => Ok(serde_json::json!({
                    "content": [{"type": "text", "text": "hello"}]
                })),
                other => Err(format!("unexpected method: {other}").into()),
            }
        }
    }

    fn test_client() -> (McpClient, RecordedRequests, Arc<AtomicUsize>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let client = McpClient::new("test", {
            let requests = requests.clone();
            let factory_calls = factory_calls.clone();
            Arc::new(move || {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                let requests = requests.clone();
                Box::pin(async move {
                    Ok(Box::new(RecordingTransport { requests }) as Box<dyn McpTransport>)
                })
            })
        });
        (client, requests, factory_calls)
    }

    #[tokio::test]
    async fn client_connects_lazily_and_reuses_transport() {
        let (client, requests, factory_calls) = test_client();
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0);

        let (tools, display) = client.list_tools().await.expect("list MCP tools");
        assert!(tools.contains("**echo**"));
        assert!(matches!(
            display.as_deref(),
            Some([DisplaySegment::Text(text)]) if text == "Loaded 1 tools"
        ));

        let result = client
            .invoke_tool("echo", serde_json::json!({"text": "hello"}))
            .await
            .expect("invoke MCP tool");
        assert!(result.contains("hello"));
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);

        let requests = requests.lock().await;
        assert_eq!(requests[0].0, "tools/list");
        assert_eq!(requests[1].0, "tools/call");
        assert_eq!(
            requests[1].1.as_ref().expect("call parameters")["name"],
            "echo"
        );
    }

    #[tokio::test]
    async fn dispatch_routes_operations_and_formats_errors() {
        let (client, requests, _) = test_client();

        let (text, display) = client
            .dispatch("test_list_tools", &serde_json::json!({}))
            .await;
        assert!(text.contains("**echo**"));
        assert!(display.is_some());

        let (text, display) = client
            .dispatch(
                "test_invoke_tool",
                &serde_json::json!({"tool_name": "echo", "arguments": {"text": "hello"}}),
            )
            .await;
        assert!(text.contains("hello"));
        assert!(display.is_none());

        let (text, display) = client.dispatch("bogus", &serde_json::json!({})).await;
        assert_eq!(text, "MCP tool error: unknown operation: bogus");
        assert!(display.is_none());

        let requests = requests.lock().await;
        assert_eq!(
            requests.len(),
            2,
            "unknown operation must not hit the server"
        );
    }
}
