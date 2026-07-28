use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::PollSender;

use crate::config::Config;
use crate::slack_client::SlackClient;

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
    /// True if the user opened the app's Messages tab (`app_home_opened`
    /// event with `tab == "messages"`). Used for onboarding, not messaging.
    #[serde(default)]
    pub is_app_home_opened: bool,
}

/// An action the dataflow instructs the sidecar to perform against the Slack API.
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
        /// If set, the sidecar will store the resulting message_ts under this
        /// choice_id so that a later `DismissChoiceButtons` can update it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice_id: Option<String>,
    },
    /// Dismiss interactive buttons for a completed choice (replace with a
    /// "resolved" indicator). The sidecar looks up the stored message_ts
    /// from the choice_id.
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
    StreamTaskUpdate {
        channel: String,
        thread_ts: String,
        task_id: String,
        title: String,
        status: String,
    },
}

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
    #[serde(default)]
    actions: Vec<InteractiveAction>,
    channel: Option<InteractiveChannel>,
    message: Option<InteractiveMessage>,
    user: Option<InteractiveUser>,
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

/// Payload of a `slash_commands` envelope (Socket Mode).
#[derive(Deserialize)]
struct SlashCommandPayload {
    command: String,
    #[serde(default)]
    text: String,
    user_id: String,
    channel_id: String,
    response_url: String,
}

// ── Sidecar constructor ─────────────────────────────────────────────────────

