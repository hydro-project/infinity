---
sidebar_position: 3
title: The Rust API
---

# The Rust API

The Infinity Runtime is a set of Rust crates, not a framework or a service. The core crate, `infinity-agent-core`, contains the full agent loop and is generic over storage, transport, HTTP, and model backends. Binding it to a platform means implementing a handful of traits; everything else (turn management, yielding, threading, subscriptions, compaction, deduplication) comes with the core.

## Crate map

| Crate | Role |
|---|---|
| `infinity-agent-core` | The agent loop: `HistoryManager`, `process_batch`, `run_completion`, built-in tools, the platform traits |
| `infinity-provider-protocol` | The `ModelProvider` trait and the out-of-process provider transport. Deliberately lightweight so provider crates can depend on it alone |
| `rap-protocol` | RAP wire types: `RapInvocation`, toolset manifests, display segments |
| `rap-client` | Client-side RAP plumbing: the `HttpClient` trait, `ToolsetLoader` for discovery, `RapNotifier` for cancellations, and a local callback server |
| `infinity-agent-lambda` | The AWS binding: SQS handler, DSQL conversation store, DynamoDB state store |
| `infinity-daemon` | The embedded binding used by Infinity Code: in-memory stores, `mpsc` transport, per-thread workers |

The last two are the production embeddings of the core. If you are writing your own, they are the reference material: `infinity-agent-lambda` is the minimal batch-oriented embedding, and `infinity-daemon` is the full interactive one.

## The platform traits

The core talks to the outside world through four traits, defined in `infinity_agent_core::traits` and `rap_client::http`. The table shows what each production embedding plugs in:

| Trait | Responsibility | Lambda | Daemon |
|---|---|---|---|
| `ConversationStore` | Per-thread history, thread hierarchy, compaction summaries | Aurora DSQL | In-memory + JSON files |
| `StateStore` | Processed IDs, metadata, active subscriptions | DynamoDB | In-memory + JSON files |
| `InputSender` | Delivering messages to the input queue | SQS FIFO | `mpsc` channels |
| `HttpClient` | POST/GET to tool servers | SigV4-signed reqwest | Plain reqwest |

`InputSender` is the smallest and the most important, because it defines the yield boundary:

```rust
#[async_trait]
pub trait InputSender: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Send a message to the input queue for processing.
    async fn send_to_input_queue(
        &self,
        message: InputMessage,
        group_id: &str,
        dedup_id: &str,
    ) -> Result<(), Self::Error>;
}
```

