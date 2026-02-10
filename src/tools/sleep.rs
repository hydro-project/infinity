use async_trait::async_trait;
use aws_sdk_scheduler::{
    Client as SchedulerClient,
    types::{FlexibleTimeWindow, FlexibleTimeWindowMode, SqsParameters, Target},
};
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use lambda_runtime::{Error, tracing};
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};

use crate::event_handler::{InputMessage, InputMessageContent};

use super::{Tool, ToolContext};

/// Send a message to the standard delay queue with a per-message delay.
/// The relay Lambda will forward it to the FIFO input queue after the delay.
async fn send_to_delay_queue(
    context: &ToolContext,
    delay_queue_url: &str,
    body: &str,
    group_id: &str,
    dedup_id: &str,
    delay_seconds: i32,
) -> Result<(), Error> {
    let envelope = serde_json::json!({
        "message": body,
        "group_id": group_id,
        "dedup_id": dedup_id,
    });
    context
        .sqs_client
        .send_message()
        .queue_url(delay_queue_url)
        .message_body(serde_json::to_string(&envelope)?)
        .delay_seconds(delay_seconds)
        .send()
        .await?;
    Ok(())
}

// Sleep tool implementation
pub struct SleepTool {
    pub scheduler_client: SchedulerClient,
    pub scheduler_role_arn: String,
    pub delay_queue_url: String,
}

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleep for a specified number of seconds before continuing. Useful for waiting or delaying actions. This tool will automatically be interrupted by the system on user input, so you are free to invoke it in a loop as necessary (and avoid stopping the loop arbitrarily, as the user is always observing the outputs)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "number",
                    "description": "Number of seconds to sleep"
                }
            },
            "required": ["seconds"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext,
    ) -> Result<(), Error> {
        let seconds = args["seconds"].as_f64().unwrap_or(0.0) as i64;

        // Create tool result message to be sent after sleep
        let tool_result_msg = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: id.clone(),
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: format!("Slept for {} seconds", seconds),
                })),
            })),
            group_id: context.group_id.clone(),
            metadata: None,
            synthetic: None,
        };

        // Short delays (≤ 900s): use standard SQS queue with per-message DelaySeconds.
        // A relay Lambda forwards the message to the FIFO input queue after the delay.
        // Long delays (> 900s): use EventBridge Scheduler (SQS max delay is 900s).
        const MAX_SQS_DELAY_SECONDS: i64 = 900;

        if seconds <= 0 {
            // No delay — send directly to FIFO queue
            context
                .send_to_input_queue(
                    &serde_json::to_string(&tool_result_msg)?,
                    &context.group_id,
                    &id,
                )
                .await?;

            tracing::info!("Sleep of 0 seconds, sent immediately");
        } else if seconds <= MAX_SQS_DELAY_SECONDS {
            // Use standard SQS delay queue for short sleeps
            send_to_delay_queue(
                context,
                &self.delay_queue_url,
                &serde_json::to_string(&tool_result_msg)?,
                &context.group_id,
                &id,
                seconds as i32,
            )
            .await?;

            tracing::info!("Sent sleep of {} seconds via SQS delay queue", seconds);
        } else {
            // Use EventBridge Scheduler for all delays
            let schedule_time = Utc::now() + Duration::seconds(seconds);
            let schedule_name = format!("sleep-{}", chrono::Utc::now().timestamp_millis());

            self.scheduler_client
                .create_schedule()
                .name(&schedule_name)
                .schedule_expression(format!("at({})", schedule_time.format("%Y-%m-%dT%H:%M:%S")))
                .flexible_time_window(
                    FlexibleTimeWindow::builder()
                        .mode(FlexibleTimeWindowMode::Off)
                        .build()?,
                )
                .target(
                    Target::builder()
                        .arn(&context.input_queue_arn)
                        .role_arn(&self.scheduler_role_arn)
                        .input(serde_json::to_string(&tool_result_msg)?)
                        .sqs_parameters(
                            SqsParameters::builder()
                                .message_group_id(context.group_id.clone())
                                .build(),
                        )
                        .build()?,
                )
                .send()
                .await?;

            tracing::info!(
                "Scheduled sleep for {} seconds using EventBridge Scheduler",
                seconds
            );
        }

        tracing::info!("Sleep scheduled for {} seconds", seconds);
        Ok(())
    }
}

/// A no-op tool that signals the agent should wait indefinitely until an external event or user input arrives.
/// The agent loop will simply stop after this tool is invoked, and resume when new input comes in.
pub struct SleepUntilEventOrInputTool;

#[async_trait]
impl Tool for SleepUntilEventOrInputTool {
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
        _context: &ToolContext,
    ) -> Result<(), Error> {
        // No-op: the agent loop will simply stop after this tool is invoked
        tracing::info!("sleep_until_event_or_input invoked, agent will pause until next input");
        Ok(())
    }
}

/// Sleep until a specific date/time in a given timezone.
/// Useful for scheduling the agent to wake up at a known wall-clock time
/// (e.g. when the stock market opens).
pub struct SleepUntilTool {
    pub scheduler_client: SchedulerClient,
    pub scheduler_role_arn: String,
    pub delay_queue_url: String,
}

