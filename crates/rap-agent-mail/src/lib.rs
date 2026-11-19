#![doc = include_str!("../README.md")]

pub mod address;
pub mod config;
mod link;
pub mod mailbox;
mod server;
pub mod store;
pub mod wire;

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

pub use config::MailConfig;
pub use mailbox::Mailbox;

/// A running mailbox. Dropping it stops every task (RAP server, peer
/// listener, dialing loops and their SSH tunnels).
pub struct Running {
    pub mailbox: Arc<Mailbox>,
    /// Base URL of the RAP server (`http://127.0.0.1:<port>`).
    pub rap_url: String,
    _tasks: tokio::task::JoinSet<()>,
}

/// Start a mailbox: serve RAP on `rap_listener`, accept peers on
/// `<state_dir>/peer.sock`, and dial every configured peer.
/// Must be called inside a Tokio runtime.
pub fn start(config: MailConfig, rap_listener: TcpListener) -> Result<Running, String> {
    use std::os::unix::fs::PermissionsExt;
    let state_dir = config.state_dir.clone();
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| format!("failed to create {}: {e}", state_dir.display()))?;
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("failed to restrict {}: {e}", state_dir.display()))?;
    let peer_listener = link::bind_peer_socket(&state_dir)?;
    let rap_url = format!(
        "http://127.0.0.1:{}",
        rap_listener
            .local_addr()
            .map_err(|e| format!("RAP listener has no address: {e}"))?
            .port()
    );

    let peers = config.peers.clone();
    let mailbox = Arc::new(Mailbox::new(config));
    let mut tasks = tokio::task::JoinSet::new();

    let app = server::router(mailbox.clone());
    tasks.spawn(async move {
        if let Err(e) = axum::serve(rap_listener, app).await {
            tracing::error!("RAP server failed: {e}");
        }
    });
    tasks.spawn(link::serve_peers(mailbox.clone(), peer_listener));
    for (peer, transport) in peers {
        tasks.spawn(link::dial_peer(mailbox.clone(), peer, transport));
    }
    let maintained = mailbox.clone();
    tasks.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            maintained.maintenance().await;
        }
    });

    tracing::info!(
        "mailbox `{}` serving RAP at {rap_url}, peers at {}",
        mailbox.name(),
        state_dir.join("peer.sock").display()
    );
    Ok(Running {
        mailbox,
        rap_url,
        _tasks: tasks,
    })
}
