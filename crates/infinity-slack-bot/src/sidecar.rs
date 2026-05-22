use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::PollSender;

use crate::config::Config;
use crate::slack_client::SlackClient;

/// A normalized event from Slack (message or button click) that flows through the dataflow.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SlackEvent {
    pub user: String,
    pub text: String,
    pub channel: String,
    pub thread_ts: String,
    pub is_button_click: bool,
    pub button_value: Option<String>,
    pub action_id: Option<String>,
    /// True if this is a bot message.
    pub is_bot: bool,
    /// True if user is not authorized.
    pub is_unauthorized: bool,
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
        }

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
                } => {
                    if let Err(e) = slack
                        .post_blocks(&channel, &fallback_text, &blocks, thread_ts.as_deref())
                        .await
                    {
                        tracing::error!("PostBlocks failed: {e}");
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

                    // Check if we should split: length > 20k, text has a newline,
                    // and we're not inside a code block.
                    let should_split = {
                        let state = active_streams.get(&thread_ts).expect("bug: just inserted");
                        state.char_count > 20_000 && text.contains('\n') && !state.in_code_block
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
                        if let Err(e) = slack.stop_stream(&channel, &state.ts).await {
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
        _ => {
            tracing::debug!("ignoring envelope type: {}", envelope.envelope_type);
            None
        }
    }
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

    Some(SlackEvent {
        user: user.id.clone(),
        text: String::new(),
        channel: chan.id.clone(),
        thread_ts,
        is_button_click: true,
        button_value: Some(action.value.clone()),
        action_id: Some(action.action_id.clone()),
        is_bot: false,
        is_unauthorized: false, // set later by caller
    })
}

fn parse_events_api(payload: serde_json::Value) -> Option<SlackEvent> {
    let p: EventPayload = serde_json::from_value(payload).ok()?;
    let event = p.event?;

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
        is_bot,
        is_unauthorized: false, // set later by caller
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
            "actions": [{"value": "1"}],
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
            envelope_type: "slash_commands".into(),
            payload: serde_json::json!({}),
        };
        assert!(parse_envelope(envelope).is_none());
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
            is_bot: true,
            is_unauthorized: false,
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
            is_bot: false,
            is_unauthorized: true,
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
            is_bot: false,
            is_unauthorized: false,
        };
        assert!(!event.is_bot && !event.is_unauthorized);
    }
}
