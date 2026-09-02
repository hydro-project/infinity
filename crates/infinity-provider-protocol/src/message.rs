//! Chat message types exchanged between the agent runtime and model
//! providers.
//!
//! These types replace the subset of `rig`'s message model that this project
//! actually uses. Their JSON representations are **byte-compatible** with the
//! rig types they replace (rig 0.31), because persisted conversation
//! histories embed them — do not change serde attributes without a migration
//! plan. Fields rig serialized but this project never populated (e.g. tool
//! call signatures) have been dropped; they deserialize as ignored unknown
//! fields from old data.

use serde::{Deserialize, Serialize};

/// A message in a conversation: one run of user input or assistant output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// User message (plain content or tool results).
    User { content: Vec<UserContent> },
    /// Assistant message (text, reasoning, or tool calls).
    Assistant { content: Vec<AssistantContent> },
}

impl Message {
    /// A user message containing a single text block.
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            content: vec![UserContent::text(text)],
        }
    }

    /// An assistant message containing a single text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Message::Assistant {
            content: vec![AssistantContent::text(text)],
        }
    }
}

/// Content of a user message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UserContent {
    Text(Text),
    ToolResult(ToolResult),
    Image(Image),
}

impl UserContent {
    /// A plain text user content block.
    pub fn text(text: impl Into<String>) -> Self {
        UserContent::Text(Text { text: text.into() })
    }
}

/// Content of an assistant message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(Text),
    ToolCall(ToolCall),
    Reasoning(Reasoning),
}

impl AssistantContent {
    /// A plain text assistant content block.
    pub fn text(text: impl Into<String>) -> Self {
        AssistantContent::Text(Text { text: text.into() })
    }
}

/// A plain text block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Text { text }
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Text { text: text.into() }
    }
}

impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.text)
    }
}

/// A tool invocation requested by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-supplied tool call id, echoed back in the [`ToolResult`].
    pub id: String,
    /// Secondary call id used by some providers; `None` for Bedrock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub function: ToolFunction,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        ToolCall {
            id: id.into(),
            call_id: None,
            function: ToolFunction {
                name: name.into(),
                arguments,
            },
        }
    }
}

/// The function name and arguments of a [`ToolCall`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The result of executing a tool call, sent back as user content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The id of the [`ToolCall`] this result answers.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub content: Vec<ToolResultContent>,
}

/// Content of a [`ToolResult`]: text or an image.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolResultContent {
    Text(Text),
    Image(Image),
}

impl ToolResultContent {
    /// A plain text tool result block.
    pub fn text(text: impl Into<String>) -> Self {
        ToolResultContent::Text(Text { text: text.into() })
    }
}

/// An assistant reasoning ("thinking") block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    /// Provider reasoning identifier, when supplied by the upstream API.
    pub id: Option<String>,
    /// Ordered reasoning content blocks.
    pub content: Vec<ReasoningContent>,
}

impl Reasoning {
    /// A single text reasoning block with an optional provider signature.
    pub fn new_with_signature(text: impl Into<String>, signature: Option<String>) -> Self {
        Reasoning {
            id: None,
            content: vec![ReasoningContent::Text {
                text: text.into(),
                signature,
            }],
        }
    }

