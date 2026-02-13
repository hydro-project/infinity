use async_trait::async_trait;
use infinity_agent_core::tools::{Tool, ToolContext};
use serde::Serialize;
use tracing;

use super::rap_http::RapHttpClient;
use super::sqs_sender::SqsMessageSender;

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

/// Generic Lambda tool that invokes another Lambda via HTTP (Function URL with IAM auth).
pub struct LambdaTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub function_url: String,
    pub http_client: RapHttpClient,
}

#[async_trait]
impl Tool<SqsMessageSender> for LambdaTool {
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
        context: &ToolContext<SqsMessageSender>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = LambdaToolRequest {
            operation: self.name.clone(),
            arguments: args,
            id,
            call_id,
            rap_receiver_url: context.rap_receiver_url.clone(),
            group_id: context.group_id.clone(),
            user_id: context.user_id.clone(),
        };

        let body = serde_json::to_string(&request)?;
        let status = self
            .http_client
            .post_signed(&self.function_url, &body)
            .await?;

        if !status.is_success() {
            tracing::warn!("Tool {} Function URL returned status {}", self.name, status);
        }
        tracing::info!("Invoked {} tool via HTTP (status: {})", self.name, status);
        Ok(())
    }
}
