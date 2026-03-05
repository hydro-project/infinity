use async_trait::async_trait;
use aws_sdk_scheduler::{
    Client as SchedulerClient,
    types::{FlexibleTimeWindow, FlexibleTimeWindowMode, SqsParameters, Target},
};
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::{
    message::{InputMessage, InputMessageContent},
    traits::InputSender,
};
use rig::{
    OneOrMany,
    agent::Text,
    message::{ToolResult, ToolResultContent, UserContent},
};
use tracing;

use super::sqs_sender::SqsMessageSender;

async fn send_to_delay_queue(
    context: &ToolContext<SqsMessageSender>,
    delay_queue_url: &str,
    body: &str,
    group_id: &str,
    dedup_id: &str,
    delay_seconds: i32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let envelope = serde_json::json!({
        "message": body,
        "group_id": group_id,
        "dedup_id": dedup_id,
    });
    context
        .message_sender
        .sqs_client
        .send_message()
        .queue_url(delay_queue_url)
        .message_body(serde_json::to_string(&envelope)?)
        .delay_seconds(delay_seconds)
        .send()
        .await?;
    Ok(())
}

pub struct SleepTool {
    pub scheduler_client: SchedulerClient,
    pub scheduler_role_arn: String,
    pub delay_queue_url: String,
}

#[async_trait]
impl Tool<SqsMessageSender> for SleepTool {
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
        context: &ToolContext<SqsMessageSender>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let seconds = args["seconds"].as_f64().unwrap_or(0.0) as i64;

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
            display_as: None,
            subscription: false,
        };

        const MAX_SQS_DELAY_SECONDS: i64 = 900;

        if seconds <= 0 {
            context
                .message_sender
                .send_to_input_queue(tool_result_msg, &context.group_id, &id)
                .await?;
        } else if seconds <= MAX_SQS_DELAY_SECONDS {
            send_to_delay_queue(
                context,
                &self.delay_queue_url,
                &serde_json::to_string(&tool_result_msg)?,
                &context.group_id,
                &id,
                seconds as i32,
            )
            .await?;
        } else {
            let schedule_time = Utc::now() + Duration::seconds(seconds);
            let schedule_name = format!("sleep-{}", Utc::now().timestamp_millis());

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
        }

        tracing::info!("Sleep scheduled for {} seconds", seconds);
        Ok(())
    }
}

pub struct SleepUntilTool {
    pub scheduler_client: SchedulerClient,
    pub scheduler_role_arn: String,
    pub delay_queue_url: String,
}

#[async_trait]
impl Tool<SqsMessageSender> for SleepUntilTool {
    fn name(&self) -> &str {
        "sleep_until"
    }

    fn description(&self) -> &str {
        "Sleep until a specific date and time in a given timezone. The agent will hibernate with zero resource usage and wake up at the specified time."
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
        context: &ToolContext<SqsMessageSender>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let date_str = args["date"].as_str().unwrap_or("");
        let time_str = args["time"].as_str().unwrap_or("");
        let tz_str = args["timezone"].as_str().unwrap_or("UTC");

        let tz: Tz = tz_str
            .parse()
            .map_err(|_| format!("Invalid timezone: '{}'", tz_str))?;
        let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")?;
        let time = NaiveTime::parse_from_str(time_str, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(time_str, "%H:%M"))?;

        let naive_dt = NaiveDateTime::new(date, time);
        let local_dt = naive_dt
            .and_local_timezone(tz)
            .single()
            .ok_or(format!("Ambiguous datetime {} in {}", naive_dt, tz_str))?;
        let target_utc = local_dt.with_timezone(&Utc);

        let now = Utc::now();
        let tool_result_msg = InputMessage {
            content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                id: id.clone(),
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text {
                    text: if target_utc <= now {
                        format!(
                            "Target time {} {} is in the past. Waking immediately.",
                            date_str, time_str
                        )
                    } else {
                        format!(
                            "Woke up at target time: {} {} ({})",
                            date_str, time_str, tz_str
                        )
                    },
                })),
            })),
            group_id: context.group_id.clone(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };

        if target_utc <= now {
            context
                .message_sender
                .send_to_input_queue(tool_result_msg, &context.group_id, &id)
                .await?;
            return Ok(());
        }

        let seconds_until = (target_utc - now).num_seconds();
        let dedup_id = format!("sleep-until-{}", Utc::now().timestamp_millis());
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
        }

        Ok(())
    }
}
