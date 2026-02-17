---
sidebar_position: 2
title: Cloud Deployment
---

# Cloud Deployment

The cloud Infinity Runtime is a Rust Lambda function that uses Amazon Bedrock for LLM completions and Aurora DSQL for conversation state. It implements the full RAP protocol plus extensions for scheduled hibernation and parallel threading.

## Infrastructure

The runtime is deployed via CDK using the `InfinityAgent` construct, which provisions:

- **Lambda function** — the Rust runtime binary, triggered by the input queue
- **SQS FIFO input queue** — serializes messages per conversation thread via `MessageGroupId`
- **Callback endpoint** — a Lambda Function URL that accepts tool results and enqueues them
- **DSQL cluster** — durable conversation history storage
- **DynamoDB table** — deduplication state and metadata
- **SQS delay queue** — standard queue for short sleep delays (≤ 900s)
- **EventBridge Scheduler role** — for scheduling longer sleep wake-ups

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

## Hibernation

After dispatching tool calls, the Lambda function persists state to DSQL and exits. Zero compute runs until the next message arrives on the input queue. This is the defining property of RAP — cost is proportional to work done, not time elapsed.

Tool calls are POSTed to Lambda Function URLs with IAM auth (SigV4). The tool Lambda uses response streaming to acknowledge immediately, then continues processing. The runtime doesn't read the response body — it fires and moves on.

When the tool finishes, it POSTs the result to the callback endpoint, which enqueues it on the input FIFO queue, triggering the runtime again.

### Timed sleep

Both runtimes support `sleep` and `sleep_until`. The cloud runtime implements them using AWS infrastructure:

**`sleep(seconds)`** — Hibernate for a fixed duration. Delays ≤ 900 seconds use SQS `DelaySeconds` via a relay queue. Longer delays create a one-time EventBridge schedule.

**`sleep_until(date, time, timezone)`** — Hibernate until a specific wall-clock time. Converts the target to a UTC delay and uses the same mechanism as `sleep`. Returns immediately if the target is in the past.

Both are interruptible — if a user message or subscription event arrives while the agent is sleeping, the runtime processes it immediately. The pending sleep result arrives later and is appended to history normally.

## Concurrency

The FIFO queue ensures messages within a conversation thread are processed one at a time (via `MessageGroupId`). Different threads process concurrently — a parent and its children can all be active simultaneously on separate Lambda invocations.

## Adding tools

Tools are CDK constructs that wire up Lambda functions, Function URLs, and IAM permissions:

- `LambdaTool` — a single Lambda-backed RAP tool
- `CustomToolSet` — a group of related tools sharing a Lambda function
- `LambdaMCPToolSet` — wraps an MCP server process in a Lambda function with a proxy layer

The `InfinityAgent` base construct collects tool definitions from all children and injects them as environment variables so the runtime knows what's available at startup.
