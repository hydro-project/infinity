//! Daemon client mode — connects to the infinity daemon over a unix socket
//! (or in-memory channels) and bridges the protocol messages to the terminal.

use std::collections::HashMap;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_protocol::{
    ClientMessage, DaemonMessage, ModelRef, SessionInfo, TokenUsage, length_delimited_codec,
};
use rig::completion::GetTokenUsage;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::term_io::{EventSource, TermOut};
use crate::terminal::{DetachResult, SessionChanged};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
pub struct DaemonTokenUsage(pub Option<TokenUsage>);

impl GetTokenUsage for DaemonTokenUsage {
    fn token_usage(&self) -> Option<rig::completion::Usage> {
        self.0.as_ref().map(|u| {
            let input = u.input_tokens.unwrap_or(0);
            let output = u.output_tokens.unwrap_or(0);
            rig::completion::Usage {
                input_tokens: input,
                output_tokens: output,
                total_tokens: u.total_tokens.unwrap_or(input + output),
                cached_input_tokens: 0,
            }
        })
    }
}

/// Convert a DaemonMessage into a (thread_id, DisplayEvent) tuple.
/// Returns None for messages that are handled separately (Connected, Welcome, etc.).
fn daemon_msg_to_display(
    msg: DaemonMessage,
) -> Option<(Option<String>, DisplayEvent<DaemonTokenUsage>)> {
    Some(match msg {
        DaemonMessage::StartOutput { thread_id } => (thread_id, DisplayEvent::StartOutput),
        DaemonMessage::TextChunk { thread_id, chunk } => {
            (thread_id, DisplayEvent::TextChunk { chunk })
        }
        DaemonMessage::ToolCall {
            name,
            args,
            thread_id,
            display_as,
        } => (
            thread_id,
            DisplayEvent::ToolCall {
                name,
                args: serde_json::from_str(&args)
                    .expect("bug: tool call args should be valid JSON"),
                display_as,
            },
        ),
        DaemonMessage::ToolResult {
            segments,
            thread_id,
        } => (thread_id, DisplayEvent::ToolResult { segments }),
        DaemonMessage::Info { thread_id, text } => (thread_id, DisplayEvent::Info(text)),
        DaemonMessage::ResponseDone {
            thread_id,
            token_usage,
        } => (
            thread_id,
            DisplayEvent::ResponseDone(Some(DaemonTokenUsage(token_usage))),
        ),
        DaemonMessage::UserInputEcho { thread_id, text } => {
            (thread_id, DisplayEvent::UserInput(text))
        }
        DaemonMessage::SubscriptionEvent {
            name,
            text,
            thread_id,
        } => (thread_id, DisplayEvent::SubscriptionEvent { name, text }),
        DaemonMessage::OAuthRequired {
            thread_id,
            auth_url,
        } => (thread_id, DisplayEvent::OAuthRequired { auth_url }),
        DaemonMessage::UserChoiceRequired {
            thread_id,
            id,
            prompt,
            choices,
            default,
        } => (
            thread_id,
            DisplayEvent::UserChoiceRequired {
                id,
                prompt,
                choices,
                default,
                response_url: String::new(),
            },
        ),
        DaemonMessage::UserChoiceComplete { choice_id } => {
            (None, DisplayEvent::UserChoiceComplete { choice_id })
        }
        DaemonMessage::ThinkingStart { thread_id } => (thread_id, DisplayEvent::ThinkingStart),
        DaemonMessage::ThinkingEnd { thread_id } => (thread_id, DisplayEvent::ThinkingEnd),
        DaemonMessage::ThinkingChunk { thread_id, chunk } => {
            (thread_id, DisplayEvent::ThinkingChunk { chunk })
        }
        DaemonMessage::CompactionApplied { thread_id } => {
            (thread_id, DisplayEvent::CompactionApplied)
        }
        DaemonMessage::Error { thread_id, text } => {
            (thread_id, DisplayEvent::Info(format!("Error: {text}")))
        }
        DaemonMessage::Connected { .. }
        | DaemonMessage::Welcome { .. }
        | DaemonMessage::Replay { .. }
        | DaemonMessage::SessionsUpdated { .. }
        | DaemonMessage::ModelSwitched { .. }
        | DaemonMessage::DisconnectNotIdle
        | DaemonMessage::DetachedIdle
        | DaemonMessage::EmigrateResult { .. }
        | DaemonMessage::MigrateStarted { .. }
        | DaemonMessage::MigrateComplete { .. }
        | DaemonMessage::MigrateError { .. }
        | DaemonMessage::ImportComplete { .. }
        | DaemonMessage::RapServersBooted { .. }
        | DaemonMessage::RemotesUpdated { .. }
        | DaemonMessage::ViewUpdate { .. }
        | DaemonMessage::DirectoryListing { .. } => return None,
    })
}

