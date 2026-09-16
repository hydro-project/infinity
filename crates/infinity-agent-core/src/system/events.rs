//! Events emitted by the high-level agent system.

use infinity_provider_protocol::Usage;

use crate::message::InfinityMessage;

/// A user choice requested by a tool server. Responses are POSTed to
/// `response_url`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserChoice {
    pub id: rap_protocol::ChoiceId,
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: usize,
    pub response_url: String,
}

/// A single observable event from a thread's execution.
///
/// This type is `Clone` and carries no
/// provider-specific generics, so embeddings can fan events out to multiple
/// subscribers or buffer them freely.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A user text input was accepted into the thread's history.
    UserInput { text: String },
    /// A tool result (from an asynchronously dispatched tool call) was
    /// accepted into the thread's history, or a synchronous tool produced a
    /// result inline. Clients should render the first segment type they
    /// support; the raw text is always included as a trailing `Text` segment.
    ToolResult {
        segments: Vec<rap_protocol::DisplaySegment>,
    },
    /// A subscription event or thread report was injected into this thread.
    SubscriptionEvent { name: String, text: String },
    /// A tool server requires OAuth authorization before it can proceed.
    OAuthRequired { auth_url: String },
    /// A compaction summary replaced the beginning of the in-memory history.
    CompactionApplied,
    /// A completion round is about to stream.
    CompletionStarted,
    /// A chunk of assistant text.
    TextChunk { text: String },
    /// The model started reasoning.
    ThinkingStarted,
    /// A chunk of reasoning text.
    ThinkingChunk { text: String },
    /// The model finished reasoning.
    ThinkingEnded,
    /// The model called a tool. `display_as` is the pretty-printed form from
    /// the tool's display script, when available.
    ToolCall {
        name: String,
        args: serde_json::Value,
        display_as: Option<String>,
    },
    /// The completion round finished (its turn is synced to the store by the
    /// time this event is observed). `usage` is the token usage reported by
    /// the provider, if any.
    CompletionFinished { usage: Option<Usage> },
    /// A tool server requested a user choice. The choice has already been
    /// persisted when this event is emitted.
    UserChoiceRequired { choice: UserChoice },
    /// A pending user choice became moot and has already been removed from
    /// persistent state.
    UserChoiceDismissed { choice_id: rap_protocol::ChoiceId },
    /// Human-readable progress/diagnostic information (retries, warnings).
    Info { text: String },
}

/// A live view of a thread, used to bring a newly attached subscriber up to
/// date. Produced when a subscriber attaches to a running thread (see
/// [`ThreadObserver::on_subscribe`](super::ThreadObserver::on_subscribe)).
#[derive(Debug, Clone)]
pub struct ReplaySnapshot {
    /// Committed history followed by the in-flight buffered turn, so a
    /// subscriber attaching while the model is streaming still sees the
    /// partial assistant message.
    pub history: Vec<InfinityMessage>,
    /// In-progress reasoning text. Streamed reasoning is only committed to
    /// history once complete, so without this a client attaching mid-thinking
    /// would appear idle.
    pub current_thinking: Option<String>,
    /// Whether a completion is currently streaming.
    pub in_progress: bool,
    /// Choices awaiting a user response for this conversation.
    pub pending_choices: Vec<UserChoice>,
}
