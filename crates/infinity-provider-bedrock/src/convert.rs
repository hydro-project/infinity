//! Conversions from [`infinity_provider_protocol`] request types to the AWS
//! Bedrock Converse API types.

use aws_sdk_bedrockruntime::types as bedrock;
use aws_smithy_types::{Document, Number};
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use infinity_provider_protocol::{
    AssistantContent, CompletionError, ErrorClass, Image, ImageMediaType, ImageSource, Message,
    Reasoning, ReasoningContent, ToolDefinition, ToolResultContent, UserContent,
};

/// Convert a JSON value into a smithy [`Document`] (the representation
/// Bedrock uses for tool input schemas and additional request fields).
pub(crate) fn json_to_document(value: serde_json::Value) -> Document {
    match value {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(b) => Document::Bool(b),
        serde_json::Value::Number(num) => {
            if let Some(u) = num.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = num.as_i64() {
                Document::Number(Number::NegInt(i))
            } else if let Some(f) = num.as_f64() {
                Document::Number(Number::Float(f))
            } else {
                // serde_json numbers are always one of the above.
                Document::Null
            }
        }
        serde_json::Value::String(s) => Document::String(s),
        serde_json::Value::Array(arr) => {
            Document::Array(arr.into_iter().map(json_to_document).collect())
        }
        serde_json::Value::Object(obj) => Document::Object(
            obj.into_iter()
                .map(|(k, v)| (k, json_to_document(v)))
                .collect(),
        ),
    }
}

/// Convert a smithy [`Document`] back into a JSON value (used in tests to
/// verify round-trips).
#[cfg(test)]
pub(crate) fn document_to_json(doc: Document) -> serde_json::Value {
    match doc {
        Document::Null => serde_json::Value::Null,
        Document::Bool(b) => serde_json::Value::Bool(b),
        Document::Number(Number::PosInt(u)) => serde_json::Value::Number(u.into()),
        Document::Number(Number::NegInt(i)) => serde_json::Value::Number(i.into()),
        Document::Number(Number::Float(f)) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            // JSON cannot represent NaN/Infinity.
            .unwrap_or(serde_json::Value::Null),
        Document::String(s) => serde_json::Value::String(s),
        Document::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(document_to_json).collect())
        }
        Document::Object(obj) => serde_json::Value::Object(
            obj.into_iter()
                .map(|(k, v)| (k, document_to_json(v)))
                .collect(),
        ),
    }
}

fn request_error(e: impl std::error::Error + Send + Sync + 'static) -> CompletionError {
    CompletionError::RequestError(Box::new(e))
}

/// Convert the chat history into Bedrock messages (one per input message).
pub(crate) fn messages(history: Vec<Message>) -> Result<Vec<bedrock::Message>, CompletionError> {
    history.into_iter().map(message).collect()
}

