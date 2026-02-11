---
sidebar_position: 1
title: Overview
---

# Infinity Runtime

The Infinity Runtime is the reference RAP agent runtime. It's a Rust Lambda function that uses Amazon Bedrock for LLM completions and Aurora DSQL for conversation state. It implements the full RAP protocol plus two extensions that make agents truly long-lived: hibernation and threading.

Hibernation lets agents sleep for arbitrary durations — seconds to weeks — at zero compute cost. Threading lets agents spawn parallel workers that share context and report results back.

These features are built on top of RAP's async tool execution model. They're not part of the RAP protocol itself — any runtime can implement them differently, or not at all. But they demonstrate what's possible when tool calls don't block.

The Infinity Runtime is deployed via CDK using the `InfinityAgent` construct. It provisions the Lambda function, SQS queues, DSQL cluster, EventBridge Scheduler role, and all the wiring between them. You add tools by composing `LambdaTool`, `CustomToolSet`, and `LambdaMCPToolSet` constructs.

```typescript
import { InfinityAgent } from './infinity-agents';

export class MyAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);
    // add tools here
  }
}
```
