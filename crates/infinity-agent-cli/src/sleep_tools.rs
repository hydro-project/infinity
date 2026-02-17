//! CLI-specific sleep tools that use tokio::spawn + tokio::time::sleep
//! to deliver delayed tool results through the in-memory input channel.

use async_trait::async_trait;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};
use tracing;

pub struct SleepTool;

#[async_trait]
impl<M: InputSender + 'static> Tool<M> for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleep for a specified number of seconds before continuing. Useful for waiting or delaying actions. This tool will automatically be interrupted by the system on user input, so you are free to invoke it in a loop as necessary."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": { "type": "number", "description": "Number of seconds to sleep" }
            },
            "required": ["seconds"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let seconds = args["seconds"].as_f64().unwrap_or(0.0);
        let group_id = context.group_id.clone();
        let sender = context.message_sender.clone();

        tokio::spawn(async move {
            if seconds > 0.0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(seconds)).await;
            }
            let msg = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: id.clone(),
                    call_id,
                    content: OneOrMany::one(ToolResultContent::Text(Text {
                        text: format!("Slept for {} seconds", seconds),
                    })),
                })),
                group_id: group_id.clone(),
                metadata: None,
                synthetic: None,
            };
            if let Err(e) = sender.send_to_input_queue(msg, &group_id, &id).await {
                tracing::error!("Failed to deliver sleep result: {}", e);
            }
        });

        tracing::info!("Sleep scheduled for {} seconds", seconds);
        Ok(())
    }
}

pub struct SleepUntilTool;

#[async_trait]
impl<M: InputSender + 'static> Tool<M> for SleepUntilTool {
    fn name(&self) -> &str {
        "sleep_until"
    }

    fn description(&self) -> &str {
        "Sleep until a specific date and time in a given timezone. The agent will hibernate and wake up at the specified time."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "date": { "type": "string", "description": "Target date in YYYY-MM-DD format" },
                "time": { "type": "string", "description": "Target time in HH:MM or HH:MM:SS 24-hour format" },
                "timezone": { "type": "string", "description": "IANA timezone name. Defaults to UTC." }
            },
            "required": ["date", "time"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let date_str = args["date"].as_str().unwrap_or("").to_string();
        let time_str = args["time"].as_str().unwrap_or("").to_string();
        let tz_str = args["timezone"].as_str().unwrap_or("UTC").to_string();

        let tz: chrono_tz::Tz = tz_str
            .parse()
            .map_err(|_| format!("Invalid timezone: '{}'", tz_str))?;
        let date = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")?;
        let time = chrono::NaiveTime::parse_from_str(&time_str, "%H:%M:%S")
            .or_else(|_| chrono::NaiveTime::parse_from_str(&time_str, "%H:%M"))?;

        let naive_dt = chrono::NaiveDateTime::new(date, time);
        let local_dt = naive_dt
            .and_local_timezone(tz)
            .single()
            .ok_or(format!("Ambiguous datetime {} in {}", naive_dt, tz_str))?;
        let target_utc = local_dt.with_timezone(&chrono::Utc);
        let now = chrono::Utc::now();

        let is_past = target_utc <= now;
        let result_text = if is_past {
            format!(
                "Target time {} {} is in the past. Waking immediately.",
                date_str, time_str
            )
        } else {
            format!(
                "Woke up at target time: {} {} ({})",
                date_str, time_str, tz_str
            )
        };

        let group_id = context.group_id.clone();
        let sender = context.message_sender.clone();

        tokio::spawn(async move {
            if !is_past {
                let duration = (target_utc - now).to_std().unwrap_or_default();
                tokio::time::sleep(duration).await;
            }
            let msg = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: id.clone(),
                    call_id,
                    content: OneOrMany::one(ToolResultContent::Text(Text { text: result_text })),
                })),
                group_id: group_id.clone(),
                metadata: None,
                synthetic: None,
            };
            if let Err(e) = sender.send_to_input_queue(msg, &group_id, &id).await {
                tracing::error!("Failed to deliver sleep_until result: {}", e);
            }
        });

        Ok(())
    }
}