fn message(message: Message) -> Result<bedrock::Message, CompletionError> {
    let (role, content) = match message {
        Message::User { content } => (
            bedrock::ConversationRole::User,
            content
                .into_iter()
                .map(user_content)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Message::Assistant { content } => (
            bedrock::ConversationRole::Assistant,
            content
                .into_iter()
                .map(assistant_content)
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };
    bedrock::Message::builder()
        .role(role)
        .set_content(Some(content))
        .build()
        .map_err(request_error)
}

fn user_content(content: UserContent) -> Result<bedrock::ContentBlock, CompletionError> {
    match content {
        UserContent::Text(text) => Ok(bedrock::ContentBlock::Text(text.text)),
        UserContent::Image(image) => Ok(bedrock::ContentBlock::Image(image_block(image)?)),
        UserContent::ToolResult(result) => {
            let content = result
                .content
                .into_iter()
                .map(|c| {
                    Ok(match c {
                        ToolResultContent::Text(text) => {
                            bedrock::ToolResultContentBlock::Text(text.text)
                        }
                        ToolResultContent::Image(image) => {
                            bedrock::ToolResultContentBlock::Image(image_block(image)?)
                        }
                    })
                })
                .collect::<Result<Vec<_>, CompletionError>>()?;
            let block = bedrock::ToolResultBlock::builder()
                .tool_use_id(result.id)
                .set_content(Some(content))
                .build()
                .map_err(request_error)?;
            Ok(bedrock::ContentBlock::ToolResult(block))
        }
    }
}

fn assistant_content(content: AssistantContent) -> Result<bedrock::ContentBlock, CompletionError> {
    match content {
        AssistantContent::Text(text) => Ok(bedrock::ContentBlock::Text(text.text)),
        AssistantContent::ToolCall(call) => {
            let block = bedrock::ToolUseBlock::builder()
                .tool_use_id(call.id)
                .name(call.function.name)
                .input(json_to_document(call.function.arguments))
                .build()
                .map_err(request_error)?;
            Ok(bedrock::ContentBlock::ToolUse(block))
        }
        AssistantContent::Reasoning(reasoning) => Ok(bedrock::ContentBlock::ReasoningContent(
            reasoning_block(reasoning)?,
        )),
    }
}

/// Convert a reasoning block. Bedrock accepts a single reasoning text with an
/// optional signature, so multi-part reasoning is flattened; mixing a signed
/// text block with other parts would corrupt the signature and is rejected.
fn reasoning_block(
    reasoning: Reasoning,
) -> Result<bedrock::ReasoningContentBlock, CompletionError> {
    let signed_text_count = reasoning
        .content
        .iter()
        .filter(|content| {
            matches!(
                content,
                ReasoningContent::Text {
                    signature: Some(_),
                    ..
                }
            )
        })
        .count();
    if signed_text_count > 1 {
        return Err(CompletionError::provider(
            ErrorClass::Fatal,
            "AWS Bedrock does not support multiple signed reasoning text blocks",
        ));
    }
    if signed_text_count == 1 && reasoning.content.len() > 1 {
        return Err(CompletionError::provider(
            ErrorClass::Fatal,
            "AWS Bedrock requires a single signed reasoning text block without additional \
             reasoning parts",
        ));
    }

    let text = reasoning.display_text();
    if text.is_empty() {
        return Err(CompletionError::provider(
            ErrorClass::Fatal,
            "AWS Bedrock reasoning conversion requires at least one text or summary block",
        ));
    }

    let mut builder = bedrock::ReasoningTextBlock::builder().text(text);
    if let Some(signature) = reasoning.first_signature() {
        builder = builder.signature(signature);
    }
    let block = builder.build().map_err(request_error)?;
    Ok(bedrock::ReasoningContentBlock::ReasoningText(block))
}

fn image_block(image: Image) -> Result<bedrock::ImageBlock, CompletionError> {
    let format = match image.media_type {
        Some(ImageMediaType::JPEG) => bedrock::ImageFormat::Jpeg,
        Some(ImageMediaType::PNG) => bedrock::ImageFormat::Png,
        Some(ImageMediaType::GIF) => bedrock::ImageFormat::Gif,
        Some(ImageMediaType::WEBP) => bedrock::ImageFormat::Webp,
        Some(other) => {
            return Err(CompletionError::provider(
                ErrorClass::Fatal,
                format!(
                    "AWS Bedrock does not support {} images",
                    other.to_mime_type()
                ),
            ));
        }
        None => {
            return Err(CompletionError::provider(
                ErrorClass::Fatal,
                "image content requires a media type for AWS Bedrock",
            ));
        }
    };

    let ImageSource::Base64(data) = image.data else {
        return Err(CompletionError::RequestError(
            "only base64-encoded image data is supported by AWS Bedrock".into(),
        ));
    };
    let bytes = BASE64_STANDARD.decode(data).map_err(|e| {
        CompletionError::provider(ErrorClass::Fatal, format!("invalid base64 image data: {e}"))
    })?;

    bedrock::ImageBlock::builder()
        .format(format)
        .source(bedrock::ImageSource::Bytes(aws_smithy_types::Blob::new(
            bytes,
        )))
        .build()
        .map_err(request_error)
}

/// Build the Bedrock tool configuration; `None` when no tools are defined
/// (Bedrock rejects an empty tool list).
pub(crate) fn tool_config(
    tools: &[ToolDefinition],
) -> Result<Option<bedrock::ToolConfiguration>, CompletionError> {
    if tools.is_empty() {
        return Ok(None);
    }
    let tools = tools
        .iter()
        .map(|tool| {
            let spec = bedrock::ToolSpecification::builder()
                .name(&tool.name)
                .description(&tool.description)
                .input_schema(bedrock::ToolInputSchema::Json(json_to_document(
                    tool.parameters.clone(),
                )))
                .build()
                .map_err(request_error)?;
            Ok(bedrock::Tool::ToolSpec(spec))
        })
        .collect::<Result<Vec<_>, CompletionError>>()?;
    let config = bedrock::ToolConfiguration::builder()
        .set_tools(Some(tools))
        .build()
        .map_err(request_error)?;
    Ok(Some(config))
}

/// Append a cache point to the last message so Bedrock caches everything up
/// to (and including) that turn on subsequent calls.
pub(crate) fn append_cache_point(messages: &mut [bedrock::Message]) {
    if let Some(last) = messages.last_mut() {
        last.content.push(bedrock::ContentBlock::CachePoint(
            bedrock::CachePointBlock::builder()
                .r#type(bedrock::CachePointType::Default)
                .build()
                .expect("bug: CachePointBlock with type set cannot fail to build"),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinity_provider_protocol::{Text, ToolResult};

    #[test]
    fn json_document_round_trip() {
        let value = serde_json::json!({
            "type": "object",
            "is_enabled": true,
            "version": 42,
            "fraction": 1.23,
            "negative": -11,
            "properties": {
                "x": { "type": "number", "description": "The first number" },
            },
            "required": ["x", null],
        });
        let round_tripped = document_to_json(json_to_document(value.clone()));
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn tool_result_with_text_and_image_converts() {
        let converted = messages(vec![Message::User {
            content: vec![UserContent::ToolResult(ToolResult {
                id: "tc-1".to_owned(),
                call_id: None,
                content: vec![
                    ToolResultContent::Text(Text::from("done")),
                    ToolResultContent::Image(Image {
                        data: ImageSource::Base64("aGVsbG8=".to_owned()),
                        media_type: Some(ImageMediaType::PNG),
                    }),
                ],
            })],
        }])
        .expect("convert");
        let bedrock::ContentBlock::ToolResult(result) = &converted[0].content[0] else {
            panic!("expected ToolResult block");
        };
        assert_eq!(result.tool_use_id(), "tc-1");
        assert_eq!(result.content.len(), 2);
        assert!(matches!(
            &result.content[0],
            bedrock::ToolResultContentBlock::Text(t) if t == "done"
        ));
        let bedrock::ToolResultContentBlock::Image(image) = &result.content[1] else {
            panic!("expected image block");
        };
        assert_eq!(image.format, bedrock::ImageFormat::Png);
        let Some(bedrock::ImageSource::Bytes(blob)) = &image.source else {
            panic!("expected bytes source");
        };
        assert_eq!(blob.as_ref(), b"hello");
    }

    #[test]
    fn url_image_is_rejected() {
        let result = messages(vec![Message::User {
            content: vec![UserContent::Image(Image {
                data: ImageSource::Url("https://example.com/x.png".to_owned()),
                media_type: Some(ImageMediaType::PNG),
            })],
        }]);
        assert!(result.is_err(), "URL images are not supported by Bedrock");
    }

    #[test]
    fn unsupported_image_media_type_is_rejected() {
        let result = messages(vec![Message::User {
            content: vec![UserContent::Image(Image {
                data: ImageSource::Base64("aGVsbG8=".to_owned()),
                media_type: Some(ImageMediaType::SVG),
            })],
        }]);
        assert!(result.is_err(), "SVG images are not supported by Bedrock");
    }

    #[test]
    fn signed_reasoning_converts() {
        let converted = messages(vec![Message::Assistant {
            content: vec![AssistantContent::Reasoning(Reasoning::new_with_signature(
                "thinking...",
                Some("sig-1".to_owned()),
            ))],
        }])
        .expect("convert");
        let bedrock::ContentBlock::ReasoningContent(bedrock::ReasoningContentBlock::ReasoningText(
            block,
        )) = &converted[0].content[0]
        else {
            panic!("expected reasoning text block");
        };
        assert_eq!(block.text(), "thinking...");
        assert_eq!(block.signature(), Some("sig-1"));
    }

    #[test]
    fn signed_reasoning_with_extra_parts_is_rejected() {
        let reasoning = Reasoning {
            id: None,
            content: vec![
                ReasoningContent::Text {
                    text: "signed".to_owned(),
                    signature: Some("sig".to_owned()),
                },
                ReasoningContent::Summary("extra".to_owned()),
            ],
        };
        let result = messages(vec![Message::Assistant {
            content: vec![AssistantContent::Reasoning(reasoning)],
        }]);
        assert!(result.is_err());
    }

    #[test]
    fn empty_reasoning_is_rejected() {
        let reasoning = Reasoning {
            id: None,
            content: vec![],
        };
        let result = messages(vec![Message::Assistant {
            content: vec![AssistantContent::Reasoning(reasoning)],
        }]);
        assert!(result.is_err());
    }

    #[test]
    fn empty_tool_list_yields_no_config() {
        assert!(tool_config(&[]).expect("build").is_none());
    }

    #[test]
    fn cache_point_appended_to_last_message_only() {
        let mut converted =
            messages(vec![Message::user("first"), Message::assistant("second")]).expect("convert");
        append_cache_point(&mut converted);
        assert_eq!(converted[0].content.len(), 1);
        assert_eq!(converted[1].content.len(), 2);
        assert!(matches!(
            &converted[1].content[1],
            bedrock::ContentBlock::CachePoint(cp)
                if cp.r#type == bedrock::CachePointType::Default
        ));
    }

    #[test]
    fn cache_point_on_empty_history_is_a_no_op() {
        let mut empty: Vec<bedrock::Message> = vec![];
        append_cache_point(&mut empty);
        assert!(empty.is_empty());
    }
}
