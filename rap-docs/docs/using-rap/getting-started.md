---
sidebar_position: 1
title: Getting Started
---

# Getting Started

The fastest way to get a RAP agent running is with the Infinity Agents CDK framework. It provisions the full stack — agent runtime, input/output queues, RAP receiver, conversation storage, and hibernation infrastructure — in a single `cdk deploy`.

## Prerequisites

- Node.js 20+
- AWS CDK CLI (`npm install -g aws-cdk`)
- Rust and [cargo-lambda](https://www.cargo-lambda.info/) (`brew install cargo-lambda/tap/cargo-lambda`)
- An AWS account with Bedrock model access enabled

## Deploy the reference agent

```bash
git clone https://github.com/your-org/rap
cd rap/agent
npm install
npx cdk deploy
```

This deploys:

- A Rust Lambda function as the agent runtime (processes messages from the input queue, runs Bedrock completions, dispatches tool calls)
- SQS FIFO input queue and standard output queue
- RAP receiver Lambda with Function URL (accepts tool results and subscription events)
- Aurora DSQL cluster for conversation history
- EventBridge Scheduler role for hibernation
- Delay relay Lambda for short sleeps

## Define your agent

Agents are CDK constructs. Extend `InfinityAgent` and add tools:

```typescript
import { InfinityAgent } from './infinity-agents';
import { LambdaMCPToolSet } from './infinity-agents/mcp';
import { FinanceToolSet } from './toolsets/finance/toolset';
import { GitHubEventToolSet } from './toolsets/github-event/toolset';

export class MyAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);

    // MCP tools (wrapped automatically)
    new LambdaMCPToolSet(this, 'GitHub', {
      name: 'github',
      command: ['npx', '-y', '@modelcontextprotocol/server-github'],
      env: { GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_TOKEN },
    });

    // Native RAP tools with subscriptions
    new GitHubEventToolSet(this, 'GitHubEvents', {
      webhookGateway: api,
    });

    new FinanceToolSet(this, 'Finance');
  }
}
```

The framework handles wiring — Function URLs, IAM permissions, queue subscriptions, tool configuration injection into the runtime via DynamoDB.

## Built-in tools

Every Infinity Agent comes with these tools automatically:

| Tool | Purpose |
|---|---|
| `sleep` | Hibernate for N seconds. Uses SQS delays (≤15 min) or EventBridge Scheduler (longer). |
| `sleep_until` | Hibernate until a specific wall-clock time in any timezone. |
| `sleep_until_event_or_input` | Hibernate indefinitely until an event or user message arrives. |
| `spawn_thread` | Create a child thread for parallel work. |
| `report_to_parent` | Send intermediate results to the parent thread. |
| `close_thread` | Shut down the current thread, optionally reporting to the parent. |

## What's next

- [Build a RAP Tool](/docs/using-rap/building-a-rap-tool) — create a custom tool that speaks RAP
- [Build a Runtime](/docs/using-rap/building-a-runtime) — implement your own RAP-compatible agent runtime
- [Specification](/spec/overview) — the full protocol reference
