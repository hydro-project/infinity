use infinity_provider_protocol::message::{
    AssistantContent, Message, Text, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use rap_protocol::{ChoiceId, ProviderCallId, ThreadId, ToolCallId};
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
    pub id: ToolCallId,
    pub call_id: Option<ProviderCallId>,
    pub auth_url: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UserChoiceRequired {
    #[serde(rename = "type")]
    pub content_type: String,
    pub id: ChoiceId,
    pub call_id: Option<ProviderCallId>,
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
    SubscriptionEvent(ToolCallId),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type")]
pub enum TaggedSyntheticKind {
    #[serde(rename = "subscription_event")]
    SubscriptionEvent {
        tool_call_id: ToolCallId,
        #[serde(default)]
        associative: bool,
        /// When true, this is the final event — the runtime removes the
        /// subscription from active tracking.
        #[serde(default)]
        r#final: bool,
    },
    #[serde(rename = "thread_report")]
    ThreadReport {
        tool_call_id: ToolCallId,
        child_thread_id: ThreadId,
    },
    #[serde(rename = "parent_message")]
    ParentMessage { tool_call_id: ToolCallId },
    #[serde(rename = "compaction")]
    Compaction,
    #[serde(rename = "compaction_complete")]
    CompactionComplete,
    /// Interrupt marker: folded into history as [`InfinityMessage::Interrupt`]
    /// at the point the thread's in-flight work was interrupted.
    #[serde(rename = "interrupt")]
    Interrupt,
}

impl SyntheticKind {
    pub fn tool_call_id(&self) -> &ToolCallId<str> {
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
            SyntheticKind::Tagged(TaggedSyntheticKind::Compaction) => ToolCallId::from_ref(""),
            SyntheticKind::Tagged(TaggedSyntheticKind::CompactionComplete) => {
                ToolCallId::from_ref("")
            }
            SyntheticKind::Tagged(TaggedSyntheticKind::Interrupt) => ToolCallId::from_ref(""),
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

    pub fn is_compaction_complete(&self) -> bool {
        matches!(
            self,
            SyntheticKind::Tagged(TaggedSyntheticKind::CompactionComplete)
        )
    }

    /// Whether this is an interrupt marker.
    pub fn is_interrupt(&self) -> bool {
        matches!(self, SyntheticKind::Tagged(TaggedSyntheticKind::Interrupt))
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
    pub group_id: ThreadId,
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

impl InputMessage {
    /// A plain (non-synthetic) user text message addressed to `thread_id`.
    pub fn user_text(thread_id: impl Into<ThreadId>, text: impl Into<String>) -> Self {
        Self {
            content: InputMessageContent::User(UserContent::text(text.into())),
            group_id: thread_id.into(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        }
    }

    /// An interrupt marker addressed to `thread_id`; the content is unused.
    pub fn interrupt(thread_id: impl Into<ThreadId>) -> Self {
        Self {
            content: InputMessageContent::User(UserContent::text(String::new())),
            group_id: thread_id.into(),
            metadata: None,
            synthetic: Some(SyntheticKind::Tagged(TaggedSyntheticKind::Interrupt)),
            display_as: None,
            subscription: false,
        }
    }
}

/// Wraps message content with optional display metadata that survives
/// serialization. On replay the display metadata is used directly instead of
/// being reconstructed or stored as sidecar data.
///
/// Each variant stores the type-specific content struct directly rather than
/// a full [`Message`]. Use [`into_messages`](Self::into_messages) to
/// reconstruct the `Message`s for LLM calls.
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
        call: ToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_as: Option<String>,
    },
    /// User tool result with optional display segments for the UI.
    #[serde(rename = "tool_result")]
    ToolResult {
        result: ToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_segments: Option<Vec<rap_protocol::DisplaySegment>>,
    },
    /// Synthetic subscription event injected into history. The display name is
    /// recomputed on replay from the original tool call in history.
    ///
    /// The payloads are boxed to keep this rare variant from inflating the
    /// size of every `InfinityMessage` (histories store many of these enums
    /// inline). `Box` is transparent to serde, so the wire format is
    /// unchanged.
    #[serde(rename = "subscription_event")]
    SubscriptionEvent {
        result: Box<ToolResult>,
        /// The tool_call_id of the original subscription tool call.
        tool_call_id: ToolCallId,
        /// Set when this is a thread report (used to build the display name).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_thread_id: Option<ThreadId>,
        /// The synthetic `receive_event__injected` tool call. When present,
        /// `into_messages` emits both the tool call and the tool result for
        /// the LLM, but replay only emits the subscription display event.
        /// `None` for backward-compat with old serialized histories.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invocation: Option<Box<ToolCall>>,
    },
    /// The thread's in-flight work was interrupted here. The model-facing
    /// [`INTERRUPT_TOKEN`] is derived at render time by [`render_history`].
    #[serde(rename = "interrupt")]
    Interrupt,
}

/// The token the model sees for an [`InfinityMessage::Interrupt`]. Older
/// versions stored it as a literal prefix of the interrupting user text.
pub const INTERRUPT_TOKEN: &str = "<interrupt>";

/// Render history into the [`Message`]s sent to the model.
///
/// An [`InfinityMessage::Interrupt`] right after a tool call settles it and
/// renders as its "interrupted" result. Otherwise it is prefixed onto the
/// user text that follows it (the shape older versions stored), or rendered
/// standalone if none does. Consecutive interrupts collapse into one token.
pub fn render_history<'a>(history: impl IntoIterator<Item = &'a InfinityMessage>) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut interrupted = false;
    let mut prev: Option<&InfinityMessage> = None;
    for msg in history {
        match (prev.replace(msg), msg) {
            (Some(InfinityMessage::ToolCall { call, .. }), InfinityMessage::Interrupt) => {
                messages.push(Message::User {
                    content: vec![UserContent::ToolResult(interrupted_result(call))],
                });
            }
            (_, InfinityMessage::Interrupt) => interrupted = true,
            (
                _,
                InfinityMessage::User {
                    content: UserContent::Text(text),
                },
            ) if interrupted => {
                interrupted = false;
                messages.push(Message::User {
                    content: vec![UserContent::text(format!("{INTERRUPT_TOKEN}{}", text.text))],
                });
            }
            (_, other) => {
                if std::mem::take(&mut interrupted) {
                    messages.extend(InfinityMessage::Interrupt.into_messages());
                }
                messages.extend(other.clone().into_messages());
            }
        }
    }
    if interrupted {
        messages.extend(InfinityMessage::Interrupt.into_messages());
    }
    messages
}

/// The result the model sees for a tool call settled by an interrupt.
fn interrupted_result(call: &ToolCall) -> ToolResult {
    ToolResult {
        id: call.id.clone(),
        call_id: call.call_id.clone(),
        content: vec![ToolResultContent::Text(Text {
            text: crate::event_processor::TOOL_CALL_INTERRUPTED_TEXT.to_owned(),
        })],
    }
}

impl InfinityMessage {
    /// Reconstruct [`Message`]s for LLM calls.
    /// Most variants produce one message; `SubscriptionEvent` with an
    /// `invocation` produces two (tool call + tool result).
    pub fn into_messages(self) -> Vec<Message> {
        match self {
            Self::User { content } => vec![Message::User {
                content: vec![content],
            }],
            Self::Assistant { content } => vec![Message::Assistant {
                content: vec![content],
            }],
            Self::ToolCall { call, .. } => vec![Message::Assistant {
                content: vec![AssistantContent::ToolCall(call)],
            }],
            Self::ToolResult { result, .. } => vec![Message::User {
                content: vec![UserContent::ToolResult(result)],
            }],
            Self::SubscriptionEvent {
                result, invocation, ..
            } => {
                let mut msgs = Vec::new();
                if let Some(call) = invocation {
                    msgs.push(Message::Assistant {
                        content: vec![AssistantContent::ToolCall(*call)],
                    });
                }
                msgs.push(Message::User {
                    content: vec![UserContent::ToolResult(*result)],
                });
                msgs
            }
            Self::Interrupt => vec![Message::User {
                content: vec![UserContent::text(INTERRUPT_TOKEN)],
            }],
        }
    }

    /// The tool result carried by a `ToolResult` or `SubscriptionEvent`
    /// message, if any.
    pub fn tool_result(&self) -> Option<&ToolResult> {
        match self {
            Self::ToolResult { result, .. } => Some(result),
            Self::SubscriptionEvent { result, .. } => Some(result),
            _ => None,
        }
    }

    /// Auto-classify a bare [`Message`] into the appropriate InfinityMessage
    /// variant. Display metadata fields are set to `None`/defaults.
    pub fn from_message(msg: Message) -> Self {
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
            Message::Assistant { content } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> InfinityMessage {
        InfinityMessage::User {
            content: UserContent::text(text),
        }
    }

    fn rendered(history: impl IntoIterator<Item = InfinityMessage>) -> Vec<Message> {
        history
            .into_iter()
            .flat_map(InfinityMessage::into_messages)
            .collect()
    }

    #[test]
    fn interrupt_without_following_text_renders_standalone() {
        let assistant = InfinityMessage::Assistant {
            content: AssistantContent::text("hi"),
        };
        assert_eq!(
            render_history(&[user("go"), InfinityMessage::Interrupt]),
            rendered([user("go"), user("<interrupt>")])
        );
        assert_eq!(
            render_history(&[InfinityMessage::Interrupt, assistant.clone()]),
            rendered([user("<interrupt>"), assistant])
        );
    }

    #[test]
    fn interrupt_after_tool_call_renders_as_interrupted_result() {
        let tool_call = ToolCall {
            id: "tc-1".to_owned(),
            call_id: None,
            function: infinity_provider_protocol::message::ToolFunction {
                name: "some_tool".to_owned(),
                arguments: serde_json::json!({}),
            },
        };
        // Older versions stored this synthetic result; it renders unchanged.
        let legacy = InfinityMessage::ToolResult {
            result: interrupted_result(&tool_call),
            display_segments: None,
        };
        let call = InfinityMessage::ToolCall {
            call: tool_call,
            display_as: None,
        };
        assert_eq!(
            render_history(&[call.clone(), InfinityMessage::Interrupt, user("hi")]),
            rendered([call.clone(), legacy.clone(), user("hi")])
        );
        assert_eq!(
            render_history(&[call.clone(), legacy.clone(), user("hi")]),
            rendered([call, legacy, user("hi")])
        );
    }

    #[test]
    fn consecutive_interrupts_collapse_into_one_token() {
        assert_eq!(
            render_history(&[
                InfinityMessage::Interrupt,
                InfinityMessage::Interrupt,
                user("again")
            ]),
            rendered([user("<interrupt>again")])
        );
    }
}
