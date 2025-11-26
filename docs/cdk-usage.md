# CDK Usage Guide

This guide explains how to use the AgentZero CDK constructs to deploy your own AI agent infrastructure.

## Quick Start

```typescript
import { AgentZero, LambdaTool, CustomToolSet, LambdaMCPToolSet } from './tools';

const agent = new AgentZero(this, 'AgentZero');

// Add Slack integration
const api = new apigateway.RestApi(this, 'WebhookApi', {
  restApiName: 'AgentZero Webhooks',
  deployOptions: { stageName: 'prod' },
});
const slackWebhookUrl = agent.setupSlackIntegration(this, api);

// Add tools
new LambdaMCPToolSet(agent, 'GithubMcp', {
  name: 'github',
  command: 'npx',
  args: ['-y', '@modelcontextprotocol/server-github'],
  env: { GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN },
});
```

## Core Constructs

### AgentZero

The main construct that creates the AI agent infrastructure.

```typescript
const agent = new AgentZero(this, 'AgentZero', {
  codePath: './custom/path/to/lambda',  // Optional
  lambdaProps: {                         // Optional
    memorySize: 1024,
    timeout: cdk.Duration.minutes(10),
  },
});
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

### LambdaTool

A tool that forwards requests to a Lambda function via SQS.

```typescript
const weatherTool = new LambdaTool(agent, 'WeatherTool', {
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
  queueProps: {                          // Optional
    visibilityTimeout: cdk.Duration.seconds(60),
  },
  lambdaProps: {                         // Optional
    memorySize: 1024,
  },
});
```

**Automatically:**
- Creates Lambda function with MCP server proxy
- Creates SQS queue
- Sets up event source mapping
- Grants permissions
- Generates `{name}_list_tools` and `{name}_invoke_tool` methods

### MCPToolSet

Uses an existing Lambda handler for an MCP server.

```typescript
new MCPToolSet(agent, 'CustomMcp', {
  name: 'custom',
  handler: existingLambdaFunction,
});
```

## Utilities

### Slack Integration

```typescript
const api = new apigateway.RestApi(this, 'WebhookApi', {
  restApiName: 'AgentZero Webhooks',
  deployOptions: { stageName: 'prod' },
});

const slackWebhookUrl = agent.setupSlackIntegration(this, api);

new cdk.CfnOutput(this, 'SlackWebhookUrl', {
  value: slackWebhookUrl,
  description: 'Slack Event Subscription URL',
});
```

**Creates:**
- Slack receiver Lambda (webhook → agent input queue)
- Slack responder Lambda (agent output queue → Slack)
- API Gateway endpoint for Slack webhooks

## Example: Complete Stack

```typescript
export class MyAgentStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    // Create agent
    const agent = new AgentZero(this, 'AgentZero');

    // Setup Slack
    const api = new apigateway.RestApi(this, 'WebhookApi', {
      restApiName: 'My Agent Webhooks',
      deployOptions: { stageName: 'prod' },
    });
    const slackWebhookUrl = agent.setupSlackIntegration(this, api);

    // Add a custom tool
    const weatherFunction = new lambda.Function(this, 'WeatherFunction', {
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset('./lambda/weather-tool'),
    });

    const weatherTool = new LambdaTool(agent, 'WeatherTool', {
      name: 'get_weather',
      description: 'Get weather for a location',
      parameters: {
        type: 'object',
        properties: {
          location: { type: 'string', description: 'City name' },
        },
        required: ['location'],
      },
      handler: weatherFunction,
    });

    new CustomToolSet(agent, 'WeatherToolSet', [weatherTool]);

    // Add MCP servers
    new LambdaMCPToolSet(agent, 'GithubMcp', {
      name: 'github',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
      env: {
        GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN,
      },
    });

    // Outputs
    new cdk.CfnOutput(this, 'SlackWebhookUrl', {
      value: slackWebhookUrl,
    });
  }
}
```

## Deployment

1. Build the Rust Lambda:
```bash
cargo lambda build --release --arm64
```

2. Deploy the CDK stack:
```bash
cd cdk
npx cdk bootstrap  # First time only
npx cdk deploy
```

3. Update Lambda code only:
```bash
cargo lambda build --release --arm64
cargo lambda deploy agentzero-leader
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

## Best Practices

1. **Organize tools by feature** - Group related tools into CustomToolSets
2. **Use LambdaMCPToolSet for MCP servers** - Simplest way to add MCP functionality
3. **Set appropriate timeouts** - Match queue visibility timeout to Lambda timeout
4. **Use environment variables** - Keep secrets in environment variables, not code
5. **Monitor dead letter queues** - Check DLQs regularly for failed messages

## Troubleshooting

**Tools not appearing:**
- Check CloudWatch logs for the leader Lambda
- Verify `TOOLS_CONFIG` environment variable is set
- Ensure tool Lambda has permission to send to agent input queue

**Messages stuck in queue:**
- Check Lambda concurrency limits
- Verify queue visibility timeout is appropriate
- Check dead letter queue for failed messages

**Permission errors:**
- Ensure agent has permission to send to tool queues
- Ensure tool Lambdas have permission to send to agent input queue
- Check IAM roles in CloudWatch logs
