use async_trait::async_trait;
use lambda_runtime::Error;
use aws_sdk_sqs::Client as SqsClient;

pub mod sleep;
pub mod lambda_tool;

// Context passed to tool implementations
pub struct ToolContext {
    pub sqs_client: SqsClient,
    pub group_id: String,
    pub input_queue_url: String,
    pub input_queue_arn: String,
}

// Trait for tool implementations
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext,
    ) -> Result<(), Error>;
}
