use async_trait::async_trait;
use aws_sdk_scheduler::{
    Client as SchedulerClient,
    types::{FlexibleTimeWindow, FlexibleTimeWindowMode, SqsParameters, Target},
};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use infinity_agent_core::ThreadId;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::{
    message::{InputMessage, InputMessageContent},
    traits::InputSender,
};
use infinity_provider_protocol::message::{Text, ToolResult, ToolResultContent, UserContent};
use tracing;

use super::sqs_sender::SqsMessageSender;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Build the tool-result message a sleep delivers when it wakes.
fn wakeup_message(
    id: &str,
    call_id: Option<String>,
    text: String,
    group_id: &ThreadId,
) -> InputMessage {
    InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
            id: id.to_owned(),
            call_id,
            content: vec![ToolResultContent::Text(Text { text })],
        })),
        group_id: group_id.clone(),
        metadata: None,
        synthetic: None,
        display_as: None,
        subscription: false,
    }
}

/// The durable-timer configuration shared by the platform sleep tools:
/// wake-ups within the SQS delay limit go through the delay-relay queue,
/// longer ones through a one-shot EventBridge schedule targeting the input
/// queue.
pub struct WakeupScheduler {
    pub scheduler_client: SchedulerClient,
    pub scheduler_role_arn: String,
    pub delay_queue_url: String,
    /// ARN of the input queue, used as the EventBridge Scheduler target.
    pub input_queue_arn: String,
}

impl WakeupScheduler {
    /// Deliver `msg` to its thread once `target` is reached (`delay_seconds`
    /// from now, > 0). Callers handle the "already due" case themselves by
    /// sending directly.
    async fn deliver_at(
        &self,
        context: &ToolContext<SqsMessageSender>,
        msg: &InputMessage,
        delay_seconds: i64,
        target: DateTime<Utc>,
        schedule_name_prefix: &str,
        delay_dedup_id: &str,
    ) -> Result<(), BoxError> {
        const MAX_SQS_DELAY_SECONDS: i64 = 900;

        if delay_seconds <= MAX_SQS_DELAY_SECONDS {
            let envelope = serde_json::json!({
                "message": serde_json::to_string(msg)?,
                "group_id": context.group_id,
                "dedup_id": delay_dedup_id,
            });
            context
                .message_sender
                .sqs_client
                .send_message()
                .queue_url(&self.delay_queue_url)
                .message_body(serde_json::to_string(&envelope)?)
                .delay_seconds(delay_seconds as i32)
                .send()
                .await?;
        } else {
            let schedule_name =
                format!("{}-{}", schedule_name_prefix, Utc::now().timestamp_millis());
            self.scheduler_client
                .create_schedule()
                .name(&schedule_name)
                .schedule_expression(format!("at({})", target.format("%Y-%m-%dT%H:%M:%S")))
                .flexible_time_window(
                    FlexibleTimeWindow::builder()
                        .mode(FlexibleTimeWindowMode::Off)
                        .build()?,
                )
                .target(
                    Target::builder()
                        .arn(&self.input_queue_arn)
                        .role_arn(&self.scheduler_role_arn)
                        .input(serde_json::to_string(msg)?)
                        .sqs_parameters(
                            SqsParameters::builder()
                                .message_group_id(context.group_id.clone().into_inner())
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

pub struct SleepTool {
    pub scheduler: WakeupScheduler,
}

#[async_trait]
impl Tool<SqsMessageSender> for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn is_passive(&self) -> bool {
        true
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

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<SqsMessageSender>,
    ) -> Result<(), BoxError> {
        let seconds = args["seconds"].as_f64().unwrap_or(0.0) as i64;
        let msg = wakeup_message(
            &id,
            call_id,
            format!("Slept for {} seconds", seconds),
            &context.group_id,
        );

        if seconds <= 0 {
            context.message_sender.send_to_input_queue(msg, &id).await?;
        } else {
            let target = Utc::now() + Duration::seconds(seconds);
            self.scheduler
                .deliver_at(context, &msg, seconds, target, "sleep", &id)
                .await?;
        }

        tracing::info!("Sleep scheduled for {} seconds", seconds);
        Ok(())
    }
}

pub struct SleepUntilTool {
    pub scheduler: WakeupScheduler,
}

#[async_trait]
impl Tool<SqsMessageSender> for SleepUntilTool {
    fn name(&self) -> &str {
        "sleep_until"
    }

    fn is_passive(&self) -> bool {
        true
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

    fn display_script(&self) -> Option<&str> {
        Some(
            r#"let s = "Sleeping until " + args.date + " " + args.time; if args.timezone != () { s += " (" + args.timezone + ")"; } s"#,
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<SqsMessageSender>,
    ) -> Result<(), BoxError> {
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
        let text = if target_utc <= now {
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
        let msg = wakeup_message(&id, call_id, text, &context.group_id);

        if target_utc <= now {
            context.message_sender.send_to_input_queue(msg, &id).await?;
            return Ok(());
        }

        let seconds_until = (target_utc - now).num_seconds();
        let dedup_id = format!("sleep-until-{}", Utc::now().timestamp_millis());
        self.scheduler
            .deliver_at(
                context,
                &msg,
                seconds_until,
                target_utc,
                "sleep-until",
                &dedup_id,
            )
            .await
    }
}
