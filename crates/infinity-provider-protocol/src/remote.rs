//! Unix-socket transport for [`ModelProvider`]s.
//!
//! This module lets any [`ModelProvider`] implementation run in a separate
//! process and be consumed remotely:
//!
//! * [`serve_provider`] exposes a provider over a Unix domain socket at a
//!   freshly generated temp path. Provider binaries call this, print the
//!   returned path on stdout, and then run the returned server future.
//! * [`RemoteModelProvider`] is the client side: it implements
//!   [`ModelProvider`] by forwarding calls over the socket.
//!
//! ## Protocol
//!
//! Newline-delimited JSON. Each request opens a fresh connection (so
//! concurrent invocations simply use concurrent connections): the client
//! sends one [`ProviderRequest`] line, then reads [`ProviderResponse`] lines.
//!
//! * `ListModels` → exactly one response: `Models` or `Error`.
//! * `InvokeModel` → either a single `Error` (the invocation failed), or
//!   `InvokeStarted` followed by zero or more `Chunk`s and a final
//!   `StreamEnd`. Mid-stream provider errors are forwarded as
//!   [`WireStreamItem::Error`] chunks without ending the stream, mirroring
//!   the in-process behavior where a stream may yield `Err` items.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rig::OneOrMany;
use rig::completion::{CompletionError, CompletionRequest, Document, ToolDefinition};
use rig::message::{Message, ReasoningContent, ToolChoice};
use rig::streaming::{
    RawStreamingChoice, RawStreamingToolCall, StreamedAssistantContent,
    StreamingCompletionResponse, ToolCallDeltaContent,
};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{Framed, LinesCodec};

use crate::{ModelEntry, ModelProvider, ProviderCompletionResponse, ProviderStreamingResponse};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A connection carrying one JSON value per line, in both directions.
type JsonLines = Framed<UnixStream, LinesCodec>;

// ── Wire types ──

/// A request sent from the client to the provider server. One request is
/// sent per connection.
#[derive(Debug, Serialize, Deserialize)]
pub enum ProviderRequest {
    /// List the models offered by the provider.
    ListModels,
    /// Invoke a model, streaming the completion response back as
    /// [`ProviderResponse::Chunk`]s.
    InvokeModel {
        model_id: String,
        request: Box<WireCompletionRequest>,
    },
}

/// A response line sent from the provider server to the client.
#[derive(Debug, Serialize, Deserialize)]
pub enum ProviderResponse {
    /// Reply to [`ProviderRequest::ListModels`].
    Models(Vec<ModelEntry>),
    /// The invocation succeeded; `Chunk`s follow.
    InvokeStarted,
    /// One streamed item of an invocation.
    Chunk(WireStreamItem),
    /// The invocation stream finished; the connection will close.
    StreamEnd,
    /// The request failed (sent in place of `Models` / `InvokeStarted`).
    Error(String),
}

/// Serializable mirror of rig's [`CompletionRequest`] (which doesn't derive
/// serde itself, although all of its fields are serializable).
#[derive(Debug, Serialize, Deserialize)]
pub struct WireCompletionRequest {
    pub model: Option<String>,
    pub preamble: Option<String>,
    pub chat_history: OneOrMany<Message>,
    pub documents: Vec<Document>,
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub additional_params: Option<serde_json::Value>,
    pub output_schema: Option<schemars::Schema>,
}

impl From<CompletionRequest> for WireCompletionRequest {
    fn from(r: CompletionRequest) -> Self {
        Self {
            model: r.model,
            preamble: r.preamble,
            chat_history: r.chat_history,
            documents: r.documents,
            tools: r.tools,
            temperature: r.temperature,
            max_tokens: r.max_tokens,
            tool_choice: r.tool_choice,
            additional_params: r.additional_params,
            output_schema: r.output_schema,
        }
    }
}

impl From<WireCompletionRequest> for CompletionRequest {
    fn from(r: WireCompletionRequest) -> Self {
        Self {
            model: r.model,
            preamble: r.preamble,
            chat_history: r.chat_history,
            documents: r.documents,
            tools: r.tools,
            temperature: r.temperature,
            max_tokens: r.max_tokens,
            tool_choice: r.tool_choice,
            additional_params: r.additional_params,
            output_schema: r.output_schema,
        }
    }
}

