use rig::message::UserContent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum InputMessageContent {
    OAuth(OAuthRequired),
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
    },
    #[serde(rename = "thread_report")]
    ThreadReport { tool_call_id: String },
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
            SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport { tool_call_id }) => {
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
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct InputMessage {
    pub content: InputMessageContent,
    pub group_id: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub synthetic: Option<SyntheticKind>,
    /// Optional short display text for the CLI. When present, the CLI shows
    /// this instead of the full tool result text. The model still sees the
    /// full text.
    #[serde(default)]
    pub display_as: Option<String>,
    /// When true, indicates this tool result started a subscription — the
    /// runtime should track the tool call ID as an active subscription so
    /// the agent can later cancel it via `cancel_subscription`.
    #[serde(default)]
    pub subscription: bool,
}
