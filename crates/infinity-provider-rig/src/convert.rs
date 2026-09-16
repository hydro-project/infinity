//! Conversions between [`infinity_provider_protocol`] types and their rig
//! equivalents.
//!
//! The protocol types were extracted from (a subset of) rig's, so most
//! conversions are mechanical. Conversions *to* rig are fallible because rig
//! requires non-empty content lists (`OneOrMany`); conversions *from* rig are
//! lossy only in fields the protocol intentionally dropped (tool call
//! signatures, provider-specific additional params, image detail).

use infinity_provider_protocol as proto;
use rig::OneOrMany;
use rig::completion::GetTokenUsage;
use rig::streaming::StreamedAssistantContent;

// ── Requests (protocol → rig) ──

/// Convert a protocol completion request into a rig completion request.
///
/// Fields the protocol does not model (per-request model override,
/// documents, temperature, tool choice, output schema) are left unset.
///
/// Errors if the chat history (or any message's content) is empty, which rig
/// cannot represent — such a request is malformed anyway.
pub fn request_to_rig(
    request: proto::CompletionRequest,
) -> Result<rig::completion::CompletionRequest, proto::CompletionError> {
    let chat_history = request
        .chat_history
        .into_iter()
        .map(message_to_rig)
        .collect::<Result<Vec<_>, _>>()?;
    let chat_history = OneOrMany::many(chat_history).map_err(|_| {
        proto::CompletionError::RequestError("completion request has an empty chat history".into())
    })?;

    Ok(rig::completion::CompletionRequest {
        model: None,
        preamble: request.preamble,
        chat_history,
        documents: vec![],
        tools: request
            .tools
            .into_iter()
            .map(tool_definition_to_rig)
            .collect(),
        temperature: None,
        max_tokens: request.max_tokens,
        tool_choice: None,
        additional_params: request.additional_params,
        output_schema: None,
    })
}

pub fn tool_definition_to_rig(tool: proto::ToolDefinition) -> rig::completion::ToolDefinition {
    rig::completion::ToolDefinition {
        name: tool.name,
        description: tool.description,
        parameters: tool.parameters,
    }
}

// ── Messages (protocol → rig) ──

/// Convert a protocol message into a rig message. Errors if the message has
/// no content blocks (unrepresentable in rig).
pub fn message_to_rig(
    message: proto::Message,
) -> Result<rig::message::Message, proto::CompletionError> {
    match message {
        proto::Message::User { content } => Ok(rig::message::Message::User {
            content: one_or_many(content.into_iter().map(user_content_to_rig).collect())?,
        }),
        proto::Message::Assistant { content } => Ok(rig::message::Message::Assistant {
            id: None,
            content: one_or_many(content.into_iter().map(assistant_content_to_rig).collect())?,
        }),
    }
}

fn one_or_many<T: Clone>(items: Vec<T>) -> Result<OneOrMany<T>, proto::CompletionError> {
    OneOrMany::many(items)
        .map_err(|_| proto::CompletionError::RequestError("message has no content blocks".into()))
}

pub fn user_content_to_rig(content: proto::UserContent) -> rig::message::UserContent {
    match content {
        proto::UserContent::Text(text) => {
            rig::message::UserContent::Text(rig::message::Text { text: text.text })
        }
        proto::UserContent::ToolResult(result) => {
            rig::message::UserContent::ToolResult(tool_result_to_rig(result))
        }
        proto::UserContent::Image(image) => rig::message::UserContent::Image(image_to_rig(image)),
    }
}

pub fn assistant_content_to_rig(
    content: proto::AssistantContent,
) -> rig::message::AssistantContent {
    match content {
        proto::AssistantContent::Text(text) => {
            rig::message::AssistantContent::Text(rig::message::Text { text: text.text })
        }
        proto::AssistantContent::ToolCall(call) => {
            rig::message::AssistantContent::ToolCall(tool_call_to_rig(call))
        }
        proto::AssistantContent::Reasoning(reasoning) => {
            rig::message::AssistantContent::Reasoning(reasoning_to_rig(reasoning))
        }
    }
}

pub fn tool_call_to_rig(call: proto::ToolCall) -> rig::message::ToolCall {
    let rig_call = rig::message::ToolCall::new(
        call.id,
        rig::message::ToolFunction {
            name: call.function.name,
            arguments: call.function.arguments,
        },
    );
    match call.call_id {
        Some(call_id) => rig_call.with_call_id(call_id),
        None => rig_call,
    }
}

