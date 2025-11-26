# CDK Usage Guide

This guide explains how to use the InfinityAgents CDK constructs to deploy your own AI agent infrastructure.

## Quick Start

The simplest way to create an agent is to extend `InfinityAgent`:

```typescript
import * as cdk from 'aws-cdk-lib';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import { Construct } from 'constructs';
import { InfinityAgent } from './infinity-agents';
import { LambdaMCPToolSet } from './infinity-agents/mcp';

export class MyAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);

    // Add MCP tools
    new LambdaMCPToolSet(this, 'GithubMcp', {
      name: 'github',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
      env: { GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN },
    });

    // Add Slack integration
    const api = new apigateway.RestApi(this, 'WebhookApi', {
      restApiName: 'My Agent Webhooks',
      deployOptions: { stageName: 'prod' },
    });
    const slackWebhookUrl = this.setupSlackIntegration(this, api);

    new cdk.CfnOutput(this, 'SlackWebhookUrl', { value: slackWebhookUrl });
  }
}

export class MyAgentStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);
    new MyAgent(this, 'MyAgent');
  }
}
```

## Core Constructs

### InfinityAgent

The base class for creating agents. Extend this to configure your agent with tools.

```typescript
export class MyAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);
    // Add tools here
  }
}
```

**Creates:**
- DynamoDB table for conversation history
- Input/output SQS queues with dead letter queues
- EventBridge Scheduler role for sleep functionality
- Leader Lambda function with Bedrock permissions

**Public properties:**
- `agent.lambdaFunction` - The leader Lambda function
- `agent.inputQueue` - SQS queue for incoming messages
- `agent.outputQueue` - SQS queue for agent responses
- `agent.historyTable` - DynamoDB table for conversation state

### Creating Custom Tool Sets

Organize tools into toolsets by creating a folder structure:

```
agent/lib/toolsets/
├── index.ts                    # Exports all toolsets
├── my-tools/
│   ├── toolset.ts              # ToolSet class
│   └── my-lambda/              # Lambda code
│       ├── index.mjs
│       └── package.json
```

Example toolset:

```typescript
// agent/lib/toolsets/weather/toolset.ts
import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as path from 'path';
import { CustomToolSet, LambdaTool, InfinityAgent } from '../../infinity-agents';

export class WeatherToolSet extends CustomToolSet {
  constructor(agent: InfinityAgent, id: string) {
    const weatherFunction = new lambda.Function(agent, 'WeatherFunction', {
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'weather-lambda')),
      timeout: cdk.Duration.seconds(30),
    });

    const weatherTool = new LambdaTool(agent, 'WeatherTool', {
      name: 'get_weather',
      description: 'Get current weather for a location',
      parameters: {
        type: 'object',
        properties: {
          location: { type: 'string', description: 'City name' },
        },
        required: ['location'],
      },
      handler: weatherFunction,
    });

    super(agent, id, [weatherTool]);
  }
}
```

Then use it in your agent:

```typescript
export class MyAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);
    new WeatherToolSet(this, 'WeatherToolSet');
  }
}
```

### LambdaTool

A tool that forwards requests to a Lambda function via SQS.

```typescript
const weatherTool = new LambdaTool(agent, 'WeatherTool', {
  name: 'get_weather',
  description: 'Get current weather for a location',
  parameters: {
    type: 'object',
    properties: {
      location: { type: 'string', description: 'City name' },
    },
    required: ['location'],
  },
  handler: weatherLambdaFunction,
  queueProps: {                          // Optional
    visibilityTimeout: cdk.Duration.seconds(30),
  },
});
```

**Automatically:**
- Creates SQS queue for the tool
- Sets up event source mapping
- Grants permissions (agent → queue, handler → agent input queue)
- Generates tool configuration

### CustomToolSet

Groups multiple tools together.

```typescript
new CustomToolSet(agent, 'MyToolSet', [tool1, tool2, tool3]);
```

### LambdaMCPToolSet

Creates an MCP server with automatic Lambda proxy setup.

```typescript
new LambdaMCPToolSet(agent, 'SlackMcp', {
  name: 'slack',
  command: 'npx',
  args: ['-y', '@modelcontextprotocol/server-slack'],
  env: {
    SLACK_BOT_TOKEN: process.env.SLACK_BOT_TOKEN,
  },
});
```

**Automatically:**
- Creates Lambda function with MCP server proxy
- Creates SQS queue
- Sets up event source mapping
- Grants permissions
- Generates `{name}_list_tools` and `{name}_invoke_tool` methods

## Slack Integration

```typescript
const api = new apigateway.RestApi(this, 'WebhookApi', {
  restApiName: 'My Agent Webhooks',
  deployOptions: { stageName: 'prod' },
});

const slackWebhookUrl = this.setupSlackIntegration(this, api);

new cdk.CfnOutput(this, 'SlackWebhookUrl', {
  value: slackWebhookUrl,
  description: 'Slack Event Subscription URL',
});
```

**Creates:**
- Slack receiver Lambda (webhook → agent input queue)
- Slack responder Lambda (agent output queue → Slack)
- API Gateway endpoint for Slack webhooks

## Deployment

1. Build the Rust Lambda:
```bash
cargo lambda build --release --arm64
```

2. Deploy the CDK stack:
```bash
cd agent
npx cdk bootstrap  # First time only
npx cdk deploy
```

3. Update Lambda code only:
```bash
cargo lambda build --release --arm64
cargo lambda deploy infinity-agents-leader
```

## Tool Implementation

When creating a Lambda tool, it receives messages with this structure:

```json
{
  "operation": "tool_name",
  "arguments": { "param1": "value1" },
  "id": "tool-call-id",
  "call_id": "optional-call-id",
  "input_queue_url": "https://sqs...",
  "input_queue_arn": "arn:aws:sqs:...",
  "group_id": "conversation-group-id"
}
```

The tool should send its result back to the agent:

```javascript
await sqs.sendMessage({
  QueueUrl: event.input_queue_url,
  MessageBody: JSON.stringify({
    content: {
      type: 'tool_result',
      id: event.id,
      call_id: event.call_id,
      content: [{ type: 'text', text: 'Result here' }],
    },
    group_id: event.group_id,
  }),
}).promise();
```
