//! Daemon I/O: manages per-thread connections to the Infinity daemon.
//!
//! Inbound: [`DaemonEvent`]s — `(thread_ts, DaemonMessage)` from all active
//! connections. Outbound: [`DaemonCommand`]s — instructions to
//! create/connect/send on daemon connections.

use std::collections::HashMap;

use infinity_protocol::DaemonMessage;
use infinity_slack_dataflow::daemon::{DaemonCommand, DaemonEvent};
use tokio::sync::mpsc;

use crate::daemon_client::DaemonClient;

/// Endpoints and task handle returned by [`spawn`].
pub struct DaemonIo {
    /// Daemon messages from all active connections; the bot wraps this in a
    /// stream and feeds it to the embedded dataflow.
    pub events: mpsc::Receiver<DaemonEvent>,
    /// Commands produced by the dataflow; the dispatch task acts on them. The
    /// channel is unbounded because the dataflow emits commands from a
    /// synchronous callback during a tick (it cannot await backpressure), and
    /// command volume is bounded by human-scale chat traffic.
    pub commands: mpsc::UnboundedSender<DaemonCommand>,
    /// Handle of the command-dispatch task. This task never finishes during
    /// normal operation (the command channel stays open for the bot's
    /// lifetime); completion means a panic or bug and should be treated as
    /// fatal by the caller.
    pub dispatch: tokio::task::JoinHandle<()>,
}

