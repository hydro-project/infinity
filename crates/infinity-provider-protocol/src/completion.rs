//! Completion request, response streaming, and error types.

use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};

use crate::message::{Message, Reasoning, ToolCall};

/// A request to stream one completion from a model.
///
/// This is the entire API surface providers must support; anything
/// provider-specific (thinking configuration, beta flags, ...) travels in
/// `additional_params` or is applied by the provider itself.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// The system prompt.
    pub preamble: Option<String>,
    /// The conversation so far, ending with the content to complete.
    pub chat_history: Vec<Message>,
    /// Definitions of the tools available to the model.
    pub tools: Vec<ToolDefinition>,
    /// Maximum number of output tokens. `None` uses the provider's default.
    pub max_tokens: Option<u64>,
    /// Extra provider-specific request parameters, merged into the request
    /// by the provider.
    pub additional_params: Option<serde_json::Value>,
}

/// Definition of one tool the model may call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON schema of the tool's arguments.
    pub parameters: serde_json::Value,
}

/// Token usage reported by a provider for one completion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// The number of input ("prompt") tokens used in a given request.
    #[serde(default)]
    pub input_tokens: u64,
    /// The number of output ("completion") tokens used in a given request.
    #[serde(default)]
    pub output_tokens: u64,
    /// Stored separately as some providers only report one number.
    #[serde(default)]
    pub total_tokens: u64,
    /// The number of cached input tokens (from prompt caching). 0 if not
    /// reported by the provider.
    #[serde(default)]
    pub cached_input_tokens: u64,
}

/// Error invoking a completion model.
#[derive(Debug, thiserror::Error)]
pub enum CompletionError {
    /// Error building the completion request.
    #[error("RequestError: {0}")]
    RequestError(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Error parsing the completion response.
    #[error("ResponseError: {0}")]
    ResponseError(String),

    /// Error returned by the completion model provider.
    #[error("ProviderError: {0}")]
    ProviderError(String),

    /// JSON (de)serialization error.
    #[error("JsonError: {0}")]
    JsonError(#[from] serde_json::Error),
}

/// One streamed item of a completion response.
///
/// Deltas stream incremental UI-facing content; the non-delta variants
/// (`ToolCall`, `Reasoning`) carry the complete block to be recorded in
/// history. A well-formed stream ends with `Final` (though providers may
/// terminate without one, e.g. on network errors).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StreamChunk {
    /// A chunk of assistant text.
    Text(String),
    /// A complete tool call (all arguments accumulated).
    ToolCall(ToolCall),
    /// An incremental piece of an in-progress tool call.
    ToolCallDelta {
        /// Provider-supplied tool call id.
        id: String,
        content: ToolCallDeltaContent,
    },
    /// A complete reasoning block.
    Reasoning(Reasoning),
    /// A chunk of reasoning text.
    ReasoningDelta { id: Option<String>, text: String },
    /// The final chunk, carrying the completion's token usage (if the
    /// provider reports it).
    Final(FinalResponse),
}

/// The content of a [`StreamChunk::ToolCallDelta`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToolCallDeltaContent {
    /// The name of the tool being called (start of the call).
    Name(String),
    /// A fragment of the call's JSON arguments.
    Delta(String),
}

/// Payload of [`StreamChunk::Final`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FinalResponse {
    pub usage: Option<Usage>,
}

/// The streaming response returned by a model invocation.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>;
