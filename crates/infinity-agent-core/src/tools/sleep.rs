use async_trait::async_trait;
use infinity_provider_protocol::message::{Text, ToolResult, ToolResultContent, UserContent};
use tracing;

use super::{Tool, ToolContext};
use crate::message::{InputMessage, InputMessageContent};
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

    fn display_script(&self) -> Option<&str> {
        Some(r#""Sleeping until event or input""#)
    }

    fn is_passive(&self) -> bool {
        true
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

/// Timed sleep tool backed by an in-process tokio timer. Suitable for
/// resident runtimes (embedded systems, daemons); serverless deployments
/// need durable timers instead (e.g. SQS delays / EventBridge Scheduler).
pub struct TokioSleepTool;

#[async_trait]
impl<M: InputSender + 'static> Tool<M> for TokioSleepTool {
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
                "seconds": { "type": "number", "description": "Number of seconds to sleep" },
                "reason": { "type": "string", "description": "Human-readable reason for sleeping" }
            },
            "required": ["seconds"]
        })
    }

    fn display_script(&self) -> Option<&str> {
        Some(
            r#"let s = "Sleeping " + args.seconds + "s"; if args.reason != () { s += ": " + args.reason; } s"#,
        )
    }

    fn is_passive(&self) -> bool {
        true
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

        tokio::spawn(rap_protocol::log_panic("sleep_tool", async move {
            if seconds > 0.0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(seconds)).await;
            }
            let msg = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: id.clone(),
                    call_id,
                    content: vec![ToolResultContent::Text(Text {
                        text: format!("Slept for {} seconds", seconds),
                    })],
                })),
                group_id: group_id.clone(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            };
            if let Err(e) = sender.send_to_input_queue(msg, &id).await {
                tracing::error!("Failed to deliver sleep result: {}", e);
            }
        }));

        tracing::info!("Sleep scheduled for {} seconds", seconds);
        Ok(())
    }
}

/// Sleep-until-wall-clock-time tool backed by an in-process tokio timer. See
/// [`TokioSleepTool`] for platform considerations.
pub struct TokioSleepUntilTool;

#[async_trait]
impl<M: InputSender + 'static> Tool<M> for TokioSleepUntilTool {
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

    fn display_script(&self) -> Option<&str> {
        Some(
            r#"let s = "Sleeping until " + args.date + " " + args.time; if args.timezone != () { s += " (" + args.timezone + ")"; } s"#,
        )
    }

    fn is_passive(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let date_str = args["date"].as_str().unwrap_or("").to_owned();
        let time_str = args["time"].as_str().unwrap_or("").to_owned();
        let tz_str = args["timezone"].as_str().unwrap_or("UTC").to_owned();

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

        tokio::spawn(rap_protocol::log_panic("sleep_until_tool", async move {
            if !is_past {
                let duration = (target_utc - now).to_std().unwrap_or_default();
                tokio::time::sleep(duration).await;
            }
            let msg = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: id.clone(),
                    call_id,
                    content: vec![ToolResultContent::Text(Text { text: result_text })],
                })),
                group_id: group_id.clone(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            };
            if let Err(e) = sender.send_to_input_queue(msg, &id).await {
                tracing::error!("Failed to deliver sleep_until result: {}", e);
            }
        }));

        Ok(())
    }
}
