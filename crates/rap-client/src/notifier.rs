/// Best-effort lifecycle notifications to RAP tool servers.
use crate::http::HttpClient;

/// Sends best-effort notifications to all configured RAP tool servers.
#[derive(Clone)]
pub struct RapNotifier<H: HttpClient> {
    server_urls: Vec<String>,
    client: H,
}

impl<H: HttpClient> RapNotifier<H> {
    /// Create a notifier for the given server URLs.
    pub fn new(server_urls: Vec<String>, client: H) -> Self {
        Self {
            server_urls,
            client,
        }
    }

    /// Best-effort notification that a thread has been closed.
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
