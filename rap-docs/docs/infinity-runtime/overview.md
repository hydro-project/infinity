---
sidebar_position: 1
title: Overview
---

# Infinity Runtime

The Infinity Runtime is the reference RAP agent runtime. It's a Rust Lambda function that uses Amazon Bedrock for LLM completions and Aurora DSQL for conversation state.

It implements the full RAP protocol plus extensions for scheduled hibernation (sleep tools) and parallel threading — features that demonstrate what's possible when tool calls don't block.

The runtime is deployed via CDK using the `InfinityAgent` construct, which provisions the Lambda function, SQS FIFO input queue, callback endpoint (a Lambda Function URL that accepts tool results and enqueues them), DSQL cluster, EventBridge Scheduler role, and all the wiring between them.

```typescript
import { InfinityAgent } from './infinity-agents';
import { LambdaMCPToolSet } from './infinity-agents/mcp';

export class MyAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);

    // MCP tools (wrapped automatically)
    new LambdaMCPToolSet(this, 'GitHub', {
      name: 'github',
      command: ['npx', '-y', '@modelcontextprotocol/server-github'],
      env: { GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_TOKEN },
    });

    // Native RAP tools
    new FinanceToolSet(this, 'Finance');
    new GitHubEventToolSet(this, 'GitHubEvents', { webhookGateway: api });
  }
}
```

You add tools by composing `LambdaTool`, `CustomToolSet`, and `LambdaMCPToolSet` constructs. The framework handles Function URLs, IAM permissions, and tool configuration injection.
