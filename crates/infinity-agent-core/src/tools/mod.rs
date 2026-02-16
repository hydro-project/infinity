pub mod config;
pub mod sleep;
pub mod thread;
pub mod toolset_loader;

use crate::traits::InputSender;
use async_trait::async_trait;

/// Context passed to tool implementations — generic over platform backends.
pub struct ToolContext<M: InputSender> {
    pub message_sender: M,
    pub group_id: String,
    pub input_queue_arn: String,
    pub rap_receiver_url: String,
    pub user_id: Option<String>,
}

#[async_trait]
pub trait Tool<M: InputSender>: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Trait for grouped tool sets.
pub trait ToolSet<M: InputSender> {
    fn into_tools(self: Box<Self>) -> Vec<Box<dyn Tool<M>>>;
}

/// Simple ToolSet implementation that wraps a vector of tools.
pub struct VecToolSet<M: InputSender> {
    tools: Vec<Box<dyn Tool<M>>>,
}

impl<M: InputSender> VecToolSet<M> {
    pub fn new(tools: Vec<Box<dyn Tool<M>>>) -> Self {
        Self { tools }
    }
}

impl<M: InputSender> ToolSet<M> for VecToolSet<M> {
    fn into_tools(self: Box<Self>) -> Vec<Box<dyn Tool<M>>> {
        self.tools
    }
}
