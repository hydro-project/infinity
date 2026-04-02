use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use infinity_protocol::ClientMessage;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::client_handler::handle_client_channels;
use crate::session::SessionManager;

/// Handle a single WebSocket client connection using JSON serialization.
pub async fn handle_ws_client(stream: TcpStream, session_manager: Arc<Mutex<SessionManager>>) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!("websocket handshake failed: {e}");
            return;
        }
    };

    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    let (client_msg_tx, client_msg_rx) = mpsc::unbounded_channel();
    let (daemon_msg_tx, mut daemon_msg_rx) = mpsc::unbounded_channel();

    let mgr = session_manager.clone();
    tokio::pin! {
        let handler = handle_client_channels(client_msg_rx, daemon_msg_tx, mgr);
    }

    loop {
        tokio::select! {
            msg = daemon_msg_rx.recv() => {
                let Some(msg) = msg else { break };
                let json = serde_json::to_string(&msg).expect("bug: failed to serialize DaemonMessage");
                if ws_tx.send(Message::Text(json.into())).await.is_err() { break; }
            }
            _ = &mut handler => {
                return;
            }
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(msg) = serde_json::from_str::<ClientMessage>(&text) else { continue };
                        let _ = client_msg_tx.send(msg);
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => continue,
                }
            }
        }
    }

    drop(client_msg_tx);
    handler.await;
}
