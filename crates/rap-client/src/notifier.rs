/// Best-effort lifecycle notifications to RAP tool servers.
use std::collections::HashMap;
use std::sync::Arc;

use crate::http::HttpClient;

/// Where a notifier finds the servers to notify.
#[derive(Clone)]
enum ServerUrls {
    Fixed(Vec<String>),
    /// Looked up per notification, for servers whose address can change
    /// while a notifier is held (a long-lived server restarted on a new
    /// port).
    Live(Arc<dyn Fn() -> Vec<String> + Send + Sync>),
}

/// Sends best-effort notifications to all configured RAP tool servers.
#[derive(Clone)]
pub struct RapNotifier<H: HttpClient> {
    server_urls: ServerUrls,
    client: H,
}

impl<H: HttpClient> RapNotifier<H> {
    /// Create a notifier for the given server URLs.
    pub fn new(server_urls: Vec<String>, client: H) -> Self {
        Self {
            server_urls: ServerUrls::Fixed(server_urls),
            client,
        }
    }

    /// Create a notifier that asks `server_urls` for the current server URLs
    /// each time it notifies.
    pub fn new_live(
        server_urls: impl Fn() -> Vec<String> + Send + Sync + 'static,
        client: H,
    ) -> Self {
        Self {
            server_urls: ServerUrls::Live(Arc::new(server_urls)),
            client,
        }
    }

    fn urls(&self) -> Vec<String> {
        match &self.server_urls {
            ServerUrls::Fixed(urls) => urls.clone(),
            ServerUrls::Live(source) => source(),
        }
    }

    /// Best-effort notification that a thread has been closed.
    pub async fn notify_thread_closed(&self, thread_id: &rap_protocol::ThreadId<str>) {
        let payload = serde_json::json!({ "thread_id": thread_id }).to_string();
        for url in &self.urls() {
            let endpoint = format!("{}/close_thread", url.trim_end_matches('/'));
            match self.client.post(&endpoint, &payload).await {
                Ok(status) => {
                    tracing::info!("Notified {} of thread close (status: {})", endpoint, status);
                }
                Err(e) => {
                    tracing::warn!("Failed to notify {} of thread close: {}", endpoint, e);
                }
            }
        }
    }

    /// Best-effort notification that a tool call has been cancelled.
    pub async fn notify_tool_cancelled(
        &self,
        thread_id: &rap_protocol::ThreadId<str>,
        tool_call_id: &rap_protocol::ToolCallId<str>,
    ) {
        let payload = serde_json::json!({
            "thread_id": thread_id,
            "tool_call_id": tool_call_id,
        })
        .to_string();
        for url in &self.urls() {
            let endpoint = format!("{}/cancel_tool_call", url.trim_end_matches('/'));
            match self.client.post(&endpoint, &payload).await {
                Ok(status) => {
                    tracing::info!(
                        "Notified {} of tool cancellation (status: {})",
                        endpoint,
                        status
                    );
                }
                Err(e) => {
                    tracing::warn!("Failed to notify {} of tool cancellation: {}", endpoint, e);
                }
            }
        }
    }

    /// Request RAP servers to migrate their state to destination servers.
    /// `servers_needing_migration` is a list of `(config_id, source_url)` pairs.
    /// `destination_urls` maps config ID → destination server URL.
    pub async fn request_migration(
        &self,
        session_id: &rap_protocol::ThreadId<str>,
        servers_needing_migration: &[(String, String)],
        destination_urls: &HashMap<String, String>,
    ) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        for (id, url) in servers_needing_migration {
            let endpoint = format!("{}/migrate", url.trim_end_matches('/'));
            let dest_url = destination_urls.get(id).cloned().unwrap_or_default();
            let payload = serde_json::json!({
                "session_id": session_id,
                "destination_url": dest_url,
            })
            .to_string();
            match self.client.post(&endpoint, &payload).await {
                Ok(status) if (200..300).contains(&status) => {
                    tracing::info!(
                        "Migration request to {} succeeded (status: {})",
                        endpoint,
                        status
                    );
                }
                Ok(status) => {
                    errors.push(format!(
                        "Migration to {} returned status {}",
                        endpoint, status
                    ));
                }
                Err(e) => {
                    errors.push(format!("Migration to {} failed: {}", endpoint, e));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
