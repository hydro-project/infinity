//! ACP (Agent Client Protocol) server mode — exposes the infinity daemon
//! as a stdio-based ACP agent server.
//!
//! Architecture: one "control" daemon connection for session listing/updates,
//! plus a separate daemon connection per active session.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, Diff as AcpDiff, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionCapabilities, SessionInfoUpdate, SessionListCapabilities, SessionNotification,
    SessionUpdate, SetSessionModelRequest, SetSessionModelResponse, StopReason, TextContent,
    ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Dispatch, Responder, on_receive_dispatch,
    on_receive_request,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use infinity_protocol::{ClientMessage, DaemonMessage, SessionInfo};
use std::sync::Mutex;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Per-session connection state. Each loaded/created session gets its own daemon socket.
struct SessionConnection {
    /// ACP session ID (what the client sees).
    acp_id: String,
    /// Daemon session ID (what the daemon sees). None until Connected arrives.
    daemon_id: Option<String>,
    /// ACP connection for sending notifications.
    cx: ConnectionTo<Client>,
    /// Pending prompt responder.
    pending_prompt: Option<Responder<PromptResponse>>,
    /// ResponseDone messages to skip (from interrupted prompts).
    skip_response_done: usize,
    /// Last generated tool call ID, used to correlate ToolResult.
    last_tool_call_id: Option<String>,
    /// Whether replay should be forwarded (true for load, false for prompt-triggered connect).
    forward_replay: bool,
}

impl SessionConnection {
    fn next_tool_call_id(&mut self) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.last_tool_call_id = Some(id.clone());
        id
    }
}

/// Global state shared across all ACP request handlers.
struct GlobalState {
    /// Sessions known from the daemon Welcome/SessionsUpdated.
    sessions: HashMap<String, SessionInfo>,
    /// Maps daemon session ID → ACP session ID.
    daemon_to_acp: HashMap<String, String>,
    /// Maps ACP session ID → daemon session ID.
    acp_to_daemon: HashMap<String, String>,
    /// ACP connection (set on initialize).
    cx: Option<ConnectionTo<Client>>,
    /// Per-session connections, keyed by ACP session ID.
    session_connections: HashMap<String, mpsc::UnboundedSender<SessionCmd>>,
    /// Lazy sessions: ACP ID → cwd, consumed on first prompt.
    lazy_sessions: HashMap<String, std::path::PathBuf>,
    /// Path to persist session mappings.
    mappings_path: std::path::PathBuf,
}

/// Command sent to a per-session task.
struct SessionCmd {
    text: String,
    responder: Responder<PromptResponse>,
}

