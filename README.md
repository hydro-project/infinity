# Infinity Agents

This project is a proof-of-concept of Infinity Agents: a new runtime and architecture for agents that can run indefinitely with zero resource usage when they are idle.

## Architecture

This prototype uses Lambda + SQS + EventBridge to enable agents that can sleep for arbitrary durations without consuming resources. When an agent needs to wait (for CI/CD subscriptions, long tool calls, user input, rate limits, etc.), it can immediately hibernate and consume zero resources. The agent resumes exactly where it left off when woken.

See [the docs](docs/) for details on the hibernation mechanism and system design.

## Quick Start

```bash
# Install cargo-lambda
brew install cargo-lambda/tap/cargo-lambda

# Deploy
cd agent
npx cdk deploy
```

## Creating Agents
Infinity Agents are created using the CDK, with a special Infinity Agents framework:

```typescript
import { InfinityAgent } from './infinity-agents';
import { LambdaMCPToolSet } from './infinity-agents/mcp';

export class MyAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);

    // Add MCP servers
    new LambdaMCPToolSet(this, 'GithubMcp', {
      name: 'github',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
      env: { GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN },
    });

    // Setup Slack
    const api = new apigateway.RestApi(this, 'Api', { /* ... */ });
    this.setupSlackIntegration(this, api);
  }
}
```

See [CDK docs](docs/docs/infinity-runtime/cloud-deployment) for complete CDK documentation.

## Key Features

- **Zero idle cost** - Lambda only runs when processing messages
- **Infinite sleep** - Agents can hibernate for hours/days/months via EventBridge Scheduler
- **Interruption handling** - User messages and subscription events wake sleeping agents immediately (in milliseconds)
- **Tool abstraction** - Each tool is an independent Lambda with its own queue
- **MCP support** - Wrap any MCP server as a tool set
- **Conversation state** - DynamoDB stores durable conversation history to ensure fault tolerance
- **Threaded execution** - Agents can spawn nested child threads to decompose tasks into parallel sub-work, each with inherited context truncated at the spawn point. See [Threading](docs/docs/infinity-runtime/threading) for details.

## Project Structure
- `src/` - Infinity Agent Leader (Rust, Bedrock streaming, tool orchestration)
- `agent/lib/infinity-agents` - Infinity Agents SDK
- `agent/lib/{example-agent.ts, toolsets}` - Example Agent and Custom Tools
- `docs/` - Architecture and usage documentation
