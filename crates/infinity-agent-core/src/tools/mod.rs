pub mod cancel_subscription;
pub mod config;
pub mod rap_tool;
pub mod sleep;
pub mod thread;

use crate::traits::InputSender;
use async_trait::async_trait;
use rig::message::ToolResult;

/// Context passed to tool implementations — generic over platform backends.
#[derive(Clone)]
pub struct ToolContext<M: InputSender> {
    pub message_sender: M,
    pub group_id: String,
    pub input_queue_arn: String,
    pub callback_url: String,
    pub user_id: Option<String>,
    /// Full thread stack: [root, ..ancestors, current_thread].
    pub thread_stack: Vec<String>,
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
