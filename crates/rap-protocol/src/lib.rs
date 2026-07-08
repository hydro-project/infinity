use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::panic::AssertUnwindSafe;

// ── RAP protocol types ──

#[derive(Debug, Serialize, Deserialize)]
pub struct RapInvocation {
    pub operation: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    pub id: String,
    pub call_id: Option<String>,
    pub callback_url: String,
    pub group_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Ordered ancestor thread group IDs, from root to the parent of the current thread.
    #[serde(default)]
    pub thread_ancestors: Option<Vec<String>>,
}

/// A segment of display content for human-facing UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "content")]
pub enum DisplaySegment {
    /// Plain text display.
    #[serde(rename = "text")]
    Text(String),
    /// A unified diff.
    #[serde(rename = "diff")]
    Diff(DiffContent),
}

/// Content for a diff display segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffContent {
    /// File path the diff applies to.
    pub path: String,
    /// Unified diff string.
    pub patch: String,
}

/// Build the prioritized display segments list: tool-provided `display_as`
/// segments first, then the raw `text` as a trailing `Text` fallback.
pub fn build_display_segments(
    display_as: Option<&[DisplaySegment]>,
    text: &str,
) -> Vec<DisplaySegment> {
    let mut segments: Vec<DisplaySegment> = display_as.map(|s| s.to_vec()).unwrap_or_default();
    segments.push(DisplaySegment::Text(text.to_owned()));
    segments
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapToolResult {
    pub group_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_as: Option<Vec<DisplaySegment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapUserChoice {
    pub group_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub prompt: String,
    pub choices: Vec<String>,
    #[serde(default)]
    pub default: usize,
    pub response_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapSubscriptionEvent {
    pub group_id: String,
    pub tool_call_id: String,
    pub text: String,
    #[serde(default)]
    pub associative: bool,
    /// When `true`, this is the final event for this subscription. The runtime
    /// SHOULD remove the subscription from its active tracking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#final: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapViewUpdate {
    pub group_id: String,
    /// The type of view being updated (e.g. "diff").
    pub view_type: String,
    /// View-specific payload. The runtime passes this through to clients without interpretation.
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapOAuth {
    pub group_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub auth_url: String,
}

/// Tagged enum for all RAP callback message types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RapCallback {
    #[serde(rename = "tool_result")]
    ToolResult(RapToolResult),
    #[serde(rename = "subscription_event")]
    SubscriptionEvent(RapSubscriptionEvent),
    #[serde(rename = "oauth")]
    OAuth(RapOAuth),
    #[serde(rename = "user_choice")]
    UserChoice(RapUserChoice),
    #[serde(rename = "view_update")]
    ViewUpdate(RapViewUpdate),
}

/// Request body for the `/tool_call_status` endpoint (runtime → tool server).
///
/// Asks the tool server whether a previously dispatched tool call — or the
/// subscription it established — is still alive (i.e. the server still
/// intends to deliver a tool result or further subscription events for it).
/// Runtimes use this after a restart to detect tool calls and subscriptions
/// that the tool server has given up on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapToolCallStatusRequest {
    /// The conversation thread identifier (`group_id`) of the thread
    /// containing the tool call. Matches the `group_id` sent in the original
    /// tool invocation.
    pub thread_id: String,
    /// The unique identifier of the tool call to query. Matches the `id`
    /// sent in the original tool invocation.
    pub tool_call_id: String,
}

/// Response body for the `/tool_call_status` endpoint (tool server → runtime).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RapToolCallStatusResponse {
    /// `true` if the server is still processing the tool call or still
    /// maintains an active subscription for it; `false` if the server has no
    /// record of the tool call (e.g. it was lost in a restart, completed
    /// long ago, or was cancelled).
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsetManifest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub endpoint: String,
    pub tools: Vec<ToolDef>,
    #[serde(default, rename = "needsMigration")]
    pub needs_migration: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
    #[serde(
        default,
        rename = "displayScript",
        skip_serializing_if = "Option::is_none"
    )]
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
            .body(body.to_owned())
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
    display_as: Option<Vec<DisplaySegment>>,
    subscription: bool,
) {
    let result = RapCallback::ToolResult(RapToolResult {
        group_id: invocation.group_id.clone(),
        id: invocation.id.clone(),
        call_id: invocation.call_id.clone(),
        text: text.to_owned(),
        display_as,
        subscription: if subscription { Some(true) } else { None },
    });
    let body = serde_json::to_string(&result).expect("bug: failed to serialize tool result");
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
    let msg = RapCallback::UserChoice(RapUserChoice {
        group_id: invocation.group_id.clone(),
        id: invocation.id.clone(),
        call_id: invocation.call_id.clone(),
        prompt: prompt.to_owned(),
        choices,
        default,
        response_url: response_url.to_owned(),
    });
    let body = serde_json::to_string(&msg).expect("bug: failed to serialize user_choice");
    if let Err(e) = client.post_json(&invocation.callback_url, &body).await {
        tracing::error!("failed to send user_choice: {e}");
    }
}

/// Send a `subscription_event` callback.
pub async fn send_subscription_event<C: CallbackClient>(
    client: &C,
    callback_url: &str,
    group_id: String,
    tool_call_id: String,
    text: &str,
    associative: bool,
    r#final: bool,
) {
    let event = RapCallback::SubscriptionEvent(RapSubscriptionEvent {
        group_id,
        tool_call_id,
        text: text.to_owned(),
        associative,
        r#final: if r#final { Some(true) } else { None },
    });
    let body = serde_json::to_string(&event).expect("bug: failed to serialize subscription event");
    if let Err(e) = client.post_json(callback_url, &body).await {
        tracing::error!("failed to send subscription event: {e}");
    }
}

/// Send a `view_update` callback.
pub async fn send_view_update<C: CallbackClient>(
    client: &C,
    callback_url: &str,
    group_id: String,
    view_type: &str,
    content: serde_json::Value,
) {
    let msg = RapCallback::ViewUpdate(RapViewUpdate {
        group_id,
        view_type: view_type.to_owned(),
        content,
    });
    let body = serde_json::to_string(&msg).expect("bug: failed to serialize view_update");
    if let Err(e) = client.post_json(callback_url, &body).await {
        tracing::error!("failed to send view_update: {e}");
    }
}

/// Run a future and log if it panics, instead of silently swallowing the panic.
/// Use this to wrap fire-and-forget spawned tasks.
pub async fn log_panic<F: Future<Output = ()>>(name: &'static str, f: F) {
    use futures_util::FutureExt;
    if let Err(e) = AssertUnwindSafe(f).catch_unwind().await {
        let msg = if let Some(s) = e.downcast_ref::<&str>() {
            *s
        } else if let Some(s) = e.downcast_ref::<String>() {
            &**s
        } else {
            "unknown panic payload"
        };
        tracing::error!("task '{name}' panicked: {msg}");
    }
}
