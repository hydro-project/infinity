use async_trait::async_trait;
use lambda_runtime::{tracing, Error};
use aws_sdk_sqs::types::MessageAttributeValue;
use serde::Serialize;

use super::{Tool, ToolContext};

#[derive(Debug, Serialize)]
struct LambdaToolRequest {
    arguments: serde_json::Value,
    id: String,
    call_id: Option<String>,
    input_queue_url: String,
    group_id: String,
}

// Generic Lambda tool that forwards requests to another Lambda via SQS
pub struct LambdaTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub queue_url: String,
}

#[async_trait]
impl Tool for LambdaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext,
    ) -> Result<(), Error> {
        // Create request with all necessary context for the Lambda to respond
        let request = LambdaToolRequest {
            arguments: args,
            id,
            call_id,
            input_queue_url: context.input_queue_url.clone(),
            group_id: context.group_id.clone(),
        };

        // Send to the tool's SQS queue
        context.sqs_client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(serde_json::to_string(&request)?)
            .message_attributes(
                "ToolName",
                MessageAttributeValue::builder()
                    .data_type("String")
                    .string_value(&self.name)
                    .build()?
            )
            .send()
            .await?;

        tracing::info!("Forwarded {} tool request to Lambda queue", self.name);
        Ok(())
    }
}
