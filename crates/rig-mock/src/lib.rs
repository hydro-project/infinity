//! Channel-based mock implementation of rig's `CompletionModel` for testing.
//!
//! Call [`mock_model()`] to get a `(MockCompletionModel, MockModelController)` pair.
//! The model is passed to production code; the controller drives it from the test.

use rig::OneOrMany;
use rig::completion::{
    CompletionError, CompletionRequest, CompletionResponse, GetTokenUsage, Usage,
};
use rig::message::AssistantContent;
use rig::streaming::{RawStreamingChoice, StreamingCompletionResponse, StreamingResult};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ── MockStreamingResponse ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockStreamingResponse;

impl GetTokenUsage for MockStreamingResponse {
    fn token_usage(&self) -> Option<Usage> {
        None
    }
}

// ── Internal handshake ──

/// Sent from the model to the controller for each `stream()` call.
struct StreamRound {
    request: CompletionRequest,
    /// Controller sends chunks through here; model reads them.
    chunk_tx:
        mpsc::UnboundedSender<Result<RawStreamingChoice<MockStreamingResponse>, CompletionError>>,
}

// ── MockCompletionModel ──

#[derive(Clone)]
pub struct MockCompletionModel {
    round_tx: mpsc::UnboundedSender<StreamRound>,
}

impl rig::completion::CompletionModel for MockCompletionModel {
    type Response = serde_json::Value;
    type StreamingResponse = MockStreamingResponse;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        panic!("Use mock_model() instead");
    }

    async fn completion(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // Non-streaming not needed for agent-core tests.
        Ok(CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text("")),
            usage: Usage::new(),
            raw_response: serde_json::Value::Null,
            message_id: None,
        })
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();

        // Notify controller of this round.
        let _ = self.round_tx.send(StreamRound { request, chunk_tx });

        // Build a stream that reads from the channel.
        let stream = async_stream::stream! {
            while let Some(item) = chunk_rx.recv().await {
                yield item;
            }
        };

        let pinned: StreamingResult<MockStreamingResponse> = Box::pin(stream);
        Ok(StreamingCompletionResponse::stream(pinned))
    }
}

// ── MockModelController ──

/// Test-side handle for driving the mock model.
pub struct MockModelController {
    round_rx: mpsc::UnboundedReceiver<StreamRound>,
    /// Sender for the current active round (set after `next_request`).
    current_tx: Option<
        mpsc::UnboundedSender<Result<RawStreamingChoice<MockStreamingResponse>, CompletionError>>,
    >,
}

impl MockModelController {
    /// Wait for the next `stream()` call and return the request.
    pub async fn next_request(&mut self) -> CompletionRequest {
        let round = self.round_rx.recv().await.expect("model dropped");
        self.current_tx = Some(round.chunk_tx);
        round.request
    }

    /// Send a raw streaming chunk.
    pub fn send_chunk(&self, chunk: RawStreamingChoice<MockStreamingResponse>) {
        self.tx().send(Ok(chunk)).ok();
    }

    /// Send a text chunk.
    pub fn send_text(&self, text: &str) {
        self.send_chunk(RawStreamingChoice::Message(text.to_owned()));
    }

    /// Send a complete tool call.
    pub fn send_tool_call(&self, id: &str, name: &str, args: serde_json::Value) {
        use rig::streaming::RawStreamingToolCall;
        self.send_chunk(RawStreamingChoice::ToolCall(RawStreamingToolCall::new(
            id.to_owned(),
            name.to_owned(),
            args,
        )));
    }

    /// Send the final response marker and drop the sender to close the stream.
    pub fn finish(&mut self) {
        self.send_chunk(RawStreamingChoice::FinalResponse(MockStreamingResponse));
        self.current_tx.take(); // drop closes the channel
    }

    /// Drop the stream sender without sending a Final marker.
    /// Simulates an unexpected stream termination (e.g. network error).
    pub fn drop_stream(&mut self) {
        self.current_tx.take();
    }

    /// Inject a stream error.
    pub fn send_error(&self, err: CompletionError) {
        self.tx().send(Err(err)).ok();
    }

    fn tx(
        &self,
    ) -> &mpsc::UnboundedSender<Result<RawStreamingChoice<MockStreamingResponse>, CompletionError>>
    {
        self.current_tx.as_ref().expect("call next_request() first")
    }
}

/// Create a mock model / controller pair.
pub fn mock_model() -> (MockCompletionModel, MockModelController) {
    let (round_tx, round_rx) = mpsc::unbounded_channel();
    (
        MockCompletionModel { round_tx },
        MockModelController {
            round_rx,
            current_tx: None,
        },
    )
}
