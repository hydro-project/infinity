//! Tests against the real Bedrock service, gated behind the non-default
//! `live-tests` feature because they need working AWS credentials (and cost
//! a few tokens):
//!
//! ```sh
//! cargo test -p infinity-provider-bedrock --features live-tests
//! ```
//!
//! These exist because the error *classification* is a table of string/code
//! heuristics matched against real Bedrock responses — unit-testing that
//! table against copies of the same strings would be tautological. Instead,
//! we provoke real service errors and assert the classification.
#![cfg(feature = "live-tests")]

use futures_util::StreamExt;
use infinity_provider_bedrock::BedrockProvider;
use infinity_provider_protocol::{
    CompletionRequest, ErrorClass, Message, ModelProvider, StreamChunk,
};

const MODEL: &str = "global.anthropic.claude-sonnet-4-6";

fn request(prompt: impl Into<String>) -> CompletionRequest {
    CompletionRequest {
        preamble: None,
        chat_history: vec![Message::user(prompt.into())],
        tools: vec![],
        max_tokens: Some(64),
        additional_params: Some(serde_json::json!({ "thinking": { "type": "disabled" } })),
    }
}

/// An input far beyond the model's 200k-token context window must be
/// classified as [`ErrorClass::ContextOverflow`] — this is what lets
/// agent-core recover from oversized inputs instead of hanging.
#[tokio::test]
async fn oversized_input_is_classified_as_context_overflow() {
    let provider = BedrockProvider::from_env();
    // ~12M characters ≈ >2M tokens, comfortably past any context window
    // Bedrock currently offers for this model.
    let huge = "lorem ipsum dolor sit amet consectetur ".repeat(300_000);
    let err = match provider.invoke_model(MODEL, request(huge)).await {
        Err(e) => e,
        // Some backends only report the overflow once the stream starts.
        Ok(mut stream) => loop {
            match stream.next().await {
                Some(Err(e)) => break e,
                Some(Ok(_)) => continue,
                None => panic!("oversized request unexpectedly succeeded"),
            }
        },
    };
    assert_eq!(
        err.class(),
        ErrorClass::ContextOverflow,
        "expected ContextOverflow, got {:?} for: {err}",
        err.class()
    );
}

/// A nonexistent model id is a permanent error: retrying can never help.
#[tokio::test]
async fn unknown_model_is_classified_as_fatal() {
    let provider = BedrockProvider::from_env();
    let err = provider
        .invoke_model("anthropic.does-not-exist-v0", request("hi"))
        .await
        .err()
        .expect("invoking a nonexistent model must fail");
    assert_eq!(
        err.class(),
        ErrorClass::Fatal,
        "expected Fatal, got {:?} for: {err}",
        err.class()
    );
}

/// Happy-path sanity check: a small request streams text and finishes,
/// proving the initiation timeout wrapper doesn't interfere with normal
/// operation.
#[tokio::test]
async fn small_request_streams_text() {
    let provider = BedrockProvider::from_env();
    let mut stream = provider
        .invoke_model(MODEL, request("Reply with the single word: ok"))
        .await
        .expect("invoke model");
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let StreamChunk::Text(t) = item.expect("stream item") {
            text.push_str(&t);
        }
    }
    assert!(!text.is_empty(), "expected some streamed text");
}