/// Run the ACP stdio server.
pub async fn run() -> Result<(), BoxError> {
    // Control connection — gets Welcome and SessionsUpdated.
    let control_stream = crate::daemon_client::ensure_daemon_running().await?;
    let mut control_framed = Framed::new(control_stream, LengthDelimitedCodec::new());

    let welcome = recv(&mut control_framed).await?;
    let sessions = match welcome {
        DaemonMessage::Welcome { sessions, .. } => sessions,
        DaemonMessage::Error { text, .. } => return Err(text.into()),
        _ => return Err("expected Welcome from daemon".into()),
    };

    let mappings_path = std::env::current_dir()
        .unwrap_or_default()
        .join(".infinity")
        .join("acp-sessions.json");
    let (daemon_to_acp, acp_to_daemon) = load_session_mappings(&mappings_path);

    let state = Arc::new(Mutex::new(GlobalState {
        sessions,
        daemon_to_acp,
        acp_to_daemon,
        cx: None,
        session_connections: HashMap::new(),
        lazy_sessions: HashMap::new(),
        mappings_path,
    }));

    let state_for_init = state.clone();
    let state_for_list = state.clone();
    let state_for_new = state.clone();
    let state_for_load = state.clone();
    let state_for_prompt = state.clone();
    let state_for_dispatch = state.clone();

    let acp_fut = async move {
        let result = Agent
            .builder()
            .name("infinity-agent")
            .on_receive_request(
                {
                    let state = state_for_init;
                    async move |_req: InitializeRequest,
                                responder: Responder<InitializeResponse>,
                                cx: ConnectionTo<Client>| {
                        state.lock().expect("bug: lock poisoned").cx = Some(cx);
                        responder.respond(
                            InitializeResponse::new(
                                agent_client_protocol::schema::ProtocolVersion::V1,
                            )
                            .agent_capabilities(
                                AgentCapabilities::new()
                                    .load_session(true)
                                    .session_capabilities(
                                        SessionCapabilities::new()
                                            .list(SessionListCapabilities::new()),
                                    ),
                            )
                            .agent_info(
                                agent_client_protocol::schema::Implementation::new(
                                    "infinity-agent",
                                    env!("CARGO_PKG_VERSION"),
                                ),
                            ),
                        )
                    }
                },
                on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = state_for_list;
                    async move |_req: ListSessionsRequest,
                                responder: Responder<ListSessionsResponse>,
                                _cx: ConnectionTo<Client>| {
                        let s = state.lock().expect("bug: lock poisoned");
                        let acp_sessions: Vec<agent_client_protocol::schema::SessionInfo> = s
                            .sessions
                            .iter()
                            .filter(|(_, info)| {
                                info.status != infinity_protocol::SessionStatus::Archived
                            })
                            .map(|(id, info)| {
                                let acp_id = s.daemon_to_acp.get(id).unwrap_or(id).clone();
                                agent_client_protocol::schema::SessionInfo::new(
                                    acp_id,
                                    std::env::current_dir().unwrap_or_default(),
                                )
                                .title(info.title.clone())
                                .updated_at(Some(info.last_updated.clone()))
                            })
                            .collect();
                        drop(s);
                        responder.respond(ListSessionsResponse::new(acp_sessions))
                    }
                },
                on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = state_for_new;
                    async move |req: NewSessionRequest,
                                responder: Responder<NewSessionResponse>,
                                _cx: ConnectionTo<Client>| {
                        let mut s = state.lock().expect("bug: lock poisoned");
                        let session_id = uuid::Uuid::new_v4().to_string();
                        s.lazy_sessions.insert(session_id.clone(), req.cwd);
                        responder.respond(NewSessionResponse::new(session_id))
                    }
                },
                on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = state_for_load;
                    async move |req: LoadSessionRequest,
                                responder: Responder<LoadSessionResponse>,
                                cx: ConnectionTo<Client>| {
                        let mut s = state.lock().expect("bug: lock poisoned");
                        let acp_id = req.session_id.0.to_string();
                        let daemon_id = s
                            .acp_to_daemon
                            .get(&acp_id)
                            .cloned()
                            .unwrap_or_else(|| acp_id.clone());
                        if !s.sessions.contains_key(&daemon_id) {
                            return responder.respond_with_error(
                                agent_client_protocol::schema::Error::invalid_params()
                                    .data(format!("session '{}' not found", acp_id)),
                            );
                        }
                        s.daemon_to_acp.insert(daemon_id.clone(), acp_id.clone());
                        s.acp_to_daemon.insert(acp_id.clone(), daemon_id.clone());
                        save_session_mappings(&s);

                        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                        s.session_connections.insert(acp_id.clone(), cmd_tx);
                        drop(s);

                        // Spawn per-session task.
                        let state2 = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = run_session_connection(
                                state2,
                                acp_id,
                                SessionStart::Load {
                                    daemon_id,
                                    responder,
                                },
                                cx,
                                cmd_rx,
                            )
                            .await
                            {
                                tracing::error!("session connection error: {e}");
                            }
                        });
                        Ok(())
                    }
                },
                on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = state_for_prompt;
                    async move |req: PromptRequest,
                                responder: Responder<PromptResponse>,
                                cx: ConnectionTo<Client>| {
                        let mut s = state.lock().expect("bug: lock poisoned");
                        let acp_id = req.session_id.0.to_string();

                        let text: String = req
                            .prompt
                            .iter()
                            .filter_map(|block| {
                                if let ContentBlock::Text(t) = block {
                                    Some(t.text.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        // If session already has a connection, send prompt to it.
                        if let Some(cmd_tx) = s.session_connections.get(&acp_id) {
                            if let Err(mpsc::error::SendError(SessionCmd { responder, .. })) =
                                cmd_tx.send(SessionCmd { text, responder })
                            {
                                s.session_connections.remove(&acp_id);
                                return responder.respond_with_error(
                                    agent_client_protocol::util::internal_error(
                                        "session disconnected",
                                    ),
                                );
                            }
                            return Ok(());
                        }

                        // Lazy session — create a new connection.
                        if let Some(cwd) = s.lazy_sessions.remove(&acp_id) {
                            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                            s.session_connections.insert(acp_id.clone(), cmd_tx);
                            drop(s);

                            let state2 = state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = run_session_connection(
                                    state2,
                                    acp_id,
                                    SessionStart::Create {
                                        cwd,
                                        text,
                                        responder,
                                    },
                                    cx,
                                    cmd_rx,
                                )
                                .await
                                {
                                    tracing::error!("session connection error: {e}");
                                }
                            });
                            return Ok(());
                        }

                        // Unknown session — try connecting by daemon ID.
                        let daemon_id = s
                            .acp_to_daemon
                            .get(&acp_id)
                            .cloned()
                            .unwrap_or_else(|| acp_id.clone());
                        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                        s.session_connections.insert(acp_id.clone(), cmd_tx);
                        drop(s);

                        let state2 = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = run_session_connection(
                                state2,
                                acp_id,
                                SessionStart::ConnectAndPrompt {
                                    daemon_id,
                                    text,
                                    responder,
                                },
                                cx,
                                cmd_rx,
                            )
                            .await
                            {
                                tracing::error!("session connection error: {e}");
                            }
                        });
                        Ok(())
                    }
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: SetSessionModelRequest,
                            responder: Responder<SetSessionModelResponse>,
                            _cx: ConnectionTo<Client>| {
                    responder.respond(SetSessionModelResponse::new())
                },
                on_receive_request!(),
            )
            .on_receive_dispatch(
                async move |message: Dispatch, cx: ConnectionTo<Client>| {
                    let _ = state_for_dispatch;
                    message.respond_with_error(
                        agent_client_protocol::util::internal_error("not implemented"),
                        cx,
                    )
                },
                on_receive_dispatch!(),
            )
            .connect_to(ByteStreams::new(
                tokio::io::stdout().compat_write(),
                tokio::io::stdin().compat(),
            ))
            .await;

        if let Err(e) = result {
            tracing::error!("ACP connection error: {e}");
        }
    };
    tokio::pin!(acp_fut);

    // Control connection loop — just keeps sessions list updated.
    loop {
        tokio::select! {
            frame = control_framed.next() => {
                match frame {
                    Some(Ok(bytes)) => {
                        let msg: DaemonMessage = serde_json::from_slice(&bytes)
                            .expect("bug: deserialize daemon message");
                        if let DaemonMessage::SessionsUpdated { sessions } = msg {
                            let mut s = state.lock().expect("bug: lock poisoned");
                            s.sessions = sessions;
                            // Notify all active sessions of info updates.
                            for (daemon_id, info) in &s.sessions {
                                let acp_id = s.daemon_to_acp.get(daemon_id).unwrap_or(daemon_id);
                                if let Some(ref cx) = s.cx {
                                    let notif = SessionNotification::new(
                                        acp_id.clone(),
                                        SessionUpdate::SessionInfoUpdate(
                                            SessionInfoUpdate::new()
                                                .title(info.title.clone())
                                                .updated_at(Some(info.last_updated.clone())),
                                        ),
                                    );
                                    let _ = cx.send_notification(notif);
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!("control connection error: {e}");
                        break;
                    }
                    None => break,
                }
            }
            _ = &mut acp_fut => break,
        }
    }

    Ok(())
}

/// How a session connection starts.
enum SessionStart {
    Load {
        daemon_id: String,
        responder: Responder<LoadSessionResponse>,
    },
    Create {
        cwd: std::path::PathBuf,
        text: String,
        responder: Responder<PromptResponse>,
    },
    ConnectAndPrompt {
        daemon_id: String,
        text: String,
        responder: Responder<PromptResponse>,
    },
}

/// Run a per-session daemon connection.
async fn run_session_connection(
    global: Arc<Mutex<GlobalState>>,
    acp_id: String,
    start: SessionStart,
    cx: ConnectionTo<Client>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
) -> Result<(), BoxError> {
    let stream = crate::daemon_client::ensure_daemon_running().await?;
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

    // Skip Welcome on this connection.
    let _ = recv(&mut framed).await?;

    let mut conn = SessionConnection {
        acp_id: acp_id.clone(),
        daemon_id: None,
        cx,
        pending_prompt: None,
        skip_response_done: 0,
        last_tool_call_id: None,
        forward_replay: false,
    };

    // Initiate based on start type.
    match start {
        SessionStart::Load {
            daemon_id,
            responder,
        } => {
            let msg = ClientMessage::Connect {
                session_id: daemon_id.clone(),
                thread_id: None,
            };
            send(&mut framed, &msg).await?;
            conn.daemon_id = Some(daemon_id);
            conn.forward_replay = true;
            // Wait for Connected, then respond.
            loop {
                let msg = recv(&mut framed).await?;
                match msg {
                    DaemonMessage::Connected { title, .. } => {
                        let _ = responder.respond(LoadSessionResponse::new());
                        if let Some(title) = title {
                            let g = global.lock().expect("bug: lock poisoned");
                            let updated_at = conn
                                .daemon_id
                                .as_ref()
                                .and_then(|id| g.sessions.get(id))
                                .map(|info| info.last_updated.clone());
                            let notif = SessionNotification::new(
                                acp_id.clone(),
                                SessionUpdate::SessionInfoUpdate(
                                    SessionInfoUpdate::new().title(title).updated_at(updated_at),
                                ),
                            );
                            let _ = conn.cx.send_notification(notif);
                        }
                        break;
                    }
                    DaemonMessage::Error { text, .. } => {
                        let _ = responder
                            .respond_with_error(agent_client_protocol::util::internal_error(&text));
                        return Ok(());
                    }
                    _ => continue,
                }
            }
        }
        SessionStart::Create {
            cwd,
            text,
            responder,
        } => {
            let msg = ClientMessage::CreateSession {
                cwd,
                location: None,
                model: None,
            };
            send(&mut framed, &msg).await?;
            // Wait for Connected to get daemon ID, then send input.
            loop {
                let msg = recv(&mut framed).await?;
                match msg {
                    DaemonMessage::Connected {
                        session_id, title, ..
                    } => {
                        conn.daemon_id = Some(session_id.clone());
                        // Register mapping.
                        {
                            let mut g = global.lock().expect("bug: lock poisoned");
                            g.daemon_to_acp.insert(session_id.clone(), acp_id.clone());
                            g.acp_to_daemon.insert(acp_id.clone(), session_id.clone());
                            save_session_mappings(&g);
                        }

                        let input_msg = ClientMessage::UserInput { session_id, text };
                        send(&mut framed, &input_msg).await?;
                        conn.pending_prompt = Some(responder);

                        if let Some(title) = title {
                            let notif = SessionNotification::new(
                                acp_id.clone(),
                                SessionUpdate::SessionInfoUpdate(
                                    SessionInfoUpdate::new().title(title),
                                ),
                            );
                            let _ = conn.cx.send_notification(notif);
                        }
                        break;
                    }
                    DaemonMessage::Error { text, .. } => {
                        let _ = responder
                            .respond_with_error(agent_client_protocol::util::internal_error(&text));
                        return Ok(());
                    }
                    _ => continue,
                }
            }
        }
        SessionStart::ConnectAndPrompt {
            daemon_id,
            text,
            responder,
        } => {
            let msg = ClientMessage::Connect {
                session_id: daemon_id.clone(),
                thread_id: None,
            };
            send(&mut framed, &msg).await?;
            conn.daemon_id = Some(daemon_id.clone());
            // Wait for Connected, then send input.
            loop {
                let msg = recv(&mut framed).await?;
                match msg {
                    DaemonMessage::Connected { .. } => {
                        let input_msg = ClientMessage::UserInput {
                            session_id: daemon_id,
                            text,
                        };
                        send(&mut framed, &input_msg).await?;
                        conn.pending_prompt = Some(responder);
                        break;
                    }
                    DaemonMessage::Error { text, .. } => {
                        let _ = responder
                            .respond_with_error(agent_client_protocol::util::internal_error(&text));
                        return Ok(());
                    }
                    _ => continue,
                }
            }
        }
    }

    // Main loop: handle daemon messages and commands from ACP handlers.
    loop {
        tokio::select! {
            frame = framed.next() => {
                match frame {
                    Some(Ok(bytes)) => {
                        let msg: DaemonMessage = serde_json::from_slice(&bytes)
                            .expect("bug: deserialize daemon message");
                        handle_session_message(msg, &mut conn, &global);
                    }
                    Some(Err(e)) => {
                        tracing::error!("session connection error: {e}");
                        break;
                    }
                    None => break,
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(SessionCmd { text, responder }) = cmd else { break };
                // Interrupt existing prompt if any.
                if let Some(old) = conn.pending_prompt.take() {
                    let _ = old.respond(PromptResponse::new(StopReason::Cancelled));
                    conn.skip_response_done += 1;
                }
                if let Some(ref daemon_id) = conn.daemon_id {
                    let msg = ClientMessage::UserInput {
                        session_id: daemon_id.clone(),
                        text,
                    };
                    if let Err(e) = send(&mut framed, &msg).await {
                        tracing::error!("failed to send to daemon: {e}");
                    }
                }
                conn.pending_prompt = Some(responder);
            }
        }
    }

    // Cleanup.
    if let Some(responder) = conn.pending_prompt.take() {
        let _ = responder.respond_with_error(agent_client_protocol::util::internal_error(
            "session disconnected",
        ));
    }
    let mut g = global.lock().expect("bug: lock poisoned");
    g.session_connections.remove(&conn.acp_id);
    Ok(())
}

/// Handle a daemon message for a specific session connection.
fn handle_session_message(
    msg: DaemonMessage,
    conn: &mut SessionConnection,
    global: &Arc<Mutex<GlobalState>>,
) {
    let session_id: Arc<str> = Arc::from(conn.acp_id.as_str());

    match msg {
        DaemonMessage::TextChunk { .. }
        | DaemonMessage::ThinkingChunk { .. }
        | DaemonMessage::UserInputEcho { .. }
        | DaemonMessage::ToolCall { .. }
        | DaemonMessage::ToolResult { .. } => {
            if let Some(update) = daemon_msg_to_session_update(msg, conn, false) {
                let notif = SessionNotification::new(session_id, update);
                let _ = conn.cx.send_notification(notif);
            }
        }
        DaemonMessage::ResponseDone { .. } => {
            if conn.skip_response_done > 0 {
                conn.skip_response_done -= 1;
            } else if let Some(responder) = conn.pending_prompt.take() {
                let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
            }
        }
        DaemonMessage::Error { text, .. } => {
            if conn.skip_response_done > 0 {
                conn.skip_response_done -= 1;
            } else if let Some(responder) = conn.pending_prompt.take() {
                let _ = responder
                    .respond_with_error(agent_client_protocol::util::internal_error(&text));
            }
        }
        DaemonMessage::SessionsUpdated { sessions } => {
            let mut g = global.lock().expect("bug: lock poisoned");
            g.sessions = sessions;
            if let Some(info) = conn.daemon_id.as_ref().and_then(|id| g.sessions.get(id)) {
                let notif = SessionNotification::new(
                    session_id,
                    SessionUpdate::SessionInfoUpdate(
                        SessionInfoUpdate::new()
                            .title(info.title.clone())
                            .updated_at(Some(info.last_updated.clone())),
                    ),
                );
                let _ = conn.cx.send_notification(notif);
            }
        }
        DaemonMessage::Replay { history, .. } => {
            if !conn.forward_replay {
                return;
            }
            conn.forward_replay = false;
            for m in history {
                if let Some(update) = daemon_msg_to_session_update(m, conn, true) {
                    let notif = SessionNotification::new(session_id.clone(), update);
                    let _ = conn.cx.send_notification(notif);
                }
            }
            // Send info update after replay.
            let g = global.lock().expect("bug: lock poisoned");
            if let Some(info) = conn.daemon_id.as_ref().and_then(|id| g.sessions.get(id)) {
                let notif = SessionNotification::new(
                    session_id,
                    SessionUpdate::SessionInfoUpdate(
                        SessionInfoUpdate::new()
                            .title(info.title.clone())
                            .updated_at(Some(info.last_updated.clone())),
                    ),
                );
                let _ = conn.cx.send_notification(notif);
            }
        }
        _ => {}
    }
}

/// Convert a daemon output message to an ACP SessionUpdate.
/// Returns None for messages that don't map to updates.
fn daemon_msg_to_session_update(
    msg: DaemonMessage,
    conn: &mut SessionConnection,
    is_replay: bool,
) -> Option<SessionUpdate> {
    match msg {
        DaemonMessage::TextChunk { chunk, .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(chunk))),
        )),
        DaemonMessage::ThinkingChunk { chunk, .. } => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(chunk))),
        )),
        DaemonMessage::UserInputEcho { text, .. } => Some(SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text))),
        )),
        DaemonMessage::ToolCall {
            name,
            args,
            display_as,
            ..
        } => {
            let tc_id = conn.next_tool_call_id();
            let kind = match name.as_str() {
                "read_file" => ToolKind::Read,
                "edit_file" | "create_file" => ToolKind::Edit,
                "execute_command" => ToolKind::Execute,
                "grep" => ToolKind::Search,
                _ => ToolKind::Other,
            };
            let parsed_args = serde_json::from_str::<serde_json::Value>(&args).ok();
            let title = display_as.unwrap_or(name);
            let status = if is_replay {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::InProgress
            };
            let tool_call = ToolCall::new(ToolCallId::new(tc_id), title)
                .kind(kind)
                .status(status)
                .raw_input(parsed_args);
            Some(SessionUpdate::ToolCall(tool_call))
        }
        DaemonMessage::ToolResult { segments, .. } => {
            let tc_id = conn
                .last_tool_call_id
                .take()
                .expect("bug: ToolResult without matching ToolCall");
            let (content, raw_output) = segments
                .into_iter()
                .map(|seg| match seg {
                    rap_protocol::DisplaySegment::Diff(d) => {
                        let (old_text, new_text) = parse_unified_diff(&d.patch);
                        let diff = ToolCallContent::Diff(
                            AcpDiff::new(&d.path, new_text).old_text(old_text),
                        );
                        (Some(vec![diff]), None)
                    }
                    rap_protocol::DisplaySegment::Text(t) => {
                        (None, Some(serde_json::Value::String(t)))
                    }
                })
                .next()
                .unwrap_or((None, None));
            let update = ToolCallUpdate::new(
                ToolCallId::new(tc_id),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(content)
                    .raw_output(raw_output),
            );
            Some(SessionUpdate::ToolCallUpdate(update))
        }
        _ => None,
    }
}

