//! Optional bridge between [rig](https://docs.rs/rig-core) provider backends
//! and the rig-free [`infinity_provider_protocol`] abstraction.
//!
//! The infinity provider stack does not depend on rig: the protocol crate
//! defines its own message/request/stream types, and the in-tree Bedrock
//! provider talks to AWS directly. This crate is a standalone helper for
//! anyone who wants to serve one of rig's many other backends (OpenAI,
//! Anthropic, Gemini, ...) as an infinity [`ModelProvider`]:
//!
//! ```no_run
//! # async fn example() {
//! use infinity_provider_rig::RigCompletionModel;
//! use infinity_provider_protocol::ModelEntry;
//! use rig::client::{CompletionClient, ProviderClient};
//!
//! let client = rig::providers::anthropic::Client::from_env();
//! let model = client.completion_model("claude-sonnet-4-5");
//! let provider = RigCompletionModel::new(model).into_provider(ModelEntry {
//!     model_id: "claude-sonnet-4-5".to_owned(),
//!     display_name: "Claude Sonnet 4.5".to_owned(),
//!     context_window: 200_000,
//!     max_output_tokens: Some(64_000),
//!     supports_image_input: true,
//! });
//! # let _ = provider;
//! # }
//! ```
//!
//! [`convert`] additionally exposes the raw type conversions for callers that
//! implement [`ModelProvider`] themselves (e.g. to inject per-model request
//! parameters before invoking the rig model).
//!
//! [`ModelProvider`]: infinity_provider_protocol::ModelProvider

pub mod convert;

use async_trait::async_trait;
use futures_util::StreamExt;
use infinity_provider_protocol::{
    CompletionError, CompletionModel, CompletionRequest, ModelEntry, ModelStream,
    SingleModelProvider,
};
use rig::completion::GetTokenUsage;
use rig::streaming::StreamingCompletionResponse;

/// Adapter exposing a rig [`CompletionModel`](rig::completion::CompletionModel)
/// as an [`infinity_provider_protocol::CompletionModel`].
///
/// Requests are converted with [`convert::request_to_rig`] and the streamed
/// response items with [`convert::chunk_from_rig`].
pub struct RigCompletionModel<M> {
    model: M,
}

impl<M> RigCompletionModel<M>
where
    M: rig::completion::CompletionModel + Send + Sync,
    M::StreamingResponse: Send + 'static,
{
    pub fn new(model: M) -> Self {
        Self { model }
    }

    /// Wrap this model in a [`SingleModelProvider`] advertising `entry`.
    pub fn into_provider(self, entry: ModelEntry) -> SingleModelProvider<Self> {
        SingleModelProvider::new(entry, self)
    }
}

#[async_trait]
impl<M> CompletionModel for RigCompletionModel<M>
where
    M: rig::completion::CompletionModel + Send + Sync,
    M::StreamingResponse: Send + 'static,
{
    async fn stream(&self, request: CompletionRequest) -> Result<ModelStream, CompletionError> {
        let request = convert::request_to_rig(request)?;
        let response = self
            .model
            .stream(request)
            .await
            .map_err(convert::error_from_rig)?;
        Ok(adapt_stream(response))
    }
}

/// Convert a rig streaming response into a [`ModelStream`], mapping each
/// streamed item (and any mid-stream error) to protocol types.
pub fn adapt_stream<R>(response: StreamingCompletionResponse<R>) -> ModelStream
where
    R: Clone + Unpin + GetTokenUsage + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut response = response;
        while let Some(item) = response.next().await {
            yield item
                .map(convert::chunk_from_rig)
                .map_err(convert::error_from_rig);
        }
    })
}
