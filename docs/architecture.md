# Architecture
The prototype demonstrates zero idle cost through Lambda hibernation. The agent processes messages, executes tools, and can sleep for arbitrary durations by scheduling wake-up events via EventBridge Scheduler.

## System Dataflow

```
┌──────────────┐                              ┌──────────────┐
│ External     │                              │ External     │
│ Input        │ (Slack, webhooks, etc.)      │ Webhook      │ (GitHub, EventBridge etc.)
└──────┬───────┘                              └──────┬───────┘
       │                                             │
       ▼                                             ▼
┌──────────────┐                              ┌──────────────┐
│ Input Queue  │◄────────────────┐◄───────────│ Webhook      │
│ (SQS)        │                 │            │ Lambda       │
└──────┬───────┘                 │            └──────────────┘
       │                         │                   ▲
       ▼                         │                   │ (looks up subscription)
┌──────────────┐                 │            ┌──────────────┐
│ Leader       │                 │            │ Subscriptions│
│ Lambda       │ (Bedrock)       │            │ (DynamoDB)   │
└──┬────┬────┬─┘                 │            └──────────────┘
   │    │    │                   │                  ▲
   │    │    └───────────────────────────┐          │
   │    └─────────────┐          │       │          │
   │                  │          │       │          │
   ▼                  ▼          │       ▼          │
┌──────────┐    ┌──────────┐     │  ┌──────────┐    │
│ Output   │    │ Tool     │     │  │ Sub Tool │    │
│ Queue    │    │ Queue    │     │  │ Queue    │    │
└────┬─────┘    └────┬─────┘     │  └────┬─────┘    │
     │               │           │       │          │
     ▼               ▼           │       ▼          │
┌──────────┐    ┌──────────┐     │  ┌──────────┐    │
│ External │    │ Tool     │─────┘  │ Sub Tool │────┘
│ Delivery │    │ Lambda   │        │ Lambda   │ (records subscription)
└──────────┘    └──────────┘        └──────────┘
```

## Hibernation Mechanism

The agent can sleep for arbitrary durations without consuming resources:

1. Agent decides to sleep or invoke a long running tool call (e.g., waiting for CI/CD, rate limiting, scheduled tasks)
2. Sleep tool sends tool request to a queue or creates EventBridge Scheduler for a timed hibernation
3. Agent Lambda exits immediately (zero cost while sleeping)
4. At scheduled time or when a tool result is enqueued, Leader Lambda is invoked
5. Leader Lambda processes wake-up, loads conversation state, continues execution

**Interruption handling:** If a user message or subscription event arrives while waiting for a tool result, the agent processes it immediately. When the event arrives, the agent detects the interruption and injects a synthetic "interrupted" tool result into the conversation history.

## Tool Execution

Tools are independent Lambdas with dedicated SQS queues. When the agent calls a tool:

1. Agent sends request to tool queue with tool call ID, arguments, and callback info
2. Agent Lambda exits
3. Tool Lambda processes request asynchronously
4. Tool sends result to agent input queue with original tool call ID
5. Agent Lambda wakes, matches result to pending tool call, continues execution

This decoupling allows tools to run in parallel and have independent scaling/timeout characteristics.

**MCP Tools:** MCP servers run in a Lambda proxy that spawns the MCP process (via npx/uvx), forwards requests, and returns results. The CDK `LambdaMCPToolSet` construct (in `agent/lib/infinity-agents/mcp`) automatically creates `{name}_list_tools` and `{name}_invoke_tool` methods.

**Subscription Tools:** Some tools register subscriptions rather than returning immediate results. The tool Lambda records a subscription in DynamoDB (e.g., "notify me when GitHub Actions completes for PR #123"). When the external event occurs, a webhook Lambda receives it, looks up matching subscriptions, and sends the event data to the agent's input queue with the original tool call ID. On the leader side, this event is revealed to the LLM using a "synthetic tool call". This enables the agent to wait for external events without polling.
