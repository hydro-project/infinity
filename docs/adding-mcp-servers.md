# Adding MCP Servers to AgentZero

This guide shows how to add Model Context Protocol (MCP) servers to expose additional tools to the agent.

## Overview

Each MCP server is deployed as a separate Lambda function that:
1. Boots up the MCP server process via stdio
2. Exposes two tools: `{name}_list_tools()` and `{name}_invoke_tool()`
3. Handles MCP protocol communication
4. Returns results to the agent

## Step 1: Bundle the MCP Server

Create a dedicated directory for your MCP server Lambda with the server pre-installed:

```bash
mkdir -p lambda/mcp-github
cd lambda/mcp-github

# Copy the generic proxy code
cp ../mcp-server-proxy/index.mjs .

# Create package.json with the MCP server as a dependency
cat > package.json << 'EOF'
{
  "name": "mcp-github",
  "version": "1.0.0",
  "type": "module",
  "dependencies": {
    "@aws-sdk/client-sqs": "^3.0.0",
    "@modelcontextprotocol/server-github": "^0.5.0"
  }
}
EOF

# Install dependencies
npm install
```

## Step 2: Add CDK Infrastructure

In `cdk/lib/agentzero-leader-stack.ts`, add a new Lambda function for your MCP server:

```typescript
// GitHub MCP Server
const mcpGithubQueue = new sqs.Queue(this, 'McpGithubQueue', {
  queueName: 'agentzero-mcp-github',
  visibilityTimeout: cdk.Duration.seconds(60),
  retentionPeriod: cdk.Duration.days(4),
});

const mcpGithubFunction = new lambda.Function(this, 'McpGithubFunction', {
  functionName: 'agentzero-mcp-github',
  runtime: lambda.Runtime.NODEJS_20_X,
  handler: 'index.handler',
  code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/mcp-github')),
  timeout: cdk.Duration.seconds(60),
  memorySize: 512,
  environment: {
    MCP_SERVER_COMMAND: 'node',
    MCP_SERVER_ARGS: JSON.stringify(['node_modules/@modelcontextprotocol/server-github/dist/index.js']),
    MCP_SERVER_ENV: JSON.stringify({
      GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN || '',
    }),
  },
});

messageQueue.grantSendMessages(mcpGithubFunction);

mcpGithubFunction.addEventSource(
  new SqsEventSource(mcpGithubQueue, {
    batchSize: 1,
    reportBatchItemFailures: true,
  })
);

lambdaFunction.addEnvironment('MCP_GITHUB_QUEUE_URL', mcpGithubQueue.queueUrl);
mcpGithubQueue.grantSendMessages(lambdaFunction);
```

## Step 3: Register Tools in Rust

In `src/event_handler.rs`, add two `LambdaTool` instances to the `tool_impls` vector:

```rust
Box::new(LambdaTool {
    name: "github_list_tools".to_string(),
    description: "List all available GitHub API tools from the MCP server.".to_string(),
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
                "description": "Arguments to pass to the tool."
            }
        },
        "required": ["tool_name"]
    }),
    queue_url: std::env::var("MCP_GITHUB_QUEUE_URL").unwrap_or_default(),
}),
```

## Step 4: Configure Environment Variables

Add required environment variables to `cdk/.env`:

```bash
GITHUB_PERSONAL_ACCESS_TOKEN=ghp_xxxxxxxxxxxxx
```

## Step 5: Deploy

```bash
cd cdk
npm install
cdk deploy
```

## Example Usage

The agent can now:

1. **List available tools:**
   ```
   Agent: I'll check what GitHub tools are available.
   [Calls github_list_tools()]
   Result: Available tools:
   - create_issue
   - create_pull_request
   - fork_repository
   - ...
   ```

2. **Invoke a tool:**
   ```
   Agent: I'll create an issue.
   [Calls github_invoke_tool({
     tool_name: "create_issue",
     arguments: {
       owner: "myorg",
       repo: "myrepo",
       title: "Bug report",
       body: "Description"
     }
   })]
   Result: Created issue #123
   ```

## Common MCP Servers

