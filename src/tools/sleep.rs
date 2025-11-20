use async_trait::async_trait;
use lambda_runtime::{tracing, Error};
use aws_sdk_sqs::types::MessageAttributeValue;
use aws_sdk_scheduler::{Client as SchedulerClient, types::{FlexibleTimeWindow, FlexibleTimeWindowMode, Target}};
use chrono::{Utc, Duration};
use rig::{OneOrMany, agent::Text, message::{ToolResult, ToolResultContent, UserContent}};
use serde::{Deserialize, Serialize};

use super::{Tool, ToolContext};

#[derive(Debug, Deserialize, Serialize)]
struct InputMessage {
    content: UserContent,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

// Sleep tool implementation
pub struct SleepTool {
    pub scheduler_client: SchedulerClient,
    pub scheduler_role_arn: String,
}

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleep for a specified number of seconds before continuing. Useful for waiting or delaying actions."
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
            content: UserContent::ToolResult(ToolResult {
                id,
                call_id,
                content: OneOrMany::one(ToolResultContent::Text(Text{
                    text: format!("Slept for {} seconds", seconds)
                })),
            }),
            metadata: None,
        };

        // SQS supports delays up to 900 seconds (15 minutes)
        // For longer delays, use EventBridge Scheduler
        if seconds <= 900 {
            // Use SQS delay for short sleeps
            context.sqs_client
                .send_message()
                .queue_url(&context.input_queue_url)
                .message_body(serde_json::to_string(&tool_result_msg)?)
                .message_attributes(
                    "ConversationGroupId",
                    MessageAttributeValue::builder()
                        .data_type("String")
                        .string_value(&context.group_id)
                        .build()?
                )
                .delay_seconds(seconds as i32)
                .send()
                .await?;

            tracing::info!("Scheduled sleep for {} seconds using SQS delay", seconds);
        } else {
            // Use EventBridge Scheduler for longer sleeps
            let schedule_time = Utc::now() + Duration::seconds(seconds);
            let schedule_name = format!("sleep-{}", chrono::Utc::now().timestamp_millis());
            
            self.scheduler_client
                .create_schedule()
                .name(&schedule_name)
                .schedule_expression(format!("at({})", schedule_time.format("%Y-%m-%dT%H:%M:%S")))
                .flexible_time_window(
                    FlexibleTimeWindow::builder()
                        .mode(FlexibleTimeWindowMode::Off)
                        .build()?
                )
                .target(
                    Target::builder()
                        .arn(&context.input_queue_arn)
                        .role_arn(&self.scheduler_role_arn)
                        .input(serde_json::to_string(&tool_result_msg)?)
                        .build()?
                )
                .send()
                .await?;

            tracing::info!("Scheduled sleep for {} seconds using EventBridge Scheduler", seconds);
        }

        tracing::info!("Sleep scheduled for {} seconds", seconds);
        Ok(())
    }
}
