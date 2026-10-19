//! Slack I/O: bridges Slack's Socket Mode WebSocket and Web API to the
//! dataflow.
//!
//! Inbound: parses Socket Mode envelopes into [`SlackEvent`]s.
//! Outbound: executes [`SlackAction`]s against the Slack Web API (message
//! posting, streaming, statuses, modals, ...).

use futures_util::{SinkExt, StreamExt};
use infinity_slack_dataflow::config::Config;
use infinity_slack_dataflow::slack::{
    SlackAction, SlackEvent, MODEL_PICKER_ACTION_ID, MODEL_PICKER_BLOCK_ID,
    MODEL_PICKER_CALLBACK_ID, REWIND_ACTION_ID, REWIND_BLOCK_ID, REWIND_MODAL_CALLBACK_ID,
    THREAD_MODEL_MODAL_CALLBACK_ID,
};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::slack_client::SlackClient;

// ── Internal deserialization types ──────────────────────────────────────────

#[derive(Deserialize)]
struct ConnectionResponse {
    ok: bool,
    url: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct SocketEnvelope {
    envelope_id: String,
    #[serde(rename = "type")]
    envelope_type: String,
    payload: serde_json::Value,
}

#[derive(Deserialize)]
struct EventPayload {
    event: Option<RawSlackEvent>,
}

#[derive(Deserialize)]
struct RawSlackEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    subtype: Option<String>,
    user: Option<String>,
    text: Option<String>,
    channel: Option<String>,
    ts: Option<String>,
    thread_ts: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    /// Which App Home tab was opened (for `app_home_opened` events).
    #[serde(default)]
    tab: Option<String>,
    /// Streaming message ts values Slack stopped client-side (for
    /// `agent_session_stopped` events).
    #[serde(default)]
    streaming_message_ts: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct InteractivePayload {
    /// `block_actions` for button clicks, `view_submission` for modal
    /// submits, `message_action` for message shortcuts (context ⋮ menu).
    #[serde(rename = "type", default)]
    payload_type: String,
    #[serde(default)]
    actions: Vec<InteractiveAction>,
    channel: Option<InteractiveChannel>,
    message: Option<InteractiveMessage>,
    user: Option<InteractiveUser>,
    /// Present on `view_submission` payloads.
    view: Option<InteractiveView>,
    /// The shortcut's callback_id (present on `message_action` payloads).
    #[serde(default)]
    callback_id: Option<String>,
    /// Present on `message_action` payloads (and some others).
    #[serde(default)]
    trigger_id: Option<String>,
    /// Present on `message_action` payloads: where ephemeral responses to
    /// the invoking user can be POSTed.
    #[serde(default)]
    response_url: Option<String>,
}

#[derive(Deserialize)]
struct InteractiveAction {
    #[serde(default)]
    action_id: String,
    #[serde(default)]
    value: String,
    /// The button label (Slack sends `{ "type": "plain_text", "text": "..." }`).
    text: Option<InteractiveText>,
}

#[derive(Deserialize)]
struct InteractiveText {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct InteractiveChannel {
    id: String,
}

#[derive(Deserialize)]
struct InteractiveMessage {
    thread_ts: Option<String>,
    ts: Option<String>,
}

#[derive(Deserialize)]
struct InteractiveUser {
    id: String,
}

/// The modal view echoed back in a `view_submission` payload.
#[derive(Deserialize)]
struct InteractiveView {
    #[serde(default)]
    callback_id: String,
    /// Arbitrary state we stashed when opening the modal (the slash command's
    /// `response_url`, so the confirmation can be sent back to the user).
    #[serde(default)]
    private_metadata: String,
    state: Option<InteractiveViewState>,
}

#[derive(Deserialize)]
struct InteractiveViewState {
    #[serde(default)]
    values: serde_json::Value,
}

/// Payload of a `slash_commands` envelope (Socket Mode).
#[derive(Deserialize)]
struct SlashCommandPayload {
    command: String,
    #[serde(default)]
    text: String,
    user_id: String,
    channel_id: String,
    response_url: String,
    #[serde(default)]
    trigger_id: Option<String>,
}

// ── I/O task constructor ─────────────────────────────────────────────────────

/// Endpoints and task handles returned by [`spawn`].
pub struct SlackIo {
    /// Parsed Slack events; the bot wraps this in a stream and feeds it to
    /// the embedded dataflow.
    pub events: mpsc::Receiver<SlackEvent>,
    /// Actions produced by the dataflow; a spawned task executes them against
    /// the Slack Web API. The channel is unbounded because the dataflow emits
    /// actions from a synchronous callback during a tick (it cannot await
    /// backpressure), and action volume is bounded by human-scale chat traffic.
    pub actions: mpsc::UnboundedSender<SlackAction>,
    /// Handle of the outbound (action executor) task. This task never
    /// finishes during normal operation; completion means a panic or bug and
    /// should be treated as fatal by the caller.
    pub outbound: tokio::task::JoinHandle<()>,
    /// Handle of the inbound (Socket Mode) task. Same liveness contract as
    /// `outbound`, with one nuance: it also returns cleanly if the
    /// dataflow-side event channel closes -- which only happens once the
    /// dataflow itself is gone, so the caller should still treat completion
    /// as fatal.
    pub inbound: tokio::task::JoinHandle<()>,
}

/// Validates the Slack credentials, then spawns the Slack I/O tasks and
/// returns the dataflow-facing endpoints.
///
/// `shutdown` coordinates a clean restart: when it flips to `true`, the
/// inbound task closes the Socket Mode WebSocket with a proper close
/// handshake (no new events; unread envelopes stay unACKed so Slack
/// redelivers them after the restart) and the outbound task drains — flushes
/// buffered text, finalizes every active stream, clears thread statuses —
/// then exits so the caller can persist the resume state.
pub async fn spawn(
    config: &'static Config,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<SlackIo, crate::BoxError> {
    // Validate both credentials on the startup path: a bad token must fail
    // startup with an actionable error. (Previously the bot token was checked
    // inside a detached task, whose panic tokio isolates -- leaving the bot
    // running but permanently unable to execute actions.)
    let slack = SlackClient::new(&config.bot_token).await.map_err(|e| {
        format!("failed to authenticate with Slack: {e}\nVerify `bot_token` in slack.json")
    })?;
    tracing::info!("Slack bot authenticated");
    let initial_ws_url = get_ws_url(&config.app_token).await.map_err(|e| {
        format!("failed to open a Socket Mode connection: {e}\nVerify `app_token` in slack.json")
    })?;

    let (to_df_tx, to_df_rx) = mpsc::channel::<SlackEvent>(1024);
    let (from_df_tx, mut from_df_rx) = mpsc::unbounded_channel::<SlackAction>();
    // The executor feeds synthesized events back into the dataflow (a fork's
    // new Slack thread once its anchor exists).
    let feedback_tx = to_df_tx.clone();

    // Outbound: execute SlackActions against the Slack API.
    let mut outbound_shutdown = shutdown.clone();
    let outbound = tokio::spawn(async move {
        /// Per-thread stream state.
        struct StreamState {
            ts: String,
            /// The channel the stream lives in (needed to finalize the
            /// stream at shutdown without an action in hand).
            channel: String,
            char_count: usize,
            in_code_block: bool,
            /// When this stream message was started. Streams older than
            /// `MAX_STREAM_AGE` are split at the next clean breaking point so
            /// long-running responses don't pile into one giant message.
            started_at: std::time::Instant,
            /// Task cards appended to THIS stream message that are still
            /// "in progress": `(task_id, title, details)`. Slack task cards
            /// are addressed by `(channel, message ts, task id)`, so a card
            /// can only be updated within the message it was created in —
            /// whenever this message is stopped (explicit stop, split, or
            /// error recovery) these cards must be completed first or their
            /// spinners freeze forever.
            open_tasks: Vec<(String, String, String)>,
        }

        impl StreamState {
            /// Remove and return the open task with the given id, if this
            /// stream message owns it.
            fn take_open_task(&mut self, task_id: &str) -> Option<(String, String, String)> {
                let idx = self
                    .open_tasks
                    .iter()
                    .position(|(id, _, _)| id == task_id)?;
                Some(self.open_tasks.remove(idx))
            }
        }

        /// Maximum age of a stream message before it is split at the next
        /// clean breaking point.
        const MAX_STREAM_AGE: std::time::Duration = std::time::Duration::from_secs(3 * 60);

        // Active streams, keyed by the Slack thread ts the stream lives in
        // (the root conversation thread, or a subagent thread's anchor ts).
        let mut active_streams: std::collections::HashMap<String, StreamState> =
            std::collections::HashMap::new();

        // Last status text set per thread_ts, to dedup repeated SetStatus calls
        // (e.g. "is thinking" emitted on every text chunk).
        let mut last_status: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        /// Anchor message ts of each dedicated subagent Slack thread, keyed
        /// by (root thread_ts, subagent id). Seeded from the persisted
        /// subagent routes so resumed subagent output continues in its
        /// existing thread instead of forking a duplicate.
        type AnchorMap = std::collections::HashMap<(String, String), String>;
        let mut anchors: AnchorMap = {
            let rt = infinity_slack_dataflow::runtime::get();
            let routes = rt.subagent_routes.lock().expect("bug: lock poisoned");
            routes
                .iter()
                .map(|(anchor_ts, route)| {
                    (
                        (route.root_thread_ts.clone(), route.subagent_id.clone()),
                        anchor_ts.clone(),
                    )
                })
                .collect()
        };

        /// Advance the delivery cursor of a stream (see
        /// `runtime::Runtime::stream_progress`): `text_bytes` bytes of
        /// streamed text plus the given counts of tool-call/-result cards
        /// were delivered. Creating the entry marks the stream's response as
        /// open (it will be resumed across a restart).
        fn note_delivered(
            thread_ts: &str,
            subagent: Option<&str>,
            text_bytes: usize,
            tool_calls: usize,
            tool_results: usize,
        ) {
            let rt = infinity_slack_dataflow::runtime::get();
            let mut progress = rt.stream_progress.lock().expect("bug: lock poisoned");
            let cursor = progress
                .entry((thread_ts.to_owned(), subagent.map(str::to_owned)))
                .or_default();
            cursor.text_bytes += text_bytes;
            cursor.tool_calls += tool_calls;
            cursor.tool_results += tool_results;
        }

        /// The stream's response ended: drop its delivery cursor (nothing to
        /// resume across a restart anymore).
        fn clear_delivery_cursor(thread_ts: &str, subagent: Option<&str>) {
            let rt = infinity_slack_dataflow::runtime::get();
            rt.stream_progress
                .lock()
                .expect("bug: lock poisoned")
                .remove(&(thread_ts.to_owned(), subagent.map(str::to_owned)));
        }

        /// Short human-readable form of an agent thread id (`ThreadRef`
        /// string form, i.e. `"{id}"` or `"{remote}/{id}"`).
        fn short_subagent_id(id: &str) -> &str {
            let bare = id.rsplit('/').next().unwrap_or(id);
            bare.char_indices()
                .nth(8)
                .map(|(i, _)| &bare[..i])
                .unwrap_or(bare)
        }

        /// Mark every still-open task card of a stream message complete, so
        /// the message is never left (or stopped) with frozen spinners. Used
        /// right before a stream message is stopped on any path: explicit
        /// finalize, length/age split, and append-error recovery. Best
        /// effort: failures are logged (the message may already be
        /// unappendable, e.g. after `msg_too_long`).
        async fn complete_open_tasks(slack: &SlackClient, channel: &str, state: &mut StreamState) {
            for (task_id, title, details) in state.open_tasks.drain(..) {
                if let Err(e) = slack
                    .append_stream_task(channel, &state.ts, &task_id, &title, "complete", &details)
                    .await
                {
                    tracing::warn!("completing open task {task_id} failed: {e}");
                }
            }
        }

        /// Whether the user stopped the current response in this target
        /// thread via Slack's native stop button (see
        /// `runtime::Runtime::halted_streams`).
        fn is_halted(target_ts: &str) -> bool {
            let rt = infinity_slack_dataflow::runtime::get();
            rt.halted_streams
                .lock()
                .expect("bug: lock poisoned")
                .contains(target_ts)
        }

        /// Latch a target thread as user-stopped: its remaining output for
        /// the current response is suppressed.
        fn set_halted(target_ts: &str) {
            let rt = infinity_slack_dataflow::runtime::get();
            rt.halted_streams
                .lock()
                .expect("bug: lock poisoned")
                .insert(target_ts.to_owned());
        }

        /// Lift the user-stop latch (next response may stream again).
        fn clear_halted(target_ts: &str) {
            let rt = infinity_slack_dataflow::runtime::get();
            rt.halted_streams
                .lock()
                .expect("bug: lock poisoned")
                .remove(target_ts);
        }

        /// Finalize the active stream in `thread_ts`: flush its buffered
        /// text, mark any still-open tool-task cards complete (so the
        /// message never finalizes with spinners), and stop the stream
        /// message with the AI-disclaimer footer. No-op when the thread has
        /// no buffered text and no active stream.
        ///
        /// Used by `StreamStop` and — crucially — before posting any
        /// discrete message (choice buttons, errors, subagent cross-links)
        /// into a thread: an interposed message must not leave the stream
        /// above it active, otherwise subsequent appends keep editing the
        /// older message and the thread reads out of order. The disclaimer
        /// footer is applied on this path too, so every finalized stream
        /// message ends uniformly.
        async fn finalize_stream(
            slack: &SlackClient,
            channel: &str,
            thread_ts: &str,
            active_streams: &mut std::collections::HashMap<String, StreamState>,
            pending_text: &mut std::collections::HashMap<String, PendingText>,
        ) {
            // Flush buffered text first so the closing message contains
            // everything streamed so far.
            if let Some(pending) = pending_text.remove(thread_ts) {
                if !pending.text.is_empty() {
                    append_text(
                        slack,
                        &pending.channel,
                        thread_ts,
                        &pending.text,
                        active_streams,
                    )
                    .await;
                }
            }
            if let Some(mut state) = active_streams.remove(thread_ts) {
                complete_open_tasks(slack, channel, &mut state).await;

                // Finalize with an AI-content disclaimer footer.
                let disclaimer = serde_json::json!([
                    {
                        "type": "context",
                        "elements": [
                            {
                                "type": "mrkdwn",
                                "text": "AI-generated response — review carefully before acting on it."
                            }
                        ]
                    }
                ]);
                if let Err(e) = slack
                    .stop_stream_with_blocks(channel, &state.ts, &disclaimer)
                    .await
                {
                    tracing::error!("stop_stream failed: {e}");
                }
            }
        }

        /// Resolve the Slack thread an action targets: the root conversation
        /// thread itself, or — when `subagent` is set — the dedicated Slack
        /// thread for that subagent, creating its anchor message (plus a
        /// cross-link in the root thread) on first use. Returns `None` if the
        /// anchor could not be created.
        ///
        /// Creating an anchor posts a cross-link into the *root* thread, so
        /// this takes the stream maps in order to finalize the root thread's
        /// active stream first (see [`finalize_stream`]).
        async fn resolve_thread(
            slack: &SlackClient,
            channel: &str,
            thread_ts: &str,
            subagent: Option<&str>,
            anchors: &mut AnchorMap,
            active_streams: &mut std::collections::HashMap<String, StreamState>,
            pending_text: &mut std::collections::HashMap<String, PendingText>,
        ) -> Option<String> {
            let Some(sub) = subagent else {
                return Some(thread_ts.to_owned());
            };
            let key = (thread_ts.to_owned(), sub.to_owned());
            if let Some(ts) = anchors.get(&key) {
                return Some(ts.clone());
            }

            // First output from this subagent: create its dedicated thread.
            // The anchor must be a top-level channel message — Slack thread
            // replies cannot host threads of their own.
            let short = short_subagent_id(sub);
            let text = format!("🤖 Subagent `{short}` — output streams in this thread.");
            let anchor_ts = match slack.post_message(channel, &text, None).await {
                Ok(Some(ts)) => ts,
                Ok(None) => {
                    tracing::error!("subagent anchor post returned no ts");
                    return None;
                }
                Err(e) => {
                    tracing::error!("subagent anchor post failed: {e}");
                    return None;
                }
            };
            anchors.insert(key, anchor_ts.clone());

            // Register the reverse route so replies and button clicks inside
            // the subagent thread are routed back to the session (and this
            // subagent's agent thread) by the dataflow.
            {
                let rt = infinity_slack_dataflow::runtime::get();
                rt.subagent_routes
                    .lock()
                    .expect("bug: lock poisoned")
                    .insert(
                        anchor_ts.clone(),
                        infinity_slack_dataflow::runtime::SubagentRoute {
                            root_thread_ts: thread_ts.to_owned(),
                            subagent_id: sub.to_owned(),
                        },
                    );
            }

            // Cross-link the new thread from the root conversation.
            let permalink = match slack.get_permalink(channel, &anchor_ts).await {
                Ok(url) => url,
                Err(e) => {
                    tracing::warn!("chat.getPermalink failed: {e}");
                    None
                }
            };
            let link_text = match permalink {
                Some(url) => format!(
                    "🧵 Spawned subagent `{short}` — <{url}|follow its progress in its own thread>."
                ),
                None => {
                    format!(
                        "🧵 Spawned subagent `{short}` — follow its progress in its own thread."
                    )
                }
            };
            // The cross-link lands below the root thread's stream message; if
            // that stream stayed active, subsequent root output would keep
            // editing the message *above* the cross-link (discontinuity).
            // Finalize the root stream first so later root output starts a
            // fresh message below the cross-link. (Root streams are keyed by
            // the bare thread ts.)
            finalize_stream(slack, channel, thread_ts, active_streams, pending_text).await;
            if let Err(e) = slack
                .post_message(channel, &link_text, Some(thread_ts))
                .await
            {
                tracing::warn!("posting subagent cross-link failed: {e}");
            }

            Some(anchor_ts)
        }

        /// Start a fresh stream, returning the new ts or None on failure.
        async fn start_fresh(
            slack: &SlackClient,
            channel: &str,
            thread_ts: &str,
            streams: &mut std::collections::HashMap<String, StreamState>,
        ) -> Option<String> {
            match slack.start_stream(channel, thread_ts, None).await {
                Ok(Some(ts)) => {
                    streams.insert(
                        thread_ts.to_owned(),
                        StreamState {
                            ts: ts.clone(),
                            channel: channel.to_owned(),
                            char_count: 0,
                            in_code_block: false,
                            started_at: std::time::Instant::now(),
                            open_tasks: Vec::new(),
                        },
                    );
                    Some(ts)
                }
                Ok(None) => {
                    tracing::error!("start_stream returned no ts");
                    None
                }
                Err(e) => {
                    tracing::error!("start_stream failed: {e}");
                    None
                }
            }
        }

        /// Count triple-backtick fences in text and update code-block state.
        fn update_code_block_state(text: &str, in_code_block: bool) -> bool {
            let mut state = in_code_block;
            for line in text.split('\n') {
                if line.trim_start().starts_with("```") {
                    state = !state;
                }
            }
            state
        }

        /// Append text to a thread's active stream, starting or splitting the
        /// stream message as needed. This is the flush path for buffered text.
        async fn append_text(
            slack: &SlackClient,
            channel: &str,
            thread_ts: &str,
            text: &str,
            active_streams: &mut std::collections::HashMap<String, StreamState>,
        ) {
            // Ensure we have an active stream.
            if !active_streams.contains_key(thread_ts)
                && start_fresh(slack, channel, thread_ts, active_streams)
                    .await
                    .is_none()
            {
                return;
            }

            // Split at a clean breaking point (text has a newline and we're
            // not inside a code block) when either:
            // - the message has grown past 20k chars, or
            // - the stream has been open longer than MAX_STREAM_AGE.
            let should_split = {
                let state = active_streams.get(thread_ts).expect("bug: just inserted");
                let clean_break = text.contains('\n') && !state.in_code_block;
                let too_long = state.char_count > 20_000;
                let too_old = state.started_at.elapsed() > MAX_STREAM_AGE;
                clean_break && (too_long || too_old)
            };

            if should_split {
                // Stop current stream and start a new one. Complete the open
                // task cards first: cards live in the message they were
                // created in, so stopping without completing them would
                // freeze their spinners forever.
                if let Some(mut old) = active_streams.remove(thread_ts) {
                    complete_open_tasks(slack, channel, &mut old).await;
                    if let Err(e) = slack.stop_stream(channel, &old.ts).await {
                        tracing::warn!("stop_stream on split failed: {e}");
                    }
                }
                if start_fresh(slack, channel, thread_ts, active_streams)
                    .await
                    .is_none()
                {
                    return;
                }
            }

            let stream_ts = active_streams
                .get(thread_ts)
                .expect("bug: stream must exist")
                .ts
                .clone();

            // Append and handle error codes.
            match slack.append_stream(channel, &stream_ts, text).await {
                Ok(None) => {
                    // Success — update state.
                    let state = active_streams
                        .get_mut(thread_ts)
                        .expect("bug: stream must exist");
                    state.char_count += text.len();
                    state.in_code_block = update_code_block_state(text, state.in_code_block);
                }
                Ok(Some(ref err)) if err == "stopped_by_user" => {
                    // The user clicked Slack's stop button on this message:
                    // no further appends are accepted and the rest of this
                    // response must NOT be resurrected in a fresh stream.
                    // (Defense-in-depth: the `agent_session_stopped` event
                    // sets the same latch.)
                    tracing::info!("stream in {thread_ts} stopped by user; suppressing output");
                    active_streams.remove(thread_ts);
                    set_halted(thread_ts);
                }
                Ok(Some(ref err)) => {
                    // Recoverable (e.g. msg_too_long, expired stream): start a
                    // new stream and retry. Complete the open task cards and
                    // stop the old message first (both best effort — e.g. after
                    // `msg_too_long` small task appends usually still fit;
                    // after `message_not_in_streaming_state` they fail and are
                    // logged) so the abandoned message isn't left with frozen
                    // spinners or in a dangling streaming state.
                    tracing::warn!("append_stream got {err}, starting new stream");
                    if let Some(mut old) = active_streams.remove(thread_ts) {
                        complete_open_tasks(slack, channel, &mut old).await;
                        if let Err(e) = slack.stop_stream(channel, &old.ts).await {
                            tracing::warn!("stop_stream on append-error recovery failed: {e}");
                        }
                    }
                    if start_fresh(slack, channel, thread_ts, active_streams)
                        .await
                        .is_none()
                    {
                        return;
                    }
                    let new_ts = active_streams
                        .get(thread_ts)
                        .expect("bug: just started")
                        .ts
                        .clone();
                    match slack.append_stream(channel, &new_ts, text).await {
                        Ok(None) => {
                            let state = active_streams
                                .get_mut(thread_ts)
                                .expect("bug: stream must exist");
                            state.char_count += text.len();
                            state.in_code_block =
                                update_code_block_state(text, state.in_code_block);
                        }
                        Ok(Some(err)) => {
                            tracing::error!(
                                "append_stream retry failed with {err}, dropping chunk"
                            );
                        }
                        Err(e) => {
                            tracing::error!("append_stream retry failed: {e}");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("append_stream failed: {e}");
                }
            }
        }

        /// Buffered stream text keyed by the resolved target Slack thread ts,
        /// flushed on a fixed interval to avoid Slack rate limits
        /// (`chat.appendStream` throttles on rapid successive updates).
        struct PendingText {
            channel: String,
            text: String,
        }
        let mut pending_text: std::collections::HashMap<String, PendingText> =
            std::collections::HashMap::new();

        /// How often buffered stream text is flushed to Slack.
        const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
        let mut flush_tick = tokio::time::interval(FLUSH_INTERVAL);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let action = tokio::select! {
                maybe_action = from_df_rx.recv() => match maybe_action {
                    Some(a) => a,
                    None => break,
                },
                result = outbound_shutdown.changed() => {
                    if result.is_err() {
                        tracing::warn!("shutdown channel closed; draining Slack streams");
                    } else {
                        tracing::info!("shutting down: draining Slack streams");
                    }
                    break;
                }
                _ = flush_tick.tick() => {
                    // Flush all buffered text (one append per thread).
                    let ready: Vec<(String, PendingText)> = pending_text.drain().collect();
                    for (thread_ts, pending) in ready {
                        if pending.text.is_empty() || is_halted(&thread_ts) {
                            continue;
                        }
                        append_text(
                            &slack,
                            &pending.channel,
                            &thread_ts,
                            &pending.text,
                            &mut active_streams,
                        )
                        .await;
                    }
                    continue;
                }
            };
            match action {
                SlackAction::PostMessage {
                    channel,
                    text,
                    thread_ts,
                    subagent,
                } => {
                    // Route subagent-tagged messages into the subagent's
                    // dedicated thread; fall back to the root thread if the
                    // dedicated thread could not be created.
                    let target_ts = match thread_ts {
                        Some(root) => {
                            let target = resolve_thread(
                                &slack,
                                &channel,
                                &root,
                                subagent.as_deref(),
                                &mut anchors,
                                &mut active_streams,
                                &mut pending_text,
                            )
                            .await
                            .unwrap_or_else(|| root.clone());
                            // Finalize the target thread's stream so the
                            // posted message doesn't end up below a
                            // still-active stream message (which later
                            // appends would keep editing — discontinuity).
                            finalize_stream(
                                &slack,
                                &channel,
                                &target,
                                &mut active_streams,
                                &mut pending_text,
                            )
                            .await;
                            Some(target)
                        }
                        None => None,
                    };
                    if let Err(e) = slack
                        .post_message(&channel, &text, target_ts.as_deref())
                        .await
                    {
                        tracing::error!("PostMessage failed: {e}");
                    }
                }
                SlackAction::PostBlocks {
                    channel,
                    fallback_text,
                    blocks,
                    thread_ts,
                    choice_id,
                    subagent,
                } => {
                    let target_ts = match thread_ts {
                        Some(root) => {
                            let target = resolve_thread(
                                &slack,
                                &channel,
                                &root,
                                subagent.as_deref(),
                                &mut anchors,
                                &mut active_streams,
                                &mut pending_text,
                            )
                            .await
                            .unwrap_or_else(|| root.clone());
                            // See PostMessage: never post blocks below a
                            // still-active stream message.
                            finalize_stream(
                                &slack,
                                &channel,
                                &target,
                                &mut active_streams,
                                &mut pending_text,
                            )
                            .await;
                            Some(target)
                        }
                        None => None,
                    };
                    match slack
                        .post_blocks(&channel, &fallback_text, &blocks, target_ts.as_deref())
                        .await
                    {
                        Ok(Some(msg_ts)) => {
                            // If this is a choice-button message, store the ts so we
                            // can dismiss it later when the choice is resolved without
                            // a Slack button click.
                            if let Some(cid) = choice_id {
                                let rt = infinity_slack_dataflow::runtime::get();
                                rt.choice_messages
                                    .lock()
                                    .expect("bug: lock poisoned")
                                    .insert(cid.into_inner(), (channel, msg_ts));
                            }
                        }
                        Ok(None) => {
                            tracing::warn!("PostBlocks succeeded but returned no ts");
                        }
                        Err(e) => {
                            tracing::error!("PostBlocks failed: {e}");
                        }
                    }
                }
                SlackAction::UpdateMessage {
                    channel,
                    ts,
                    text,
                    blocks,
                } => {
                    if let Err(e) = slack.update_message(&channel, &ts, &text, &blocks).await {
                        tracing::error!("UpdateMessage failed: {e}");
                    }
                }
                SlackAction::StreamAppend {
                    channel,
                    thread_ts,
                    text,
                    subagent,
                } => {
                    // Resolve the target Slack thread first (creating the
                    // subagent's dedicated thread if needed) so the buffer is
                    // keyed by the actual thread the text will stream into.
                    // Fall back to the root thread if creation failed, so
                    // output is never dropped.
                    let root_ts = thread_ts.clone();
                    let target_ts = resolve_thread(
                        &slack,
                        &channel,
                        &thread_ts,
                        subagent.as_deref(),
                        &mut anchors,
                        &mut active_streams,
                        &mut pending_text,
                    )
                    .await
                    .unwrap_or(thread_ts);
                    // The user stopped this response: drop the chunk rather
                    // than resurrecting the stream below the stop point.
                    if is_halted(&target_ts) {
                        tracing::debug!("dropping stream append for user-stopped {target_ts}");
                        continue;
                    }
                    // Count the text as delivered once buffered: the buffer
                    // is flushed before shutdown completes, so the resume
                    // cursor stays in step with what Slack received.
                    note_delivered(&root_ts, subagent.as_deref(), text.len(), 0, 0);
                    // Buffer the text; it is flushed on the next interval tick
                    // (or immediately on a tool update / stream stop). This
                    // coalesces rapid chunks into one append to avoid Slack
                    // throttling.
                    let entry = pending_text
                        .entry(target_ts)
                        .or_insert_with(|| PendingText {
                            channel: channel.clone(),
                            text: String::new(),
                        });
                    entry.text.push_str(&text);
                }
                SlackAction::StreamStop {
                    channel,
                    thread_ts,
                    subagent,
                    end_of_response,
                } => {
                    // A response-final stop ends the stream's life: nothing
                    // left to resume across a restart.
                    if end_of_response {
                        clear_delivery_cursor(&thread_ts, subagent.as_deref());
                    }
                    // Look up the target thread without creating one: if the
                    // subagent never produced output, there is no stream to
                    // stop and no dedicated thread should be forked for it.
                    let target_ts = match &subagent {
                        Some(sub) => anchors.get(&(thread_ts.clone(), sub.clone())).cloned(),
                        None => Some(thread_ts.clone()),
                    };
                    if let Some(target_ts) = target_ts {
                        if is_halted(&target_ts) {
                            // The user stopped this response; its stream is
                            // already finalized by Slack and the suppressed
                            // remainder was dropped. The response is over —
                            // lift the latch so the next response streams.
                            clear_halted(&target_ts);
                            pending_text.remove(&target_ts);
                            if let Some(state) = active_streams.remove(&target_ts) {
                                // Shouldn't normally exist while halted, but
                                // never leave a stream in streaming state.
                                if let Err(e) = slack.stop_stream(&channel, &state.ts).await {
                                    tracing::warn!("stop_stream after user stop failed: {e}");
                                }
                            }
                        } else {
                            finalize_stream(
                                &slack,
                                &channel,
                                &target_ts,
                                &mut active_streams,
                                &mut pending_text,
                            )
                            .await;
                        }
                    }
                    // Clear the thread status indicator. The status lives on
                    // the root conversation thread and tracks the root
                    // response, so subagent stream stops leave it alone.
                    if subagent.is_none() {
                        if let Err(e) = slack.set_thread_status(&channel, &thread_ts, "").await {
                            tracing::warn!("clearing thread status failed: {e}");
                        }
                        last_status.remove(&thread_ts);
                    }
                }
                SlackAction::HaltStreams {
                    channel,
                    thread_ts,
                    subagent,
                    stopped_message_ts,
                } => {
                    // The user clicked Slack's native stop button. Slack has
                    // already finalized the streaming message(s) client-side —
                    // do not re-touch them. Discard buffered output and
                    // in-flight task cards, and latch the thread so the
                    // remainder of the current response is suppressed.
                    // Look up the target thread without creating one: a stop
                    // can only come from a thread that already exists.
                    let target_ts = match &subagent {
                        Some(sub) => anchors.get(&(thread_ts.clone(), sub.clone())).cloned(),
                        None => Some(thread_ts.clone()),
                    };
                    let Some(target_ts) = target_ts else {
                        tracing::warn!(
                            "HaltStreams for unknown subagent thread (root {thread_ts})"
                        );
                        continue;
                    };
                    set_halted(&target_ts);
                    let dropped_text = pending_text
                        .remove(&target_ts)
                        .map(|p| p.text.len())
                        .unwrap_or(0);
                    // Any open task cards in the removed stream are frozen by
                    // Slack's client-side stop; discard them rather than
                    // appending completions to a dead message.
                    let mut dropped_tasks = 0;
                    if let Some(state) = active_streams.remove(&target_ts) {
                        dropped_tasks = state.open_tasks.len();
                        if !stopped_message_ts.contains(&state.ts) {
                            // Our live stream wasn't among the messages Slack
                            // stopped (e.g. it raced a split); stop it
                            // ourselves so nothing is left streaming.
                            if let Err(e) = slack.stop_stream(&channel, &state.ts).await {
                                tracing::warn!("stop_stream on user stop failed: {e}");
                            }
                        }
                    }
                    tracing::info!(
                        "user stopped response in {target_ts} (dropped {dropped_text} buffered \
                         chars, {dropped_tasks} task cards)"
                    );
                    // Transition the thread status out of "processing" (an
                    // empty status bridges to the `active` session state).
                    // Subagent threads carry no status indicator.
                    if subagent.is_none() {
                        if let Err(e) = slack.set_thread_status(&channel, &thread_ts, "").await {
                            tracing::warn!("clearing thread status on stop failed: {e}");
                        }
                        last_status.remove(&thread_ts);
                    }
                }
                SlackAction::SetStatus {
                    channel,
                    thread_ts,
                    status,
                } => {
                    // While the user has this thread's response stopped, do
                    // not re-arm the "processing" status.
                    if is_halted(&thread_ts) {
                        continue;
                    }
                    // Skip redundant updates (the same status is emitted on
                    // every text chunk) to avoid spamming the API.
                    if last_status.get(&thread_ts) == Some(&status) {
                        continue;
                    }
                    match slack.set_thread_status(&channel, &thread_ts, &status).await {
                        Ok(()) => {
                            last_status.insert(thread_ts, status);
                        }
                        Err(e) => {
                            tracing::error!("set_thread_status failed: {e}");
                        }
                    }
                }
                SlackAction::CommandResponse { response_url, text } => {
                    if let Err(e) = slack.respond_to_command(&response_url, &text).await {
                        tracing::error!("CommandResponse failed: {e}");
                    }
                }
                SlackAction::OpenRewindModal {
                    channel,
                    thread_ts,
                    target_ts,
                    trigger_id,
                    response_url,
                } => {
                    // Resolve the governing user turn (needs the thread's
                    // replies) and open the edit modal; validation failures go
                    // back to the invoking user ephemerally. Must stay quick:
                    // the trigger_id expires ~3s after the shortcut click.
                    if let Err(user_msg) =
                        open_rewind_modal(&slack, &channel, &thread_ts, &target_ts, &trigger_id)
                            .await
                    {
                        if let Err(e) = slack.respond_to_command(&response_url, &user_msg).await {
                            tracing::error!("rewind error response failed: {e}");
                        }
                    }
                }
                SlackAction::ForkThread {
                    channel,
                    source_thread_ts,
                    source_message_ts,
                    user_turn,
                    text,
                } => {
                    // Non-destructive fork: open the fork's own Slack thread,
                    // then let the dataflow start the forked session in it.
                    let Some(fork_ts) = post_fork_thread(
                        &slack,
                        &channel,
                        &source_thread_ts,
                        &source_message_ts,
                        &text,
                    )
                    .await
                    else {
                        continue;
                    };
                    let ready =
                        fork_ready_event(&channel, &fork_ts, &source_thread_ts, user_turn, &text);
                    if feedback_tx.send(ready).await.is_err() {
                        tracing::error!("dataflow channel closed; dropping fork of {fork_ts}");
                    }
                }
                SlackAction::OpenView { trigger_id, view } => {
                    if let Err(e) = slack.open_view(&trigger_id, &view).await {
                        tracing::error!("OpenView failed: {e}");
                    }
                }
                SlackAction::SetThreadTitle {
                    channel,
                    thread_ts,
                    title,
                } => {
                    if let Err(e) = slack.set_thread_title(&channel, &thread_ts, &title).await {
                        tracing::error!("SetThreadTitle failed: {e}");
                    }
                }
                SlackAction::SetSuggestedPrompts { channel } => {
                    let prompts = serde_json::json!([
                        {
                            "title": "Summarize my working copy",
                            "message": "Summarize the current changes in my working copy."
                        },
                        {
                            "title": "Fix the build",
                            "message": "Run the build and fix any errors you find."
                        },
                        {
                            "title": "Review recent commits",
                            "message": "Review the most recent commits and highlight anything concerning."
                        }
                    ]);
                    if let Err(e) = slack
                        .set_suggested_prompts(&channel, "Try one of these:", &prompts)
                        .await
                    {
                        tracing::error!("SetSuggestedPrompts failed: {e}");
                    }
                }
                SlackAction::StreamTaskUpdate {
                    channel,
                    thread_ts,
                    task_id,
                    title,
                    status,
                    details,
                    subagent,
                } => {
                    // Resolve the target Slack thread (creating the
                    // subagent's dedicated thread if needed); fall back to
                    // the root thread so the task card is never dropped.
                    let root_ts = thread_ts.clone();
                    let target_ts = resolve_thread(
                        &slack,
                        &channel,
                        &thread_ts,
                        subagent.as_deref(),
                        &mut anchors,
                        &mut active_streams,
                        &mut pending_text,
                    )
                    .await
                    .unwrap_or(thread_ts);
                    // The user stopped this response: drop the task card
                    // rather than opening a fresh stream below the stop point.
                    if is_halted(&target_ts) {
                        tracing::debug!("dropping task update for user-stopped {target_ts}");
                        continue;
                    }
                    // Advance the delivery cursor (calls and completions are
                    // consumed here even when the owning message is gone —
                    // the card was completed at finalize in that case).
                    if status == "in_progress" {
                        note_delivered(&root_ts, subagent.as_deref(), 0, 1, 0);
                    } else {
                        note_delivered(&root_ts, subagent.as_deref(), 0, 0, 1);
                    }
                    // Flush buffered text first so the tool indicator appears
                    // after the text that preceded it (preserves ordering).
                    if let Some(pending) = pending_text.remove(&target_ts) {
                        if !pending.text.is_empty() {
                            append_text(
                                &slack,
                                &pending.channel,
                                &target_ts,
                                &pending.text,
                                &mut active_streams,
                            )
                            .await;
                        }
                    }

                    if status == "in_progress" {
                        // New task card: ensure we have an active stream to
                        // attach it to, and record it against that specific
                        // message so every stop/split path can complete it.
                        if !active_streams.contains_key(&target_ts)
                            && start_fresh(&slack, &channel, &target_ts, &mut active_streams)
                                .await
                                .is_none()
                        {
                            continue;
                        }
                        let state = active_streams
                            .get_mut(&target_ts)
                            .expect("bug: just ensured stream exists");
                        let stream_ts = state.ts.clone();
                        state
                            .open_tasks
                            .push((task_id.clone(), title.clone(), details.clone()));
                        if let Err(e) = slack
                            .append_stream_task(
                                &channel, &stream_ts, &task_id, &title, &status, &details,
                            )
                            .await
                        {
                            tracing::error!("append_stream_task failed: {e}");
                        }
                    } else {
                        // Completion (or error) update: Slack task cards can
                        // only be updated inside the message that created
                        // them. If that message was already finalized (stream
                        // stop, split, msg_too_long recovery, interposed
                        // block), its card was completed at finalize time —
                        // drop this update instead of resurrecting a stream
                        // with an orphan card.
                        let owning_ts = active_streams
                            .get(&target_ts)
                            .filter(|s| s.open_tasks.iter().any(|(id, _, _)| id == &task_id))
                            .map(|s| s.ts.clone());
                        match owning_ts {
                            Some(stream_ts) => {
                                match slack
                                    .append_stream_task(
                                        &channel, &stream_ts, &task_id, &title, &status, &details,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        if let Some(state) = active_streams.get_mut(&target_ts) {
                                            state.take_open_task(&task_id);
                                        }
                                    }
                                    // Keep the card registered on failure so
                                    // finalize can still complete it.
                                    Err(e) => tracing::error!("append_stream_task failed: {e}"),
                                }
                            }
                            None => {
                                tracing::debug!(
                                    "dropping task update {task_id} ({status}): its stream \
                                     message is gone (card already completed at finalize)"
                                );
                            }
                        }
                    }
                }
                SlackAction::DismissChoiceButtons { choice_id } => {
                    let info = {
                        let rt = infinity_slack_dataflow::runtime::get();
                        rt.choice_messages
                            .lock()
                            .expect("bug: lock poisoned")
                            .remove(choice_id.as_str())
                    };
                    if let Some((channel, msg_ts)) = info {
                        let blocks = serde_json::json!([
                            {
                                "type": "section",
                                "text": {
                                    "type": "mrkdwn",
                                    "text": "⏭️ Choice resolved automatically"
                                }
                            }
                        ]);
                        if let Err(e) = slack
                            .update_message(
                                &channel,
                                &msg_ts,
                                "Choice resolved automatically",
                                &blocks,
                            )
                            .await
                        {
                            tracing::error!("DismissChoiceButtons update failed: {e}");
                        }
                    }
                }
            }
        }

        // ── Drain (clean shutdown, or the dataflow is gone) ──
        //
        // Leave no Slack message in streaming state: flush what was buffered
        // (it is already counted in the delivery cursors), finalize every
        // active stream, and clear thread statuses. Everything still queued
        // in the action channel is deliberately dropped — it was never
        // counted as delivered, so the restarted bot replays it instead.
        let targets: std::collections::HashSet<String> = pending_text
            .keys()
            .chain(active_streams.keys())
            .cloned()
            .collect();
        for target_ts in targets {
            if is_halted(&target_ts) {
                // Slack already finalized this stream via the user's stop;
                // its suppressed remainder stays suppressed.
                pending_text.remove(&target_ts);
                active_streams.remove(&target_ts);
                continue;
            }
            let channel = pending_text
                .get(&target_ts)
                .map(|p| p.channel.clone())
                .or_else(|| active_streams.get(&target_ts).map(|s| s.channel.clone()));
            let Some(channel) = channel else {
                // Unreachable: every entry carries its channel.
                tracing::warn!("bug: no channel for stream {target_ts} at drain");
                continue;
            };
            finalize_stream(
                &slack,
                &channel,
                &target_ts,
                &mut active_streams,
                &mut pending_text,
            )
            .await;
        }
        // Clear lingering "is thinking"/"is running X" indicators; resumed
        // responses re-arm them on their next output.
        let statuses: Vec<String> = last_status.keys().cloned().collect();
        for thread_ts in statuses {
            let channel = {
                let rt = infinity_slack_dataflow::runtime::get();
                rt.channels
                    .lock()
                    .expect("bug: lock poisoned")
                    .get(&thread_ts)
                    .cloned()
            };
            let Some(channel) = channel else {
                continue;
            };
            if let Err(e) = slack.set_thread_status(&channel, &thread_ts, "").await {
                tracing::warn!("clearing thread status at drain failed: {e}");
            }
        }
        tracing::info!("Slack outbound drained");
    });

    // Inbound: Slack WebSocket → dataflow. The URL fetched during startup
    // validation is consumed by the first connection attempt (Socket Mode
    // URLs are short-lived tickets, so refetch on every reconnect after).
    let mut pending_ws_url = Some(initial_ws_url);
    let mut inbound_shutdown = shutdown;
    let inbound = tokio::spawn(async move {
        loop {
            if *inbound_shutdown.borrow() {
                return;
            }
            tracing::info!("connecting to Slack Socket Mode...");
            let url = match pending_ws_url.take() {
                Some(url) => url,
                None => match get_ws_url(&config.app_token).await {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::error!("failed to get Socket Mode URL: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                },
            };

            let (ws_stream, _) = match tokio_tungstenite::connect_async(&url).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("WebSocket connect failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            tracing::info!("Socket Mode connected ✓");

            let (mut ws_tx, mut ws_rx) = ws_stream.split();

            loop {
                let msg = tokio::select! {
                    msg = ws_rx.next() => match msg {
                        Some(m) => m,
                        None => break,
                    },
                    _ = inbound_shutdown.changed() => {
                        // Clean shutdown: close the WebSocket with a proper
                        // close handshake so Slack sees the app go offline
                        // immediately. Envelopes already read were ACKed (and
                        // forwarded); unread ones stay unACKed, so Slack
                        // redelivers them after the restart — a user message
                        // racing the shutdown is delayed, never lost.
                        tracing::info!("shutting down: closing Socket Mode connection");
                        if let Err(e) = ws_tx.send(Message::Close(None)).await {
                            tracing::warn!("Socket Mode close failed: {e}");
                        }
                        // Give the close handshake a moment to complete.
                        let handshake = async {
                            while let Some(reply) = ws_rx.next().await {
                                if matches!(reply, Ok(Message::Close(_)) | Err(_)) {
                                    break;
                                }
                            }
                        };
                        if tokio::time::timeout(std::time::Duration::from_secs(2), handshake)
                            .await
                            .is_err()
                        {
                            tracing::warn!("Socket Mode close handshake timed out");
                        }
                        tracing::info!("Socket Mode connection closed");
                        return;
                    }
                };
                let text = match msg {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) => {
                        tracing::info!("Socket Mode closed, reconnecting...");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!("Socket Mode error: {e}");
                        break;
                    }
                };

                let envelope: SocketEnvelope = match serde_json::from_str(&text) {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                // ACK immediately
                let ack = serde_json::json!({ "envelope_id": envelope.envelope_id });
                if ws_tx
                    .send(Message::Text(ack.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }

                if let Some(mut event) = parse_envelope(envelope) {
                    event.is_unauthorized = !config.is_allowed(&event.user);
                    if event.is_bot {
                        continue;
                    }
                    tracing::info!(
                        user = %event.user,
                        channel = %event.channel,
                        text = %event.text,
                        "received Slack event"
                    );
                    if to_df_tx.send(event).await.is_err() {
                        tracing::warn!("dataflow channel closed, stopping Slack inbound task");
                        return;
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    Ok(SlackIo {
        events: to_df_rx,
        actions: from_df_tx,
        outbound,
        inbound,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn get_ws_url(app_token: &str) -> Result<String, crate::BoxError> {
    let client = reqwest::Client::new();
    let resp: ConnectionResponse = client
        .post("https://slack.com/api/apps.connections.open")
        .bearer_auth(app_token)
        .send()
        .await?
        .json()
        .await?;

    if !resp.ok {
        return Err(format!(
            "apps.connections.open failed: {}",
            resp.error.unwrap_or_default()
        )
        .into());
    }
    resp.url
        .ok_or_else(|| "No URL in apps.connections.open response".into())
}

/// Resolve the governing user turn for a "Rewind to here" invocation over a
/// thread's replies (oldest to newest): the clicked message itself when it
/// is a plain human message, otherwise the nearest human message before it.
/// Returns the turn's 0-based index among the thread's human messages and
/// its text (for the modal prefill).
fn resolve_rewind_turn(
    replies: &[crate::slack_client::ThreadMessage],
    target_ts: &str,
) -> Option<(usize, String)> {
    let target: f64 = target_ts.parse().ok()?;
    let mut governing: Option<(usize, String)> = None;
    let mut human_count = 0usize;
    for msg in replies {
        let Ok(ts) = msg.ts.parse::<f64>() else {
            continue;
        };
        if ts > target {
            break;
        }
        if msg.is_human_message() {
            governing = Some((human_count, msg.text.clone()));
            human_count += 1;
        }
    }
    governing
}

/// Fetch the thread, resolve the rewind point for `target_ts`, and open the
/// edit modal. Returns a user-facing message on failure (reported
/// ephemerally by the caller).
async fn open_rewind_modal(
    slack: &SlackClient,
    channel: &str,
    thread_ts: &str,
    target_ts: &str,
    trigger_id: &str,
) -> Result<(), String> {
    let replies = slack
        .conversations_replies(channel, thread_ts)
        .await
        .map_err(|e| {
            tracing::error!("conversations.replies for fork failed: {e}");
            "⚠️ Could not read the thread to resolve the fork point.".to_owned()
        })?;
    let Some((user_turn, original)) = resolve_rewind_turn(&replies, target_ts) else {
        return Err(
            "⏪ No user message at or before that point — nothing to fork from.".to_owned(),
        );
    };

    let metadata = serde_json::json!({
        "channel": channel,
        "thread_ts": thread_ts,
        "message_ts": target_ts,
        "user_turn": user_turn,
    });
    // Slack caps `plain_text_input.initial_value`; keep a healthy margin.
    let prefill: String = if original.chars().count() > 2900 {
        let head: String = original.chars().take(2900).collect();
        format!("{head}…")
    } else {
        original
    };
    let view = serde_json::json!({
        "type": "modal",
        "callback_id": REWIND_MODAL_CALLBACK_ID,
        "private_metadata": metadata.to_string(),
        "title": { "type": "plain_text", "text": "Fork from here" },
        "submit": { "type": "plain_text", "text": "Fork & send" },
        "close": { "type": "plain_text", "text": "Cancel" },
        "blocks": [
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": "Starts a new conversation in a new thread from just before \
                             this message, continuing with your edited version. The \
                             original conversation is kept as it is."
                }
            },
            {
                "type": "input",
                "block_id": REWIND_BLOCK_ID,
                "label": { "type": "plain_text", "text": "Message" },
                "element": {
                    "type": "plain_text_input",
                    "action_id": REWIND_ACTION_ID,
                    "multiline": true,
                    "initial_value": prefill,
                }
            }
        ],
    });
    slack.open_view(trigger_id, &view).await.map_err(|e| {
        tracing::error!("views.open for fork failed: {e}");
        "⚠️ Could not open the fork dialog.".to_owned()
    })?;
    Ok(())
}

/// Open a forked conversation's own Slack thread: a top-level anchor message
/// in `channel` (thread replies cannot host threads) linking back to the
/// message it forks from, plus a cross-link in the source thread. Returns the
/// anchor's ts — the fork's `thread_ts` — or `None` if it could not be
/// posted (the user is told in the source thread).
async fn post_fork_thread(
    slack: &SlackClient,
    channel: &str,
    source_thread_ts: &str,
    source_message_ts: &str,
    text: &str,
) -> Option<String> {
    let permalink = |ts: String| async move {
        match slack.get_permalink(channel, &ts).await {
            Ok(link) => link,
            Err(e) => {
                tracing::warn!("chat.getPermalink for fork failed: {e}");
                None
            }
        }
    };
    let origin_ts = if source_message_ts.is_empty() {
        source_thread_ts
    } else {
        source_message_ts
    };
    let quoted = infinity_slack_dataflow::flow::quote_snippet(text);
    let anchor_text = match permalink(origin_ts.to_owned()).await {
        Some(url) => {
            format!("⏪ Forked from <{url}|an earlier message>, continuing with:\n{quoted}")
        }
        None => format!("⏪ Forked from an earlier conversation, continuing with:\n{quoted}"),
    };
    let fork_ts = match slack.post_message(channel, &anchor_text, None).await {
        Ok(Some(ts)) => Some(ts),
        Ok(None) => {
            tracing::error!("posting the fork anchor in {channel} returned no ts");
            None
        }
        Err(e) => {
            tracing::error!("posting the fork anchor in {channel} failed: {e}");
            None
        }
    };
    let Some(fork_ts) = fork_ts else {
        if let Err(e) = slack
            .post_message(
                channel,
                "⚠️ Could not start the forked conversation.",
                Some(source_thread_ts),
            )
            .await
        {
            tracing::error!("reporting the failed fork failed: {e}");
        }
        return None;
    };
    let link_text = match permalink(fork_ts.clone()).await {
        Some(url) => {
            format!("⏪ Forked into <{url}|a new thread>. This conversation is unchanged.")
        }
        None => "⏪ Forked into a new thread. This conversation is unchanged.".to_owned(),
    };
    if let Err(e) = slack
        .post_message(channel, &link_text, Some(source_thread_ts))
        .await
    {
        tracing::warn!("posting the fork cross-link failed: {e}");
    }
    Some(fork_ts)
}

/// The event the executor feeds back once a fork's Slack thread exists: the
/// dataflow starts the forked session keyed on `fork_ts`.
fn fork_ready_event(
    channel: &str,
    fork_ts: &str,
    source_thread_ts: &str,
    user_turn: usize,
    text: &str,
) -> SlackEvent {
    SlackEvent {
        user: String::new(),
        text: text.to_owned(),
        channel: channel.to_owned(),
        thread_ts: fork_ts.to_owned(),
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
        rewind_user_turn: Some(user_turn),
        fork_source_thread_ts: Some(source_thread_ts.to_owned()),
        thread_model_selection: None,
    }
}

fn parse_envelope(envelope: SocketEnvelope) -> Option<SlackEvent> {
    match envelope.envelope_type.as_str() {
        "interactive" => parse_interactive(envelope.payload),
        "events_api" => parse_events_api(envelope.payload),
        "slash_commands" => parse_slash_command(envelope.payload),
        _ => {
            tracing::debug!("ignoring envelope type: {}", envelope.envelope_type);
            None
        }
    }
}

fn parse_slash_command(payload: serde_json::Value) -> Option<SlackEvent> {
    let p: SlashCommandPayload = serde_json::from_value(payload).ok()?;

    Some(SlackEvent {
        user: p.user_id,
        text: p.text.trim().to_owned(),
        channel: p.channel_id,
        // Slash commands are not tied to a thread.
        thread_ts: String::new(),
        is_button_click: false,
        button_value: None,
        action_id: None,
        message_ts: None,
        button_text: None,
        is_bot: false,
        is_unauthorized: false, // set later by caller
        slash_command: Some(p.command),
        response_url: Some(p.response_url),
        trigger_id: p.trigger_id,
        is_app_home_opened: false,
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: None,
        rewind_user_turn: None,
        fork_source_thread_ts: None,
        thread_model_selection: None,
    })
}

fn parse_interactive(payload: serde_json::Value) -> Option<SlackEvent> {
    let p: InteractivePayload = serde_json::from_value(payload).ok()?;

    if p.payload_type == "view_submission" {
        return parse_view_submission(p);
    }
    if p.payload_type == "message_action" {
        return parse_message_action(p);
    }

    let action = p.actions.first()?;
    let chan = p.channel.as_ref()?;
    let user = p.user.as_ref()?;

    let thread_ts = p
        .message
        .as_ref()
        .and_then(|m| m.thread_ts.as_ref().or(m.ts.as_ref()))
        .cloned()
        .unwrap_or_default();

    let message_ts = p.message.as_ref().and_then(|m| m.ts.clone());
    let button_text = action.text.as_ref().map(|t| t.text.clone());

    Some(SlackEvent {
        user: user.id.clone(),
        text: String::new(),
        channel: chan.id.clone(),
        thread_ts,
        is_button_click: true,
        button_value: Some(action.value.clone()),
        action_id: Some(action.action_id.clone()),
        message_ts,
        button_text,
        is_bot: false,
        is_unauthorized: false, // set later by caller
        slash_command: None,
        response_url: None,
        trigger_id: None,
        is_app_home_opened: false,
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: None,
        rewind_user_turn: None,
        fork_source_thread_ts: None,
        thread_model_selection: None,
    })
}

/// Parse a `message_action` payload (a message shortcut from the context ⋮
/// menu, e.g. "Rewind to here"). `message_ts` carries the target message's
/// ts and `thread_ts` the thread it belongs to (the message itself when it
/// is a top-level/thread-parent message).
fn parse_message_action(p: InteractivePayload) -> Option<SlackEvent> {
    let callback_id = p.callback_id?;
    let user = p.user?;
    let chan = p.channel?;
    let msg = p.message?;
    let target_ts = msg.ts?;
    let thread_ts = msg.thread_ts.unwrap_or_else(|| target_ts.clone());

    Some(SlackEvent {
        user: user.id,
        text: String::new(),
        channel: chan.id,
        thread_ts,
        is_button_click: false,
        button_value: None,
        action_id: None,
        message_ts: Some(target_ts),
        button_text: None,
        is_bot: false,
        is_unauthorized: false, // set later by caller
        slash_command: None,
        response_url: p.response_url,
        trigger_id: p.trigger_id,
        is_app_home_opened: false,
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: Some(callback_id),
        rewind_user_turn: None,
        fork_source_thread_ts: None,
        thread_model_selection: None,
    })
}

/// Parse a modal `view_submission`: the model-picker modal (normalized into
/// a `/model <selection>` slash-command event so it flows through the
/// existing command handling) or the rewind modal (normalized into a
/// rewind-submission event carrying the edited text and the user turn
/// resolved at modal-open time).
fn parse_view_submission(p: InteractivePayload) -> Option<SlackEvent> {
    let view = p.view?;
    match view.callback_id.as_str() {
        MODEL_PICKER_CALLBACK_ID => parse_model_picker_submission(view, p.user?),
        REWIND_MODAL_CALLBACK_ID => parse_rewind_submission(view, p.user?),
        THREAD_MODEL_MODAL_CALLBACK_ID => parse_thread_model_submission(view, p.user?),
        other => {
            tracing::debug!("ignoring view_submission with callback_id: {other}");
            None
        }
    }
}

fn parse_model_picker_submission(
    view: InteractiveView,
    user: InteractiveUser,
) -> Option<SlackEvent> {
    let selected = view
        .state?
        .values
        .get(MODEL_PICKER_BLOCK_ID)?
        .get(MODEL_PICKER_ACTION_ID)?
        .get("selected_option")?
        .get("value")?
        .as_str()?
        .to_owned();

    Some(SlackEvent {
        user: user.id,
        text: selected,
        // Modal submissions are not tied to a channel or thread.
        channel: String::new(),
        thread_ts: String::new(),
        is_button_click: false,
        button_value: None,
        action_id: None,
        message_ts: None,
        button_text: None,
        is_bot: false,
        is_unauthorized: false, // set later by caller
        slash_command: Some("/model".to_owned()),
        response_url: (!view.private_metadata.is_empty()).then_some(view.private_metadata),
        trigger_id: None,
        is_app_home_opened: false,
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: None,
        rewind_user_turn: None,
        fork_source_thread_ts: None,
        thread_model_selection: None,
    })
}

/// The context the rewind modal round-trips through its `private_metadata`:
/// where the rewind applies and which user turn it discards.
#[derive(Debug, serde::Serialize, Deserialize)]
struct RewindMetadata {
    channel: String,
    thread_ts: String,
    /// The clicked message (for the fork's back-link). Absent in modals
    /// opened by older builds.
    #[serde(default)]
    message_ts: Option<String>,
    user_turn: usize,
}

fn parse_rewind_submission(view: InteractiveView, user: InteractiveUser) -> Option<SlackEvent> {
    let meta: RewindMetadata = match serde_json::from_str(&view.private_metadata) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::error!("bug: unparsable rewind modal metadata: {e}");
            return None;
        }
    };
    let text = view
        .state?
        .values
        .get(REWIND_BLOCK_ID)?
        .get(REWIND_ACTION_ID)?
        .get("value")?
        .as_str()?
        .to_owned();

    Some(SlackEvent {
        user: user.id,
        text,
        channel: meta.channel,
        thread_ts: meta.thread_ts,
        is_button_click: false,
        button_value: None,
        action_id: None,
        message_ts: meta.message_ts,
        button_text: None,
        is_bot: false,
        is_unauthorized: false, // set later by caller
        slash_command: None,
        response_url: None,
        trigger_id: None,
        is_app_home_opened: false,
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: None,
        rewind_user_turn: Some(meta.user_turn),
        fork_source_thread_ts: None,
        thread_model_selection: None,
    })
}

/// The context the per-thread model picker round-trips through its
/// `private_metadata`: the Slack thread (root conversation thread or a
/// subagent's dedicated thread) whose session thread the switch targets.
#[derive(Debug, serde::Serialize, Deserialize)]
struct ThreadModelMetadata {
    channel: String,
    thread_ts: String,
}

fn parse_thread_model_submission(
    view: InteractiveView,
    user: InteractiveUser,
) -> Option<SlackEvent> {
    let meta: ThreadModelMetadata = match serde_json::from_str(&view.private_metadata) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::error!("bug: unparsable thread-model modal metadata: {e}");
            return None;
        }
    };
    let selected = view
        .state?
        .values
        .get(MODEL_PICKER_BLOCK_ID)?
        .get(MODEL_PICKER_ACTION_ID)?
        .get("selected_option")?
        .get("value")?
        .as_str()?
        .to_owned();

    Some(SlackEvent {
        user: user.id,
        text: String::new(),
        channel: meta.channel,
        thread_ts: meta.thread_ts,
        is_button_click: false,
        button_value: None,
        action_id: None,
        message_ts: None,
        button_text: None,
        is_bot: false,
        is_unauthorized: false, // set later by caller
        slash_command: None,
        response_url: None,
        trigger_id: None,
        is_app_home_opened: false,
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: None,
        rewind_user_turn: None,
        fork_source_thread_ts: None,
        thread_model_selection: Some(selected),
    })
}

fn parse_events_api(payload: serde_json::Value) -> Option<SlackEvent> {
    let p: EventPayload = serde_json::from_value(payload).ok()?;
    let event = p.event?;

    // A user opened the app's Messages tab — signal for onboarding
    // (suggested prompts), not a message.
    if event.event_type == "app_home_opened" {
        if event.tab.as_deref() != Some("messages") {
            return None;
        }
        return Some(SlackEvent {
            user: event.user.unwrap_or_default(),
            text: String::new(),
            channel: event.channel?,
            thread_ts: String::new(),
            is_button_click: false,
            button_value: None,
            action_id: None,
            message_ts: None,
            button_text: None,
            is_bot: false,
            is_unauthorized: false, // set later by caller
            slash_command: None,
            response_url: None,
            trigger_id: None,
            is_app_home_opened: true,
            is_session_stopped: false,
            streaming_message_ts: Vec::new(),
            shortcut: None,
            rewind_user_turn: None,
            fork_source_thread_ts: None,
            thread_model_selection: None,
        });
    }

    // The user clicked Slack's native stop button on a streaming response.
    // Payload: { type: "agent_session_stopped", channel, thread_ts, user,
    // streaming_message_ts: [..] } — `streaming_message_ts` lists the
    // in-progress streaming messages Slack already stopped client-side.
    if event.event_type == "agent_session_stopped" {
        return Some(SlackEvent {
            user: event.user.unwrap_or_default(),
            text: String::new(),
            channel: event.channel?,
            thread_ts: event.thread_ts.or(event.ts).unwrap_or_default(),
            is_button_click: false,
            button_value: None,
            action_id: None,
            message_ts: None,
            button_text: None,
            is_bot: false,
            is_unauthorized: false, // set later by caller
            slash_command: None,
            response_url: None,
            trigger_id: None,
            is_app_home_opened: false,
            is_session_stopped: true,
            streaming_message_ts: event.streaming_message_ts.unwrap_or_default(),
            shortcut: None,
            rewind_user_turn: None,
            fork_source_thread_ts: None,
            thread_model_selection: None,
        });
    }

    // Skip subtypes (message_changed, etc.)
    if event.subtype.is_some() {
        return None;
    }
    if event.event_type != "message" {
        return None;
    }

    let user = event.user.unwrap_or_default();
    let text = event.text?;
    let channel = event.channel?;
    let thread_ts = event.thread_ts.or(event.ts).unwrap_or_default();
    let is_bot = event.bot_id.is_some();

    Some(SlackEvent {
        user,
        text,
        channel,
        thread_ts,
        is_button_click: false,
        button_value: None,
        action_id: None,
        message_ts: None,
        button_text: None,
        is_bot,
        is_unauthorized: false, // set later by caller
        slash_command: None,
        response_url: None,
        trigger_id: None,
        is_app_home_opened: false,
        is_session_stopped: false,
        streaming_message_ts: Vec::new(),
        shortcut: None,
        rewind_user_turn: None,
        fork_source_thread_ts: None,
        thread_model_selection: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_events_api_agent_session_stopped() {
        let payload = serde_json::json!({
            "event": {
                "type": "agent_session_stopped",
                "user": "U123",
                "channel": "D456",
                "thread_ts": "1111.0000",
                "event_ts": "2222.0000",
                "streaming_message_ts": ["1234.5678", "1234.9999"]
            }
        });
        let event = parse_events_api(payload).expect("should parse");
        assert!(event.is_session_stopped);
        assert_eq!(event.user, "U123");
        assert_eq!(event.channel, "D456");
        assert_eq!(event.thread_ts, "1111.0000");
        assert_eq!(event.streaming_message_ts, vec!["1234.5678", "1234.9999"]);
        assert!(!event.is_button_click);
        assert!(!event.is_app_home_opened);
    }

    #[test]
    fn parse_events_api_agent_session_stopped_without_message_list() {
        // `streaming_message_ts` may be absent/empty — the stop still applies.
        let payload = serde_json::json!({
            "event": {
                "type": "agent_session_stopped",
                "user": "U123",
                "channel": "D456",
                "thread_ts": "1111.0000"
            }
        });
        let event = parse_events_api(payload).expect("should parse");
        assert!(event.is_session_stopped);
        assert!(event.streaming_message_ts.is_empty());
    }

    #[test]
    fn parse_events_api_normal_message() {
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U123",
                "text": "hello",
                "channel": "C456",
                "ts": "1234.5678"
            }
        });
        let event = parse_events_api(payload).expect("should parse");
        assert_eq!(event.user, "U123");
        assert_eq!(event.text, "hello");
        assert_eq!(event.channel, "C456");
        assert_eq!(event.thread_ts, "1234.5678");
        assert!(!event.is_bot);
        assert!(!event.is_button_click);
    }

    #[test]
    fn parse_events_api_bot_message_sets_flag() {
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U123",
                "text": "bot msg",
                "channel": "C456",
                "ts": "1234.5678",
                "bot_id": "B789"
            }
        });
        let event = parse_events_api(payload).expect("should parse");
        assert!(event.is_bot);
    }

    #[test]
    fn parse_events_api_skips_subtypes() {
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "subtype": "message_changed",
                "user": "U123",
                "text": "edited",
                "channel": "C456",
                "ts": "1234.5678"
            }
        });
        assert!(parse_events_api(payload).is_none());
    }

    #[test]
    fn parse_events_api_skips_non_message() {
        let payload = serde_json::json!({
            "event": {
                "type": "reaction_added",
                "user": "U123",
                "text": "hi",
                "channel": "C456",
                "ts": "1234.5678"
            }
        });
        assert!(parse_events_api(payload).is_none());
    }

    #[test]
    fn parse_events_api_uses_thread_ts_over_ts() {
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "user": "U123",
                "text": "reply",
                "channel": "C456",
                "ts": "1111.0000",
                "thread_ts": "9999.0000"
            }
        });
        let event = parse_events_api(payload).expect("should parse");
        assert_eq!(event.thread_ts, "9999.0000");
    }

    #[test]
    fn parse_interactive_button_click() {
        let payload = serde_json::json!({
            "actions": [{"action_id": "choice_abc_1", "value": "1", "text": {"type": "plain_text", "text": "Allow"}}],
            "channel": {"id": "C456"},
            "message": {"ts": "1234.5678", "thread_ts": "1111.0000"},
            "user": {"id": "U123"}
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(event.user, "U123");
        assert_eq!(event.channel, "C456");
        assert_eq!(event.thread_ts, "1111.0000");
        assert!(event.is_button_click);
        assert_eq!(event.button_value.as_deref(), Some("1"));
        assert_eq!(event.action_id.as_deref(), Some("choice_abc_1"));
        assert_eq!(event.message_ts.as_deref(), Some("1234.5678"));
        assert_eq!(event.button_text.as_deref(), Some("Allow"));
    }

    #[test]
    fn parse_interactive_missing_channel_returns_none() {
        let payload = serde_json::json!({
            "actions": [{"value": "0"}],
            "user": {"id": "U123"}
        });
        assert!(parse_interactive(payload).is_none());
    }

    #[test]
    fn parse_envelope_unknown_type_returns_none() {
        let envelope = SocketEnvelope {
            envelope_id: "e1".into(),
            envelope_type: "app_rate_limited".into(),
            payload: serde_json::json!({}),
        };
        assert!(parse_envelope(envelope).is_none());
    }

    #[test]
    fn parse_envelope_routes_slash_commands() {
        let envelope = SocketEnvelope {
            envelope_id: "e3".into(),
            envelope_type: "slash_commands".into(),
            payload: serde_json::json!({
                "command": "/model",
                "text": "  bedrock/claude-sonnet-4  ",
                "user_id": "U123",
                "channel_id": "C456",
                "response_url": "https://hooks.slack.com/commands/T1/123/abc"
            }),
        };
        let event = parse_envelope(envelope).expect("should parse");
        assert_eq!(event.slash_command.as_deref(), Some("/model"));
        assert_eq!(event.text, "bedrock/claude-sonnet-4");
        assert_eq!(event.user, "U123");
        assert_eq!(event.channel, "C456");
        assert_eq!(
            event.response_url.as_deref(),
            Some("https://hooks.slack.com/commands/T1/123/abc")
        );
        assert!(!event.is_button_click);
        assert!(!event.is_bot);
    }

    #[test]
    fn parse_slash_command_empty_text_defaults() {
        let payload = serde_json::json!({
            "command": "/model",
            "user_id": "U123",
            "channel_id": "C456",
            "response_url": "https://hooks.slack.com/commands/T1/123/abc"
        });
        let event = parse_slash_command(payload).expect("should parse");
        assert_eq!(event.text, "");
        assert_eq!(event.slash_command.as_deref(), Some("/model"));
    }

    #[test]
    fn parse_slash_command_missing_response_url_returns_none() {
        let payload = serde_json::json!({
            "command": "/model",
            "user_id": "U123",
            "channel_id": "C456"
        });
        assert!(parse_slash_command(payload).is_none());
    }

    #[test]
    fn parse_slash_command_captures_trigger_id() {
        let payload = serde_json::json!({
            "command": "/model",
            "user_id": "U123",
            "channel_id": "C456",
            "response_url": "https://hooks.slack.com/commands/T1/123/abc",
            "trigger_id": "12345.98765.abcd"
        });
        let event = parse_slash_command(payload).expect("should parse");
        assert_eq!(event.trigger_id.as_deref(), Some("12345.98765.abcd"));
    }

    #[test]
    fn parse_view_submission_model_picker_yields_model_command() {
        let payload = serde_json::json!({
            "type": "view_submission",
            "user": {"id": "U123"},
            "view": {
                "callback_id": MODEL_PICKER_CALLBACK_ID,
                "private_metadata": "https://hooks.slack.com/commands/T1/123/abc",
                "state": {
                    "values": {
                        MODEL_PICKER_BLOCK_ID: {
                            MODEL_PICKER_ACTION_ID: {
                                "type": "radio_buttons",
                                "selected_option": {
                                    "text": {"type": "plain_text", "text": "Sonnet"},
                                    "value": "bedrock/claude-sonnet-4"
                                }
                            }
                        }
                    }
                }
            }
        });
        let event = parse_interactive(payload).expect("should parse");
        assert!(!event.is_button_click);
        assert_eq!(event.slash_command.as_deref(), Some("/model"));
        assert_eq!(event.text, "bedrock/claude-sonnet-4");
        assert_eq!(event.user, "U123");
        assert_eq!(
            event.response_url.as_deref(),
            Some("https://hooks.slack.com/commands/T1/123/abc")
        );
    }

    #[test]
    fn parse_view_submission_unknown_callback_returns_none() {
        let payload = serde_json::json!({
            "type": "view_submission",
            "user": {"id": "U123"},
            "view": {
                "callback_id": "some_other_modal",
                "state": {"values": {}}
            }
        });
        assert!(parse_interactive(payload).is_none());
    }

    #[test]
    fn parse_message_action_rewind_shortcut() {
        let payload = serde_json::json!({
            "type": "message_action",
            "callback_id": "rewind_to_here",
            "trigger_id": "12345.98765.abcd",
            "response_url": "https://hooks.slack.com/app/T1/1/x",
            "user": {"id": "U123"},
            "channel": {"id": "D456"},
            "message": {"ts": "1111.2222", "thread_ts": "1000.0000", "text": "fix the bug"}
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(event.shortcut.as_deref(), Some("rewind_to_here"));
        assert_eq!(event.user, "U123");
        assert_eq!(event.channel, "D456");
        assert_eq!(event.thread_ts, "1000.0000");
        assert_eq!(event.message_ts.as_deref(), Some("1111.2222"));
        assert_eq!(event.trigger_id.as_deref(), Some("12345.98765.abcd"));
        assert_eq!(
            event.response_url.as_deref(),
            Some("https://hooks.slack.com/app/T1/1/x")
        );
        assert!(!event.is_button_click);
        assert!(event.rewind_user_turn.is_none());
    }

    #[test]
    fn parse_message_action_without_thread_uses_message_ts() {
        // A shortcut on a top-level message (the thread parent): no
        // `thread_ts` field — the message is its own thread.
        let payload = serde_json::json!({
            "type": "message_action",
            "callback_id": "rewind_to_here",
            "trigger_id": "t",
            "response_url": "https://hooks.slack.com/app/T1/1/x",
            "user": {"id": "U123"},
            "channel": {"id": "D456"},
            "message": {"ts": "1111.2222", "text": "hello"}
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(event.thread_ts, "1111.2222");
        assert_eq!(event.message_ts.as_deref(), Some("1111.2222"));
    }

    #[test]
    fn parse_message_action_compact_shortcut() {
        let payload = serde_json::json!({
            "type": "message_action",
            "callback_id": "compact_session",
            "trigger_id": "77.88.compact",
            "response_url": "https://hooks.slack.com/app/T1/2/c",
            "user": {"id": "U123"},
            "channel": {"id": "D456"},
            "message": {"ts": "2222.3333", "thread_ts": "2000.0000", "text": "anything"}
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(event.shortcut.as_deref(), Some("compact_session"));
        assert_eq!(event.thread_ts, "2000.0000");
        assert_eq!(
            event.response_url.as_deref(),
            Some("https://hooks.slack.com/app/T1/2/c")
        );
        assert!(!event.is_button_click);
    }

    #[test]
    fn parse_view_submission_rewind_modal() {
        let payload = serde_json::json!({
            "type": "view_submission",
            "user": {"id": "U123"},
            "view": {
                "callback_id": REWIND_MODAL_CALLBACK_ID,
                "private_metadata": "{\"channel\":\"D456\",\"thread_ts\":\"1000.0000\",\"user_turn\":2}",
                "state": {
                    "values": {
                        REWIND_BLOCK_ID: {
                            REWIND_ACTION_ID: {
                                "type": "plain_text_input",
                                "value": "the edited message"
                            }
                        }
                    }
                }
            }
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(event.rewind_user_turn, Some(2));
        assert_eq!(event.text, "the edited message");
        assert_eq!(event.channel, "D456");
        assert_eq!(event.thread_ts, "1000.0000");
        // Metadata from an older build has no message_ts; still parses.
        assert!(event.message_ts.is_none());
        assert!(event.fork_source_thread_ts.is_none());
        assert!(event.shortcut.is_none());
        assert!(event.slash_command.is_none());
    }

    #[test]
    fn parse_view_submission_fork_modal_round_trips_message_ts() {
        let payload = serde_json::json!({
            "type": "view_submission",
            "user": {"id": "U123"},
            "view": {
                "callback_id": REWIND_MODAL_CALLBACK_ID,
                "private_metadata": "{\"channel\":\"D456\",\"thread_ts\":\"1000.0000\",\"message_ts\":\"1000.0042\",\"user_turn\":1}",
                "state": {
                    "values": {
                        REWIND_BLOCK_ID: {
                            REWIND_ACTION_ID: {
                                "type": "plain_text_input",
                                "value": "edited"
                            }
                        }
                    }
                }
            }
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(event.message_ts.as_deref(), Some("1000.0042"));
        assert_eq!(event.rewind_user_turn, Some(1));
    }

    #[test]
    fn fork_ready_event_targets_the_new_thread() {
        let event = fork_ready_event("C1", "2000.0001", "1000.0000", 3, "edited");
        assert_eq!(event.thread_ts, "2000.0001");
        assert_eq!(event.fork_source_thread_ts.as_deref(), Some("1000.0000"));
        assert_eq!(event.rewind_user_turn, Some(3));
        assert_eq!(event.text, "edited");
        assert!(
            !event.is_bot && !event.is_unauthorized,
            "must pass the dataflow filter"
        );
    }

    #[test]
    fn parse_message_action_switch_model_shortcut() {
        let payload = serde_json::json!({
            "type": "message_action",
            "callback_id": "switch_model",
            "trigger_id": "12345.98765.abcd",
            "response_url": "https://hooks.slack.com/app/T1/1/x",
            "user": {"id": "U123"},
            "channel": {"id": "D456"},
            "message": {"ts": "1111.2222", "thread_ts": "1000.0000", "text": "whatever"}
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(event.shortcut.as_deref(), Some("switch_model"));
        assert_eq!(event.thread_ts, "1000.0000");
        assert_eq!(event.trigger_id.as_deref(), Some("12345.98765.abcd"));
        assert!(event.thread_model_selection.is_none());
    }

    #[test]
    fn parse_view_submission_thread_model_modal() {
        let payload = serde_json::json!({
            "type": "view_submission",
            "user": {"id": "U123"},
            "view": {
                "callback_id": THREAD_MODEL_MODAL_CALLBACK_ID,
                "private_metadata": "{\"channel\":\"D456\",\"thread_ts\":\"1000.0000\"}",
                "state": {
                    "values": {
                        MODEL_PICKER_BLOCK_ID: {
                            MODEL_PICKER_ACTION_ID: {
                                "type": "static_select",
                                "selected_option": {
                                    "text": {"type": "plain_text", "text": "Opus"},
                                    "value": "bedrock/global.anthropic.claude-opus-5-5"
                                }
                            }
                        }
                    }
                }
            }
        });
        let event = parse_interactive(payload).expect("should parse");
        assert_eq!(
            event.thread_model_selection.as_deref(),
            Some("bedrock/global.anthropic.claude-opus-5-5")
        );
        assert_eq!(event.channel, "D456");
        assert_eq!(event.thread_ts, "1000.0000");
        assert!(event.shortcut.is_none());
        assert!(event.slash_command.is_none());
        assert!(event.rewind_user_turn.is_none());
    }

    #[test]
    fn resolve_rewind_turn_targets_human_and_bot_messages() {
        use crate::slack_client::ThreadMessage;
        let human = |ts: &str, text: &str| ThreadMessage {
            ts: ts.to_owned(),
            text: text.to_owned(),
            user: Some("U1".to_owned()),
            bot_id: None,
            subtype: None,
        };
        let bot = |ts: &str, text: &str| ThreadMessage {
            ts: ts.to_owned(),
            text: text.to_owned(),
            user: None,
            bot_id: Some("B1".to_owned()),
            subtype: None,
        };
        let replies = vec![
            human("1.0", "first"),
            bot("2.0", "answer one"),
            human("3.0", "second"),
            bot("4.0", "answer two"),
        ];

        // On a human message: that message is the turn.
        assert_eq!(
            resolve_rewind_turn(&replies, "3.0"),
            Some((1, "second".to_owned()))
        );
        assert_eq!(
            resolve_rewind_turn(&replies, "1.0"),
            Some((0, "first".to_owned()))
        );
        // On a bot message: the nearest human message before it.
        assert_eq!(
            resolve_rewind_turn(&replies, "4.0"),
            Some((1, "second".to_owned()))
        );
        assert_eq!(
            resolve_rewind_turn(&replies, "2.0"),
            Some((0, "first".to_owned()))
        );
        // Before any human message: nothing to rewind to.
        let bot_first = vec![bot("1.0", "hi"), human("2.0", "hello")];
        assert_eq!(resolve_rewind_turn(&bot_first, "1.0"), None);
    }

    #[test]
    fn parse_events_api_app_home_opened_messages_tab() {
        let payload = serde_json::json!({
            "event": {
                "type": "app_home_opened",
                "user": "U123",
                "channel": "D456",
                "tab": "messages"
            }
        });
        let event = parse_events_api(payload).expect("should parse");
        assert!(event.is_app_home_opened);
        assert_eq!(event.user, "U123");
        assert_eq!(event.channel, "D456");
        assert!(event.text.is_empty());
    }

    #[test]
    fn parse_events_api_app_home_opened_other_tab_skipped() {
        let payload = serde_json::json!({
            "event": {
                "type": "app_home_opened",
                "user": "U123",
                "channel": "D456",
                "tab": "home"
            }
        });
        assert!(parse_events_api(payload).is_none());
    }

    #[test]
    fn parse_envelope_routes_events_api() {
        let envelope = SocketEnvelope {
            envelope_id: "e1".into(),
            envelope_type: "events_api".into(),
            payload: serde_json::json!({
                "event": {
                    "type": "message",
                    "user": "U1",
                    "text": "hi",
                    "channel": "C1",
                    "ts": "1.0"
                }
            }),
        };
        let event = parse_envelope(envelope).expect("should parse");
        assert_eq!(event.text, "hi");
        assert!(!event.is_button_click);
    }

    #[test]
    fn parse_envelope_routes_interactive() {
        let envelope = SocketEnvelope {
            envelope_id: "e2".into(),
            envelope_type: "interactive".into(),
            payload: serde_json::json!({
                "actions": [{"value": "2"}],
                "channel": {"id": "C1"},
                "message": {"ts": "1.0"},
                "user": {"id": "U1"}
            }),
        };
        let event = parse_envelope(envelope).expect("should parse");
        assert!(event.is_button_click);
        assert_eq!(event.button_value.as_deref(), Some("2"));
    }
}
