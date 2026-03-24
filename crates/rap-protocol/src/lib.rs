use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ── RAP protocol types ──

#[derive(Debug, Deserialize)]
pub struct RapInvocation {
    pub operation: String,
    pub arguments: serde_json::Value,
    pub id: String,
    pub call_id: Option<String>,
    pub callback_url: String,
    pub group_id: String,
    pub user_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RapToolResult {
    pub r#type: String,
    pub group_id: String,
    pub id: String,
    pub call_id: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_as: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct RapUserChoice {
    pub r#type: String,
    pub group_id: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: usize,
    pub response_url: String,
}

#[derive(Debug, Serialize)]
pub struct RapSubscriptionEvent {
    pub r#type: String,
    pub group_id: String,
    pub tool_call_id: String,
    pub text: String,
    pub associative: bool,
    /// When `true`, this is the final event for this subscription. The runtime
    /// SHOULD remove the subscription from its active tracking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#final: Option<bool>,
}

#[derive(Serialize)]
pub struct ToolsetManifest {
    pub name: String,
    pub description: String,
    pub endpoint: String,
    pub tools: Vec<ToolDef>,
}

#[derive(Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    #[serde(rename = "displayScript", skip_serializing_if = "Option::is_none")]
    pub display_script: Option<String>,
}

// ── Callback client ──

/// Trait for sending tool results back to the RAP callback URL.
#[async_trait]
pub trait CallbackClient: Send + Sync + 'static {
    async fn post_json(
        &self,
        url: &str,
        body: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
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
    async fn post_json(
        &self,
        url: &str,
        body: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await?;
        Ok(())
    }
}

// ── Helper functions for sending RAP callbacks ──

/// Send a `tool_result` callback.
pub async fn send_tool_result<C: CallbackClient>(
    client: &C,
    invocation: &RapInvocation,
    text: &str,
    display_as: Option<String>,
    subscription: bool,
) {
    let result = RapToolResult {
        r#type: "tool_result".to_string(),
        group_id: invocation.group_id.clone(),
        id: invocation.id.clone(),
        call_id: invocation.call_id.clone(),
        text: text.to_string(),
        display_as,
        subscription: if subscription { Some(true) } else { None },
    };
    let body = serde_json::to_string(&result).unwrap();
    if let Err(e) = client.post_json(&invocation.callback_url, &body).await {
        tracing::error!("failed to send tool result: {e}");
    }
}

/// Send a `user_choice` callback requesting user confirmation.
pub async fn send_user_choice<C: CallbackClient>(
    client: &C,
    invocation: &RapInvocation,
    prompt: &str,
    choices: Vec<String>,
    default: usize,
    response_url: &str,
) {
    let msg = RapUserChoice {
        r#type: "user_choice".to_string(),
        group_id: invocation.group_id.clone(),
        id: invocation.id.clone(),
        call_id: invocation.call_id.clone(),
        prompt: prompt.to_string(),
        choices,
        default,
        response_url: response_url.to_string(),
    };
    let body = serde_json::to_string(&msg).unwrap();
    if let Err(e) = client.post_json(&invocation.callback_url, &body).await {
        tracing::error!("failed to send user_choice: {e}");
    }
}

/// Send a `subscription_event` callback.
pub async fn send_subscription_event<C: CallbackClient>(
    client: &C,
    invocation: &RapInvocation,
    text: &str,
    associative: bool,
    r#final: bool,
) {
    let event = RapSubscriptionEvent {
        r#type: "subscription_event".to_string(),
        group_id: invocation.group_id.clone(),
        tool_call_id: invocation.id.clone(),
        text: text.to_string(),
        associative,
        r#final: if r#final { Some(true) } else { None },
    };
    let body = serde_json::to_string(&event).unwrap();
    if let Err(e) = client.post_json(&invocation.callback_url, &body).await {
        tracing::error!("failed to send subscription event: {e}");
    }
}
