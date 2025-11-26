# AgentZero CDK Tools Abstraction

This directory contains CDK constructs that abstract tool and tool set creation, automatically handling infrastructure setup and configuration generation.

## Overview

The abstraction mirrors the Rust tool structure:
- `Tool` - Abstract base class for individual tools
- `ToolSet` - Abstract base class for grouped tools
- `AgentZero` - Main construct that manages the leader Lambda and tools

## Classes

### AgentZero

The main construct that creates the leader Lambda and manages tools.

```typescript
const agent = new AgentZero(this, 'AgentZero', {
  historyTable,
  inputQueue: messageQueue,
  outputQueue,
  schedulerRole,
});

// Add tool sets (they auto-register and update config)
agent.addToolSet(myToolSet);
```

**Features:**
- Creates the leader Lambda function
- Automatically grants permissions (DynamoDB, Bedrock, Scheduler, etc.)
- Manages tool configuration via `TOOLS_CONFIG` environment variable
- Grants queue permissions when tools are added

### LambdaTool

A tool that forwards requests to a Lambda function via SQS.

```typescript
const myTool = new LambdaTool(agent, 'MyTool', {
  name: 'my_tool',
  description: 'Does something useful',
  parameters: {
    type: 'object',
    properties: {
      input: {
        type: 'string',
        description: 'Input parameter',
      },
    },
    required: ['input'],
  },
  handler: myLambdaFunction,
  queueProps: {
    visibilityTimeout: cdk.Duration.seconds(30),
  },
});
```

**Automatically:**
- Creates SQS queue for the tool
- Creates event source mapping from queue to handler Lambda
- Grants agent permission to send to the queue
- Grants handler permission to send to agent's input queue
- Generates tool configuration

### CustomToolSet

A collection of individual tools.

```typescript
new CustomToolSet(agent, 'MyToolSet', [tool1, tool2, tool3]);
```

**Automatically registers** with the agent during construction.

### MCPToolSet

An MCP server that automatically creates `list_tools` and `invoke_tool` methods.

```typescript
new MCPToolSet(agent, 'GithubMcp', {
  name: 'github',
  handler: mcpProxyLambda,
  queueProps: {
    visibilityTimeout: cdk.Duration.seconds(60),
  },
});
```

**Automatically:**
- Creates SQS queue for the MCP server
- Creates event source mapping from queue to handler Lambda
- Grants agent permission to send to the queue
- Grants handler permission to send to agent's input queue
- Generates tool set configuration with `github_list_tools` and `github_invoke_tool`
- Registers with the agent

## Example Usage

```typescript
import { AgentZero, LambdaTool, CustomToolSet, MCPToolSet } from './tools';

// Create the agent
const agent = new AgentZero(this, 'AgentZero', {
  historyTable,
  inputQueue: messageQueue,
  outputQueue,
  schedulerRole,
});

// Create a Lambda function that implements a tool
const weatherFunction = new lambda.Function(this, 'WeatherFunction', {
  runtime: lambda.Runtime.NODEJS_20_X,
  handler: 'index.handler',
  code: lambda.Code.fromAsset('./lambda/weather-tool'),
});

// Create the tool (automatically creates queue, wiring, and permissions)
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
  handler: weatherFunction,
});

// Add to agent (auto-registers)
new CustomToolSet(agent, 'WeatherToolSet', [weatherTool]);

// Add an MCP server
const slackMcpFunction = new lambda.Function(this, 'SlackMcpFunction', {
  runtime: lambda.Runtime.NODEJS_20_X,
  handler: 'index.handler',
  code: lambda.Code.fromAsset('./lambda/mcp-server-proxy'),
  environment: {
    MCP_SERVER_COMMAND: 'npx',
    MCP_SERVER_ARGS: JSON.stringify(['-y', '@modelcontextprotocol/server-slack']),
  },
});

new MCPToolSet(agent, 'SlackMcp', {
  name: 'slack',
  handler: slackMcpFunction,
});
```

## Benefits

1. **Less boilerplate** - No need to manually create queues, event sources, or permissions
2. **Self-registering** - Tools and tool sets automatically register with the agent
3. **Automatic permissions** - Queue permissions are granted automatically
4. **Type safety** - TypeScript ensures correct configuration
5. **Consistency** - All tools follow the same pattern
6. **Automatic config generation** - Tools configuration is built from the constructs
7. **Easy to extend** - Create new tool types by extending `Tool` or `ToolSet`

## Creating Custom Tool Types

To create a new tool type, extend the `Tool` class:

```typescript
export class HttpTool extends Tool {
  constructor(scope: Construct, id: string, props: HttpToolProps) {
    super();
    // Setup infrastructure
  }

  toConfig(): ToolConfig {
    return {
      type: 'http',
      // ... config
    };
  }

  getQueue(): sqs.IQueue | undefined {
    return undefined; // or return a queue if needed
  }
}
```

## Migration from Old Stack

The refactored stack (`agentzero-leader-stack-refactored.ts`) shows how to use the new abstractions. Key differences:

**Old way:**
```typescript
// Manually create queue
const queue = new sqs.Queue(this, 'Queue', { ... });

// Manually create event source
handler.addEventSource(new SqsEventSource(queue, { ... }));

// Manually grant permissions
queue.grantSendMessages(lambdaFunction);
messageQueue.grantSendMessages(handler);

// Manually build config
const toolsConfig = {
  tool_sets: [
    {
      type: 'vec',
      tools: [
        {
          type: 'lambda',
          name: 'my_tool',
          queue_url: queue.queueUrl,
          // ...
        },
      ],
    },
  ],
};
lambdaFunction.addEnvironment('TOOLS_CONFIG', JSON.stringify(toolsConfig));
```

**New way:**
```typescript
const tool = new LambdaTool(agent, 'MyTool', {
  name: 'my_tool',
  description: '...',
  parameters: { ... },
  handler: myFunction,
});

new CustomToolSet(agent, 'MyToolSet', [tool]);
```

Everything else (queue, event source, permissions, config) is handled automatically! The `TOOLS_CONFIG` environment variable is updated each time a tool set is registered.