pub fn tool_result_to_rig(result: proto::ToolResult) -> rig::message::ToolResult {
    // rig requires at least one content block; represent an empty result as
    // a single empty text block.
    let content = OneOrMany::many(
        result
            .content
            .into_iter()
            .map(tool_result_content_to_rig)
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| OneOrMany::one(rig::message::ToolResultContent::text("")));
    rig::message::ToolResult {
        id: result.id,
        call_id: result.call_id,
        content,
    }
}

pub fn tool_result_content_to_rig(
    content: proto::ToolResultContent,
) -> rig::message::ToolResultContent {
    match content {
        proto::ToolResultContent::Text(text) => {
            rig::message::ToolResultContent::Text(rig::message::Text { text: text.text })
        }
        proto::ToolResultContent::Image(image) => {
            rig::message::ToolResultContent::Image(image_to_rig(image))
        }
    }
}

pub fn reasoning_to_rig(reasoning: proto::Reasoning) -> rig::message::Reasoning {
    // `rig::message::Reasoning` is `#[non_exhaustive]`, so it cannot be
    // built with a struct literal; construct it empty and fill the public
    // fields.
    let mut rig_reasoning = rig::message::Reasoning::multi(vec![]);
    rig_reasoning.id = reasoning.id;
    rig_reasoning.content = reasoning
        .content
        .into_iter()
        .map(reasoning_content_to_rig)
        .collect();
    rig_reasoning
}

pub fn reasoning_content_to_rig(
    content: proto::ReasoningContent,
) -> rig::message::ReasoningContent {
    match content {
        proto::ReasoningContent::Text { text, signature } => {
            rig::message::ReasoningContent::Text { text, signature }
        }
        proto::ReasoningContent::Encrypted(data) => rig::message::ReasoningContent::Encrypted(data),
        proto::ReasoningContent::Redacted { data } => {
            rig::message::ReasoningContent::Redacted { data }
        }
        proto::ReasoningContent::Summary(summary) => {
            rig::message::ReasoningContent::Summary(summary)
        }
    }
}

pub fn image_to_rig(image: proto::Image) -> rig::message::Image {
    rig::message::Image {
        data: match image.data {
            proto::ImageSource::Url(url) => rig::message::DocumentSourceKind::Url(url),
            proto::ImageSource::Base64(data) => rig::message::DocumentSourceKind::Base64(data),
        },
        media_type: image.media_type.map(image_media_type_to_rig),
        detail: None,
        additional_params: None,
    }
}

pub fn image_media_type_to_rig(media_type: proto::ImageMediaType) -> rig::message::ImageMediaType {
    match media_type {
        proto::ImageMediaType::JPEG => rig::message::ImageMediaType::JPEG,
        proto::ImageMediaType::PNG => rig::message::ImageMediaType::PNG,
        proto::ImageMediaType::GIF => rig::message::ImageMediaType::GIF,
        proto::ImageMediaType::WEBP => rig::message::ImageMediaType::WEBP,
        proto::ImageMediaType::HEIC => rig::message::ImageMediaType::HEIC,
        proto::ImageMediaType::HEIF => rig::message::ImageMediaType::HEIF,
        proto::ImageMediaType::SVG => rig::message::ImageMediaType::SVG,
    }
}

// ── Streamed output (rig → protocol) ──

/// Convert one streamed rig item into a protocol [`proto::StreamChunk`].
///
/// Drops rig-specific bookkeeping the protocol does not model: the
/// rig-internal `internal_call_id`, and tool call signatures / additional
/// params (no rig backend we bridge populates them in a way downstream code
/// consumes).
pub fn chunk_from_rig<R>(content: StreamedAssistantContent<R>) -> proto::StreamChunk
where
    R: Clone + Unpin + GetTokenUsage,
{
    match content {
        StreamedAssistantContent::Text(text) => proto::StreamChunk::Text(text.text),
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            proto::StreamChunk::ToolCall(tool_call_from_rig(tool_call))
        }
        StreamedAssistantContent::ToolCallDelta { id, content, .. } => {
            proto::StreamChunk::ToolCallDelta {
                id,
                content: match content {
                    rig::streaming::ToolCallDeltaContent::Name(name) => {
                        proto::ToolCallDeltaContent::Name(name)
                    }
                    rig::streaming::ToolCallDeltaContent::Delta(delta) => {
                        proto::ToolCallDeltaContent::Delta(delta)
                    }
                },
            }
        }
        StreamedAssistantContent::Reasoning(reasoning) => {
            proto::StreamChunk::Reasoning(reasoning_from_rig(reasoning))
        }
        StreamedAssistantContent::ReasoningDelta { id, reasoning } => {
            proto::StreamChunk::ReasoningDelta {
                id,
                text: reasoning,
            }
        }
        StreamedAssistantContent::Final(response) => {
            proto::StreamChunk::Final(proto::FinalResponse {
                usage: response.token_usage().map(usage_from_rig),
            })
        }
    }
}