### Filesystem
```bash
# In lambda/mcp-filesystem/package.json
{
  "dependencies": {
    "@aws-sdk/client-sqs": "^3.0.0",
    "@modelcontextprotocol/server-filesystem": "^0.5.0"
  }
}
```
```typescript
environment: {
  MCP_SERVER_COMMAND: 'node',
  MCP_SERVER_ARGS: JSON.stringify([
    'node_modules/@modelcontextprotocol/server-filesystem/dist/index.js',
    '/tmp'  // Allowed path in Lambda
  ]),
  MCP_SERVER_ENV: JSON.stringify({}),
}
```

### PostgreSQL
```bash
# In lambda/mcp-postgres/package.json
{
  "dependencies": {
    "@aws-sdk/client-sqs": "^3.0.0",
    "@modelcontextprotocol/server-postgres": "^0.5.0"
  }
}
```
```typescript
environment: {
  MCP_SERVER_COMMAND: 'node',
  MCP_SERVER_ARGS: JSON.stringify(['node_modules/@modelcontextprotocol/server-postgres/dist/index.js']),
  MCP_SERVER_ENV: JSON.stringify({
    POSTGRES_CONNECTION_STRING: 'postgresql://user:pass@host:5432/db',
  }),
}
```

### Slack
```bash
# In lambda/mcp-slack/package.json
{
  "dependencies": {
    "@aws-sdk/client-sqs": "^3.0.0",
    "@modelcontextprotocol/server-slack": "^0.5.0"
  }
}
```
```typescript
environment: {
  MCP_SERVER_COMMAND: 'node',
  MCP_SERVER_ARGS: JSON.stringify(['node_modules/@modelcontextprotocol/server-slack/dist/index.js']),
  MCP_SERVER_ENV: JSON.stringify({
    SLACK_BOT_TOKEN: 'xoxb-...',
  }),
}
```

### Python-based MCP Server (Custom Docker Image Required)

For Python MCP servers, you'll need to use a custom Docker image since Lambda doesn't include Python by default in Node.js runtimes:

```dockerfile
# Dockerfile
FROM public.ecr.aws/lambda/nodejs:20

# Install Python and uv
RUN yum install -y python3 python3-pip
RUN pip3 install uv

# Copy Lambda code
COPY index.mjs package.json ./
RUN npm install

CMD ["index.handler"]
```

```typescript
environment: {
  MCP_SERVER_COMMAND: 'uvx',
  MCP_SERVER_ARGS: JSON.stringify(['mcp-server-package']),
  MCP_SERVER_ENV: JSON.stringify({
    API_KEY: 'your-api-key',
  }),
}
```

## Tips

1. **Naming Convention**: Use `{server_name}_list_tools` and `{server_name}_invoke_tool` for consistency
2. **Bundle Dependencies**: Always bundle MCP servers with the Lambda (don't rely on `npx -y` in production)
3. **Timeout**: Adjust Lambda timeout based on expected tool execution time (60s is usually sufficient)
4. **Memory**: MCP servers with heavy dependencies may need more memory (512MB+)
5. **Environment Variables**: Store sensitive credentials in AWS Secrets Manager for production
6. **Testing**: Test MCP servers locally first using the MCP inspector or CLI tools
7. **Cold Starts**: First invocation will be slower as the MCP server boots up (~2-5 seconds)

## Troubleshooting

- **Lambda timeout**: Increase timeout in CDK configuration (default 60s should be sufficient)
- **MCP server not found**: Verify the path in `MCP_SERVER_ARGS` points to the correct entry point
- **Module not found**: Ensure `npm install` was run in the Lambda directory before deployment
- **Permission errors**: Check that environment variables are properly set and secrets are accessible
- **Protocol errors**: Verify MCP server supports stdio transport and protocol version 2024-11-05
- **Cold start issues**: Consider provisioned concurrency for frequently-used MCP servers

## Alternative: Using npx (Not Recommended for Production)

While `npx` is available in Node.js Lambda runtimes, it's not recommended because:
- Downloads packages on every cold start (slow and unreliable)
- Requires internet access from Lambda
- Can fail if npm registry is unavailable
- Increases cold start time significantly (10-30 seconds)

If you must use `npx` for testing:
```typescript
environment: {
  MCP_SERVER_COMMAND: 'npx',
  MCP_SERVER_ARGS: JSON.stringify(['-y', '@modelcontextprotocol/server-github']),
  // ...
}
```

But always bundle dependencies for production deployments.