pub async fn ensure_daemon_running() -> Result<UnixStream, BoxError> {
    if let Ok(stream) = UnixStream::connect(&infinity_protocol::socket_path()).await {
        return Ok(stream);
    }
    launch_daemon().await?;
    // The daemon binds its socket before announcing readiness, so a single
    // connect attempt suffices.
    UnixStream::connect(&infinity_protocol::socket_path())
        .await
        .map_err(|e| {
            format!("daemon reported ready but connecting to its socket failed: {e}").into()
        })
}

/// How long to wait for a launched daemon to either report readiness or
/// exit. Generous because daemon startup spawns model provider processes.
const DAEMON_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Launch the daemon as a background process and wait until it prints its
/// ready line on stdout. If the daemon exits first (e.g. a configuration
/// error), the error includes everything it printed to stdout/stderr, which
/// would otherwise be invisible for a detached process.
async fn launch_daemon() -> Result<(), BoxError> {
    let current_exe = std::env::current_exe()?;
    let mut child = tokio::process::Command::new(&current_exe)
        .arg("daemon")
        .env(
            "RUST_LOG",
            std::env::var_os("RUST_LOG").unwrap_or("debug".into()),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .expect("bug: piped child stdout missing");
    let stderr = child
        .stderr
        .take()
        .expect("bug: piped child stderr missing");
    let mut stdout_lines = tokio::io::BufReader::new(stdout).lines();

    // Accumulate stderr in the background for the failure report.
    let stderr_task = tokio::task::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut out = String::new();
        let mut reader = tokio::io::BufReader::new(stderr);
        let _ = reader.read_to_string(&mut out).await;
        out
    });

    let timeout = tokio::time::sleep(DAEMON_STARTUP_TIMEOUT);
    tokio::pin!(timeout);

    // Race the ready line against the daemon exiting.
    let mut stdout_so_far = String::new();
    let exit_status = loop {
        tokio::select! {
            line = stdout_lines.next_line() => match line {
                Ok(Some(line)) if line == infinity_daemon::DAEMON_READY_LINE => {
                    // Leave the daemon running detached; dropping the child
                    // handle and pipes does not kill it.
                    stderr_task.abort();
                    return Ok(());
                }
                Ok(Some(line)) => {
                    stdout_so_far.push_str(&line);
                    stdout_so_far.push('\n');
                }
                // stdout closed without the ready line: the daemon is
                // exiting; reap it below (with a timeout in case it
                // lingers with closed pipes).
                Ok(None) | Err(_) => {
                    match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                        .await
                    {
                        Ok(status) => break status?,
                        Err(_) => {
                            let _ = child.start_kill();
                            break child.wait().await?;
                        }
                    }
                }
            },
            status = child.wait() => break status?,
            _ = &mut timeout => {
                // Still booting: leave it running (it may just be slow) but
                // stop waiting.
                return Err(format!(
                    "daemon did not report ready within {}s; check ~/.infinity/daemon.log",
                    DAEMON_STARTUP_TIMEOUT.as_secs()
                ).into());
            }
        }
    };

    // Drain anything still buffered on stdout, then collect stderr.
    while let Ok(Some(line)) = stdout_lines.next_line().await {
        stdout_so_far.push_str(&line);
        stdout_so_far.push('\n');
    }
    let stderr_so_far = stderr_task.await.unwrap_or_default();

    let mut report = format!("daemon exited during startup ({exit_status})");
    if !stdout_so_far.trim().is_empty() {
        report.push_str(&format!(
            "\n--- daemon stdout ---\n{}",
            stdout_so_far.trim_end()
        ));
    }
    if !stderr_so_far.trim().is_empty() {
        report.push_str(&format!(
            "\n--- daemon stderr ---\n{}",
            stderr_so_far.trim_end()
        ));
    }
    Err(report.into())
}

