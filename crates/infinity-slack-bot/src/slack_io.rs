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
    MODEL_PICKER_CALLBACK_ID,
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
}

#[derive(Deserialize)]
struct InteractivePayload {
    /// `block_actions` for button clicks, `view_submission` for modal submits.
    #[serde(rename = "type", default)]
    payload_type: String,
    #[serde(default)]
    actions: Vec<InteractiveAction>,
    channel: Option<InteractiveChannel>,
    message: Option<InteractiveMessage>,
    user: Option<InteractiveUser>,
    /// Present on `view_submission` payloads.
    view: Option<InteractiveView>,
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
pub async fn spawn(config: &'static Config) -> Result<SlackIo, crate::BoxError> {
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

    // Outbound: execute SlackActions against the Slack API.
    let outbound = tokio::spawn(async move {
        /// Per-thread stream state.
        struct StreamState {
            ts: String,
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
        /// by (root thread_ts, subagent id).
        type AnchorMap = std::collections::HashMap<(String, String), String>;
        let mut anchors: AnchorMap = AnchorMap::new();

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
                _ = flush_tick.tick() => {
                    // Flush all buffered text (one append per thread).
                    let ready: Vec<(String, PendingText)> = pending_text.drain().collect();
                    for (thread_ts, pending) in ready {
                        if pending.text.is_empty() {
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
                } => {
                    // Look up the target thread without creating one: if the
                    // subagent never produced output, there is no stream to
                    // stop and no dedicated thread should be forked for it.
                    let target_ts = match &subagent {
                        Some(sub) => anchors.get(&(thread_ts.clone(), sub.clone())).cloned(),
                        None => Some(thread_ts.clone()),
                    };
                    if let Some(target_ts) = target_ts {
                        finalize_stream(
                            &slack,
                            &channel,
                            &target_ts,
                            &mut active_streams,
                            &mut pending_text,
                        )
                        .await;
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
                SlackAction::SetStatus {
                    channel,
                    thread_ts,
                    status,
                } => {
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
    });

    // Inbound: Slack WebSocket → dataflow. The URL fetched during startup
    // validation is consumed by the first connection attempt (Socket Mode
    // URLs are short-lived tickets, so refetch on every reconnect after).
    let mut pending_ws_url = Some(initial_ws_url);
    let inbound = tokio::spawn(async move {
        loop {
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

            while let Some(msg) = ws_rx.next().await {
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
    })
}

fn parse_interactive(payload: serde_json::Value) -> Option<SlackEvent> {
    let p: InteractivePayload = serde_json::from_value(payload).ok()?;

    if p.payload_type == "view_submission" {
        return parse_view_submission(p);
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
    })
}

/// Parse a modal `view_submission`. Currently only the model-picker modal is
/// supported: the submission is normalized into a `/model <selection>` slash
/// command event so it flows through the existing command handling.
fn parse_view_submission(p: InteractivePayload) -> Option<SlackEvent> {
    let view = p.view?;
    if view.callback_id != MODEL_PICKER_CALLBACK_ID {
        tracing::debug!(
            "ignoring view_submission with callback_id: {}",
            view.callback_id
        );
        return None;
    }
    let user = p.user?;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
