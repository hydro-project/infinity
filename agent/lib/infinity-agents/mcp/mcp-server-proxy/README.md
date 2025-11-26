# MCP Server Proxy

Generic Lambda function that boots up an MCP server via stdio and exposes its tools through two operations:
- `*_list_tools()` - Lists all available tools from the MCP server
- `*_invoke_tool(tool_name, args)` - Invokes a specific tool with arguments

## How It Works

1. Lambda receives SQS message with operation (`list_tools` or `invoke_tool`)
2. Spawns MCP server process with configured command/args
3. Initializes MCP protocol connection via stdio
4. Sends appropriate MCP request (tools/list or tools/call)
5. Formats response and sends to agent input queue
6. Terminates MCP server process

## Configuration

Each MCP server instance needs its own Lambda deployment with environment variables:

- `MCP_SERVER_COMMAND` - Command to run (e.g., `npx`, `uvx`, `node`)
- `MCP_SERVER_ARGS` - JSON array of arguments (e.g., `["-y", "@modelcontextprotocol/server-github"]`)
- `MCP_SERVER_ENV` - JSON object of environment variables (e.g., `{"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_..."}`)

## Example: GitHub API MCP Server

### Environment Variables
```bash
MCP_SERVER_COMMAND=npx
MCP_SERVER_ARGS=["-y", "@modelcontextprotocol/server-github"]
MCP_SERVER_ENV={"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_xxxxx"}
```

### Tool Registration (Rust)
```rust
Box::new(LambdaTool {
    name: "github_list_tools".to_string(),
    description: "List all available GitHub API tools.".to_string(),
    parameters: serde_json::json!({
        "type": "object",
        "properties": {},
        "required": []
    }),
    queue_url: std::env::var("MCP_GITHUB_QUEUE_URL").unwrap_or_default(),
}),
Box::new(LambdaTool {
    name: "github_invoke_tool".to_string(),
    description: "Invoke a GitHub API tool (e.g., create_issue, create_pull_request, etc.).".to_string(),
    parameters: serde_json::json!({
        "type": "object",
        "properties": {
            "tool_name": {
                "type": "string",
                "description": "Name of the GitHub tool to invoke."
            },
            "arguments": {
                "type": "object",
                "description": "Arguments for the tool."
            }
        },
        "required": ["tool_name"]
    }),
    queue_url: std::env::var("MCP_GITHUB_QUEUE_URL").unwrap_or_default(),
}),
```

## Example: Filesystem MCP Server

### Environment Variables
```bash
MCP_SERVER_COMMAND=npx
MCP_SERVER_ARGS=["-y", "@modelcontextprotocol/server-filesystem", "/allowed/path"]
MCP_SERVER_ENV={}
```

## Supported MCP Servers

Any MCP server that implements the stdio transport can be used:
- `@modelcontextprotocol/server-github` - GitHub API
- `@modelcontextprotocol/server-filesystem` - File operations
- `@modelcontextprotocol/server-postgres` - PostgreSQL queries
- `@modelcontextprotocol/server-slack` - Slack API
- Custom MCP servers

## Message Format

The Lambda expects SQS messages with this structure:

```json
{
  "arguments": {},
  "id": "tool-call-id",
  "call_id": "optional-call-id",
  "input_queue_url": "https://sqs.region.amazonaws.com/account/queue",
  "input_queue_arn": "arn:aws:sqs:region:account:queue",
  "group_id": "conversation-group-id",
  "operation": "list_tools"
}
```

For `invoke_tool` operation:
```json
{
  "arguments": {
    "tool_name": "create_issue",
    "arguments": {
      "owner": "myorg",
      "repo": "myrepo",
      "title": "Bug report",
      "body": "Description"
    }
  },
  "operation": "invoke_tool",
  ...
}
```
