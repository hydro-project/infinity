/// Best-effort lifecycle notifications to RAP tool servers.
use std::collections::HashMap;

use rap_protocol::{RapToolCallStatusRequest, RapToolCallStatusResponse};

use crate::http::HttpClient;

/// Liveness of a tool call (or its subscription) as reported by a tool server
/// via the `/tool_call_status` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallLiveness {
    /// The server is still processing the tool call, or still maintains an
    /// active subscription established by it.
    Alive,
    /// The server no longer tracks the tool call — either it answered
    /// `alive: false`, or it responded without supporting the endpoint
    /// (4xx / invalid body), in which case it cannot be tracking the call.
    /// No result or further events should be expected.
    Gone,
    /// Liveness could not be determined: the server was unreachable or
    /// returned a transient server error (5xx). Callers SHOULD treat this
    /// conservatively (i.e. as if the call may still be alive).
    Unknown,
}

/// Query a tool server's `/tool_call_status` endpoint to check whether a tool
/// call (or the subscription it established) is still alive.
///
/// `server_url` is the tool server's base URL — the same base used to derive
/// the `/.well-known/rap-toolset` discovery endpoint.
///
/// A server that responds but does not support the endpoint — a 4xx status
/// (e.g. a 404 from a server that predates it) or an unparseable body —
/// yields [`ToolCallLiveness::Gone`]: such a server cannot confirm the call
/// is alive, so the call is treated as failed. Transport errors and 5xx
/// responses yield [`ToolCallLiveness::Unknown`], since the server may just
/// be temporarily unavailable.
pub async fn check_tool_call_status<H: HttpClient>(
    client: &H,
    server_url: &str,
    thread_id: &str,
    tool_call_id: &str,
) -> ToolCallLiveness {
    let endpoint = format!("{}/tool_call_status", server_url.trim_end_matches('/'));
    let payload = serde_json::to_string(&RapToolCallStatusRequest {
        thread_id: thread_id.to_owned(),
        tool_call_id: tool_call_id.to_owned(),
    })
    .expect("bug: failed to serialize tool_call_status request");
    match client.post_read(&endpoint, &payload).await {
        Ok((status, body)) if (200..300).contains(&status) => {
            match serde_json::from_slice::<RapToolCallStatusResponse>(&body) {
                Ok(resp) if resp.alive => ToolCallLiveness::Alive,
                Ok(_) => ToolCallLiveness::Gone,
                Err(e) => {
                    tracing::warn!(
                        "invalid tool_call_status response from {endpoint}: {e}; \
                         treating the tool call as gone"
                    );
                    ToolCallLiveness::Gone
                }
            }
        }
        Ok((status, _)) if (400..500).contains(&status) => {
            tracing::info!(
                "tool_call_status at {endpoint} returned status {status} \
                 (endpoint unsupported); treating the tool call as gone"
            );
            ToolCallLiveness::Gone
        }
        Ok((status, _)) => {
            tracing::warn!("tool_call_status at {endpoint} returned status {status}");
            ToolCallLiveness::Unknown
        }
        Err(e) => {
            tracing::warn!("failed to query tool_call_status at {endpoint}: {e}");
            ToolCallLiveness::Unknown
        }
    }
}

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

    /// Request RAP servers to migrate their state to destination servers.
    /// `servers_needing_migration` is a list of `(config_id, source_url)` pairs.
    /// `destination_urls` maps config ID → destination server URL.
    pub async fn request_migration(
        &self,
        session_id: &str,
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