/// Creates the Slack WebSocket sidecar for use with Hydro's `sidecar_bidi`.
///
/// This runs inside the deployed Hydro process. It bootstraps all runtime
/// state (config, Slack client, session store) from scratch, then bridges
/// the Slack WebSocket into the dataflow.
///
/// Returns `(inbound_stream, outbound_sink)` where:
/// - `inbound_stream` emits parsed `SlackEvent`s into the dataflow
/// - `outbound_sink` is unused (event handling happens in the dataflow via `for_each`)
pub fn create() -> (ReceiverStream<SlackEvent>, PollSender<SlackAction>) {
    // Initialize tracing in the deployed child process (must use stderr —
    // stdout is reserved for the Hydro deploy protocol).
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let log_path = std::env::var("SLACK_BOT_LOG").unwrap_or_else(|_| {
        infinity_protocol::state_dir()
            .join("slack.log")
            .to_string_lossy()
            .into_owned()
    });
    let file_layer = {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap_or_else(|e| panic!("failed to open log file {log_path}: {e}"));
        tracing_subscriber::fmt::layer()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
    };

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();

    // Bootstrap config and session store for the runtime.
    let config: &'static Config =
        Box::leak(Box::new(Config::load().expect("failed to load slack.json")));

    let store_path = infinity_protocol::state_dir().join("slack_sessions.json");
    let store =
        crate::session_store::SessionStore::load(store_path).expect("failed to load session store");
    let sessions = std::sync::Arc::new(std::sync::Mutex::new(store));
    crate::runtime::init(config, sessions);

    let (to_df_tx, to_df_rx) = mpsc::channel::<SlackEvent>(1024);
    let (from_df_tx, mut from_df_rx) = mpsc::channel::<SlackAction>(1024);

    // Outbound: execute SlackActions against the Slack API.
    tokio::spawn(async move {
        let slack = SlackClient::new(&config.bot_token)
            .await
            .expect("failed to authenticate with Slack");
        tracing::info!("Slack bot authenticated");

        /// Per-thread stream state.
        struct StreamState {
            ts: String,
            char_count: usize,
            in_code_block: bool,
            /// When this stream message was started. Streams older than
            /// `MAX_STREAM_AGE` are split at the next clean breaking point so
            /// long-running responses don't pile into one giant message.
            started_at: std::time::Instant,
        }

        /// Maximum age of a stream message before it is split at the next
        /// clean breaking point.
        const MAX_STREAM_AGE: std::time::Duration = std::time::Duration::from_secs(3 * 60);

        // Active streams: thread_ts → state
        let mut active_streams: std::collections::HashMap<String, StreamState> =
            std::collections::HashMap::new();

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

        while let Some(action) = from_df_rx.recv().await {
            match action {
                SlackAction::PostMessage {
                    channel,
                    text,
                    thread_ts,
                } => {
                    if let Err(e) = slack
                        .post_message(&channel, &text, thread_ts.as_deref())
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
                } => {
                    match slack
                        .post_blocks(&channel, &fallback_text, &blocks, thread_ts.as_deref())
                        .await
                    {
                        Ok(Some(msg_ts)) => {
                            // If this is a choice-button message, store the ts so we
                            // can dismiss it later when the choice is resolved without
                            // a Slack button click.
                            if let Some(cid) = choice_id {
                                let rt = crate::runtime::get();
                                rt.choice_messages
                                    .lock()
                                    .expect("bug: lock poisoned")
                                    .insert(cid, (channel, msg_ts));
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
                } => {
                    // Ensure we have an active stream.
                    if !active_streams.contains_key(&thread_ts)
                        && start_fresh(&slack, &channel, &thread_ts, &mut active_streams)
                            .await
                            .is_none()
                    {
                        continue;
                    }

                    // Split at a clean breaking point (text has a newline and
                    // we're not inside a code block) when either:
                    // - the message has grown past 20k chars, or
                    // - the stream has been open longer than MAX_STREAM_AGE.
                    let should_split = {
                        let state = active_streams.get(&thread_ts).expect("bug: just inserted");
                        let clean_break = text.contains('\n') && !state.in_code_block;
                        let too_long = state.char_count > 20_000;
                        let too_old = state.started_at.elapsed() > MAX_STREAM_AGE;
                        clean_break && (too_long || too_old)
                    };

                    if should_split {
                        // Stop current stream and start a new one.
                        if let Some(old) = active_streams.remove(&thread_ts) {
                            let _ = slack.stop_stream(&channel, &old.ts).await;
                        }
                        if start_fresh(&slack, &channel, &thread_ts, &mut active_streams)
                            .await
                            .is_none()
                        {
                            continue;
                        }
                    }

                    let stream_ts = active_streams
                        .get(&thread_ts)
                        .expect("bug: stream must exist")
                        .ts
                        .clone();

                    // Append and handle error codes.
                    match slack.append_stream(&channel, &stream_ts, &text).await {
                        Ok(None) => {
                            // Success — update state.
                            let state = active_streams
                                .get_mut(&thread_ts)
                                .expect("bug: stream must exist");
                            state.char_count += text.len();
                            state.in_code_block =
                                update_code_block_state(&text, state.in_code_block);
                        }
                        Ok(Some(ref err)) => {
                            // Recoverable: start a new stream and retry.
                            tracing::warn!("append_stream got {err}, starting new stream");
                            active_streams.remove(&thread_ts);
                            if start_fresh(&slack, &channel, &thread_ts, &mut active_streams)
                                .await
                                .is_none()
                            {
                                continue;
                            }
                            let new_ts = active_streams
                                .get(&thread_ts)
                                .expect("bug: just started")
                                .ts
                                .clone();
                            match slack.append_stream(&channel, &new_ts, &text).await {
                                Ok(None) => {
                                    let state = active_streams
                                        .get_mut(&thread_ts)
                                        .expect("bug: stream must exist");
                                    state.char_count += text.len();
                                    state.in_code_block =
                                        update_code_block_state(&text, state.in_code_block);
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
                SlackAction::StreamStop { channel, thread_ts } => {
                    if let Some(state) = active_streams.remove(&thread_ts) {
                        // Complete any tool tasks that never got a result so
                        // the message doesn't finalize with spinners.
                        let pending: Vec<(String, String)> = {
                            let rt = crate::runtime::get();
                            let mut tasks = rt.tool_tasks.lock().expect("bug: lock poisoned");
                            tasks
                                .get_mut(&thread_ts)
                                .map(|q| q.drain(..).collect())
                                .unwrap_or_default()
                        };
                        for (task_id, title) in pending {
                            if let Err(e) = slack
                                .append_stream_task(
                                    &channel, &state.ts, &task_id, &title, "complete",
                                )
                                .await
                            {
                                tracing::warn!("completing pending task {task_id} failed: {e}");
                            }
                        }

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
                            .stop_stream_with_blocks(&channel, &state.ts, &disclaimer)
                            .await
                        {
                            tracing::error!("stop_stream failed: {e}");
                        }
                    }
                    // Clear the thread status indicator.
                    let _ = slack.set_thread_status(&channel, &thread_ts, "").await;
                }
                SlackAction::SetStatus {
                    channel,
                    thread_ts,
                    status,
                } => {
                    if let Err(e) = slack.set_thread_status(&channel, &thread_ts, &status).await {
                        tracing::error!("set_thread_status failed: {e}");
                    }
                }
                SlackAction::CommandResponse { response_url, text } => {
                    if let Err(e) = slack.respond_to_command(&response_url, &text).await {
                        tracing::error!("CommandResponse failed: {e}");
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
                } => {
                    // Ensure we have an active stream to attach the task to.
                    if !active_streams.contains_key(&thread_ts)
                        && start_fresh(&slack, &channel, &thread_ts, &mut active_streams)
                            .await
                            .is_none()
                    {
                        continue;
                    }
                    let stream_ts = active_streams
                        .get(&thread_ts)
                        .expect("bug: just ensured stream exists")
                        .ts
                        .clone();
                    match slack
                        .append_stream_task(&channel, &stream_ts, &task_id, &title, &status)
                        .await
                    {
                        Ok(None) => {}
                        Ok(Some(err)) => {
                            // Workspace may not support task chunks yet — fall
                            // back to the plain-text tool indicator (only for
                            // the start, to avoid duplicate lines).
                            tracing::warn!("append_stream_task got {err}, falling back to text");
                            if status == "in_progress" {
                                let _ = slack
                                    .append_stream(
                                        &channel,
                                        &stream_ts,
                                        &format!("\n\n🔧 `{title}(…)`\n"),
                                    )
                                    .await;
                            }
                        }
                        Err(e) => {
                            tracing::error!("append_stream_task failed: {e}");
                        }
                    }
                }
                SlackAction::DismissChoiceButtons { choice_id } => {
                    let info = {
                        let rt = crate::runtime::get();
                        rt.choice_messages
                            .lock()
                            .expect("bug: lock poisoned")
                            .remove(&choice_id)
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

    // Inbound: Slack WebSocket → dataflow.
    tokio::spawn(async move {
        loop {
            tracing::info!("connecting to Slack Socket Mode...");
            let url = match get_ws_url(&config.app_token).await {
                Ok(u) => u,
                Err(e) => {
                    tracing::error!("failed to get Socket Mode URL: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
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
                        tracing::warn!("dataflow channel closed, stopping sidecar inbound");
                        return;
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    (ReceiverStream::new(to_df_rx), PollSender::new(from_df_tx))
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
        is_app_home_opened: false,
    })
}

fn parse_interactive(payload: serde_json::Value) -> Option<SlackEvent> {
    let p: InteractivePayload = serde_json::from_value(payload).ok()?;
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

    #[test]
    fn filter_logic_drops_bot_messages() {
        let event = SlackEvent {
            user: "U1".into(),
            text: "bot".into(),
            channel: "C1".into(),
            thread_ts: "1.0".into(),
            is_button_click: false,
            button_value: None,
            action_id: None,
            message_ts: None,
            button_text: None,
            is_bot: true,
            is_unauthorized: false,
            slash_command: None,
            response_url: None,
            is_app_home_opened: false,
        };
        // Same filter as the dataflow
        assert!(!(!event.is_bot && !event.is_unauthorized));
    }

    #[test]
    fn filter_logic_drops_unauthorized() {
        let event = SlackEvent {
            user: "UBAD".into(),
            text: "hi".into(),
            channel: "C1".into(),
            thread_ts: "1.0".into(),
            is_button_click: false,
            button_value: None,
            action_id: None,
            message_ts: None,
            button_text: None,
            is_bot: false,
            is_unauthorized: true,
            slash_command: None,
            response_url: None,
            is_app_home_opened: false,
        };
        assert!(!(!event.is_bot && !event.is_unauthorized));
    }

    #[test]
    fn filter_logic_passes_valid_message() {
        let event = SlackEvent {
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
            is_app_home_opened: false,
        };
        assert!(!event.is_bot && !event.is_unauthorized);
    }
}
