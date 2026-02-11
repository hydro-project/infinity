use super::{Tool, ToolSet, lambda_tool::LambdaTool};

/// Abstraction for MCP servers wrapped as Lambda functions.
/// The leader invokes the MCP proxy Lambda via HTTP (Function URL with IAM auth).
pub struct LambdaMCP {
    pub name: String,
    pub function_url: String,
}

impl LambdaMCP {
    pub fn new(name: impl Into<String>, function_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            function_url: function_url.into(),
        }
    }
}

impl ToolSet for LambdaMCP {
    fn into_tools(self: Box<Self>) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(LambdaTool {
                name: format!("{}_list_tools", self.name),
                description: format!(
                    "List all available tools from the {} MCP server. Performs OAuth if required by the server. If you are assigned a task that will require autonomous actions in the future, you should use this tool to get auth before sleeping.",
                    self.name
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
                function_url: self.function_url.clone(),
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
                function_url: self.function_url.clone(),
            }),
        ]
    }
}
