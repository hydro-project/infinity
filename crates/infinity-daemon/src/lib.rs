pub mod client_handler;
pub mod config;
pub mod mcp_proxy;
pub mod memory_store;
pub mod model_picker;
pub mod rap_callback;
pub mod rap_tools;
pub mod session;
pub mod session_store;
pub mod set_title_tool;
pub mod sleep_tools;

use std::sync::Arc;

use infinity_protocol::socket_path;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
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

    let session_manager = Arc::new(Mutex::new(
        session::SessionManager::new(infinity_protocol::state_dir()).await?,
    ));

    let shutdown = async {
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
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
                        tokio::task::spawn_local(client_handler::handle_client(stream, mgr));
                    }
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
                    }
                }
            }
        } => {}
        _ = shutdown => {}
    }

    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&infinity_protocol::pid_path());

    let mut mgr = session_manager.lock().await;
    let session_ids: Vec<String> = mgr.sessions.keys().cloned().collect();
    for sid in session_ids {
        mgr.cleanup_session(&sid).await;
    }

    tracing::info!("daemon shut down");
    Ok(())
}
