//! Extensible model provider abstraction and its remote wire protocol.
//!
//! This crate is intentionally lightweight so that out-of-process provider
//! implementations (e.g. `infinity-provider-bedrock`) can depend on it
//! without pulling in the rest of the agent stack.
//!
//! A [`ModelProvider`] exposes async APIs for listing the models it offers and
//! for invoking one of them. Implementations internally wrap a rig
//! [`CompletionModel`] and are responsible for any provider-specific request
//! parameters (e.g. Bedrock's `additional_model_request_fields`), so callers
//! only ever deal in plain rig [`CompletionRequest`]s.
//!
//! The trait is dyn-compatible (via `async_trait`), which requires erasing the
//! provider-specific streaming response type. [`ProviderStreamingResponse`] is
//! the erased final-response type: it carries only the token usage, which is
//! all downstream code needs.

use async_trait::async_trait;
use futures_util::StreamExt;
use rig::completion::{CompletionError, CompletionModel, CompletionRequest, GetTokenUsage, Usage};
use rig::streaming::{
    RawStreamingChoice, RawStreamingToolCall, StreamedAssistantContent, StreamingCompletionResponse,
};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub mod remote;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Type-erased final streaming response yielded by [`ModelProvider::invoke_model`].
/// Carries the token usage reported by the underlying provider, if any.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStreamingResponse {
    pub usage: Option<Usage>,
}

impl GetTokenUsage for ProviderStreamingResponse {
    fn token_usage(&self) -> Option<Usage> {
        self.usage
    }
}

/// The streaming response type returned by [`ModelProvider::invoke_model`] —
/// identical to what [`CompletionModel::stream`] returns, with the
/// provider-specific final response erased to [`ProviderStreamingResponse`].
pub type ProviderCompletionResponse = StreamingCompletionResponse<ProviderStreamingResponse>;

/// A model offered by a [`ModelProvider`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Provider-scoped identifier for this model. Must be unique within the
    /// provider, but need not match the upstream API's model id (providers may
    /// expose multiple configurations of the same upstream model).
    pub model_id: String,
    /// Human-readable name shown in pickers.
    pub display_name: String,
    /// Context window size in tokens (used for compaction thresholds).
    pub context_window: usize,
    /// Maximum number of output tokens the model can generate per request.
    /// `None` falls back to the provider's default.
    pub max_output_tokens: Option<u64>,
}

/// A backend that can list and invoke completion models.
///
/// Implementations wrap rig [`CompletionModel`]s internally and handle any
/// provider-specific request parameters inside [`invoke_model`](Self::invoke_model).
/// Providers have no identity of their own — callers that manage multiple
/// providers (e.g. the daemon) assign each one a stable unique id at
/// registration time.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// List the models available from this provider. The first entry is the
    /// provider's default model.
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError>;

    /// Invoke a model by its provider-scoped id, streaming the completion
    /// response. Behaves exactly like [`CompletionModel::stream`], with the
    /// streaming response type erased.
    async fn invoke_model(
        &self,
        model_id: &str,
        request: CompletionRequest,
    ) -> Result<ProviderCompletionResponse, CompletionError>;
}

/// Erase the provider-specific streaming response type of a rig
/// [`StreamingCompletionResponse`], preserving the streamed content and the
/// final token usage.
pub fn erase_streaming_response<R>(
    response: StreamingCompletionResponse<R>,
) -> ProviderCompletionResponse
where
    R: Clone + Unpin + GetTokenUsage + Send + 'static,
{
    let stream = async_stream::stream! {
        let mut response = response;
        while let Some(item) = response.next().await {
            match item {
                Ok(StreamedAssistantContent::Text(t)) => {
                    yield Ok(RawStreamingChoice::Message(t.text));
                }
                Ok(StreamedAssistantContent::ToolCall {
                    tool_call,
                    internal_call_id,
                }) => {
                    yield Ok(RawStreamingChoice::ToolCall(RawStreamingToolCall {
                        id: tool_call.id,
                        internal_call_id,
                        call_id: tool_call.call_id,
                        name: tool_call.function.name,
                        arguments: tool_call.function.arguments,
                        signature: tool_call.signature,
                        additional_params: tool_call.additional_params,
                    }));
                }
                Ok(StreamedAssistantContent::ToolCallDelta {
                    id,
                    internal_call_id,
                    content,
                }) => {
                    yield Ok(RawStreamingChoice::ToolCallDelta {
                        id,
                        internal_call_id,
                        content,
                    });
                }
                Ok(StreamedAssistantContent::Reasoning(reasoning)) => {
                    for content in reasoning.content {
                        yield Ok(RawStreamingChoice::Reasoning {
                            id: reasoning.id.clone(),
                            content,
                        });
                    }
                }
                Ok(StreamedAssistantContent::ReasoningDelta { id, reasoning }) => {
                    yield Ok(RawStreamingChoice::ReasoningDelta { id, reasoning });
                }
                Ok(StreamedAssistantContent::Final(r)) => {
                    yield Ok(RawStreamingChoice::FinalResponse(ProviderStreamingResponse {
                        usage: r.token_usage(),
                    }));
                }
                Err(e) => {
                    yield Err(e);
                }
            }
        }
    };
    StreamingCompletionResponse::stream(Box::pin(stream))
}

/// Adapter exposing a single rig [`CompletionModel`] as a [`ModelProvider`].
/// Useful for tests and simple single-model deployments. Provider-specific
/// request parameters are passed through unchanged.
pub struct SingleModelProvider<M: CompletionModel> {
    entry: ModelEntry,
    model: M,
}

impl<M: CompletionModel> SingleModelProvider<M> {
    pub fn new(entry: ModelEntry, model: M) -> Self {
        Self { entry, model }
    }
}

#[async_trait]
impl<M> ModelProvider for SingleModelProvider<M>
where
    M: CompletionModel + Send + Sync,
    M::StreamingResponse: Send + 'static,
{
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        Ok(vec![self.entry.clone()])
    }

    async fn invoke_model(
        &self,
        _model_id: &str,
        request: CompletionRequest,
    ) -> Result<ProviderCompletionResponse, CompletionError> {
        Ok(erase_streaming_response(self.model.stream(request).await?))
    }
}
