//! Conversion of the Bedrock ConverseStream event stream into a
//! [`ModelStream`] of [`StreamChunk`]s.

use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamOutput as ConverseStreamResponse;
use aws_sdk_bedrockruntime::types as bedrock;
use infinity_provider_protocol::{
    CompletionError, FinalResponse, ModelStream, Reasoning, ReasoningContent, StreamChunk,
    ToolCall, ToolCallDeltaContent, Usage,
};

/// An in-progress tool-use content block.
#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    input_json: String,
}

/// An in-progress reasoning content block.
#[derive(Default)]
struct ReasoningState {
    text: String,
    signature: Option<String>,
}

/// Finalize a completed tool-use content block into a full tool call.
fn finish_tool_call(state: ToolCallState) -> Result<StreamChunk, CompletionError> {
    // Tools without parameters stream no input at all.
    let arguments = if state.input_json.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&state.input_json)?
    };
    Ok(StreamChunk::ToolCall(ToolCall::new(
        state.id, state.name, arguments,
    )))
}

fn usage_from_metadata(usage: bedrock::TokenUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens as u64,
        output_tokens: usage.output_tokens as u64,
        total_tokens: usage.total_tokens as u64,
        cached_input_tokens: usage.cache_read_input_tokens.unwrap_or(0) as u64,
    }
}

/// Adapt the SDK event stream into a [`ModelStream`].
pub(crate) fn convert_stream(response: ConverseStreamResponse) -> ModelStream {
    Box::pin(async_stream::stream! {
        let mut stream = response.stream;
        let mut tool_call: Option<ToolCallState> = None;
        let mut reasoning: Option<ReasoningState> = None;

        loop {
            let output = match stream.recv().await {
                Ok(Some(output)) => output,
                Ok(None) => break,
                Err(e) => {
                    // Forward mid-stream transport/service errors instead of
                    // silently ending the stream; agent-core retries on some
                    // of these messages.
                    tracing::error!(error = ?e, "Bedrock ConverseStream receive error");
                    yield Err(CompletionError::ProviderError(crate::sdk_error_message(&e)));
                    break;
                }
            };

            match output {
                bedrock::ConverseStreamOutput::ContentBlockStart(event) => {
                    match event.start {
                        Some(bedrock::ContentBlockStart::ToolUse(start)) => {
                            tool_call = Some(ToolCallState {
                                id: start.tool_use_id.clone(),
                                name: start.name.clone(),
                                input_json: String::new(),
                            });
                            yield Ok(StreamChunk::ToolCallDelta {
                                id: start.tool_use_id,
                                content: ToolCallDeltaContent::Name(start.name),
                            });
                        }
                        _ => {
                            yield Err(CompletionError::ProviderError(
                                "AWS Bedrock sent an unsupported ContentBlockStart".to_owned(),
                            ));
                        }
                    }
                }
                bedrock::ConverseStreamOutput::ContentBlockDelta(event) => {
                    let Some(delta) = event.delta else {
                        yield Err(CompletionError::ProviderError(
                            "The delta for a content block is missing".to_owned(),
                        ));
                        continue;
                    };
                    match delta {
                        bedrock::ContentBlockDelta::Text(text) => {
                            // Text between tool-use start and stop belongs to
                            // the tool call, not the assistant message.
                            if tool_call.is_none() {
                                yield Ok(StreamChunk::Text(text));
                            }
                        }
                        bedrock::ContentBlockDelta::ToolUse(delta) => {
                            if let Some(state) = tool_call.as_mut() {
                                state.input_json.push_str(delta.input());
                                yield Ok(StreamChunk::ToolCallDelta {
                                    id: state.id.clone(),
                                    content: ToolCallDeltaContent::Delta(delta.input),
                                });
                            }
                        }
                        bedrock::ContentBlockDelta::ReasoningContent(delta) => match delta {
                            bedrock::ReasoningContentBlockDelta::Text(text) => {
                                reasoning
                                    .get_or_insert_with(ReasoningState::default)
                                    .text
                                    .push_str(&text);
                                if !text.is_empty() {
                                    yield Ok(StreamChunk::ReasoningDelta { id: None, text });
                                }
                            }
                            bedrock::ReasoningContentBlockDelta::Signature(signature) => {
                                reasoning
                                    .get_or_insert_with(ReasoningState::default)
                                    .signature = Some(signature);
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                bedrock::ConverseStreamOutput::ContentBlockStop(_) => {
                    // Each tool-use content block is closed individually;
                    // yield the completed tool call here so that multiple
                    // (concurrent) tool calls in a single assistant message
                    // are all emitted, not just the last one.
                    if let Some(state) = tool_call.take() {
                        yield finish_tool_call(state);
                    }
                    if let Some(state) = reasoning.take()
                        && !state.text.is_empty()
                    {
                        yield Ok(StreamChunk::Reasoning(Reasoning {
                            id: None,
                            content: vec![ReasoningContent::Text {
                                text: state.text,
                                signature: state.signature,
                            }],
                        }));
                    }
                }
                bedrock::ConverseStreamOutput::MessageStop(event) => match event.stop_reason {
                    bedrock::StopReason::ToolUse => {
                        // Tool calls are normally yielded at ContentBlockStop;
                        // this only fires if the stream ended without closing
                        // the block.
                        if let Some(state) = tool_call.take() {
                            yield finish_tool_call(state);
                        }
                    }
                    bedrock::StopReason::MaxTokens => {
                        yield Err(CompletionError::ProviderError(
                            "Exceeded max tokens".to_owned(),
                        ));
                    }
                    _ => {}
                },
                bedrock::ConverseStreamOutput::Metadata(event) => {
                    if let Some(usage) = event.usage {
                        yield Ok(StreamChunk::Final(FinalResponse {
                            usage: Some(usage_from_metadata(usage)),
                        }));
                    }
                }
                _ => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tool_input_becomes_empty_object() {
        let chunk = finish_tool_call(ToolCallState {
            id: "tc-1".to_owned(),
            name: "list_docs".to_owned(),
            input_json: String::new(),
        })
        .expect("finish tool call");
        let StreamChunk::ToolCall(call) = chunk else {
            panic!("expected tool call chunk");
        };
        assert_eq!(call.id, "tc-1");
        assert_eq!(call.function.name, "list_docs");
        assert_eq!(call.function.arguments, serde_json::json!({}));
    }

    #[test]
    fn malformed_tool_input_is_an_error() {
        let result = finish_tool_call(ToolCallState {
            id: "tc-3".to_owned(),
            name: "broken".to_owned(),
            input_json: "{not json".to_owned(),
        });
        assert!(matches!(result, Err(CompletionError::JsonError(_))));
    }

    #[test]
    fn usage_maps_cache_read_tokens() {
        let usage = usage_from_metadata(
            bedrock::TokenUsage::builder()
                .input_tokens(100)
                .output_tokens(50)
                .total_tokens(150)
                .cache_read_input_tokens(30)
                .build()
                .expect("build TokenUsage"),
        );
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cached_input_tokens, 30);
    }
}
