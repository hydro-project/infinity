---
sidebar_position: 1
title: Getting Started
---

# Getting Started

The [Infinity Runtime](/docs/infinity-runtime/overview) is the reference agent runtime for RAP. There are two ways to run the Infinity Runtime: locally with the CLI for fast iteration, or deployed to AWS for durable, production-grade execution.

| | Local CLI | Cloud (Lambda) |
|---|---|---|
| State | In-memory, lost on exit | Persistent (Aurora DSQL) |
| Hibernation | Blocks in-process | RAP Callback wake-up |
| Tool servers | Local HTTP servers | Remote HTTP (Function URLs) |
| Infrastructure | None | CDK deploy |
| Best for | Development, testing | Production, long-running agents |

Both paths need:

- Rust toolchain (stable)
- AWS credentials with Bedrock model access

The cloud path additionally needs:

- Node.js 20+
- AWS CDK CLI (`npm install -g aws-cdk`)
- [cargo-lambda](https://www.cargo-lambda.info/) (`brew install cargo-lambda/tap/cargo-lambda`)

## Local Development

The CLI runs the agent loop in your terminal with in-memory state. No infrastructure required.

```bash
git clone https://github.com/hydro-project/infinity
cd InfinityAgents
cargo run -p infinity-agent-cli
```

This gives you a streaming chat interface where you can interact with the agent, see tool calls and results in real time, and test threading and hibernation flows. The CLI also supports loading local RAP tool servers — see [Local CLI](/docs/infinity-runtime/local-cli) for details on configuration and limitations.

## Durable Cloud Agent

For persistent state, real hibernation, and remote tool servers, deploy the full stack with CDK.

```bash
git clone https://github.com/hydro-project/infinity
cd InfinityAgents/agent
npm install
npx cdk deploy
```

This provisions:

- Rust Lambda (agent runtime)
- SQS FIFO input queue
- Lambda Function URL (tool callback endpoint)
- Aurora DSQL cluster (conversation history)
- EventBridge Scheduler role (timed hibernation)

### Define your agent

Agents are CDK constructs. Extend `InfinityAgent` and add tools:

```typescript
import { InfinityAgent } from './infinity-agents';
import { LambdaMCPToolSet } from './infinity-agents/mcp';
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
  }
}
```

The framework handles wiring — Function URLs, IAM permissions, tool configuration injection into the runtime.

## What's next

- [Build a RAP Tool](/docs/using-rap/building-a-rap-tool) — create a custom tool that speaks RAP
- [Build a Runtime](/docs/using-rap/building-a-runtime) — implement your own RAP-compatible runtime
- [Built-in Tools](/docs/infinity-runtime/built-in-tools) — sleep, threading, and utility tools
- [Local CLI](/docs/infinity-runtime/local-cli) — in-memory CLI details and limitations
- [Specification](/spec/overview) — the full protocol reference