/// Send a task to the daemon and exit without opening the TUI.
pub async fn run_headless(message: String) -> Result<(), BoxError> {
    let stream = ensure_daemon_running().await?;
    let mut framed = Framed::new(stream, length_delimited_codec());

    /// Receive the next DaemonMessage from the framed socket.
    async fn recv(
        framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    ) -> Result<DaemonMessage, BoxError> {
        match framed.next().await {
            Some(Ok(bytes)) => Ok(serde_json::from_slice::<DaemonMessage>(&bytes)
                .expect("bug: failed to deserialize daemon message")),
            _ => Err("daemon disconnected".into()),
        }
    }

    // Read Welcome (must be first message).
    match recv(&mut framed).await? {
        DaemonMessage::Welcome { .. } => {}
        DaemonMessage::Error { text, .. } => return Err(text.into()),
        _ => return Err("expected Welcome from daemon".into()),
    }

    let cwd = std::env::current_dir()?;

    // Create a new session.
    let msg = ClientMessage::CreateSession {
        cwd,
        location: None,
        model: None,
    };
    framed.send(Bytes::from(serde_json::to_vec(&msg)?)).await?;

    // Wait for Connected to get the session ID.
    let session_id = loop {
        match recv(&mut framed).await? {
            DaemonMessage::Connected { session_id, .. } => break session_id,
            DaemonMessage::Error { text, .. } => return Err(text.into()),
            _ => continue, // skip SessionsUpdated, etc.
        }
    };

    // Send the user's message.
    let msg = ClientMessage::UserInput {
        session_id: session_id.clone(),
        text: message,
    };
    framed.send(Bytes::from(serde_json::to_vec(&msg)?)).await?;

    // Wait for the agent to start processing before disconnecting,
    // so the session isn't killed by the idle check. Also surface any
    // initialization errors.
    loop {
        match recv(&mut framed).await? {
            DaemonMessage::StartOutput { .. } => break,
            DaemonMessage::Error { text, .. } => return Err(text.into()),
            _ => continue,
        }
    }

    // Disconnect (agent keeps running in the background).
    let msg = ClientMessage::Disconnect;
    framed.send(Bytes::from(serde_json::to_vec(&msg)?)).await?;

    println!("Session {session_id} created — agent is running in the background.");
    println!("To connect: infinity --session '{session_id}'");

    Ok(())
}