Anything the runtime wants to happen later (a child thread's seed message, a report to a parent, a timer wake-up) goes through `send_to_input_queue` rather than a function call. The `group_id` selects the target thread and the `dedup_id` makes redelivery safe. Whatever ordering guarantee your transport gives per group is the concurrency control for the whole runtime, so it must be FIFO within a group.

`ConversationStore` is the largest trait. Beyond appending and loading messages it models the thread tree (`spawn_thread`, `get_ancestor_chain`, `close_thread`) and compaction summaries. The provided `load_history_with_ancestors` default method handles the subtle part, reconstructing a child thread's inherited history with compaction applied, so implementations only supply the primitive queries.

Model access goes through a fifth trait, [`ModelProvider`](./model-providers.md), which streams completions and lists available models. The core never calls a model API directly.

## Tools

Tools implement the `Tool` trait, generic over the `InputSender` so the same tool runs on any transport:

```rust
#[async_trait]
pub trait Tool<M: InputSender>: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    /// Fire-and-forget execution: dispatch the call and return.
    /// The result arrives later as an InputMessage.
    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), BoxError>;

    /// Opt into synchronous execution: return Some(result) to have it
    /// injected into history immediately, looping back into the
    /// completion instead of yielding.
    async fn execute_synchronous(
        &self,
        args: &serde_json::Value,
        id: &str,
        call_id: Option<&str>,
        context: &ToolContext<M>,
    ) -> Option<ToolResult> { None }
}
```

Note what `execute` does not return: a result. Dispatching is the whole job, which is what lets the slice end immediately after. The `ToolContext` carries the `callback_url` results should be POSTed to, the thread stack, and the `InputSender`.

Most agents need no custom `Tool` implementations at all. `RapTool` (in `infinity_agent_core::tools::rap_tool`) is a generic implementation that POSTs a `RapInvocation` to any RAP tool server endpoint, and `ToolsetLoader` builds the definitions from a server's `/.well-known/rap-toolset` manifest at startup. The threading, subscription management, and indefinite-sleep [built-in tools](./built-in-tools.md) ship with the core; the timed sleep tools are per-platform, since they need durable timers (EventBridge and SQS delays on AWS, tokio timers in a live process).

## Driving the loop

The entry point is `process_batch` in `infinity_agent_core::batch_processor`. It takes the batch of input messages for one thread, the `HistoryManager`, the provider, and the tool registry, and returns a completion future when any input was actionable:

```rust
let history = HistoryManager::new_with_history(
    conversation_store.clone(),
    state_store.clone(),
    thread_id.clone(),
).await?;

let (display_tx, display_rx) = mpsc::unbounded_channel();

if let Some((completion, cancel_tx)) = batch_processor::process_batch(
    inputs.into_iter(),          // (InputMessage, message_id) pairs
    &history,
    &conversation_store,
    &display_tx,
    &thread_id,
    &provider, model_id,
    &tool_names, &tool_defs, &tool_registry,
    tool_context,
    &extra_system_prompt,
    rap_notifier.as_ref(),
    None,
).await {
    completion.await;            // Lambda awaits; the daemon stores it
}
```

The split between "prepare" and "complete" is deliberate:

- **Preparation** happens per message inside `process_batch`: deduplication, closed-thread checks, subscription event routing, OAuth and user-choice surfacing, compaction handling. Messages that need no completion (duplicates, events routed elsewhere) are absorbed here.
- **The completion future** runs the model, streams events, persists the turn, and dispatches at most one tool call via `execute_action`. When it resolves, the slice is done and the caller yields however its platform yields: the Lambda handler returns, a daemon worker goes back to `rx.recv().await`.

The `cancel_tx` handle aborts the completion early, which is how an embedded runtime implements user interruption: cancel, then feed the interrupting message as the next batch.

Everything the caller might want to show a user streams through the `DisplayEvent` channel: text chunks, reasoning start/stop, tool calls and results with display segments, OAuth challenges, user choice prompts. The Lambda embedding drains it into an output queue message; the daemon forwards it to attached terminal and web clients live. A headless deployment can simply drop the receiver.

If you need finer control than `process_batch`, the pieces underneath are public: `event_processor::prepare_input` handles one message, `event_processor::run_completion` returns the raw completion event stream, and `event_processor::execute_action` dispatches a tool call.

## Embedding the runtime in your own process

An embedded runtime is a loop per thread plus a callback route. Concretely, the daemon-style skeleton is:

1. **Implement the traits**, or start from in-memory versions like the daemon's `InMemoryConversationStore`, `InMemoryStateStore`, and `InMemoryMessageSender` (an `mpsc` wrapper that routes each `group_id` to its worker's channel, spawning a worker for new groups).

2. **Run a callback server.** Tool servers deliver results by POSTing to the `callback_url` in each invocation. `rap_client::callback_server` provides one; its handler converts each result into an `InputMessage` and hands it to your `InputSender`.

3. **Run a worker loop per thread.** Each worker owns one thread's `HistoryManager` and drains its channel:

```rust
loop {
    // Collect whatever has queued up for this thread.
    let mut batch = vec![rx.recv().await?];
    while let Ok(msg) = rx.try_recv() { batch.push(msg); }

    if let Some((completion, _cancel)) = process_batch(
        batch.into_iter(), &history, /* ... */
    ).await {
        completion.await;
    }
    // Loop back to recv().await: this is the yield.
}
```

4. **Register the built-in tools** (`SpawnThreadTool`, `ReportToParentTool`, `CloseThreadTool`, `SleepUntilEventOrInputTool`, `CancelSubscriptionTool`) plus a `RapTool` per remote tool definition, and pick a [model provider](./model-providers.md).

Idle threads cost one parked task awaiting a channel, so hibernation in an embedded runtime is zero CPU rather than zero process. The programming model is unchanged: the same tools, the same slices, the same yield semantics as on Lambda.