/// Serializable mirror of [`RawStreamingChoice<ProviderStreamingResponse>`],
/// plus an `Error` variant carrying mid-stream provider errors.
#[derive(Debug, Serialize, Deserialize)]
pub enum WireStreamItem {
    /// A text chunk.
    Message(String),
    /// A complete tool call.
    ToolCall {
        id: String,
        internal_call_id: String,
        call_id: Option<String>,
        name: String,
        arguments: serde_json::Value,
        signature: Option<String>,
        additional_params: Option<serde_json::Value>,
    },
    /// A tool call partial/delta.
    ToolCallDelta {
        id: String,
        internal_call_id: String,
        content: ToolCallDeltaContent,
    },
    /// A reasoning block (in its entirety).
    Reasoning {
        id: Option<String>,
        content: ReasoningContent,
    },
    /// A reasoning partial/delta.
    ReasoningDelta {
        id: Option<String>,
        reasoning: String,
    },
    /// The final response carrying token usage.
    Final(ProviderStreamingResponse),
    /// A mid-stream error from the underlying provider.
    Error(String),
}

/// Convert one streamed item from the server-side provider into wire items.
/// `Reasoning` fans out into one item per content block, mirroring
/// [`crate::erase_streaming_response`].
fn content_to_wire(
    item: Result<StreamedAssistantContent<ProviderStreamingResponse>, CompletionError>,
) -> Vec<WireStreamItem> {
    match item {
        Ok(StreamedAssistantContent::Text(t)) => vec![WireStreamItem::Message(t.text)],
        Ok(StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        }) => vec![WireStreamItem::ToolCall {
            id: tool_call.id,
            internal_call_id,
            call_id: tool_call.call_id,
            name: tool_call.function.name,
            arguments: tool_call.function.arguments,
            signature: tool_call.signature,
            additional_params: tool_call.additional_params,
        }],
        Ok(StreamedAssistantContent::ToolCallDelta {
            id,
            internal_call_id,
            content,
        }) => vec![WireStreamItem::ToolCallDelta {
            id,
            internal_call_id,
            content,
        }],
        Ok(StreamedAssistantContent::Reasoning(reasoning)) => reasoning
            .content
            .into_iter()
            .map(|content| WireStreamItem::Reasoning {
                id: reasoning.id.clone(),
                content,
            })
            .collect(),
        Ok(StreamedAssistantContent::ReasoningDelta { id, reasoning }) => {
            vec![WireStreamItem::ReasoningDelta { id, reasoning }]
        }
        Ok(StreamedAssistantContent::Final(r)) => vec![WireStreamItem::Final(r)],
        Err(e) => vec![WireStreamItem::Error(e.to_string())],
    }
}

/// Convert a wire item back into a client-side streaming choice.
fn wire_to_choice(
    item: WireStreamItem,
) -> Result<RawStreamingChoice<ProviderStreamingResponse>, CompletionError> {
    Ok(match item {
        WireStreamItem::Message(text) => RawStreamingChoice::Message(text),
        WireStreamItem::ToolCall {
            id,
            internal_call_id,
            call_id,
            name,
            arguments,
            signature,
            additional_params,
        } => RawStreamingChoice::ToolCall(RawStreamingToolCall {
            id,
            internal_call_id,
            call_id,
            name,
            arguments,
            signature,
            additional_params,
        }),
        WireStreamItem::ToolCallDelta {
            id,
            internal_call_id,
            content,
        } => RawStreamingChoice::ToolCallDelta {
            id,
            internal_call_id,
            content,
        },
        WireStreamItem::Reasoning { id, content } => RawStreamingChoice::Reasoning { id, content },
        WireStreamItem::ReasoningDelta { id, reasoning } => {
            RawStreamingChoice::ReasoningDelta { id, reasoning }
        }
        WireStreamItem::Final(r) => RawStreamingChoice::FinalResponse(r),
        WireStreamItem::Error(e) => return Err(CompletionError::ProviderError(e)),
    })
}

// ── Framing ──
//
// Connections are framed with tokio's [`LinesCodec`]: one JSON value per
// line in each direction.

/// Serialize `value` as JSON and send it as one line.
async fn send_json<T: Serialize>(framed: &mut JsonLines, value: &T) -> std::io::Result<()> {
    let line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    framed.send(line).await.map_err(std::io::Error::other)
}

/// Receive one line and parse it as JSON. Returns `Ok(None)` on a clean EOF.
async fn recv_json<T: for<'de> Deserialize<'de>>(
    framed: &mut JsonLines,
) -> std::io::Result<Option<T>> {
    match framed.next().await {
        None => Ok(None),
        Some(Ok(line)) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Some(Err(e)) => Err(std::io::Error::other(e)),
    }
}

