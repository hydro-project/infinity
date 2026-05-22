//! Daemon sidecar: manages per-thread daemon connections.
//!
//! Inbound: `(String, DaemonMessage)` — (thread_ts, msg) from all active connections.
//! Outbound: `DaemonCommand` — instructions to create/connect/send on daemon connections.

use std::collections::HashMap;
use std::path::PathBuf;

use infinity_protocol::DaemonMessage;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::PollSender;

use crate::daemon_client::DaemonClient;

/// A command sent from the dataflow to the daemon sidecar.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum DaemonCommand {
    /// Create a new session for this thread.
    CreateSession { thread_ts: String, cwd: PathBuf },
    /// Connect to an existing session.
    ConnectSession {
        thread_ts: String,
        session_id: String,
    },
    /// Send user input on the connection for this thread.
    SendInput {
        thread_ts: String,
        session_id: String,
        text: String,
    },
    /// Answer a choice prompt on the connection for this thread.
    AnswerChoice {
        thread_ts: String,
        choice_id: String,
        selected: usize,
    },
}

/// A message received from a daemon connection, tagged with the thread it belongs to.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DaemonEvent {
    pub thread_ts: String,
    pub message: DaemonMessage,
}

/// Creates the daemon sidecar for use with Hydro's `sidecar_bidi`.
///
/// - Inbound stream: `DaemonEvent` items from all active daemon connections.
/// - Outbound sink: `DaemonCommand` items instructing the sidecar to act.
pub fn create() -> (ReceiverStream<DaemonEvent>, PollSender<DaemonCommand>) {
    let (to_df_tx, to_df_rx) = mpsc::channel::<DaemonEvent>(1024);
    let (from_df_tx, mut from_df_rx) = mpsc::channel::<DaemonCommand>(1024);

    tokio::spawn(async move {
        // Map of thread_ts → sender half of the daemon client.
        // Each connection's receiver is forwarded to `to_df_tx` by a spawned task.
        let mut connections: HashMap<String, DaemonClient> = HashMap::new();

        while let Some(cmd) = from_df_rx.recv().await {
            tracing::info!("daemon sidecar received command: {cmd:?}");
            match cmd {
                DaemonCommand::CreateSession { thread_ts, cwd } => {
                    match DaemonClient::connect().await {
                        Ok(daemon) => {
                            if let Err(e) = daemon.create_session(cwd).await {
                                tracing::error!("CreateSession failed for {thread_ts}: {e}");
                                continue;
                            }
                            spawn_receiver(thread_ts.clone(), &mut connections, daemon, &to_df_tx);
                        }
                        Err(e) => {
                            tracing::error!("daemon connect failed for {thread_ts}: {e}");
                        }
                    }
                }
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

    (ReceiverStream::new(to_df_rx), PollSender::new(from_df_tx))
}

/// Spawn a task that forwards DaemonMessages from a connection into the dataflow.
/// On `Connected`, automatically sends any pending input text for this thread.
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
        while let Some(msg) = rx.recv().await {
            // On Connected, send pending input automatically.
            if let DaemonMessage::Connected { ref session_id, .. } = msg {
                let rt = crate::runtime::get();
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
