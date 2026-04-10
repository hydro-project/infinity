use rig::OneOrMany;
use rig::message::{AssistantContent, Message, UserContent};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum InputMessageContent {
    OAuth(OAuthRequired),
    UserChoice(UserChoiceRequired),
    User(UserContent),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OAuthRequired {
    #[serde(rename = "type")]
    pub content_type: String,
    pub id: String,
    pub call_id: Option<String>,
    pub auth_url: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UserChoiceRequired {
    #[serde(rename = "type")]
    pub content_type: String,
    pub id: String,
    pub call_id: Option<String>,
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: usize,
    pub response_url: String,
}

/// Distinguishes subscription events (which spawn a subthread) from thread reports
/// (which inject directly into the target thread).
/// A plain string deserializes as SubscriptionEvent for backward compatibility.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum SyntheticKind {
    Tagged(TaggedSyntheticKind),
    /// Backward compat: a bare string is treated as a subscription event
    SubscriptionEvent(String),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type")]
pub enum TaggedSyntheticKind {
    #[serde(rename = "subscription_event")]
    SubscriptionEvent {
        tool_call_id: String,
        #[serde(default)]
        associative: bool,
        /// When true, this is the final event — the runtime removes the
        /// subscription from active tracking.
        #[serde(default)]
        r#final: bool,
    },
    #[serde(rename = "thread_report")]
    ThreadReport {
        tool_call_id: String,
        child_thread_id: String,
    },
    #[serde(rename = "parent_message")]
    ParentMessage { tool_call_id: String },
    #[serde(rename = "compaction")]
    Compaction,
    #[serde(rename = "compaction_complete")]
    CompactionComplete,
}

impl SyntheticKind {
    pub fn tool_call_id(&self) -> &str {
        match self {
            SyntheticKind::Tagged(TaggedSyntheticKind::SubscriptionEvent {
                tool_call_id, ..
            }) => tool_call_id,
            SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport { tool_call_id, .. }) => {
                tool_call_id
            }
            SyntheticKind::Tagged(TaggedSyntheticKind::ParentMessage { tool_call_id }) => {
                tool_call_id
            }
            SyntheticKind::Tagged(TaggedSyntheticKind::Compaction) => "",
            SyntheticKind::Tagged(TaggedSyntheticKind::CompactionComplete) => "",
            SyntheticKind::SubscriptionEvent(id) => id,
        }
    }

    pub fn is_thread_report(&self) -> bool {
        matches!(
            self,
            SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport { .. })
        )
    }

    pub fn is_parent_message(&self) -> bool {
        matches!(
            self,
            SyntheticKind::Tagged(TaggedSyntheticKind::ParentMessage { .. })
        )
    }

    pub fn is_compaction(&self) -> bool {
        matches!(self, SyntheticKind::Tagged(TaggedSyntheticKind::Compaction))
    }

    /// Associative subscription events are injected inline into the subscribing
    /// thread's history (like thread reports) rather than spawning a child thread.
    pub fn is_associative(&self) -> bool {
        matches!(
            self,
            SyntheticKind::Tagged(TaggedSyntheticKind::SubscriptionEvent {
                associative: true,
                ..
            })
        )
    }

    /// When true, this is the final subscription event — the runtime should
    /// remove the subscription from active tracking.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            SyntheticKind::Tagged(TaggedSyntheticKind::SubscriptionEvent { r#final: true, .. })
        )
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct InputMessage {
    pub content: InputMessageContent,
    pub group_id: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub synthetic: Option<SyntheticKind>,
    /// Optional structured display segments for the CLI/UI. When present,
    /// the UI shows these instead of the full tool result text. The model
    /// still sees the full text.
    #[serde(default)]
    pub display_as: Option<Vec<rap_protocol::DisplaySegment>>,
    /// When true, indicates this tool result started a subscription — the
    /// runtime should track the tool call ID as an active subscription so
    /// the agent can later cancel it via `cancel_subscription`.
    #[serde(default)]
    pub subscription: bool,
}

/// Wraps rig message content with optional display metadata that survives
/// serialization. On replay the display metadata is used directly instead of
/// being reconstructed or stored as sidecar data.
///
/// Each variant stores the type-specific rig struct directly rather than a
/// full `rig::message::Message`. Use [`into_message`](Self::into_message) to
/// reconstruct the `Message` for LLM calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum InfinityMessage {
    /// Plain user text message.
    #[serde(rename = "user")]
    User { content: UserContent },
    /// Assistant text or reasoning message.
    #[serde(rename = "assistant")]
    Assistant { content: AssistantContent },
    /// Assistant tool call with optional pretty-printed display string.
    #[serde(rename = "tool_call")]
    ToolCall {
        call: rig::message::ToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_as: Option<String>,
    },
    /// User tool result with optional display segments for the UI.
    #[serde(rename = "tool_result")]
    ToolResult {
        result: rig::message::ToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_segments: Option<Vec<rap_protocol::DisplaySegment>>,
    },
    /// Synthetic subscription event injected into history. The display name is
    /// recomputed on replay from the original tool call in history.
    #[serde(rename = "subscription_event")]
    SubscriptionEvent {
        result: rig::message::ToolResult,
        /// The tool_call_id of the original subscription tool call.
        tool_call_id: String,
        /// Set when this is a thread report (used to build the display name).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_thread_id: Option<String>,
    },
}

impl InfinityMessage {
    /// Reconstruct a `rig::message::Message` for LLM calls.
    pub fn into_message(self) -> Message {
        match self {
            Self::User { content } => Message::User {
                content: OneOrMany::one(content),
            },
            Self::Assistant { content } => Message::Assistant {
                id: None,
                content: OneOrMany::one(content),
            },
            Self::ToolCall { call, .. } => Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(call)),
            },
            Self::ToolResult { result, .. } | Self::SubscriptionEvent { result, .. } => {
                Message::User {
                    content: OneOrMany::one(UserContent::ToolResult(result)),
                }
            }
        }
    }

    /// Auto-classify a bare rig Message into the appropriate InfinityMessage variant.
    /// Display metadata fields are set to `None`/defaults.
    pub fn from_rig_message(msg: Message) -> Self {
        match msg {
            Message::User { content } => {
                let first = content.into_iter().next().expect("bug: empty content");
                match first {
                    UserContent::ToolResult(result) => Self::ToolResult {
                        result,
                        display_segments: None,
                    },
                    other => Self::User { content: other },
                }
            }
            Message::Assistant { content, .. } => {
                let first = content.into_iter().next().expect("bug: empty content");
                match first {
                    AssistantContent::ToolCall(call) => Self::ToolCall {
                        call,
                        display_as: None,
                    },
                    other => Self::Assistant { content: other },
                }
            }
        }
    }
}
