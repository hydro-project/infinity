pub mod cancel_subscription;
pub mod config;
pub mod rap_tool;
pub mod sleep;
pub mod thread;

use crate::message::{InputMessage, InputMessageContent};
use crate::traits::InputSender;
use async_trait::async_trait;
use infinity_provider_protocol::message::Text;
use infinity_provider_protocol::message::{ToolResult, ToolResultContent, UserContent};

pub(crate) type ToolError = Box<dyn std::error::Error + Send + Sync>;

/// Enqueue an error as the result of a tool call so the agent can recover.
pub(crate) async fn send_tool_error<M: InputSender>(
    context: &ToolContext<M>,
    id: &str,
    call_id: Option<String>,
    error: impl Into<String>,
) -> Result<(), ToolError>
where
    M::Error: 'static,
{
    let message = InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
            id: id.to_owned(),
            call_id,
            content: vec![ToolResultContent::Text(Text {
                text: format!("Error: {}", error.into()),
            })],
        })),
        group_id: context.group_id.clone(),
        metadata: None,
        synthetic: None,
        display_as: None,
        subscription: false,
    };

    context
        .message_sender
        .send_to_input_queue(message, id)
        .await
        .map_err(|error| Box::new(error) as ToolError)
}

/// Context passed to tool implementations — generic over platform backends.
#[derive(Clone)]
pub struct ToolContext<M: InputSender> {
    pub message_sender: M,
    pub group_id: rap_protocol::ThreadId,
    pub callback_url: String,
    pub user_id: Option<String>,
    /// Full thread stack: [root, ..ancestors, current_thread].
    pub thread_stack: Vec<rap_protocol::ThreadId>,
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

    fn supports_sync(&self) -> bool {
        false
    }

    /// Whether a dispatched call to this tool represents *waiting* rather
    /// than in-flight work (e.g. the sleep tools). While a thread's last
    /// history entry is an unanswered call to a non-passive tool, deferrable
    /// synthetic events are held back so they don't interrupt the running
    /// call; calls to passive tools may be interrupted freely.
    fn is_passive(&self) -> bool {
        false
    }

    /// Optional Rhai script for pretty-printing this tool call.
    fn display_script(&self) -> Option<&str> {
        None
    }

    /// Execute the tool synchronously, returning results that should be
    /// injected into the conversation history immediately. When this returns
    /// `Some`, `execute` will not be called — the returned messages are
    /// processed inline and the completion loop continues. This avoids race
    /// conditions where a concurrent event can make the tool call appear
    /// cancelled even though it already launched.
    async fn execute_synchronous(
        &self,
        _args: &serde_json::Value,
        _id: &str,
        _call_id: Option<&str>,
        _context: &ToolContext<M>,
    ) -> Option<ToolResult> {
        None
    }
}

/// Evaluate a Rhai display script with tool arguments as scope variables.
/// Returns `Some(pretty_string)` on success, `None` if script is absent or fails.
pub fn eval_display_script(script: Option<&str>, args: &serde_json::Value) -> Option<String> {
    let script = script?;
    let engine = rhai::Engine::new();
    let mut scope = rhai::Scope::new();
    let mut map = rhai::Map::new();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            let val: rhai::Dynamic = match v {
                serde_json::Value::String(s) => s.clone().into(),
                serde_json::Value::Bool(b) => (*b).into(),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        i.into()
                    } else if let Some(f) = n.as_f64() {
                        f.into()
                    } else {
                        continue;
                    }
                }
                other => other.to_string().into(),
            };
            map.insert(k.as_str().into(), val);
        }
    }
    scope.push("args", map);
    engine.eval_with_scope::<String>(&mut scope, script).ok()
}
