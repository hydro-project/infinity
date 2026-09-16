//! Extensible model provider abstraction and its remote wire protocol.
//!
//! This crate is intentionally lightweight (no HTTP stack, no LLM SDKs) so
//! that both the agent runtime and out-of-process provider implementations
//! (e.g. `infinity-provider-bedrock`) can depend on it.
//!
//! It defines:
//!
//! * The chat [`message`] types and [`completion`] request/stream types that
//!   form the entire model API surface of this project.
//! * [`ModelProvider`] — an async trait for listing models and invoking one
//!   of them, returning a [`ModelStream`] of [`StreamChunk`]s.
//! * [`remote`] (feature `remote`) — a Unix-socket transport so providers
//!   can run out of process.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod completion;
pub mod message;
#[cfg(feature = "mock")]
pub mod mock;
// The remote transport is unix-only for now (Unix domain sockets); the
// `remote` feature is the stable switch so a Windows transport can slot in
// behind the same flag later.
#[cfg(all(unix, feature = "remote"))]
pub mod remote;

pub use completion::{
    CompletionError, CompletionRequest, ErrorClass, FinalResponse, ModelStream, StreamChunk,
    ToolCallDeltaContent, ToolDefinition, Usage,
};
pub use message::{
    AssistantContent, Image, ImageMediaType, ImageSource, Message, Reasoning, ReasoningContent,
    Text, ToolCall, ToolFunction, ToolResult, ToolResultContent, UserContent,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
    /// Whether the model accepts image content in its input (e.g. image
    /// tool results). When `false`, the runtime replaces image content with
    /// a text placeholder before invoking the model.
    #[serde(default)]
    pub supports_image_input: bool,
}

/// A backend that can list and invoke completion models.
///
/// Implementations handle any provider-specific request parameters inside
/// [`invoke_model`](Self::invoke_model). Providers have no identity of their
/// own — callers that manage multiple providers (e.g. the daemon) assign each
/// one a stable unique id at registration time.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// List the models available from this provider. The first entry is the
    /// provider's default model.
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError>;

    /// Invoke a model by its provider-scoped id, streaming the completion
    /// response.
    async fn invoke_model(
        &self,
        model_id: &str,
        request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError>;
}

/// A single completion model (as opposed to a provider, which is a catalog
/// of models). Mostly useful for tests; see [`SingleModelProvider`].
#[async_trait]
pub trait CompletionModel: Send + Sync {
    /// Stream one completion.
    async fn stream(&self, request: CompletionRequest) -> Result<ModelStream, CompletionError>;
}

/// Adapter exposing a single [`CompletionModel`] as a [`ModelProvider`].
/// Useful for tests and simple single-model deployments.
pub struct SingleModelProvider<M> {
    entry: ModelEntry,
    model: M,
}

impl<M: CompletionModel> SingleModelProvider<M> {
    pub fn new(entry: ModelEntry, model: M) -> Self {
        Self { entry, model }
    }
}

#[async_trait]
impl<M: CompletionModel> ModelProvider for SingleModelProvider<M> {
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        Ok(vec![self.entry.clone()])
    }

    async fn invoke_model(
        &self,
        _model_id: &str,
        request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError> {
        self.model.stream(request).await
    }
}
