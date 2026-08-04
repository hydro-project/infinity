---
sidebar_position: 4
title: Step Mode
---

# Step Mode

Step mode is the serverless face of the agent system: nothing runs between messages, nothing is held in memory, and each delivery of messages for a thread is processed by one call. It exists because on a platform like AWS Lambda the platform *is* the scheduler. SQS batches messages per thread, invokes the function, and the function's whole job is one slice.

Build a step-mode system with `AgentSystemBuilder::new`, passing your platform's [`InputSender`](../low-level/overview.md#the-platform-traits):

```rust
let system = AgentSystemBuilder::new(conversation_store, state_store, model, sqs_sender)
    .tools(tool_impls)
    .callback_url(callback_url)
    .rap_notifier(rap_notifier)
    .build();
```

The sender is the loopback path: everything the runtime wants to happen *later* (a child thread's seed message, a report to a parent, a timer wake-up) is sent through it instead of being called directly, which is exactly what makes the slice free to end.

## Anatomy of a step

`AgentSystem::step(thread_id, inputs, observer, defer)` is the whole per-slice job, and it decomposes into two calls you can also make individually:

```rust
// 1. Load the thread: restore history and dedup state from the stores.
let thread = system.thread(&group_id).await?;

// 2. Run one step: apply the deferral policy, prepare each input, run at
//    most one completion (with synchronous-tool loopback), sync history,
//    dispatch at most one asynchronous tool call.
let collector = EventCollector::new();
let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
let outcome = thread.step(inputs, &collector, &mut NoDeferral, cancel_rx).await?;
```

`inputs` is a `Vec<(InputMessage, String)>`: each message paired with a stable dedup ID (on SQS, the message ID), so redeliveries are absorbed idempotently. The observer receives every event as it happens; `EventCollector` simply buffers them for inspection after the step, which is the natural shape for a handler that turns the slice's output into a response message (see [Observers](./observers.md)). Keep the cancel sender alive for the duration of the step, since dropping it signals cancellation, which is how a driving loop would implement user interruption.

The deferral phase splits out too: `thread.filter_deferrable(inputs, &mut defer)` applies the policy and returns the batch to run, and `thread.step_no_defer(batch, observer, cancel_rx)` runs it. The [local driver](./running-locally.md) composes these itself because it must act between them, skipping the step entirely when every input was deferred so an interruption arriving in that window is not misdirected at a no-op step.

`StepOutcome` tells you whether anything happened: `Skipped` means every input was absorbed during preparation (duplicates, events routed to subscribing threads, messages for closed threads); `Completed` carries the token usage and context window, which a scheduler can use to trigger [compaction](../architecture.md).

## Deferral

While a thread is waiting on a non-passive tool call, subscription events and child reports should not barge in and interrupt the pending call. `filter_deferrable` implements that policy against a `DeferQueue`:

- `NoDeferral` processes everything immediately. This is appropriate when there is nowhere durable to park events, as in a Lambda.
- `InMemoryDeferQueue` holds deferred events in memory. This is what the [local driver](./running-locally.md) uses, flushing when the tool call settles.
- Your own `DeferQueue` implementation can park events durably (a database table, a delay queue) for a step-mode platform that wants driver-grade semantics.

## The Lambda embedding

`infinity-agent-lambda` (`src/event_handler.rs`) is the production step-mode embedding and the reference to copy from. An SQS FIFO input queue keyed by thread ID provides everything the step API assumes about its transport: per-thread ordering (FIFO within a message group), automatic batching of messages that arrive together, and stable message IDs to use as dedup IDs.

Each invocation builds the system fresh, since a Lambda holds no state worth caching: DSQL and DynamoDB stores, Bedrock behind `StaticModel`, the platform sleep tools, and a SigV4-signed RAP notifier, with the same SQS queue as the loopback sender. The handler then runs exactly the three calls above with `NoDeferral` and an `EventCollector`, and finally drains the collected events into an output-queue message (accumulated text, tool call notices, or an OAuth challenge). [Deploying on AWS Lambda](../deploying-on-lambda.mdx) covers the surrounding infrastructure.