async fn send(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    msg: &ClientMessage,
) -> Result<(), BoxError> {
    let bytes = Bytes::from(serde_json::to_vec(msg).expect("bug: serialize"));
    framed.send(bytes).await.map_err(|e| e.into())
}

async fn recv(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
) -> Result<DaemonMessage, BoxError> {
    match framed.next().await {
        Some(Ok(bytes)) => Ok(serde_json::from_slice::<DaemonMessage>(&bytes)
            .expect("bug: failed to deserialize daemon message")),
        Some(Err(e)) => {
            tracing::error!("daemon connection error: {e}");
            Err(e.into())
        }
        None => Err("daemon disconnected".into()),
    }
}

/// Parse a unified diff into (old_text, new_text).
fn parse_unified_diff(patch: &str) -> (Option<String>, String) {
    let mut old_lines = Vec::new();
    let mut new_lines = Vec::new();
    for line in patch.lines() {
        if line.starts_with("@@")
            || line.starts_with("---")
            || line.starts_with("+++")
            || line.starts_with('\\')
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            old_lines.push(rest);
        } else if let Some(rest) = line.strip_prefix('+') {
            new_lines.push(rest);
        } else if let Some(rest) = line.strip_prefix(' ') {
            old_lines.push(rest);
            new_lines.push(rest);
        } else {
            old_lines.push(line);
            new_lines.push(line);
        }
    }
    let old = if old_lines.is_empty() {
        None
    } else {
        Some(old_lines.join("\n"))
    };
    let new = new_lines.join("\n");
    (old, new)
}

