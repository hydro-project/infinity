use async_trait::async_trait;
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent},
};
use tracing;

use super::{Tool, ToolContext};
use crate::traits::{InputSender, StateStore};
use rap_client::http::HttpClient;
use rap_client::notifier::RapNotifier;

/// Built-in synchronous tool that cancels an active subscription.
///
/// When invoked, it:
/// 1. Checks that the subscription exists in the current thread's metadata
/// 2. Sends a `/cancel_tool_call` notification to all tool servers
/// 3. Removes the subscription from the thread's metadata
///
/// Ownership is implicit: each thread tracks its own active subscriptions,
/// so only the thread that created a subscription can see (and cancel) it.
pub struct CancelSubscriptionTool<S: StateStore, H: HttpClient> {
    pub state_store: S,
    pub rap_notifier: Option<RapNotifier<H>>,
}

#[async_trait]
impl<M: InputSender + 'static, S: StateStore + 'static, H: HttpClient + 'static> Tool<M>
    for CancelSubscriptionTool<S, H>
{
    fn name(&self) -> &str {
        "cancel_subscription"
    }

    fn description(&self) -> &str {
        "Cancel an active subscription by its tool call ID. This sends a cancellation notification to the tool server and removes the subscription from tracking. You can only cancel subscriptions that were created by the current thread."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool_call_id": {
                    "type": "string",
                    "description": "The tool call ID of the subscription to cancel. This is the ID of the original tool call that started the subscription."
                }
            },
            "required": ["tool_call_id"]
        })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _id: String,
        _call_id: Option<String>,
        _context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Synchronous-only tool; execute is a no-op.
        Ok(())
    }

    fn supports_sync(&self) -> bool {
        true
    }

    async fn execute_synchronous(
        &self,
        args: &serde_json::Value,
        id: &str,
        call_id: Option<&str>,
        context: &ToolContext<M>,
    ) -> Option<ToolResult> {
        let Some(tool_call_id) = args.get("tool_call_id").and_then(|v| v.as_str()) else {
            return Some(error_result(id, call_id, "Error: tool_call_id is required"));
        };

        // Check that the subscription exists in this thread's active subscriptions.
        // Ownership is implicit — each thread only tracks its own subscriptions.
        let Ok(active) = self
            .state_store
            .get_active_subscriptions(&context.group_id)
            .await
        else {
            return Some(error_result(
                id,
                call_id,
                "Error: failed to read thread subscriptions",
            ));
        };

        if !active.iter().any(|s| s == tool_call_id) {
            return Some(error_result(
                id,
                call_id,
                &format!(
                    "Error: no active subscription found for tool call ID '{}'",
                    tool_call_id
                ),
            ));
        }

        // Send cancellation notification to tool servers
        if let Some(ref notifier) = self.rap_notifier {
            tracing::info!(
                "Sending cancellation notification for subscription {}",
                tool_call_id
            );
            notifier
                .notify_tool_cancelled(&context.group_id, tool_call_id)
                .await;
        }

        // Remove from this thread's subscriptions
        if let Err(e) = self
            .state_store
            .remove_active_subscription(&context.group_id, tool_call_id)
            .await
        {
            tracing::warn!("Failed to persist subscription removal: {}", e);
        }

        tracing::info!(
            "Cancelled subscription {} in thread {}",
            tool_call_id,
            context.group_id
        );

        Some(ToolResult {
            id: id.to_owned(),
            call_id: call_id.map(String::from),
            content: OneOrMany::one(ToolResultContent::Text(Text {
                text: format!("Subscription '{}' cancelled successfully.", tool_call_id),
            })),
        })
    }
}

fn error_result(id: &str, call_id: Option<&str>, text: &str) -> ToolResult {
    ToolResult {
        id: id.to_owned(),
        call_id: call_id.map(String::from),
        content: OneOrMany::one(ToolResultContent::Text(Text {
            text: text.to_owned(),
        })),
    }
}
