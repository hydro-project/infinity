//! Daemon client mode — connects to the infinity daemon over a unix socket
//! (or in-memory channels) and bridges the protocol messages to the terminal.

use std::collections::HashMap;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_protocol::{ClientMessage, DaemonMessage, SessionInfo, TokenUsage};
use rig::completion::GetTokenUsage;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use infinity_agent_cli::model_picker::ModelEntry;
use infinity_agent_cli::terminal::{DetachResult, SessionChanged};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
pub struct DaemonTokenUsage(pub Option<TokenUsage>);

impl GetTokenUsage for DaemonTokenUsage {
    fn token_usage(&self) -> Option<rig::completion::Usage> {
        self.0.as_ref().map(|u| rig::completion::Usage {
            input_tokens: u.input_tokens.unwrap_or(0),
            output_tokens: u.output_tokens.unwrap_or(0),
            total_tokens: u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0),
            cached_input_tokens: 0,
        })
    }
}

fn daemon_msg_to_display(msg: DaemonMessage) -> Option<DisplayEvent<DaemonTokenUsage>> {
    Some(match msg {
        DaemonMessage::StartOutput { prefix } => DisplayEvent::StartOutput { prefix },
        DaemonMessage::TextChunk { prefix, chunk } => DisplayEvent::TextChunk { prefix, chunk },
        DaemonMessage::ToolCall {
            name,
            args,
            prefix,
            display_as,
        } => DisplayEvent::ToolCall {
            name,
            args: serde_json::from_str(&args).unwrap(),
            prefix,
            display_as,
        },
        DaemonMessage::ToolResult {
            text,
            display_as,
            prefix,
        } => DisplayEvent::ToolResult {
            text,
            display_as,
            prefix,
        },
        DaemonMessage::Info(s) => DisplayEvent::Info(s),
        DaemonMessage::ResponseDone {
            thread_id,
            token_usage,
        } => DisplayEvent::ResponseDone(thread_id, Some(DaemonTokenUsage(token_usage))),
        DaemonMessage::UserInputEcho(s) => DisplayEvent::UserInput(s),
        DaemonMessage::SubscriptionEvent { name, text, prefix } => {
            DisplayEvent::SubscriptionEvent { name, text, prefix }
        }
        DaemonMessage::OAuthRequired { auth_url } => DisplayEvent::OAuthRequired { auth_url },
        DaemonMessage::UserChoiceRequired {
            id,
            prompt,
            choices,
            default,
        } => DisplayEvent::UserChoiceRequired {
            id,
            prompt,
            choices,
            default,
            response_url: String::new(),
        },
        DaemonMessage::ThinkingStart { prefix } => DisplayEvent::ThinkingStart { prefix },
        DaemonMessage::ThinkingEnd { prefix } => DisplayEvent::ThinkingEnd { prefix },
        DaemonMessage::ThinkingChunk { prefix, chunk } => {
            DisplayEvent::ThinkingChunk { prefix, chunk }
        }
        DaemonMessage::CompactionApplied { prefix } => DisplayEvent::CompactionApplied { prefix },
        DaemonMessage::Error(e) => DisplayEvent::Info(format!("Error: {e}")),
        DaemonMessage::Connected { .. }
        | DaemonMessage::Welcome { .. }
        | DaemonMessage::Replay { .. }
        | DaemonMessage::SessionsUpdated { .. }
        | DaemonMessage::DisconnectNotIdle
        | DaemonMessage::DetachedIdle => return None,
    })
}

async fn ensure_daemon_running() -> Result<UnixStream, BoxError> {
    if let Ok(stream) = UnixStream::connect(&infinity_protocol::socket_path()).await {
        return Ok(stream);
    }
    launch_daemon()?;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(stream) = UnixStream::connect(&infinity_protocol::socket_path()).await {
            return Ok(stream);
        }
    }
    Err("daemon failed to start within 5 seconds".into())
}

