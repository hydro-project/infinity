//! The terminal's display-event vocabulary.
//!
//! [`DisplayEvent`] is what the TUI renders: [`daemon_client`](crate::daemon_client)
//! converts each incoming [`DaemonMessage`](infinity_protocol::DaemonMessage)
//! into one of these, and [`terminal::run`](crate::terminal::run) turns them
//! into viewport updates.

/// One renderable event in a conversation's display stream.
pub enum DisplayEvent {
    StartOutput,
    TextChunk {
        chunk: String,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
        display_as: Option<String>,
    },
    ToolResult {
        /// Prioritized display segments. The renderer uses the first type it
        /// supports; the raw tool output is always included as a trailing
        /// `Text` segment as a fallback.
        segments: Vec<rap_protocol::DisplaySegment>,
    },
    Info(String),
    /// A completion round finished. Carries the usage the daemon reported,
    /// when any: a usage-less `ResponseDone` (e.g. the marker appended after
    /// a session replay) must not reset the context indicator.
    ResponseDone(Option<infinity_provider_protocol::Usage>),
    UserInput(String),
    SubscriptionEvent {
        name: String,
        text: String,
    },
    OAuthRequired {
        auth_url: String,
    },
    UserChoiceRequired {
        id: infinity_protocol::ChoiceId,
        prompt: String,
        choices: Vec<String>,
        default: usize,
        response_url: String,
    },
    UserChoiceComplete {
        choice_id: infinity_protocol::ChoiceId,
    },
    ThinkingStart,
    ThinkingEnd,
    ThinkingChunk {
        chunk: String,
    },
    CompactionApplied,
}
