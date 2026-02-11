---
sidebar_position: 1
title: Getting Started
---

# Getting Started

The fastest way to get a RAP agent running is with the Infinity Runtime's CDK framework. It provisions the full stack in a single `cdk deploy`.

## Prerequisites

- Node.js 20+
- AWS CDK CLI (`npm install -g aws-cdk`)
- Rust and [cargo-lambda](https://www.cargo-lambda.info/) (`brew install cargo-lambda/tap/cargo-lambda`)
- An AWS account with Bedrock model access enabled

## Deploy

```bash
git clone https://github.com/hydro-project/infinity
cd agent
npm install
npx cdk deploy
```

This deploys the agent runtime (Rust Lambda), SQS FIFO input queue, callback endpoint (Lambda Function URL), Aurora DSQL cluster for conversation history, EventBridge Scheduler role for timed hibernation, and a delay relay Lambda for short sleeps.

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
    new GitHubEventToolSet(this, 'GitHubEvents', { webhookGateway: api });
    new FinanceToolSet(this, 'Finance');
  }
}
```

The framework handles wiring — Function URLs, IAM permissions, tool configuration injection into the runtime.

## What's next

- [Build a RAP Tool](/docs/using-rap/building-a-rap-tool) — create a custom tool that speaks RAP
- [Build a Runtime](/docs/using-rap/building-a-runtime) — implement your own RAP-compatible runtime
- [Built-in Tools](/docs/infinity-runtime/built-in-tools) — sleep, threading, and utility tools that ship with the Infinity Runtime
- [Specification](/spec/overview) — the full protocol reference