// ── Server ──

/// Generate a fresh socket path in the system temp directory. Kept short
/// because Unix socket paths have a low length limit (~104 bytes on macOS).
fn new_socket_path() -> PathBuf {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("inf-provider-{}.sock", &suffix[..12]))
}

/// Expose a [`ModelProvider`] over a Unix domain socket at a freshly
/// generated temp path.
///
/// Returns the socket path and the server future. Provider binaries should
/// print the path on stdout (so a supervisor like the daemon can discover
/// it) and then await the future, which serves connections forever.
///
/// Must be called from within a tokio runtime.
pub fn serve_provider(
    provider: Arc<dyn ModelProvider>,
) -> std::io::Result<(PathBuf, impl Future<Output = ()> + Send)> {
    let path = new_socket_path();
    let listener = UnixListener::bind(&path)?;
    let serve = async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let provider = provider.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(provider, stream).await {
                            tracing::warn!("model provider connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("model provider accept error: {e}");
                    // Avoid spinning hot on persistent accept failures.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    };
    Ok((path, serve))
}

/// Serve a single connection: read one request, write the response(s).
async fn handle_connection(
    provider: Arc<dyn ModelProvider>,
    stream: UnixStream,
) -> std::io::Result<()> {
    let mut framed = Framed::new(stream, LinesCodec::new());

    let request = match recv_json::<ProviderRequest>(&mut framed).await {
        // Client disconnected without sending a request.
        Ok(None) => return Ok(()),
        Ok(Some(request)) => request,
        Err(e) => {
            send_json(
                &mut framed,
                &ProviderResponse::Error(format!("invalid request: {e}")),
            )
            .await?;
            return Ok(());
        }
    };

    match request {
        ProviderRequest::ListModels => {
            let response = match provider.list_models().await {
                Ok(models) => ProviderResponse::Models(models),
                Err(e) => ProviderResponse::Error(e.to_string()),
            };
            send_json(&mut framed, &response).await?;
        }
        ProviderRequest::InvokeModel { model_id, request } => {
            handle_invoke(provider, &model_id, (*request).into(), &mut framed).await?;
        }
    }
    Ok(())
}

/// Invoke the model and forward its stream as `Chunk` lines.
async fn handle_invoke(
    provider: Arc<dyn ModelProvider>,
    model_id: &str,
    request: CompletionRequest,
    framed: &mut JsonLines,
) -> std::io::Result<()> {
    let mut response = match provider.invoke_model(model_id, request).await {
        Ok(response) => response,
        Err(e) => {
            send_json(framed, &ProviderResponse::Error(e.to_string())).await?;
            return Ok(());
        }
    };
    send_json(framed, &ProviderResponse::InvokeStarted).await?;
    while let Some(item) = response.next().await {
        for wire in content_to_wire(item) {
            send_json(framed, &ProviderResponse::Chunk(wire)).await?;
        }
    }
    send_json(framed, &ProviderResponse::StreamEnd).await
}

// ── Client ──

/// A [`ModelProvider`] that forwards all calls to a provider process served
/// over a Unix domain socket (see [`serve_provider`]).
pub struct RemoteModelProvider {
    socket_path: PathBuf,
}

impl RemoteModelProvider {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Open a fresh connection and send a single request line.
    async fn connect_and_send(&self, request: &ProviderRequest) -> std::io::Result<JsonLines> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let mut framed = Framed::new(stream, LinesCodec::new());
        send_json(&mut framed, request).await?;
        Ok(framed)
    }
}

#[async_trait]
impl ModelProvider for RemoteModelProvider {
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        let mut framed = self.connect_and_send(&ProviderRequest::ListModels).await?;
        match recv_json::<ProviderResponse>(&mut framed).await? {
            Some(ProviderResponse::Models(models)) => Ok(models),
            Some(ProviderResponse::Error(e)) => Err(e.into()),
            Some(_) => Err("unexpected response from model provider".into()),
            None => Err("model provider closed the connection without responding".into()),
        }
    }

    async fn invoke_model(
        &self,
        model_id: &str,
        request: CompletionRequest,
    ) -> Result<ProviderCompletionResponse, CompletionError> {
        let mut framed = self
            .connect_and_send(&ProviderRequest::InvokeModel {
                model_id: model_id.to_owned(),
                request: Box::new(request.into()),
            })
            .await
            .map_err(|e| {
                CompletionError::ProviderError(format!(
                    "failed to reach model provider at {}: {e}",
                    self.socket_path.display()
                ))
            })?;

        match recv_json::<ProviderResponse>(&mut framed)
            .await
            .map_err(|e| {
                CompletionError::ResponseError(format!("failed reading from model provider: {e}"))
            })? {
            Some(ProviderResponse::InvokeStarted) => {}
            Some(ProviderResponse::Error(e)) => return Err(CompletionError::ProviderError(e)),
            Some(_) => {
                return Err(CompletionError::ResponseError(
                    "unexpected response from model provider".to_owned(),
                ));
            }
            None => {
                return Err(CompletionError::ResponseError(
                    "model provider closed the connection without responding".to_owned(),
                ));
            }
        }

        let stream = async_stream::stream! {
            loop {
                match recv_json::<ProviderResponse>(&mut framed).await {
                    Ok(Some(ProviderResponse::Chunk(item))) => yield wire_to_choice(item),
                    Ok(Some(ProviderResponse::StreamEnd)) => break,
                    Ok(Some(_)) => {
                        yield Err(CompletionError::ResponseError(
                            "unexpected mid-stream response from model provider".to_owned(),
                        ));
                        break;
                    }
                    Ok(None) => {
                        yield Err(CompletionError::ResponseError(
                            "model provider closed the connection mid-stream".to_owned(),
                        ));
                        break;
                    }
                    Err(e) => {
                        yield Err(CompletionError::ResponseError(format!(
                            "failed reading from model provider: {e}"
                        )));
                        break;
                    }
                }
            }
        };
        Ok(StreamingCompletionResponse::stream(Box::pin(stream)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SingleModelProvider;
    use rig::completion::Usage;
    use rig::message::UserContent;
    use rig_mock::mock_model;

    fn test_entry() -> ModelEntry {
        ModelEntry {
            model_id: "mock".to_owned(),
            display_name: "mock".to_owned(),
            context_window: 100,
            max_output_tokens: None,
            supports_image_input: true,
        }
    }

    fn test_request(prompt: &str) -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: Some("system prompt".to_owned()),
            chat_history: OneOrMany::one(Message::User {
                content: OneOrMany::one(UserContent::text(prompt)),
            }),
            documents: vec![],
            tools: vec![],
            temperature: None,
            max_tokens: Some(42),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        }
    }

    #[tokio::test]
    async fn round_trip_over_unix_socket() {
        let (model, mut ctrl) = mock_model();
        let provider = Arc::new(SingleModelProvider::new(test_entry(), model));
        let (path, server) = serve_provider(provider).expect("bind provider socket");
        tokio::spawn(server);

        let remote = RemoteModelProvider::new(&path);

        // list_models round-trips the provider's entries.
        let models = remote.list_models().await.expect("list models");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "mock");
        assert_eq!(models[0].context_window, 100);
        assert!(models[0].supports_image_input);

        // invoke_model forwards the request and streams the response back.
        let mut response = remote
            .invoke_model("mock", test_request("hello"))
            .await
            .expect("invoke model");

        let request = ctrl.next_request().await;
        assert_eq!(request.preamble.as_deref(), Some("system prompt"));
        assert_eq!(request.max_tokens, Some(42));

        ctrl.send_text("hello ");
        ctrl.send_text("world");
        ctrl.finish_with_usage(Some(Usage {
            input_tokens: 7,
            output_tokens: 3,
            ..Usage::new()
        }));

        let mut text = String::new();
        let mut usage = None;
        while let Some(item) = response.next().await {
            match item.expect("stream item") {
                StreamedAssistantContent::Text(t) => text.push_str(&t.text),
                StreamedAssistantContent::Final(r) => usage = r.usage,
                other => panic!("unexpected stream item: {other:?}"),
            }
        }
        assert_eq!(text, "hello world");
        let usage = usage.expect("final usage");
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn connecting_to_missing_socket_fails_cleanly() {
        let remote = RemoteModelProvider::new("/nonexistent/provider.sock");
        match remote.invoke_model("mock", test_request("hi")).await {
            Err(CompletionError::ProviderError(_)) => {}
            Err(other) => panic!("unexpected error variant: {other}"),
            Ok(_) => panic!("connect should fail"),
        }
        assert!(remote.list_models().await.is_err());
    }
}
