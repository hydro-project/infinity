---
sidebar_position: 3
title: Adding RAP & MCP Servers
---

# Adding RAP & MCP Servers

There are three ways to add tools to your agent, all exported by the `infinity-agents-cdk` package. `RapToolSet` connects a native RAP tool server (a Lambda or a remote URL), which serves `/.well-known/rap-toolset` so that the runtime can fetch its tool definitions at startup. `HTTPMCPToolSet` and `LambdaMCPToolSet` wrap an MCP server in a proxy Lambda that translates between the two protocols; see [MCP Compatibility](/docs/rap/about/mcp-compatibility) for how the translation works.

All three constructs will automatically handle Function URL creation, IAM permissions (SigV4 auth between the agent Lambda and the tool Lambdas), and tool configuration registration. Each construct takes the `InfinityAgent` (or a construct nested under it) as its scope.

## RapToolSet

`RapToolSet` is the most common way to add tools. You should use it when you have a tool server that implements the RAP protocol, which means that it serves a toolset definition at `/.well-known/rap-toolset` and handles invocations asynchronously.

For a Lambda-based tool server, you can pass the handler directly. The construct will create a Function URL with IAM auth and response streaming, and will wire up all of the permissions automatically:

```typescript
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as cdk from 'aws-cdk-lib';
import * as path from 'path';
import { RapToolSet } from 'infinity-agents-cdk';

const weatherFunction = new lambda.Function(this, 'WeatherTool', {
  runtime: lambda.Runtime.NODEJS_24_X,
  handler: 'index.handler',
  code: lambda.Code.fromAsset(path.join(__dirname, 'weather-tool')),
  timeout: cdk.Duration.seconds(30),
});

new RapToolSet(agent, 'WeatherTools', {
  handler: weatherFunction,
});
```

For tool servers hosted outside your CDK stack (e.g. in another account, a third-party service, or a container), you can pass the base URL instead:

```typescript
new RapToolSet(agent, 'ExternalTools', {
  serverUrl: 'https://tools.example.com',
});
```

The runtime will fetch the toolset definition from `https://tools.example.com/.well-known/rap-toolset` at startup, and will dispatch invocations to whatever `endpoint` the toolset declares.

## HTTPMCPToolSet

`HTTPMCPToolSet` connects to a remote MCP server over HTTP (Streamable HTTP transport). A proxy Lambda translates between the MCP protocol and RAP, so the runtime treats it like any other tool server. The construct also supports OAuth for servers that require user authentication.

```typescript
import { HTTPMCPToolSet } from 'infinity-agents-cdk';

// Simple HTTP MCP server (no auth)
new HTTPMCPToolSet(agent, 'SlackMcp', {
  name: 'slack',
  url: 'https://mcp.slack.com/sse',
  headers: {
    'Authorization': 'Bearer xoxb-your-bot-token',
  },
});
```

When OAuth is configured, the construct will create a DynamoDB table for token storage and an API Gateway callback endpoint:

```typescript
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import { HTTPMCPToolSet } from 'infinity-agents-cdk';

const gateway = new apigateway.RestApi(this, 'WebhookApi');

new HTTPMCPToolSet(agent, 'GithubMcp', {
  name: 'github',
  url: 'https://api.githubcopilot.com/mcp/',
  oauth: {
    callbackGateway: gateway,
    stageName: 'prod',
    clientId: process.env.GITHUB_OAUTH_CLIENT_ID,
    clientSecret: process.env.GITHUB_OAUTH_CLIENT_SECRET,
  },
});
```

Set the OAuth credentials in the environment before deploying:

```bash
export GITHUB_OAUTH_CLIENT_ID=your-client-id
export GITHUB_OAUTH_CLIENT_SECRET=your-client-secret
```

## LambdaMCPToolSet

`LambdaMCPToolSet` runs an MCP server as a stdio subprocess inside a Lambda proxy. This is useful for MCP servers that are distributed as CLI tools (e.g. `npx` packages) and do not expose an HTTP endpoint.

```typescript
import { LambdaMCPToolSet } from 'infinity-agents-cdk';

new LambdaMCPToolSet(agent, 'FileSystemMcp', {
  name: 'filesystem',
  command: ['npx', '-y', '@modelcontextprotocol/server-filesystem', '/tmp'],
});
```

You can also pass environment variables and custom Lambda configuration:

```typescript
new LambdaMCPToolSet(agent, 'DatabaseMcp', {
  name: 'database',
  command: ['npx', '-y', '@modelcontextprotocol/server-postgres'],
  env: {
    POSTGRES_CONNECTION_STRING: process.env.POSTGRES_CONNECTION_STRING,
  },
  lambdaProps: {
    memorySize: 1024,
    timeout: cdk.Duration.seconds(120),
  },
});
```

## Example Tool Sets

The Infinity repo's [example agent](https://github.com/hydro-project/infinity/blob/main/agent/lib/example-agent.ts) wires up several complete tool sets under `agent/lib/toolsets/` that you can use as references for your own:

- **`GetTimeToolSet`**: a minimal RAP tool server that returns the current time in any timezone, and the simplest reference implementation.
- **`Ec2ToolSet`**: launches EC2 instances and monitors their state transitions via a subscription, so the agent can hibernate while an instance boots.
- **`GitHubEventToolSet`**: `subscribe_github_events` backed by an API Gateway webhook receiver, plus a GitHub Actions status checker.
- **`FinanceToolSet`**: stock price subscriptions driven by a poller, plus paper-trading tools.
- **`SandboxToolSet`**: cloud sandboxed coding tools (the same sandbox server Infinity Code uses locally, deployed via the `sandbox-remote` crate) as a container-image Lambda with git and jj, backed by EFS for repository storage and DynamoDB for metadata. It creates a VPC unless you pass one in.

To implement your own tool server, see [Build a RAP Tool](/docs/rap/using-rap/building-a-rap-tool). [MCP Compatibility](/docs/rap/about/mcp-compatibility) documents how the proxy Lambdas translate between MCP and RAP.