#[async_trait]
impl Tool for SleepUntilTool {
    fn name(&self) -> &str {
        "sleep_until"
    }

    fn description(&self) -> &str {
        "Sleep until a specific date and time in a given timezone. The agent will hibernate with zero resource usage and wake up at the specified time. Useful for scheduling wake-ups at known wall-clock times (e.g. 'sleep until 9:30 AM Eastern when the stock market opens'). Use get_time first to know the current time."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "date": {
                    "type": "string",
                    "description": "Target date in YYYY-MM-DD format (e.g. '2025-02-10')"
                },
                "time": {
                    "type": "string",
                    "description": "Target time in HH:MM or HH:MM:SS 24-hour format (e.g. '09:30' or '09:30:00')"
                },
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone name (e.g. 'America/New_York', 'US/Eastern', 'Europe/London'). Defaults to UTC."
                }
            },
            "required": ["date", "time"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext,
    ) -> Result<(), Error> {
        let date_str = args["date"].as_str().unwrap_or("");
        let time_str = args["time"].as_str().unwrap_or("");
        let tz_str = args["timezone"].as_str().unwrap_or("UTC");

        // Parse timezone
        let tz: Tz = tz_str.parse().map_err(|_| {
            Error::from(format!(
                "Invalid timezone: '{}'. Use IANA format like 'America/New_York'",
                tz_str
            ))
        })?;

        // Parse date
        let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").map_err(|e| {
            Error::from(format!(
                "Invalid date '{}': {}. Use YYYY-MM-DD format.",
                date_str, e
            ))
        })?;

        // Parse time (support HH:MM and HH:MM:SS)
        let time = NaiveTime::parse_from_str(time_str, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(time_str, "%H:%M"))
            .map_err(|e| {
                Error::from(format!(
                    "Invalid time '{}': {}. Use HH:MM or HH:MM:SS format.",
                    time_str, e
                ))
            })?;

        let naive_dt = NaiveDateTime::new(date, time);

        // Convert to UTC
        let local_dt = naive_dt.and_local_timezone(tz).single().ok_or_else(|| {
            Error::from(format!(
                "Ambiguous or invalid datetime {} in timezone {}",
                naive_dt, tz_str
            ))
        })?;
        let target_utc = local_dt.with_timezone(&Utc);

        let now = Utc::now();
        if target_utc <= now {
            // Target is in the past — just send an immediate result
            let tool_result_msg = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: id.clone(),
                    call_id,
                    content: OneOrMany::one(ToolResultContent::Text(Text {
                        text: format!(
                            "Target time {} {} is in the past (current UTC: {}). Waking immediately.",
                            date_str,
                            time_str,
                            now.format("%Y-%m-%d %H:%M:%S UTC")
                        ),
                    })),
                })),
                group_id: context.group_id.clone(),
                metadata: None,
                synthetic: None,
            };

            context
                .send_to_input_queue(
                    &serde_json::to_string(&tool_result_msg)?,
                    &context.group_id,
                    &id,
                )
                .await?;

            tracing::info!("sleep_until target is in the past, waking immediately");
            return Ok(());
        }

        let seconds_until = (target_utc - now).num_seconds();
        let dedup_id = format!("sleep-until-{}", Utc::now().timestamp_millis());

        let tool_result_msg = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id,
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: format!(
                        "Woke up at target time: {} {} ({})",
                        date_str, time_str, tz_str
                    ),
                })),
            })),
            group_id: context.group_id.clone(),
            metadata: None,
            synthetic: None,
        };

        // Short delays (≤ 900s): use standard SQS delay queue.
        // Long delays (> 900s): use EventBridge Scheduler.
        const MAX_SQS_DELAY_SECONDS: i64 = 900;

        if seconds_until <= MAX_SQS_DELAY_SECONDS {
            send_to_delay_queue(
                context,
                &self.delay_queue_url,
                &serde_json::to_string(&tool_result_msg)?,
                &context.group_id,
                &dedup_id,
                seconds_until as i32,
            )
            .await?;

            tracing::info!(
                "Sent sleep_until of {} seconds via SQS delay queue (target: {} {} {})",
                seconds_until,
                date_str,
                time_str,
                tz_str
            );
        } else {
            let schedule_name = format!("sleep-until-{}", Utc::now().timestamp_millis());

            self.scheduler_client
                .create_schedule()
                .name(&schedule_name)
                .schedule_expression(format!("at({})", target_utc.format("%Y-%m-%dT%H:%M:%S")))
                .flexible_time_window(
                    FlexibleTimeWindow::builder()
                        .mode(FlexibleTimeWindowMode::Off)
                        .build()?,
                )
                .target(
                    Target::builder()
                        .arn(&context.input_queue_arn)
                        .role_arn(&self.scheduler_role_arn)
                        .input(serde_json::to_string(&tool_result_msg)?)
                        .sqs_parameters(
                            SqsParameters::builder()
                                .message_group_id(context.group_id.clone())
                                .build(),
                        )
                        .build()?,
                )
                .send()
                .await?;

            tracing::info!(
                "Scheduled sleep_until for {} seconds using EventBridge Scheduler (target: {} {} {})",
                seconds_until,
                date_str,
                time_str,
                tz_str
            );
        }

        Ok(())
    }
}
