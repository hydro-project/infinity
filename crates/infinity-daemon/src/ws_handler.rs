use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use infinity_protocol::ClientMessage;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::client_handler::handle_client_channels;
use crate::session::SessionManager;
use crate::web_assets;

/// Handle a TCP connection that may be an HTTP request or a WebSocket upgrade.
pub async fn handle_http_client(stream: TcpStream, session_manager: Arc<Mutex<SessionManager>>) {
    let io = TokioIo::new(stream);
    let mgr = session_manager.clone();

    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
        let mgr = mgr.clone();
        async move { handle_request(req, mgr) }
    });

    if let Err(e) = http1::Builder::new()
        .serve_connection(io, service)
        .with_upgrades()
        .await
    {
        tracing::debug!("http connection error: {e}");
    }
}

fn handle_request(
    req: Request<Incoming>,
    session_manager: Arc<Mutex<SessionManager>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Check for WebSocket upgrade
    let is_upgrade = req
        .headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    if is_upgrade {
        let key = req
            .headers()
            .get("Sec-WebSocket-Key")
            .expect("bug: missing Sec-WebSocket-Key on upgrade request")
            .clone();
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let response = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "Upgrade")
            .header("Sec-WebSocket-Accept", accept)
            .body(Full::new(Bytes::new()))
            .expect("bug: failed to build upgrade response");

        tokio::task::spawn_local(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let ws = WebSocketStream::from_raw_socket(
                        TokioIo::new(upgraded),
                        Role::Server,
                        None,
                    )
                    .await;
                    run_ws_loop(ws, session_manager).await;
                }
                Err(e) => tracing::warn!("websocket upgrade failed: {e}"),
            }
        });

        return Ok(response);
    }

    // Serve static files
    let path = req.uri().path();
    if let Some((content_type, body)) = web_assets::serve_static(path) {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, content_type)
            .body(Full::new(Bytes::from_static(body)))
            .expect("bug: failed to build static response"));
    }

    // SPA fallback: serve index.html for non-asset paths
    if !path.contains('.')
        && let Some((content_type, body)) = web_assets::serve_static("/")
    {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, content_type)
            .body(Full::new(Bytes::from_static(body)))
            .expect("bug: failed to build fallback response"));
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from_static(b"not found")))
        .expect("bug: failed to build 404 response"))
}

/// WebSocket message loop, shared between upgrade-based and direct connections.
async fn run_ws_loop<S>(ws_stream: WebSocketStream<S>, session_manager: Arc<Mutex<SessionManager>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
