use super::{lambda_tool::LambdaTool, Tool, ToolSet};

/// Abstraction for MCP servers wrapped as Lambda functions
/// This provides a consistent interface for tools that proxy to MCP servers
pub struct LambdaMCP {
    pub name: String,
    pub queue_url: String,
}

impl LambdaMCP {
    pub fn new(name: impl Into<String>, queue_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            queue_url: queue_url.into(),
        }
    }
}

impl ToolSet for LambdaMCP {
    fn into_tools(self: Box<Self>) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(LambdaTool {
                name: format!("{}_list_tools", self.name),
                description: format!(
                    "List all available tools from the {} MCP server.",
                    self.name
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
                queue_url: self.queue_url.clone(),
            }),
            Box::new(LambdaTool {
                name: format!("{}_invoke_tool", self.name),
                description: format!(
                    "Invoke a tool from the {} MCP server. Use {}_list_tools first to see available tools.",
                    self.name, self.name
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tool_name": {
                            "type": "string",
                            "description": "Name of the tool to invoke (e.g., 'create_issue', 'create_pull_request')."
                        },
                        "arguments": {
                            "type": "object",
                            "description": "Arguments to pass to the tool as a JSON object. Structure depends on the specific tool."
                        }
                    },
                    "required": ["tool_name"]
                }),
                queue_url: self.queue_url.clone(),
            }),
        ]
    }
}
