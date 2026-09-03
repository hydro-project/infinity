//! Message types exchanged between the Slack I/O layer and the dataflow.

/// Callback ID for the model-picker modal (set on the view when opening,
/// echoed back by Slack in the `view_submission` payload).
pub const MODEL_PICKER_CALLBACK_ID: &str = "model_picker";
/// Block ID of the model-select input block in the model-picker modal.
pub const MODEL_PICKER_BLOCK_ID: &str = "model_block";
/// Action ID of the model-select element in the model-picker modal.
pub const MODEL_PICKER_ACTION_ID: &str = "model_select";

/// A normalized event from Slack (message, button click, or slash command)
/// that flows through the dataflow.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SlackEvent {
    pub user: String,
    pub text: String,
    pub channel: String,
    pub thread_ts: String,
    pub is_button_click: bool,
    pub button_value: Option<String>,
    pub action_id: Option<String>,
    /// The ts of the message that was interacted with (for button clicks).
    pub message_ts: Option<String>,
    /// The label text of the clicked button.
    pub button_text: Option<String>,
    /// True if this is a bot message.
    pub is_bot: bool,
    /// True if user is not authorized.
    pub is_unauthorized: bool,
    /// The slash command that produced this event (e.g. `/model`), if any.
    /// For slash commands, `text` holds the arguments after the command.
    #[serde(default)]
    pub slash_command: Option<String>,
    /// Slash-command response URL: POSTing JSON here sends an (ephemeral by
    /// default) response visible only to the invoking user.
    #[serde(default)]
    pub response_url: Option<String>,
    /// Trigger ID for opening modals (present in slash commands and some
    /// interactive payloads).
    #[serde(default)]
    pub trigger_id: Option<String>,
    /// True if the user opened the app's Messages tab (`app_home_opened`
    /// event with `tab == "messages"`). Used for onboarding, not messaging.
    #[serde(default)]
    pub is_app_home_opened: bool,
}

/// An action the dataflow instructs the Slack I/O layer to perform against
/// the Slack API.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum SlackAction {
    /// Post a text message to a channel/thread.
    PostMessage {
        channel: String,
        text: String,
        thread_ts: Option<String>,
    },
    /// Post a message with Block Kit blocks.
    PostBlocks {
        channel: String,
        fallback_text: String,
        blocks: serde_json::Value,
        thread_ts: Option<String>,
        /// If set, the Slack I/O layer will store the resulting message_ts
        /// under this choice_id so that a later `DismissChoiceButtons` can
        /// update it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice_id: Option<String>,
    },
    /// Dismiss interactive buttons for a completed choice (replace with a
    /// "resolved" indicator). The Slack I/O layer looks up the stored
    /// message_ts from the choice_id.
    DismissChoiceButtons { choice_id: String },
    /// Update an existing message's blocks (e.g. to replace buttons with a selection indicator).
    UpdateMessage {
        channel: String,
        ts: String,
        text: String,
        blocks: serde_json::Value,
    },
    /// Append text to the active stream for this thread (starts a stream if needed).
    StreamAppend {
        channel: String,
        thread_ts: String,
        text: String,
    },
    /// Stop/finalize the active stream for this thread.
    StreamStop { channel: String, thread_ts: String },
    /// Set a status indicator on the thread (e.g. "Thinking...").
    SetStatus {
        channel: String,
        thread_ts: String,
        status: String,
    },
    /// Respond to a slash command by POSTing to its `response_url`.
    /// The response is ephemeral (visible only to the invoking user).
    CommandResponse { response_url: String, text: String },
    /// Open a modal view using the Slack `views.open` API.
    OpenView {
        trigger_id: String,
        view: serde_json::Value,
    },
    /// Set the title of an agent thread.
    SetThreadTitle {
        channel: String,
        thread_ts: String,
        title: String,
    },
    /// Pin suggested prompts to the top of the app's Messages tab.
    SetSuggestedPrompts { channel: String },
    /// Append a task update (tool call progress) to the active stream for
    /// this thread. `status` is `in_progress`, `complete`, or `error`.
    /// `details` optionally carries the tool call arguments.
    StreamTaskUpdate {
        channel: String,
        thread_ts: String,
        task_id: String,
        title: String,
        status: String,
        details: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_event() -> SlackEvent {
        SlackEvent {
            user: "U1".into(),
            text: "hello".into(),
            channel: "C1".into(),
            thread_ts: "1.0".into(),
            is_button_click: false,
            button_value: None,
            action_id: None,
            message_ts: None,
            button_text: None,
            is_bot: false,
            is_unauthorized: false,
            slash_command: None,
            response_url: None,
            trigger_id: None,
            is_app_home_opened: false,
        }
    }

    #[test]
    fn filter_logic_drops_bot_messages() {
        let event = SlackEvent {
            is_bot: true,
            ..base_event()
        };
        // Same filter as the dataflow
        assert!(!(!event.is_bot && !event.is_unauthorized));
    }

    #[test]
    fn filter_logic_drops_unauthorized() {
        let event = SlackEvent {
            is_unauthorized: true,
            ..base_event()
        };
        assert!(!(!event.is_bot && !event.is_unauthorized));
    }

    #[test]
    fn filter_logic_passes_valid_message() {
        let event = base_event();
        assert!(!event.is_bot && !event.is_unauthorized);
    }
}
