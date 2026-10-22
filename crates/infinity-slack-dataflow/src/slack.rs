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

/// Serde adapter storing a `serde_json::Value` as its JSON string form.
///
/// `SlackAction`s cross the embedded-dataflow / simulator boundary in a
/// non-self-describing binary format, which cannot *deserialize* a raw
/// `serde_json::Value` (it requires `deserialize_any`). Round-tripping
/// through a string keeps the ergonomic `Value` fields while staying
/// compatible with that format.
mod json_as_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &serde_json::Value, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<serde_json::Value, D::Error> {
        let s = String::deserialize(d)?;
        serde_json::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Key identifying a logical output stream: the root Slack thread plus an
/// optional subagent id. Used by the dataflow and the Slack I/O layer to key
/// per-stream state (in-flight tool tasks, tool-call flags) consistently.
pub fn stream_key(thread_ts: &str, subagent: Option<&str>) -> String {
    match subagent {
        Some(id) => format!("{thread_ts}::{id}"),
        None => thread_ts.to_owned(),
    }
}

/// An action the dataflow instructs the Slack I/O layer to perform against
/// the Slack API.
///
/// Actions that carry a `subagent` field target a *dedicated subagent Slack
/// thread*: when set (to the subagent's agent thread id), the I/O layer
/// routes the action into a separate Slack thread created lazily for that
/// subagent instead of the root conversation thread (`thread_ts`). This
/// keeps concurrent subagent output from interleaving in one thread.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum SlackAction {
    /// Post a text message to a channel/thread.
    PostMessage {
        channel: String,
        text: String,
        thread_ts: Option<String>,
        /// Route into the dedicated thread of this subagent (see enum docs).
        #[serde(default)]
        subagent: Option<String>,
    },
    /// Post a message with Block Kit blocks.
    PostBlocks {
        channel: String,
        fallback_text: String,
        #[serde(with = "json_as_string")]
        blocks: serde_json::Value,
        thread_ts: Option<String>,
        /// If set, the Slack I/O layer will store the resulting message_ts
        /// under this choice_id so that a later `DismissChoiceButtons` can
        /// update it.
        ///
        /// NOTE: fields of this enum must not use `skip_serializing_if` —
        /// actions cross the embedded-dataflow boundary in a
        /// non-self-describing binary format where missing fields corrupt
        /// the stream.
        #[serde(default)]
        choice_id: Option<infinity_protocol::ChoiceId>,
        /// Route into the dedicated thread of this subagent (see enum docs).
        #[serde(default)]
        subagent: Option<String>,
    },
    /// Dismiss interactive buttons for a completed choice (replace with a
    /// "resolved" indicator). The Slack I/O layer looks up the stored
    /// message_ts from the choice_id.
    DismissChoiceButtons {
        choice_id: infinity_protocol::ChoiceId,
    },
    /// Update an existing message's blocks (e.g. to replace buttons with a selection indicator).
    UpdateMessage {
        channel: String,
        ts: String,
        text: String,
        #[serde(with = "json_as_string")]
        blocks: serde_json::Value,
    },
    /// Append text to the active stream for this thread (starts a stream if needed).
    StreamAppend {
        channel: String,
        thread_ts: String,
        text: String,
        /// Route into the dedicated thread of this subagent (see enum docs).
        #[serde(default)]
        subagent: Option<String>,
    },
    /// Stop/finalize the active stream for this thread.
    StreamStop {
        channel: String,
        thread_ts: String,
        /// Route into the dedicated thread of this subagent (see enum docs).
        #[serde(default)]
        subagent: Option<String>,
    },
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
        #[serde(with = "json_as_string")]
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
        /// Route into the dedicated thread of this subagent (see enum docs).
        #[serde(default)]
        subagent: Option<String>,
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

    #[test]
    fn stream_key_is_distinct_per_subagent() {
        assert_eq!(stream_key("1.0", None), "1.0");
        assert_eq!(stream_key("1.0", Some("sub1")), "1.0::sub1");
        assert_ne!(
            stream_key("1.0", Some("sub1")),
            stream_key("1.0", Some("sub2"))
        );
        assert_ne!(
            stream_key("1.0", Some("sub1")),
            stream_key("2.0", Some("sub1"))
        );
    }
}