fn launch_daemon() -> Result<(), BoxError> {
    let current_exe = std::env::current_exe()?;
    std::process::Command::new(&current_exe)
        .arg("daemon")
        .env(
            "RUST_LOG",
            std::env::var_os("RUST_LOG").unwrap_or("debug".into()),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Connect to the daemon over a unix socket (serialized framing).
pub async fn run_with_daemon(initial_message: Option<String>) -> Result<(), BoxError> {
    let stream = ensure_daemon_running().await?;
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

    let (to_daemon_tx, mut to_daemon_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (from_daemon_tx, from_daemon_rx) = mpsc::unbounded_channel::<DaemonMessage>();

    tokio::pin! {
        let client_fut = run_client(from_daemon_rx, to_daemon_tx, initial_message, None);
    }

    loop {
        tokio::select! {
            msg = to_daemon_rx.recv() => {
                let Some(msg) = msg else {
                    drop(from_daemon_tx);
                    return client_fut.await;
                };
                let bytes = Bytes::from(bincode::serialize(&msg).unwrap());
                if framed.send(bytes).await.is_err() {
                    drop(from_daemon_tx);
                    return client_fut.await;
                }
            }
            client_res = &mut client_fut => {
                #[expect(clippy::let_underscore_future, reason = "dropping completed future")]
                let _ = client_fut;
                // TODO(shadaj): maybe use join to simplify the state management?
                // Drain any remaining messages (e.g. Disconnect) before closing the socket.
                while let Some(msg) = to_daemon_rx.recv().await {
                    let bytes = Bytes::from(bincode::serialize(&msg).unwrap());
                    let _ = framed.send(bytes).await;
                }
                return client_res;
            }
            frame = framed.next() => {
                match frame {
                    Some(Ok(bytes)) => {
                        let msg = bincode::deserialize::<DaemonMessage>(&bytes).unwrap();
                        let _ = from_daemon_tx.send(msg);
                    }
                    _ => {
                        // Daemon closed the socket — drop from_daemon_tx so
                        // run_client sees the channel close.
                        drop(from_daemon_tx);
                        return client_fut.await;
                    }
                }
            }
        }
    }
}

/// Run in-memory mode — no serialization, direct channel passing.
pub async fn run_in_memory(
    from_daemon_rx: mpsc::UnboundedReceiver<DaemonMessage>,
    to_daemon_tx: mpsc::UnboundedSender<ClientMessage>,
    initial_message: Option<String>,
    startup_info: Option<String>,
) -> Result<(), BoxError> {
    run_client(from_daemon_rx, to_daemon_tx, initial_message, startup_info).await
}

/// Core client logic — works with channels regardless of transport.
async fn run_client(
    mut from_daemon: mpsc::UnboundedReceiver<DaemonMessage>,
    to_daemon: mpsc::UnboundedSender<ClientMessage>,
    initial_message: Option<String>,
    startup_info: Option<String>,
) -> Result<(), BoxError> {
    // Read Welcome
    let welcome = from_daemon.recv().await.ok_or("daemon disconnected")?;
    let (model_name, context_window, sessions, available_models, provider_name) = match welcome {
        DaemonMessage::Welcome {
            default_model_name,
            default_context_window,
            sessions,
            available_models,
            provider_name,
        } => (
            default_model_name,
            default_context_window,
            sessions,
            available_models,
            provider_name,
        ),
        DaemonMessage::Error(e) => return Err(e.into()),
        _ => return Err("expected Welcome from daemon".into()),
    };

    let cwd = std::env::current_dir().unwrap_or_default();

    let model_entries: Vec<ModelEntry> = available_models
        .into_iter()
        .map(|m| ModelEntry {
            display_name: m.display_name,
            model_id: m.model_id,
            additional_request_params: None,
            context_window: m.context_window,
        })
        .collect();

    let (display_tx, display_rx) = mpsc::unbounded_channel::<DisplayEvent<DaemonTokenUsage>>();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    let (load_session_tx, mut load_session_rx) =
        mpsc::unbounded_channel::<(Option<String>, bool)>();
    let (model_switch_tx, mut model_switch_rx) = mpsc::unbounded_channel::<usize>();
    let (session_tx, session_rx) = mpsc::unbounded_channel::<SessionChanged>();
    let (sessions_updated_tx, sessions_updated_rx) =
        mpsc::unbounded_channel::<HashMap<String, SessionInfo>>();
    let (soft_detach_tx, mut soft_detach_rx) = mpsc::unbounded_channel::<()>();
    let (detach_result_tx, detach_result_rx) = mpsc::unbounded_channel::<DetachResult>();
    let (choice_answered_tx, mut choice_answered_rx) = mpsc::unbounded_channel::<(String, usize)>();

    if let Some(info) = startup_info {
        let _ = display_tx.send(DisplayEvent::Info(info));
    }
    let _ = display_tx.send(DisplayEvent::Info(format!(
        "Using provider {} ({})",
        provider_name, model_name
    )));

    let models_for_switch = model_entries.clone();

    let mut terminal_handle = tokio::task::spawn_local(infinity_agent_cli::terminal::run(
        input_tx,
        display_rx,
        model_name,
        context_window,
        sessions,
        load_session_tx,
        model_switch_tx,
        model_entries,
        initial_message,
        session_rx,
        sessions_updated_rx,
        soft_detach_tx,
        detach_result_rx,
        choice_answered_tx,
    ));

    let mut active_session: Option<String> = None;
    let mut pending_input: Vec<String> = Vec::new();
    let mut terminal_result: Option<Result<Result<bool, BoxError>, tokio::task::JoinError>> = None;
    let mut pending_soft_detach = false;

    loop {
        tokio::select! {
            biased;

            msg = from_daemon.recv() => {
                let Some(msg) = msg else {
                    break;
                };
                match msg {
                    DaemonMessage::Connected { session_id, title, total_tokens_used, .. } => {
                        active_session = Some(session_id.clone());
                        let _ = session_tx.send(SessionChanged { session_id, title, total_tokens_used });
                        for text in pending_input.drain(..) {
                            let sid = active_session.as_ref().unwrap().clone();
                            let _ = to_daemon.send(ClientMessage::UserInput { session_id: sid, text });
                        }
                    }
                    DaemonMessage::Replay { history, pending_choices } => {
                        for m in history {
                            if let Some(evt) = daemon_msg_to_display(m) {
                                let _ = display_tx.send(evt);
                            }
                        }
                        let _ = display_tx.send(DisplayEvent::ResponseDone(None, Some(DaemonTokenUsage(None))));
                        for m in pending_choices {
                            if let Some(evt) = daemon_msg_to_display(m) {
                                let _ = display_tx.send(evt);
                            }
                        }
                    }
                    DaemonMessage::SessionsUpdated { sessions } => {
                        let _ = sessions_updated_tx.send(sessions);
                    }
                    DaemonMessage::DisconnectNotIdle => {
                        if pending_soft_detach {
                            pending_soft_detach = false;
                            let _ = detach_result_tx.send(DetachResult::NotIdle);
                        }
                    }
                    DaemonMessage::DetachedIdle => {
                        if pending_soft_detach {
                            pending_soft_detach = false;
                            active_session = None;
                            let _ = detach_result_tx.send(DetachResult::Idle);
                        }
                    }
                    msg => {
                        if let Some(evt) = daemon_msg_to_display(msg) {
                            let _ = display_tx.send(evt);
                        }
                    }
                }
            }

            msg = soft_detach_rx.recv() => {
                let Some(()) = msg else { break };
                if let Some(ref sid) = active_session {
                    pending_soft_detach = true;
                    let _ = to_daemon.send(ClientMessage::SoftDetach { session_id: sid.clone() });
                }
            }

            maybe_target = load_session_rx.recv() => {
                let Some((maybe_target, shut_down_old)) = maybe_target else { break };
                if let Some(ref sid) = active_session {
                    if shut_down_old {
                        let _ = to_daemon.send(ClientMessage::ShutdownSession { session_id: sid.clone() });
                    } else {
                        let _ = to_daemon.send(ClientMessage::Disconnect { session_id: sid.clone() });
                    }
                }
                active_session = None;

                if let Some(target) = maybe_target {
                    let _ = to_daemon.send(ClientMessage::LoadSession { target_session_id: target });
                } // if none, will be created on next user input
            }

            idx = model_switch_rx.recv() => {
                let Some(idx) = idx else { break };
                if let (Some(sid), Some(entry)) = (&active_session, models_for_switch.get(idx)) {
                    let _ = to_daemon.send(ClientMessage::SwitchModel {
                        session_id: sid.clone(), model_id: entry.model_id.clone(),
                    });
                }
            }

            answered = choice_answered_rx.recv() => {
                if let Some((choice_id, selected)) = answered {
                    let _ = to_daemon.send(ClientMessage::UserChoiceAnswered { choice_id, selected });
                }
            }

            res = &mut terminal_handle => {
                terminal_result = Some(res);
                break;
            }

            text = input_rx.recv() => {
                let Some(text) = text else { break };
                if let Some(ref sid) = active_session {
                    if text == "__compact__" {
                        let _ = to_daemon.send(ClientMessage::TriggerCompaction { session_id: sid.clone() });
                    } else {
                        let _ = to_daemon.send(ClientMessage::UserInput { session_id: sid.clone(), text });
                    }
                } else {
                    pending_input.push(text);
                    let _ = to_daemon.send(ClientMessage::CreateSession { cwd: cwd.clone() });
                }
            }
        }
    }

    let result = match terminal_result {
        Some(r) => r,
        None => terminal_handle.await,
    };
    let keep_running = matches!(result, Ok(Ok(true)));

    if let Some(sid) = active_session {
        if keep_running {
            let _ = to_daemon.send(ClientMessage::Disconnect { session_id: sid });
        } else {
            let _ = to_daemon.send(ClientMessage::ShutdownSession { session_id: sid });
        }
    }

    Ok(())
}