    /// The first text reasoning block, if present.
    pub fn first_text(&self) -> Option<&str> {
        self.content.iter().find_map(|content| match content {
            ReasoningContent::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// The first signature from text reasoning, if present.
    pub fn first_signature(&self) -> Option<&str> {
        self.content.iter().find_map(|content| match content {
            ReasoningContent::Text {
                signature: Some(signature),
                ..
            } => Some(signature.as_str()),
            _ => None,
        })
    }

    /// Render reasoning as displayable text by joining text-like blocks with
    /// newlines.
    pub fn display_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|content| match content {
                ReasoningContent::Text { text, .. } => Some(text.as_str()),
                ReasoningContent::Summary(summary) => Some(summary.as_str()),
                ReasoningContent::Redacted { data } => Some(data.as_str()),
                ReasoningContent::Encrypted(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One block of reasoning content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ReasoningContent {
    /// Plain reasoning text with an optional provider signature.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Provider-encrypted reasoning payload.
    Encrypted(String),
    /// Redacted reasoning payload preserved as opaque data.
    Redacted { data: String },
    /// Provider-generated reasoning summary text.
    Summary(String),
}

/// Image content with its source data and media type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Image {
    pub data: ImageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<ImageMediaType>,
}

/// Where an [`Image`]'s data comes from.
///
/// Serde representation matches rig's `DocumentSourceKind` (which this type
/// replaces), since images are embedded in persisted tool results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum ImageSource {
    /// A URL pointing at the image.
    Url(String),
    /// Base64-encoded image bytes.
    Base64(String),
}

/// Image media type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageMediaType {
    JPEG,
    PNG,
    GIF,
    WEBP,
    HEIC,
    HEIF,
    SVG,
}

impl ImageMediaType {
    pub fn from_mime_type(mime_type: &str) -> Option<Self> {
        match mime_type {
            "image/jpeg" => Some(ImageMediaType::JPEG),
            "image/png" => Some(ImageMediaType::PNG),
            "image/gif" => Some(ImageMediaType::GIF),
            "image/webp" => Some(ImageMediaType::WEBP),
            "image/heic" => Some(ImageMediaType::HEIC),
            "image/heif" => Some(ImageMediaType::HEIF),
            "image/svg+xml" => Some(ImageMediaType::SVG),
            _ => None,
        }
    }

    pub fn to_mime_type(&self) -> &'static str {
        match self {
            ImageMediaType::JPEG => "image/jpeg",
            ImageMediaType::PNG => "image/png",
            ImageMediaType::GIF => "image/gif",
            ImageMediaType::WEBP => "image/webp",
            ImageMediaType::HEIC => "image/heic",
            ImageMediaType::HEIF => "image/heif",
            ImageMediaType::SVG => "image/svg+xml",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert `value` serializes to exactly `json` and deserializes back.
    #[track_caller]
    fn assert_round_trip<T>(value: &T, json: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let serialized = serde_json::to_string(value).expect("serialize");
        assert_eq!(serialized, json);
        let deserialized: T = serde_json::from_str(json).expect("deserialize");
        assert_eq!(&deserialized, value);
    }

    // The JSON strings in these tests are the exact formats rig 0.31
    // produced; persisted histories contain them. Do not update the
    // expectations without a data migration.

    #[test]
    fn user_text_message_format() {
        assert_round_trip(
            &Message::user("hi"),
            r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#,
        );
    }

    #[test]
    fn assistant_text_message_format() {
        assert_round_trip(
            &Message::assistant("hello"),
            r#"{"role":"assistant","content":[{"text":"hello"}]}"#,
        );
    }

    #[test]
    fn tool_call_format() {
        assert_round_trip(
            &AssistantContent::ToolCall(ToolCall::new(
                "call-1",
                "get_weather",
                serde_json::json!({"city": "Berlin"}),
            )),
            r#"{"id":"call-1","function":{"name":"get_weather","arguments":{"city":"Berlin"}}}"#,
        );
    }

    #[test]
    fn tool_result_format() {
        assert_round_trip(
            &UserContent::ToolResult(ToolResult {
                id: "call-1".to_owned(),
                call_id: None,
                content: vec![ToolResultContent::text("ok")],
            }),
            r#"{"type":"toolresult","id":"call-1","content":[{"type":"text","text":"ok"}]}"#,
        );
    }

    #[test]
    fn reasoning_format() {
        assert_round_trip(
            &AssistantContent::Reasoning(Reasoning::new_with_signature(
                "thinking...",
                Some("sig".to_owned()),
            )),
            r#"{"id":null,"content":[{"type":"text","content":{"text":"thinking...","signature":"sig"}}]}"#,
        );
    }

    #[test]
    fn reasoning_content_variants_round_trip() {
        assert_round_trip(
            &ReasoningContent::Encrypted("opaque".to_owned()),
            r#"{"type":"encrypted","content":"opaque"}"#,
        );
        assert_round_trip(
            &ReasoningContent::Redacted {
                data: "r".to_owned(),
            },
            r#"{"type":"redacted","content":{"data":"r"}}"#,
        );
        assert_round_trip(
            &ReasoningContent::Summary("s".to_owned()),
            r#"{"type":"summary","content":"s"}"#,
        );
    }

    #[test]
    fn image_format() {
        assert_round_trip(
            &UserContent::Image(Image {
                data: ImageSource::Base64("aGVsbG8=".to_owned()),
                media_type: Some(ImageMediaType::PNG),
            }),
            r#"{"type":"image","data":{"type":"base64","value":"aGVsbG8="},"media_type":"png"}"#,
        );
    }
}