pub fn tool_call_from_rig(call: rig::message::ToolCall) -> proto::ToolCall {
    proto::ToolCall {
        id: call.id,
        call_id: call.call_id,
        function: proto::ToolFunction {
            name: call.function.name,
            arguments: call.function.arguments,
        },
    }
}

pub fn reasoning_from_rig(reasoning: rig::message::Reasoning) -> proto::Reasoning {
    proto::Reasoning {
        id: reasoning.id,
        content: reasoning
            .content
            .into_iter()
            .map(reasoning_content_from_rig)
            .collect(),
    }
}

/// Convert rig reasoning content. Unknown future variants (the rig enum is
/// `#[non_exhaustive]`) are preserved as redacted opaque text.
pub fn reasoning_content_from_rig(
    content: rig::message::ReasoningContent,
) -> proto::ReasoningContent {
    match content {
        rig::message::ReasoningContent::Text { text, signature } => {
            proto::ReasoningContent::Text { text, signature }
        }
        rig::message::ReasoningContent::Encrypted(data) => proto::ReasoningContent::Encrypted(data),
        rig::message::ReasoningContent::Redacted { data } => {
            proto::ReasoningContent::Redacted { data }
        }
        rig::message::ReasoningContent::Summary(summary) => {
            proto::ReasoningContent::Summary(summary)
        }
        other => proto::ReasoningContent::Redacted {
            data: format!("{other:?}"),
        },
    }
}

pub fn usage_from_rig(usage: rig::completion::Usage) -> proto::Usage {
    proto::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
    }
}

/// Convert a rig completion error. Variants the protocol models map 1:1;
/// transport-level errors (HTTP, URL parsing) are stringified into
/// `ProviderError`.
///
/// Rig does not expose structured retry information, so provider errors are
/// classified from their message: known rate-limit and overflow phrasings
/// used by the common OpenAI-compatible backends are recognized, everything
/// else defaults to [`proto::ErrorClass::Fatal`].
pub fn error_from_rig(error: rig::completion::CompletionError) -> proto::CompletionError {
    use rig::completion::CompletionError as RigError;
    match error {
        RigError::RequestError(e) => proto::CompletionError::RequestError(e),
        RigError::ResponseError(e) => proto::CompletionError::ResponseError(e),
        RigError::ProviderError(e) => {
            let class = classify_provider_message(&e);
            proto::CompletionError::provider(class, e)
        }
        RigError::JsonError(e) => proto::CompletionError::JsonError(e),
        other @ (RigError::HttpError(_) | RigError::UrlError(_)) => {
            proto::CompletionError::provider(proto::ErrorClass::Transient, other.to_string())
        }
    }
}

fn classify_provider_message(message: &str) -> proto::ErrorClass {
    let msg = message.to_ascii_lowercase();
    if msg.contains("rate limit")
        || msg.contains("too many requests")
        || msg.contains("please try again")
        || msg.contains("overloaded")
    {
        return proto::ErrorClass::Throttled;
    }
    if msg.contains("context length")
        || msg.contains("context window")
        || msg.contains("too long")
        || msg.contains("too large")
        || msg.contains("maximum context")
    {
        return proto::ErrorClass::ContextOverflow;
    }
    if msg.contains("internal server error") || msg.contains("service unavailable") {
        return proto::ErrorClass::Transient;
    }
    proto::ErrorClass::Fatal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_history_is_rejected() {
        let request = proto::CompletionRequest::default();
        match request_to_rig(request) {
            Err(proto::CompletionError::RequestError(e)) => {
                assert!(e.to_string().contains("empty chat history"));
            }
            other => panic!("expected RequestError, got {other:?}"),
        }
    }

    #[test]
    fn empty_message_content_is_rejected() {
        let request = proto::CompletionRequest {
            chat_history: vec![proto::Message::User { content: vec![] }],
            ..Default::default()
        };
        assert!(request_to_rig(request).is_err());
    }

    #[test]
    fn reasoning_round_trips_through_rig() {
        let reasoning = proto::Reasoning {
            id: Some("r-1".to_owned()),
            content: vec![
                proto::ReasoningContent::Text {
                    text: "a".to_owned(),
                    signature: Some("s".to_owned()),
                },
                proto::ReasoningContent::Encrypted("enc".to_owned()),
                proto::ReasoningContent::Redacted {
                    data: "red".to_owned(),
                },
                proto::ReasoningContent::Summary("sum".to_owned()),
            ],
        };
        let round_tripped = reasoning_from_rig(reasoning_to_rig(reasoning.clone()));
        assert_eq!(round_tripped, reasoning);
    }
}
