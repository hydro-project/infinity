use async_trait::async_trait;
use tracing;

use super::{Tool, ToolContext};
use crate::traits::InputSender;

/// A no-op tool that signals the agent should wait indefinitely until an external event or user input arrives.
/// The agent loop will simply stop after this tool is invoked, and resume when new input comes in.
pub struct SleepUntilEventOrInputTool;

#[async_trait]
impl<M: InputSender + 'static> Tool<M> for SleepUntilEventOrInputTool {
    fn name(&self) -> &str {
        "sleep_until_event_or_input"
    }

    fn description(&self) -> &str {
        "Sleep indefinitely until an external event or user input arrives. Use this when you have completed all current tasks and are waiting for something to happen (e.g., a webhook, a scheduled event, or user input). The agent will pause and automatically resume when new input is received."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _id: String,
        _call_id: Option<String>,
        _context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tracing::info!("sleep_until_event_or_input invoked, agent will pause until next input");
        Ok(())
    }
}
