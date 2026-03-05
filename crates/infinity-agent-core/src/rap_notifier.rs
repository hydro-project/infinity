//! Unified RAP notifier that sends best-effort notifications to RAP tool servers.
//!
//! Generic over an HTTP client so it works with both the plain reqwest client
//! (CLI) and the SigV4-signed client (Lambda).

use crate::traits::HttpClient;

/// Sends best-effort notifications to all configured RAP tool servers.
///
/// This replaces the separate `ThreadCloseNotifier` and `ToolCancelNotifier`
/// traits with a single concrete type that is generic over the HTTP client.
#[derive(Clone)]
pub struct RapNotifier<H: HttpClient> {
    server_urls: Vec<String>,
    client: H,
}

impl<H: HttpClient> RapNotifier<H> {
    pub fn new(server_urls: Vec<String>, client: H) -> Self {
        Self {
            server_urls,
            client,
        }
    }

    /// Best-effort notification that a thread has been closed.
    ///
    /// POSTs `{"thread_id":"…"}` to each server's `/close_thread` endpoint.
    /// Failures are logged but never propagated.
    pub async fn notify_thread_closed(&self, thread_id: &str) {
        let payload = serde_json::json!({ "thread_id": thread_id }).to_string();
        for url in &self.server_urls {
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
    ///
    /// POSTs `{"thread_id":"…","tool_call_id":"…"}` to each server's
    /// `/cancel_tool_call` endpoint. Failures are logged but never propagated.
    pub async fn notify_tool_cancelled(&self, thread_id: &str, tool_call_id: &str) {
        let payload = serde_json::json!({
            "thread_id": thread_id,
            "tool_call_id": tool_call_id,
        })
        .to_string();
        for url in &self.server_urls {
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
}
