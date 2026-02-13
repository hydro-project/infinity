use rig::message::UserContent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum InputMessageContent {
    OAuth(OAuthRequired),
    User(UserContent),
}

#[derive(Debug, Deserialize, Serialize)]
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
    SubscriptionEvent { tool_call_id: String },
    #[serde(rename = "thread_report")]
    ThreadReport { tool_call_id: String },
}

impl SyntheticKind {
    pub fn tool_call_id(&self) -> &str {
        match self {
            SyntheticKind::Tagged(TaggedSyntheticKind::SubscriptionEvent { tool_call_id }) => {
                tool_call_id
            }
            SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport { tool_call_id }) => {
                tool_call_id
            }
            SyntheticKind::SubscriptionEvent(id) => id,
        }
    }

    pub fn is_thread_report(&self) -> bool {
        matches!(
            self,
            SyntheticKind::Tagged(TaggedSyntheticKind::ThreadReport { .. })
        )
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct InputMessage {
    pub content: InputMessageContent,
    pub group_id: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub synthetic: Option<SyntheticKind>,
}
