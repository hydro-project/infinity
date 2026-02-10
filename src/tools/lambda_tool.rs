use async_trait::async_trait;
use aws_sdk_sqs::types::MessageAttributeValue;
use lambda_runtime::{Error, tracing};
use serde::Serialize;

use super::{Tool, ToolContext};

#[derive(Debug, Serialize)]
struct LambdaToolRequest {
    operation: String,
    arguments: serde_json::Value,
    id: String,
    call_id: Option<String>,
    rap_receiver_url: String,
    group_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
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
        // Create request with RAP receiver URL for the Lambda to respond through
        let request = LambdaToolRequest {
            operation: self.name.clone(),
            arguments: args,
            id,
            call_id,
            rap_receiver_url: context.rap_receiver_url.clone(),
            group_id: context.group_id.clone(),
            user_id: context.user_id.clone(),
        };

        // Send to the tool's SQS queue
        context
            .sqs_client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(serde_json::to_string(&request)?)
            .message_attributes(
                "ToolName",
                MessageAttributeValue::builder()
                    .data_type("String")
                    .string_value(&self.name)
                    .build()?,
            )
            .send()
            .await?;

        tracing::info!("Forwarded {} tool request to Lambda queue", self.name);
        Ok(())
    }
}
