---
sidebar_position: 9
title: Step Mode
---

# Step Mode
**Step mode** runs an agent system with no resident tasks: each delivery of messages is processed by one call to `AgentSystem::step`, and nothing survives between calls. This is the mode for platforms that schedule work themselves. On AWS, SQS FIFO batches messages per thread and Lambda invokes your handler; the handler's whole job is one slice.

A step-mode system takes your platform's [`InputSender`](../low-level/overview.md#the-platform-traits) instead of creating an internal queue:

```rust
let system = AgentSystemBuilder::new(conversation_store, state_store, model, sqs_sender)
    .tools(tool_impls)
    .callback_url(callback_url)
    .rap_notifier(rap_notifier)
    .build();
```

The sender is the loopback path. Everything the runtime wants to happen *later* (a child thread's seed message, a report to a parent, a timer wake-up) is sent through it rather than called directly, which is what leaves the slice free to end. On Lambda the sender is an SQS client pointed at the same FIFO queue that triggered the invocation, so the message comes back around as a future delivery.

## Anatomy of a Step
Once the system is built, the handler body is one call:

```rust
let collector = EventCollector::new();
let outcomes = system
    .step(inputs, &collector, &mut NoDeferral)
    .await?;

for (thread_id, event) in collector.take() {
    // turn each thread's events into your platform's output
}
```

`inputs` is a `Vec<(InputMessage, String)>`: each message paired with a stable dedup ID (on SQS, the message ID), so redeliveries are absorbed idempotently. The batch may span multiple threads, because an SQS FIFO delivery with a batch size above 1 can interleave several message groups (order is guaranteed only within a group). `step` partitions the batch by thread and runs the per-thread slices concurrently. Each slice loads its thread's history and dedup state from the stores, applies the deferral policy, prepares its inputs into history, runs at most one completion (with synchronous-tool loopback), syncs history durably, and dispatches at most one asynchronous tool call. Because nothing is cached between calls, the process is free to exit afterwards.

The observer receives every event as it happens, tagged with the emitting thread. `EventCollector` buffers `(thread_id, event)` pairs for inspection after the slice, which suits a handler that turns each thread's output into a response message; see [Observers](./observers.md) for richer integrations. Each thread's `StepOutcome` reports what happened: `Skipped` means every input was absorbed during preparation (duplicates, events routed to subscribing threads, messages for closed threads), and `Completed` carries the token usage and context window, which a scheduler can use to trigger [compaction](../architecture.md).

Steps must be serialized per thread across calls, because two concurrent steps for the same thread would race each other's history writes. In-process, `step` takes `&mut self`, so one system instance runs one call at a time. Across processes, serialization is the transport's contract: SQS FIFO message groups provide it, and Lambda never processes the same group in two invocations concurrently.

## Deferral
While a thread waits on a non-passive tool call, subscription events and child reports are held back rather than allowed to interrupt the pending call. The `DeferQueue` you pass to `step` decides where held-back events wait:

- `NoDeferral` processes everything immediately. Use it when there is nowhere durable to park events, as in a Lambda.
- `InMemoryDeferQueue` holds deferred events in memory. The [local driver](./running-locally.md) uses it, flushing when the tool call settles.
- Your own `DeferQueue` implementation can park events durably (a database table, a delay queue) for a step-mode platform that wants driver-grade semantics.

## The Lambda Embedding
`infinity-agent-lambda` (`src/event_handler.rs`) is the production step-mode embedding and the reference implementation. An SQS FIFO input queue keyed by thread ID supplies the transport guarantees required by `step`: FIFO ordering within each message group, automatic batching of messages that arrive together, and stable message IDs for deduplication.

Each invocation builds the stores, model source, tools, sender, and system before processing its batch:

```rust
let mut system = AgentSystemBuilder::new(
    dsql_conversations,
    dynamodb_state,
    model,
    sqs_sender,
)
.thread_config(thread_config)
.build();

let collector = EventCollector::new();
system.step(inputs, &collector, &mut NoDeferral).await?;
publish_events(collector.take()).await?;
```

The real handler uses DSQL and DynamoDB stores, Bedrock behind `StaticModel`, and the same SQS queue as the loopback sender. A [`ThreadConfigSource`](./dynamic-configuration.md#dynamic-per-thread-configuration) loads each thread's RAP toolsets through a DynamoDB manifest cache and adds the platform sleep tools, so a batch spanning several sessions resolves each session's own tools.

After `step` returns, the handler drains each thread's collected events into an output-queue message, including accumulated text, tool-call notices, and OAuth challenges. No runtime task needs to survive the invocation because future work has already returned to SQS. [Deploying on AWS Lambda](../deploying-on-lambda.mdx) covers the queues, stores, IAM policy, and callback receiver around this handler.
