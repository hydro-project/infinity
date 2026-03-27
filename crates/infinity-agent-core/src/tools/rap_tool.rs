//! Generic RAP tool that invokes a remote tool server via HTTP.
//!
//! This is the core implementation used by both the Lambda runtime and the CLI.
//! Auth is handled by the `HttpClient` implementation (SigV4 for Lambda, plain for CLI).

use async_trait::async_trait;
use rap_protocol::RapInvocation;
use tracing;

use super::{Tool, ToolContext};
use crate::traits::InputSender;
use rap_client::http::HttpClient;

/// A RAP tool that invokes a remote tool server endpoint via HTTP.
/// Generic over the HTTP client (SigV4-signed for Lambda, plain for CLI)
/// and the input sender (SQS for Lambda, mpsc for CLI).
pub struct RapTool<H: HttpClient> {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub endpoint: String,
    pub http_client: H,
    pub display_script: Option<String>,
}

#[async_trait]
impl<H: HttpClient + 'static, M: InputSender + 'static> Tool<M> for RapTool<H> {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn display_script(&self) -> Option<&str> {
        self.display_script.as_deref()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let thread_ancestors = if context.thread_stack.len() > 1 {
            Some(context.thread_stack[..context.thread_stack.len() - 1].to_vec())
        } else {
            None
        };

        let invocation = RapInvocation {
            operation: self.name.clone(),
            arguments: args,
            id,
            call_id,
            callback_url: context.callback_url.clone(),
            group_id: context.group_id.clone(),
            user_id: context.user_id.clone(),
            thread_ancestors,
        };

        let body = serde_json::to_string(&invocation)?;
        let status = self
            .http_client
            .post(&self.endpoint, &body)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !(200..300).contains(&status) {
            tracing::warn!("RAP tool {} returned status {}", self.name, status);
        }
        tracing::info!("Invoked RAP tool {} (status: {})", self.name, status);
        Ok(())
    }
}
