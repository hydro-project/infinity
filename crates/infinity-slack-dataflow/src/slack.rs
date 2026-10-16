//! Message types exchanged between the Slack I/O layer and the dataflow.

/// Callback ID for the model-picker modal (set on the view when opening,
/// echoed back by Slack in the `view_submission` payload).
pub const MODEL_PICKER_CALLBACK_ID: &str = "model_picker";
/// Block ID of the model-select input block in the model-picker modal.
pub const MODEL_PICKER_BLOCK_ID: &str = "model_block";
/// Action ID of the model-select element in the model-picker modal.
pub const MODEL_PICKER_ACTION_ID: &str = "model_select";

/// Callback ID of the "Fork from here" message shortcut (historical id —
/// kept so installed app manifests keep working) (declared in the
/// Slack app manifest under `features.shortcuts`).
pub const REWIND_SHORTCUT_CALLBACK_ID: &str = "rewind_to_here";
/// Callback ID for the rewind modal (set on the view when opening, echoed
/// back by Slack in the `view_submission` payload).
pub const REWIND_MODAL_CALLBACK_ID: &str = "rewind_submit";
/// Block ID of the edited-message input block in the rewind modal.
pub const REWIND_BLOCK_ID: &str = "rewind_block";
/// Action ID of the edited-message input element in the rewind modal.
pub const REWIND_ACTION_ID: &str = "rewind_text";

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
    /// True if the user clicked Slack's native stop button on a streaming
    /// response (`agent_session_stopped` event). `thread_ts` identifies the
    /// Slack thread the stop happened in (the root conversation thread or a
    /// subagent's dedicated thread), and `streaming_message_ts` lists the
    /// in-progress streaming message ts values Slack already stopped.
    #[serde(default)]
    pub is_session_stopped: bool,
    /// For stop events: the streaming message ts values Slack stopped
    /// client-side (may be empty — the stop still applies to the thread).
    #[serde(default)]
    pub streaming_message_ts: Vec<String>,
    /// The callback_id of a message shortcut (context-menu `message_action`
    /// payload), e.g. [`REWIND_SHORTCUT_CALLBACK_ID`]. `message_ts` carries
    /// the target message's ts, `thread_ts` the thread it lives in, and
    /// `response_url` where validation errors can be reported ephemerally.
    #[serde(default)]
    pub shortcut: Option<String>,
    /// For rewind-modal submissions ([`REWIND_MODAL_CALLBACK_ID`]): the
    /// 0-based user turn to fork from, resolved when the modal was opened
    /// and round-tripped through the view's `private_metadata` (which also
    /// restores `channel`, `thread_ts` and `message_ts`). `text` carries the
    /// edited message text to send as the fork's first turn. Also set on
    /// fork-ready events (see `fork_source_thread_ts`).
    #[serde(default)]
    pub rewind_user_turn: Option<usize>,
    /// Set by the Slack I/O layer once a fork's new Slack thread exists: a
    /// "fork-ready" event whose `thread_ts` is the NEW thread (its anchor
    /// message), naming the original thread it forks. `rewind_user_turn`
    /// and `text` carry the fork point and the edited first turn.
    #[serde(default)]
    pub fork_source_thread_ts: Option<String>,
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
        /// True when this stop ends the response itself (the dataflow saw the
        /// response-final `ResponseDone`), as opposed to an interposed
        /// finalize (choice buttons, errors, button clicks, rewind). The I/O
        /// layer clears the stream's delivery cursor on a response-final
        /// stop: the stream no longer needs resuming across a restart.
        #[serde(default)]
        end_of_response: bool,
    },
    /// The user clicked Slack's native stop button (`agent_session_stopped`):
    /// halt the stream for this thread *without* re-touching the stopped
    /// message (Slack already finalized it client-side), discard buffered
    /// output, and suppress further appends for the current response. The
    /// suppression latch clears on the response's final `StreamStop` or on
    /// the user's next message in the thread.
    HaltStreams {
        channel: String,
        thread_ts: String,
        /// Route to the dedicated thread of this subagent (see enum docs).
        #[serde(default)]
        subagent: Option<String>,
        /// Streaming message ts values Slack reported as already stopped.
        stopped_message_ts: Vec<String>,
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
    /// Open the "Fork from here" modal for a message-shortcut invocation:
    /// the I/O layer fetches the Slack thread, resolves the governing user
    /// turn of `target_ts` (the clicked message if it is a human message,
    /// otherwise the nearest human message before it), and opens a modal
    /// pre-filled with that turn's text. Validation failures are reported
    /// ephemerally via `response_url`.
    OpenRewindModal {
        channel: String,
        thread_ts: String,
        target_ts: String,
        trigger_id: String,
        response_url: String,
    },
    /// Fork `source_thread_ts`'s session into a NEW Slack thread: post a
    /// top-level anchor message linking back to `source_message_ts`, a
    /// cross-link in the source thread, then feed a fork-ready event keyed on
    /// the anchor's ts back into the dataflow. The source thread and its
    /// session are left untouched.
    ForkThread {
        channel: String,
        source_thread_ts: String,
        source_message_ts: String,
        user_turn: usize,
        text: String,
    },
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
            is_session_stopped: false,
            streaming_message_ts: Vec::new(),
            shortcut: None,
            rewind_user_turn: None,
            fork_source_thread_ts: None,
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
