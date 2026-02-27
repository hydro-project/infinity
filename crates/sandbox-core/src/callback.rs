use async_trait::async_trait;

use crate::error::SandboxError;

/// Trait for sending tool results back to the RAP callback URL.
/// Plain HTTP for local mode, SigV4-signed for Lambda mode.
#[async_trait]
pub trait CallbackClient: Send + Sync + 'static {
    async fn post_json(&self, url: &str, body: &str) -> Result<(), SandboxError>;
}

/// Plain reqwest client for local mode (no auth needed).
pub struct PlainCallbackClient {
    client: reqwest::Client,
}

impl Default for PlainCallbackClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PlainCallbackClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl CallbackClient for PlainCallbackClient {
    async fn post_json(&self, url: &str, body: &str) -> Result<(), SandboxError> {
        self.client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| SandboxError::Other(format!("callback POST failed: {e}")))?;
        Ok(())
    }
}
