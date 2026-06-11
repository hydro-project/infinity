pub mod client_handler;
pub mod config;
pub mod mcp_proxy;
pub mod memory_store;
pub mod migrate;
pub mod models;
pub mod rap_callback;
pub mod rap_tools;
pub mod remote;
pub mod session;
pub mod session_store;
pub mod set_title_tool;
pub mod sleep_tools;
pub mod web_assets;
pub mod ws_handler;

use infinity_protocol::socket_path;
use tokio::net::{TcpListener, UnixListener};
use tracing_subscriber::EnvFilter;

pub async fn run_daemon() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state_dir = infinity_protocol::state_dir();
    std::fs::create_dir_all(&state_dir)?;

    let log_file = std::fs::File::create(state_dir.join("daemon.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    let sock_path = socket_path();
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }

    let listener = UnixListener::bind(&sock_path)?; // TODO(shadaj): do we also need to lock the file?
    tracing::info!("daemon listening on {}", sock_path.display());

    let pid_path = infinity_protocol::pid_path();
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let session_manager = rap_callback::start_callback_server(infinity_protocol::state_dir())
        .await
        .map_err(|e| format!("Failed to start callback server: {e}"))?;
    tracing::info!("shared callback server started");

    // Initialize remote daemon connections
    let remote_configs = remote::load_remotes_config();
    if !remote_configs.is_empty() {
        tracing::info!("loaded {} remote daemon config(s)", remote_configs.len());
        session_manager.lock().await.init_remotes(remote_configs);
    }

    // Start WebSocket server
    let ws_port: u16 = std::env::var("INFINITY_WS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let ws_bind_addr = std::env::var("INFINITY_WS_BIND").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let ws_listener = TcpListener::bind((&*ws_bind_addr, ws_port)).await?;
    tracing::info!("websocket server listening on {ws_bind_addr}:{ws_port}");

    let ws_session_manager = session_manager.clone();
    let ws_accept = async move {
        loop {
            match ws_listener.accept().await {
                Ok((stream, _)) => {
                    let mgr = ws_session_manager.clone();
                    tokio::task::spawn_local(rap_protocol::log_panic(
                        "http_client_handler",
                        ws_handler::handle_http_client(stream, mgr),
                    ));
                }
                Err(e) => {
                    tracing::warn!("ws accept error: {e}");
                }
            }
        }
    };

    let shutdown = async {
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to register SIGINT handler");
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        tracing::info!("received shutdown signal");
    };

    tokio::select! {
        _ = async {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let mgr = session_manager.clone();
                        tokio::task::spawn_local(rap_protocol::log_panic("client_handler", client_handler::handle_client(stream, mgr)));
                    }
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
                    }
                }
            }
        } => {}
        _ = ws_accept => {}
        _ = shutdown => {}
    }

    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(infinity_protocol::pid_path());

    let mut mgr = session_manager.lock().await;
    let session_ids: Vec<String> = mgr.sessions.keys().cloned().collect();
    for sid in session_ids {
        mgr.cleanup_session(&sid).await;
    }

    tracing::info!("daemon shut down");
    Ok(())
}