/// Connect to the daemon over a unix socket (serialized framing).
pub async fn run_with_daemon(
    initial_message: Option<String>,
    session: Option<String>,
) -> Result<(), BoxError> {
    let stream = ensure_daemon_running().await?;
    let mut framed = Framed::new(stream, length_delimited_codec());

    let (to_daemon_tx, mut to_daemon_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (from_daemon_tx, from_daemon_rx) = mpsc::unbounded_channel::<DaemonMessage>();

    tokio::pin! {
        let client_fut = run_client(
            crate::term_io::CrosstermTerm::new(),
            crate::term_io::CrosstermEvents,
            std::env::current_dir().unwrap_or_default(),
            from_daemon_rx,
            to_daemon_tx,
            initial_message,
            session,
            None,
        );
    }

    loop {
        tokio::select! {
            msg = to_daemon_rx.recv() => {
                let Some(msg) = msg else { // intentional shut down, terminal disconnecting from daemon
                    drop(from_daemon_tx);
                    return client_fut.await;
                };
                let bytes = Bytes::from(serde_json::to_vec(&msg).expect("bug: failed to serialize daemon message"));
                if framed.send(bytes).await.is_err() {
                    tracing::error!("failed to send message to daemon, it may have disconnected");
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
                    let bytes = Bytes::from(serde_json::to_vec(&msg).expect("bug: failed to serialize daemon message"));
                    let _ = framed.send(bytes).await;
                }
                return client_res;
            }
            frame = framed.next() => {
                match frame {
                    Some(Ok(bytes)) => {
                        let msg = serde_json::from_slice::<DaemonMessage>(&bytes).expect("bug: failed to deserialize daemon message");
                        let _ = from_daemon_tx.send(msg);
                    }
                    Some(Err(e)) => {
                        tracing::error!("error reading from daemon socket: {e}");
                        drop(from_daemon_tx);
                        return client_fut.await;
                    }
                    None => {
                        // Daemon closed the socket — drop from_daemon_tx so
                        // run_client sees the channel close.
                        tracing::error!("Daemon closed the connection unexpectedly");
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
    session: Option<String>,
    startup_info: Option<String>,
) -> Result<(), BoxError> {
    run_client(
        crate::term_io::CrosstermTerm::new(),
        crate::term_io::CrosstermEvents,
        std::env::current_dir().unwrap_or_default(),
        from_daemon_rx,
        to_daemon_tx,
        initial_message,
        session,
        startup_info,
    )
    .await
}

/// Core client logic — works with channels regardless of transport, and any
/// terminal implementation. Production wrappers pass the crossterm-backed
/// terminal and the process cwd; tests substitute a virtual terminal,
/// scripted events, and a temp cwd.
#[expect(clippy::too_many_arguments, reason = "explicit client wiring")]
pub async fn run_client<T, E>(
    term: T,
    events: E,
    cwd: PathBuf,
    mut from_daemon: mpsc::UnboundedReceiver<DaemonMessage>,
    to_daemon: mpsc::UnboundedSender<ClientMessage>,
    initial_message: Option<String>,
    session: Option<String>,
    startup_info: Option<String>,
) -> Result<(), BoxError>
where
    T: TermOut + 'static,
    E: EventSource + 'static,
{
    // Read Welcome
    let welcome = from_daemon.recv().await.ok_or("daemon disconnected")?;
    let (model_name, context_window, sessions, available_models, provider_name) = match welcome {
        DaemonMessage::Welcome {
            default_model_name,
            default_context_window,
            sessions,
            available_models,
            provider_name,
            ..
        } => (
            default_model_name,
            default_context_window,
            sessions,
            available_models,
            provider_name,
        ),
        DaemonMessage::Error { text, .. } => return Err(text.into()),
        _ => return Err("expected Welcome from daemon".into()),
    };

    let (display_tx, display_rx) =
        mpsc::unbounded_channel::<(Option<String>, DisplayEvent<DaemonTokenUsage>)>();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<String>();
    let (load_session_tx, load_session_rx) = mpsc::unbounded_channel::<(Option<String>, bool)>();
    let (model_switch_tx, model_switch_rx) = mpsc::unbounded_channel::<usize>();
    let (session_tx, session_rx) = mpsc::unbounded_channel::<SessionChanged>();
    let (model_switched_tx, model_switched_rx) =
        mpsc::unbounded_channel::<crate::terminal::ModelSwitched>();
    let (sessions_updated_tx, sessions_updated_rx) =
        mpsc::unbounded_channel::<HashMap<String, SessionInfo>>();
    let (soft_detach_tx, soft_detach_rx) = mpsc::unbounded_channel::<()>();
    let (detach_result_tx, detach_result_rx) = mpsc::unbounded_channel::<DetachResult>();
    let (choice_answered_tx, choice_answered_rx) = mpsc::unbounded_channel::<(String, usize)>();

    if let Some(info) = startup_info {
        let _ = display_tx.send((None, DisplayEvent::Info(info)));
    }

    // If --session was provided, connect to it immediately (supports prefix matching).
    if let Some(ref session_id) = session {
        let matches: Vec<&String> = sessions
            .keys()
            .filter(|k| k.starts_with(session_id.as_str()))
            .collect();
        let resolved = match matches.len() {
            0 => return Err(format!("no session found matching prefix '{session_id}'").into()),
            1 => matches[0].clone(),
            _ => {
                return Err(format!(
                    "ambiguous session prefix '{session_id}' — matches: {}",
                    matches
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
        };
        to_daemon.send(ClientMessage::Connect {
            session_id: resolved,
            thread_id: None,
        })?;
    }

    let models_for_switch = available_models.clone();

    let mut terminal_handle = tokio::task::spawn_local(crate::terminal::run(
        term,
        events,
        input_tx,
        display_rx,
        model_name,
        provider_name,
        context_window,
        sessions,
        load_session_tx,
        model_switch_tx,
        available_models,
        initial_message,
        session_rx,
        model_switched_rx,
        sessions_updated_rx,
        soft_detach_tx,
        detach_result_rx,
        choice_answered_tx,
    ));

    let mut active_session: Option<String> = None;
    let mut pending_input: Vec<String> = Vec::new();
    // The model most recently selected via the model picker. Stored locally so
    // it can be passed when creating new sessions, even if no session is active.
    let mut selected_model: Option<ModelRef> = None;
    let mut terminal_result: Option<Result<Result<bool, BoxError>, tokio::task::JoinError>> = None;
    let mut pending_soft_detach = false;

    {
        // move into a local scope so they drop when the loop exits
        let mut input_rx = input_rx;
        let display_tx = display_tx;
        let mut load_session_rx = load_session_rx;
        let mut model_switch_rx = model_switch_rx;
        let session_tx = session_tx;
        let sessions_updated_tx = sessions_updated_tx;
        let mut soft_detach_rx = soft_detach_rx;
        let detach_result_tx = detach_result_tx;
        let mut choice_answered_rx = choice_answered_rx;

        loop {
            tokio::select! {
                biased;

                msg = from_daemon.recv() => {
                    let Some(msg) = msg else {
                        break;
                    };
                    match msg {
                        DaemonMessage::Connected { session_id, title, total_tokens_used, model_name, context_window, provider_id, .. } => {
                            active_session = Some(session_id.clone());
                            let _ = session_tx.send(SessionChanged { session_id, title, total_tokens_used, model_name, context_window, provider_id });
                            for text in pending_input.drain(..) {
                                let sid = active_session.as_ref().expect("bug: active_session should be set after Connected").clone();
                                let _ = to_daemon.send(ClientMessage::UserInput { session_id: sid, text });
                            }
                        }
                        DaemonMessage::ModelSwitched { thread_id, model_name, context_window, provider_id } => {
                            let _ = model_switched_tx.send(crate::terminal::ModelSwitched {
                                thread_id, model_name, provider_id, context_window,
                            });
                        }
                        DaemonMessage::Replay { history, pending_choices, in_progress, .. } => {
                            for m in history {
                                if let Some(evt) = daemon_msg_to_display(m) {
                                    let _ = display_tx.send(evt);
                                }
                            }
                            // End-of-replay marker: closes any open stream/spinner
                            // state. Deliberately carries no usage info so it does
                            // not clobber the total from the Connected message.
                            // Skipped when the response is still in progress — the
                            // spinner state implied by the end of the history (e.g.
                            // mid-thinking) is live, and more events will follow.
                            if !in_progress {
                                let _ = display_tx.send((None, DisplayEvent::ResponseDone(None)));
                            }
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
                            let _ = to_daemon.send(ClientMessage::Disconnect);
                        }
                    }

                    if shut_down_old && maybe_target.is_none() {
                        // When stopping (shut_down_old=true) with no target, keep active_session
                        // so the next user input resumes the same session.
                    } else {
                        active_session = None;
                    }

                    if let Some(target) = maybe_target {
                        let _ = to_daemon.send(ClientMessage::Connect { session_id: target, thread_id: None });
                    } // if none, will be created on next user input
                }

                idx = model_switch_rx.recv() => {
                    let Some(idx) = idx else { break };
                    if let Some(entry) = models_for_switch.get(idx) {
                        let model = ModelRef {
                            provider_id: entry.provider_id.clone(),
                            model_id: entry.model_id.clone(),
                        };
                        // Remember the selection locally so new sessions use it.
                        selected_model = Some(model.clone());
                        if let Some(sid) = &active_session {
                            let _ = to_daemon.send(ClientMessage::SwitchModel {
                                session_id: sid.clone(), model,
                            });
                        }
                    }
                }

                answered = choice_answered_rx.recv() => {
                    let Some((choice_id, selected)) = answered else { break; };
                    let _ = to_daemon.send(ClientMessage::UserChoiceAnswered { choice_id, selected });
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
                        } else if text == "__archive__" {
                            let _ = to_daemon.send(ClientMessage::ArchiveSession { session_id: sid.clone() });
                            active_session = None;
                        } else {
                            let _ = to_daemon.send(ClientMessage::UserInput { session_id: sid.clone(), text });
                        }
                    } else {
                        pending_input.push(text);
                        let _ = to_daemon.send(ClientMessage::CreateSession { cwd: cwd.clone(), location: None, model: selected_model.clone() });
                    }
                }
            }
        }
    }

    let result = match terminal_result {
        Some(r) => r,
        None => terminal_handle.await,
    };
    if let Err(ref e) = result {
        if e.is_panic() {
            tracing::error!("terminal task panicked: {e}");
        } else {
            tracing::warn!("terminal task cancelled: {e}");
        }
    }
    let keep_running = matches!(result, Ok(Ok(true)));

    if let Some(sid) = active_session {
        if keep_running {
            let _ = to_daemon.send(ClientMessage::Disconnect);
        } else {
            let _ = to_daemon.send(ClientMessage::ShutdownSession { session_id: sid });
        }
    }

    Ok(())
}