/// Spawns the daemon I/O task and returns the dataflow-facing endpoints.
pub fn spawn() -> DaemonIo {
    let (to_df_tx, to_df_rx) = mpsc::channel::<DaemonEvent>(1024);
    let (from_df_tx, mut from_df_rx) = mpsc::unbounded_channel::<DaemonCommand>();

    let dispatch = tokio::spawn(async move {
        // Map of thread_ts → sender half of the daemon client.
        // Each connection's receiver is forwarded to `to_df_tx` by a spawned task.
        let mut connections: HashMap<String, DaemonClient> = HashMap::new();

        while let Some(cmd) = from_df_rx.recv().await {
            tracing::info!("daemon I/O task received command: {cmd:?}");
            match cmd {
                DaemonCommand::CreateSession {
                    thread_ts,
                    cwd,
                    model,
                } => match DaemonClient::connect().await {
                    Ok(daemon) => {
                        if let Err(e) = daemon.create_session(cwd, model).await {
                            tracing::error!("CreateSession failed for {thread_ts}: {e}");
                            continue;
                        }
                        spawn_receiver(thread_ts.clone(), &mut connections, daemon, &to_df_tx);
                    }
                    Err(e) => {
                        tracing::error!("daemon connect failed for {thread_ts}: {e}");
                    }
                },
                DaemonCommand::ConnectSession {
                    thread_ts,
                    session_id,
                } => match DaemonClient::connect().await {
                    Ok(daemon) => {
                        if let Err(e) = daemon.connect_session(&session_id, None).await {
                            tracing::error!("ConnectSession failed for {thread_ts}: {e}");
                            continue;
                        }
                        spawn_receiver(thread_ts.clone(), &mut connections, daemon, &to_df_tx);
                    }
                    Err(e) => {
                        tracing::error!("daemon connect failed for {thread_ts}: {e}");
                    }
                },
                DaemonCommand::SendInput {
                    thread_ts,
                    session_id,
                    text,
                } => {
                    if !connections.contains_key(&thread_ts) {
                        // Reconnect after restart: establish a new daemon connection
                        // and attach to the existing session.
                        match DaemonClient::connect().await {
                            Ok(daemon) => {
                                if let Err(e) = daemon.connect_session(&session_id, None).await {
                                    tracing::error!(
                                        "reconnect to session failed for {thread_ts}: {e}"
                                    );
                                    continue;
                                }
                                spawn_receiver(
                                    thread_ts.clone(),
                                    &mut connections,
                                    daemon,
                                    &to_df_tx,
                                );
                            }
                            Err(e) => {
                                tracing::error!("daemon connect failed for {thread_ts}: {e}");
                                continue;
                            }
                        }
                    }
                    if let Some(daemon) = connections.get(&thread_ts) {
                        if let Err(e) = daemon.send_input(&session_id, &text).await {
                            tracing::warn!("SendInput failed for {thread_ts}: {e}, reconnecting");
                            connections.remove(&thread_ts);
                            // Reconnect and retry.
                            match DaemonClient::connect().await {
                                Ok(daemon) => {
                                    if let Err(e) = daemon.connect_session(&session_id, None).await
                                    {
                                        tracing::error!(
                                            "reconnect to session failed for {thread_ts}: {e}"
                                        );
                                        continue;
                                    }
                                    let retry_result = daemon.send_input(&session_id, &text).await;
                                    spawn_receiver(
                                        thread_ts.clone(),
                                        &mut connections,
                                        daemon,
                                        &to_df_tx,
                                    );
                                    if let Err(e) = retry_result {
                                        tracing::error!(
                                            "SendInput retry failed for {thread_ts}: {e}"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("daemon reconnect failed for {thread_ts}: {e}");
                                }
                            }
                        }
                    }
                }
                DaemonCommand::AnswerChoice {
                    thread_ts,
                    choice_id,
                    selected,
                } => {
                    if let Some(daemon) = connections.get(&thread_ts) {
                        if let Err(e) = daemon.answer_choice(&choice_id, selected).await {
                            tracing::error!("AnswerChoice failed for {thread_ts}: {e}");
                        }
                    } else {
                        tracing::warn!("AnswerChoice: no connection for thread {thread_ts}");
                    }
                }
            }
        }
    });

    DaemonIo {
        events: to_df_rx,
        commands: from_df_tx,
        dispatch,
    }
}

/// Spawn a task that forwards DaemonMessages from a connection into the dataflow.
/// On `Connected`, automatically sends any pending input text for this thread.
/// Intercepts the initial `Welcome` message to update available models in the runtime.
fn spawn_receiver(
    thread_ts: String,
    connections: &mut HashMap<String, DaemonClient>,
    mut daemon: DaemonClient,
    to_df_tx: &mpsc::Sender<DaemonEvent>,
) {
    let rx = std::mem::replace(&mut daemon.rx, mpsc::channel(1).1);
    let tx_half = daemon;

    let to_df = to_df_tx.clone();
    let ts = thread_ts.clone();
    let tx_for_input = tx_half.tx.clone();
    tokio::spawn(async move {
        let mut rx = rx;

        // The daemon sends a Welcome message as the first message on every
        // connection. Intercept it to capture the available models list.
        if let Some(first_msg) = rx.recv().await {
            if let DaemonMessage::Welcome {
                available_models, ..
            } = &first_msg
            {
                let rt = infinity_slack_dataflow::runtime::get();
                let mut models = rt.available_models.lock().expect("bug: lock poisoned");
                *models = available_models.clone();
                tracing::info!(
                    "updated available models from daemon Welcome ({} models)",
                    models.len()
                );
            } else {
                // Not a Welcome — forward it normally.
                if let DaemonMessage::Connected { ref session_id, .. } = first_msg {
                    let rt = infinity_slack_dataflow::runtime::get();
                    let pending_text = {
                        let mut pending = rt.pending_input.lock().expect("bug: lock poisoned");
                        pending.remove(&ts)
                    };
                    if let Some(text) = pending_text {
                        let _ = tx_for_input
                            .send(infinity_protocol::ClientMessage::UserInput {
                                session_id: session_id.clone(),
                                text,
                            })
                            .await;
                    }
                }

                if to_df
                    .send(DaemonEvent {
                        thread_ts: ts.clone(),
                        message: first_msg,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        while let Some(msg) = rx.recv().await {
            // On Connected, send pending input automatically.
            if let DaemonMessage::Connected { ref session_id, .. } = msg {
                let rt = infinity_slack_dataflow::runtime::get();
                let pending_text = {
                    let mut pending = rt.pending_input.lock().expect("bug: lock poisoned");
                    pending.remove(&ts)
                };
                if let Some(text) = pending_text {
                    let _ = tx_for_input
                        .send(infinity_protocol::ClientMessage::UserInput {
                            session_id: session_id.clone(),
                            text,
                        })
                        .await;
                }
            }

            // Skip Welcome messages that arrive later (e.g. after reconnect).
            if matches!(msg, DaemonMessage::Welcome { .. }) {
                continue;
            }

            if to_df
                .send(DaemonEvent {
                    thread_ts: ts.clone(),
                    message: msg,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    connections.insert(thread_ts, tx_half);
}
