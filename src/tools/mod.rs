use async_trait::async_trait;
use aws_sdk_sqs::Client as SqsClient;
use lambda_runtime::Error;

pub mod config;
pub mod lambda_mcp;
pub mod lambda_tool;
pub mod sleep;
pub mod thread;

// Context passed to tool implementations
pub struct ToolContext {
    pub sqs_client: SqsClient,
    pub group_id: String,
    pub input_queue_url: String,
    pub input_queue_arn: String,
    pub rap_receiver_url: String,
    pub user_id: Option<String>,
}

impl ToolContext {
    /// Send a message to the input FIFO queue with the correct MessageGroupId and dedup ID.
    /// `dedup_id` should be the tool call ID for the message being sent.
    pub async fn send_to_input_queue(
        &self,
        body: &str,
        group_id: &str,
        dedup_id: &str,
    ) -> Result<(), Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        self.sqs_client
            .send_message()
            .queue_url(&self.input_queue_url)
            .message_body(body)
            .message_group_id(group_id)
            .message_deduplication_id(format!("{}-{}", dedup_id, now))
            .send()
            .await?;
        Ok(())
    }
}
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

// Trait for grouped tool sets
pub trait ToolSet {
    fn into_tools(self: Box<Self>) -> Vec<Box<dyn Tool>>;
}

// Simple ToolSet implementation that wraps a vector of tools
pub struct VecToolSet {
    tools: Vec<Box<dyn Tool>>,
}

impl VecToolSet {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }
}

impl ToolSet for VecToolSet {
    fn into_tools(self: Box<Self>) -> Vec<Box<dyn Tool>> {
        self.tools
    }
}
