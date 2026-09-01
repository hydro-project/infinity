//! HTTP callback routing for RAP tool results.
//!
//! The reusable bridge decodes callbacks, assigns deduplication IDs, and
//! forwards agent inputs straight into the agent system's input queue.
//! Admission is owned by the router: its thread-existence and stopped-thread
//! checks refuse events for unknown or user-stopped sessions before any
//! driver spawns. The only
//! callback content the daemon handles itself is display-only view updates,
//! which never enter agent history.

use tokio::sync::Mutex;

use infinity_rap_bridge::RapCallbackBridge;

use crate::session::{SessionManager, SharedSessionManager};

/// Start the callback accept loop for an already-built [`SessionManager`]
/// (whose `callback_url` must match the bridge's). Agent inputs flow into
/// the session manager's input sender; view updates are persisted and
/// broadcast to subscribers.
pub fn serve_callbacks(
    bridge: RapCallbackBridge,
    session_manager: SessionManager,
) -> SharedSessionManager {
    let (mut views, _callback_server_task) = bridge.serve_into(session_manager.input_sender());
    let session_manager = SharedSessionManager::new(Mutex::new(session_manager));

    let sm = session_manager.clone();
    tokio::task::spawn_local(async move {
        while let Some(update) = views.recv().await {
            tracing::info!(
                "RAP view_update: type={} group={}",
                update.view_type,
                update.group_id
            );
            let manager = sm.lock().await;
            manager.handle_view_update(update.group_id.as_str(), &update.view_type, update.content);
        }
    });

    session_manager
}
