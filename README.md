# Infinity Agents

This project is a proof-of-concept of Infinity Agents: a new runtime and architecture for agents that can run indefinitely with zero resource usage when they are idle.

## Architecture

This prototype uses Lambda + SQS + EventBridge to enable agents that can sleep for arbitrary durations without consuming resources. When an agent needs to wait (for CI/CD subscriptions, long tool calls, user input, rate limits, etc.), it can immediately hibernate and consume zero resources. The agent resumes exactly where it left off when woken.

See [docs/architecture.md](docs/architecture.md) for details on the hibernation mechanism and system design.

## Quick Start

```bash
# Build Lambda
cargo lambda build --release --arm64

# Deploy
cd cdk
npx cdk deploy
```

## CDK Usage

```typescript
import { InfinityAgents, LambdaMCPToolSet } from './tools';

const agent = new InfinityAgents(this, 'Agent');

// Add MCP servers
new LambdaMCPToolSet(agent, 'GithubMcp', {
  name: 'github',
  command: 'npx',
  args: ['-y', '@modelcontextprotocol/server-github'],
  env: { GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN },
});

// Setup Slack
const api = new apigateway.RestApi(this, 'Api', { /* ... */ });
agent.setupSlackIntegration(this, api);
```

See [docs/cdk-usage.md](docs/cdk-usage.md) for complete CDK documentation.

## Key Features

- **Zero idle cost** - Lambda only runs when processing messages
- **Infinite sleep** - Agents can hibernate for hours/days/months via EventBridge Scheduler
- **Interruption handling** - User messages and subscription events wake sleeping agents immediately (in milliseconds)
- **Tool abstraction** - Each tool is an independent Lambda with its own queue
- **MCP support** - Wrap any MCP server as a tool set
- **Conversation state** - DynamoDB stores durable conversation history to ensure fault tolerance

## Project Structure

- `src/` - Rust leader Lambda (Bedrock streaming, tool orchestration)
- `cdk/` - CDK infrastructure and tool abstractions
- `lambda/` - Tool implementations (Node.js)
- `docs/` - Architecture and usage documentation
