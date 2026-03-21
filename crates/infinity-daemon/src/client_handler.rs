use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use infinity_protocol::{ClientMessage, DaemonMessage};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::session::SessionManager;

/// Handle a client over a unix socket (serialized framing).
pub async fn handle_client(stream: UnixStream, session_manager: Arc<Mutex<SessionManager>>) {
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
    let (client_msg_tx, client_msg_rx) = mpsc::unbounded_channel();
    let (daemon_msg_tx, mut daemon_msg_rx) = mpsc::unbounded_channel();

    // Spawn the core handler
    let mgr = session_manager.clone();
    tokio::pin! {
        let handler = handle_client_channels(client_msg_rx, daemon_msg_tx, mgr);
    }

    // Bridge: framed socket <-> channels
    loop {
        tokio::select! {
            msg = daemon_msg_rx.recv() => {
                let Some(msg) = msg else { break };
                let bytes = Bytes::from(bincode::serialize(&msg).unwrap());
                if framed.send(bytes).await.is_err() { break; }
            }
            _ = &mut handler => {
                return;
            }
            frame = framed.next() => {
                let Some(Ok(bytes)) = frame else { break };
                let Ok(msg) = bincode::deserialize::<ClientMessage>(&bytes) else { continue };
                client_msg_tx.send(msg).unwrap();
            }
        }
    }

    drop(client_msg_tx);

    handler.await;
}

/// Handle a client over raw mpsc channels (for in-memory mode, no serialization).
pub async fn handle_client_channels(
    mut client_rx: mpsc::UnboundedReceiver<ClientMessage>,
    daemon_tx: mpsc::UnboundedSender<DaemonMessage>,
    session_manager: Arc<Mutex<SessionManager>>,
) {
    let (client_tx, mut client_tx_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    let mut attached_session_id: Option<String> = None;

    // Send Welcome immediately
    {
        let mgr = session_manager.lock().await;
        let _ = daemon_tx.send(DaemonMessage::Welcome {
            sessions: mgr.list_sessions(Some(daemon_tx.clone())).await,
            available_models: mgr
                .available_models
                .iter()
                .map(|m| infinity_protocol::ModelInfo {
                    display_name: m.display_name.clone(),
                    model_id: m.model_id.clone(),
                    context_window: m.context_window,
                })
                .collect(),
            default_model_name: mgr.default_model_name.clone(),
            default_context_window: mgr.default_context_window,
            provider_name: "bedrock".to_string(),
        });
    }

    loop {
        tokio::select! {
            // Forward session display events to client
            msg = client_tx_rx.recv() => {
                let Some(msg) = msg else { break };
                if daemon_tx.send(msg).is_err() { break; }
            }
            // Handle client messages
            msg = client_rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    ClientMessage::CreateSession { cwd } => {
                        let mut mgr = session_manager.lock().await;
                        let mut emit = async |msg: DaemonMessage| {
                            let _ = daemon_tx.send(msg);
                        };
                        match mgr.create_session(&cwd, client_tx.clone(), &mut emit).await {
                            Ok(sid) => { attached_session_id = Some(sid); }
                            Err(e) => { let _ = daemon_tx.send(DaemonMessage::Error(format!("failed to create session: {e}"))); }
                        }
                    }
                    ClientMessage::Connect { session_id } => {
                        let mut mgr = session_manager.lock().await;
                        let mut emit = async |msg: DaemonMessage| {
                            let _ = daemon_tx.send(msg);
                        };
                        match mgr.resume_session(&session_id, client_tx.clone(), &mut emit).await {
                            Ok(()) => {
                                mgr.attach_client(&session_id, client_tx.clone()).await;
                                attached_session_id = Some(session_id);
                            }
                            Err(e) => { let _ = daemon_tx.send(DaemonMessage::Error(format!("failed to resume session: {e}"))); }
                        }
                    }
                    ClientMessage::UserInput { session_id, text } => {
                        let mut mgr = session_manager.lock().await;
                        if !mgr.send_input(&session_id, text, client_tx.clone()).await {
                            let _ = daemon_tx.send(DaemonMessage::Error(format!("session {} not found", session_id)));
                        }
                    }
                    ClientMessage::Disconnect { session_id } => {
                        let mut mgr = session_manager.lock().await;
                        mgr.detach_client(&session_id);
                        attached_session_id = None;
                    }
                    ClientMessage::SoftDetach { session_id } => {
                        let mut mgr = session_manager.lock().await;
                        if mgr.is_session_idle(&session_id) {
                            mgr.detach_client(&session_id);
                            attached_session_id = None;
                            // Break out — the daemon_tx drop signals the client
                            break;
                        } else {
                            let _ = daemon_tx.send(DaemonMessage::DisconnectNotIdle);
                        }
                    }
                    ClientMessage::ShutdownSession { session_id } => {
                        let mut mgr = session_manager.lock().await;
                        mgr.cleanup_session(&session_id).await;
                        attached_session_id = None;
                    }
                    ClientMessage::LoadSession { target_session_id } => {
                        let mut mgr = session_manager.lock().await;
                        let mut emit = async |msg: DaemonMessage| {
                            let _ = daemon_tx.send(msg);
                        };
                        match mgr.resume_session(&target_session_id, client_tx.clone(), &mut emit).await {
                            Ok(()) => {
                                mgr.attach_client(&target_session_id, client_tx.clone()).await;
                                attached_session_id = Some(target_session_id);
                            }
                            Err(e) => { let _ = daemon_tx.send(DaemonMessage::Error(format!("failed to resume session: {e}"))); }
                        }
                    }
                    ClientMessage::SwitchModel { .. } => {
                        let _ = daemon_tx.send(DaemonMessage::Info("Model switching not yet implemented".to_string()));
                    }
                }
            }
        }
    }

    tracing::trace!("Shutting down client handler");
    if let Some(sid) = attached_session_id {
        tracing::trace!("Cleaning up active session");
        let mut mgr = session_manager.lock().await;
        mgr.cleanup_session(&sid).await;
    }
}