/// Load persisted session ID mappings from disk.
fn load_session_mappings(
    path: &std::path::Path,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let Ok(data) = std::fs::read_to_string(path) else {
        return (HashMap::new(), HashMap::new());
    };
    let map: HashMap<String, String> = serde_json::from_str(&data).unwrap_or_default();
    let mut daemon_to_acp = HashMap::new();
    let mut acp_to_daemon = HashMap::new();
    for (acp_id, daemon_id) in map {
        daemon_to_acp.insert(daemon_id.clone(), acp_id.clone());
        acp_to_daemon.insert(acp_id, daemon_id);
    }
    (daemon_to_acp, acp_to_daemon)
}

/// Persist session ID mappings to disk.
fn save_session_mappings(state: &GlobalState) {
    let map: HashMap<&str, &str> = state
        .acp_to_daemon
        .iter()
        .map(|(acp, daemon)| (acp.as_str(), daemon.as_str()))
        .collect();
    let json =
        serde_json::to_string_pretty(&map).expect("bug: failed to serialize session mappings");
    if let Some(parent) = state.mappings_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Err(e) = std::fs::write(&state.mappings_path, &json) {
        tracing::warn!(
            "failed to save session mappings to {:?}: {}",
            state.mappings_path,
            e
        );
    }
}
