# Tool Configuration

AgentZero supports loading tool configurations from a JSON file, making it easy to add, remove, or modify tools without changing code.

## Configuration

The system loads tool configurations from the `TOOLS_CONFIG` environment variable, which is automatically set by the CDK during deployment with actual queue URLs.

For local development, you can use a `tools.json` file instead. Set the `TOOLS_CONFIG_PATH` environment variable to specify a custom path (defaults to `tools.json`).

## Configuration Format

The configuration is defined in the CDK stack and passed to the Lambda via the `TOOLS_CONFIG` environment variable:

```json
{
  "tool_sets": [
    {
      "type": "vec",
      "tools": [
        {
          "type": "lambda",
          "name": "tool_name",
          "description": "Tool description",
          "parameters": { ... },
          "queue_url": "https://sqs.region.amazonaws.com/account/queue-name"
        }
      ]
    },
    {
      "type": "mcp",
      "name": "github",
      "queue_url": "https://sqs.region.amazonaws.com/account/mcp-github-queue"
    }
  ]
}
```

## Tool Set Types

### Vec Tool Set (`vec`)

A collection of individual tools:

```json
{
  "type": "vec",
  "tools": [
    {
      "type": "lambda",
      "name": "tool_name",
      "description": "Tool description",
      "parameters": { ... },
      "queue_url": "https://sqs.region.amazonaws.com/account/queue-name"
    }
  ]
}
```

### MCP Tool Set (`mcp`)

Automatically creates `list_tools` and `invoke_tool` methods for an MCP server:

```json
{
  "type": "mcp",
  "name": "github",
  "queue_url": "https://sqs.region.amazonaws.com/account/mcp-github-queue"
}
```

This creates two tools:
- `github_list_tools` - Lists all available tools from the MCP server
- `github_invoke_tool` - Invokes a specific tool from the MCP server

## Tool Types

### Lambda Tool (`lambda`)

A single tool that forwards requests to another Lambda function via SQS:

```json
{
  "type": "lambda",
  "name": "get_time",
  "description": "Get the current time in a specified timezone or UTC.",
  "parameters": {
    "type": "object",
    "properties": {
      "timezone": {
        "type": "string",
        "description": "IANA timezone name"
      }
    },
    "required": []
  },
  "queue_url": "https://sqs.region.amazonaws.com/account/queue-name"
}
```

Fields:
- `name` - Tool name (used by the agent to invoke it)
- `description` - Description shown to the agent
- `parameters` - JSON Schema defining the tool's parameters
- `queue_url` - SQS queue URL (resolved from CDK queue object)

## Hardcoded Tools

The `sleep` tool is hardcoded and always available. It doesn't need to be configured in the JSON file.

## Adding a New Tool

1. Create the Lambda function that implements the tool
2. Create an SQS queue for the tool in the CDK stack
3. Add the tool configuration to the `toolsConfig` object in the CDK stack
4. Deploy

Example for adding a new "weather" tool in the CDK stack:

```typescript
// Create the queue
const weatherToolQueue = new sqs.Queue(this, 'WeatherToolQueue', {
  queueName: 'agentzero-weather-tool',
  visibilityTimeout: cdk.Duration.seconds(30),
  retentionPeriod: cdk.Duration.days(4),
});

// Add to toolsConfig.tool_sets array:
{
  type: 'vec',
  tools: [
    {
      type: 'lambda',
      name: 'get_weather',
      description: 'Get current weather for a location',
      parameters: {
        type: 'object',
        properties: {
          location: {
            type: 'string',
            description: 'City name or coordinates',
          },
        },
        required: ['location'],
      },
      queue_url: weatherToolQueue.queueUrl,
    },
  ],
}
```

## Adding a New MCP Server

To add a new MCP server (e.g., Slack), add it to the CDK stack:

```typescript
// Create the MCP queue and Lambda
const mcpSlackQueue = new sqs.Queue(this, 'McpSlackQueue', {
  queueName: 'agentzero-mcp-slack',
  visibilityTimeout: cdk.Duration.seconds(60),
  retentionPeriod: cdk.Duration.days(4),
});

// Add to toolsConfig.tool_sets array:
{
  type: 'mcp',
  name: 'slack',
  queue_url: mcpSlackQueue.queueUrl,
}
```

This automatically creates `slack_list_tools` and `slack_invoke_tool` methods.
