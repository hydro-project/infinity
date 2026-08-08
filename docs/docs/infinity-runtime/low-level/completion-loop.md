---
sidebar_position: 3
title: The Completion Loop
---

# The Completion Loop

A slice, one unit of agent execution as described in [Architecture](../architecture.md), is a composition of three public functions in `infinity_agent_core::event_processor`, plus a batch-level wrapper in `infinity_agent_core::batch_processor`. The high-level API's `Thread::step` runs exactly this composition; this page documents the pieces for embeddings that need to arrange them differently.

## `prepare_input`: absorb one message

```rust
pub async fn prepare_input<C, S, M>(
    input_msg: InputMessage,
    message_id: String,
    current_history: &HistoryManager<C, S>,
    conversation_store: &C,
    message_sender: &M,
) -> Result<PrepareResult, BoxError>
```

`prepare_input` takes one raw `InputMessage` and decides what it means for this thread. It drops messages for closed threads, deduplicates redeliveries, handles the synthetic compaction messages (spawning a compaction child thread, or applying a finished compaction summary to the in-memory history), routes subscription events, surfaces OAuth challenges and user-choice prompts, and appends actionable content to the [`HistoryManager`](./history-manager.md). The returned `PrepareResult` says what to do next:

- `Ready`: the message was appended to history; a completion should run.
- `Handled`: the message was fully absorbed (duplicate, closed thread, routed elsewhere); no completion needed.
- `OAuthRequired` / `UserChoiceRequired`: forward the challenge or prompt to the user; no completion.
- `CompactionApplied`: the in-memory history was compacted; no completion.

Many messages end at this phase, and that is the point. A slice only pays for a model call when an input actually warrants one.

## `run_completion`: stream the model

`run_completion` returns a `Stream` of `CompletionEvent`s: text chunks, reasoning start/stop/chunks, synchronous tool calls and their results, informational messages, and finally a terminal `Action`. It takes the provider and model ID, the [`HistoryManager`](./history-manager.md), the tool definitions and registry, an optional extra system prompt, and a `oneshot::Receiver<()>` cancellation handle. Dropping or firing the sender aborts the stream, which is how interruption is implemented.

As the stream runs, it buffers the turn into the history manager (`handle_completion`) and flushes at turn boundaries, retrying transient provider failures from clean committed history. Tools that opt into synchronous execution (`Tool::execute_synchronous`) are invoked inline and their results loop back into the completion without ending the slice. The stream's terminal event is a `CompletionAction`:

```rust
pub enum CompletionAction<R> {
    /// Model produced text and is done (no tool call).
    Done(R),
    /// Model wants to execute a tool call (fire-and-forget under RAP).
    ExecuteToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        call_id: Option<String>,
        display_as: Option<String>,
    },
}
```

## `execute_action`: dispatch and stop

```rust
pub async fn execute_action<M, R>(
    action: CompletionAction<R>,
    tool_registry: &HashMap<String, &dyn Tool<M>>,
    tool_context: &ToolContext<M>,
) -> Result<(), BoxError>
```

For `Done`, this is a no-op. For `ExecuteToolCall`, it calls the tool's `execute`, which dispatches the invocation (typically an HTTP POST to a RAP tool server) and returns without waiting for a result. **Call this only after `history.sync()` has persisted the turn.** The dispatch-after-durability ordering is what makes a crash between persist and dispatch recoverable. The tool result arrives later as a new `InputMessage` through your `InputSender`, starting the next slice.

## `process_batch`: the composed slice

`process_batch` runs `prepare_input` over a batch of messages for one thread and, if any of them was actionable, hands back the completion as an unstarted future plus its cancellation handle:

```rust
let (display_tx, display_rx) = mpsc::unbounded_channel();

if let Some((completion, cancel_tx)) = batch_processor::process_batch(
    inputs.into_iter(),          // (InputMessage, message_id) pairs
    &history,
    &conversation_store,
    &display_tx,
    &thread_id,
    &provider, model_id,
    supports_image_input,
    &tool_names, &tool_defs, &tool_registry,
    tool_context,
    &extra_system_prompt,
    rap_notifier.as_ref(),
    None,                        // optional output-token counter
).await {
    completion.await;            // run the slice to its yield point
}
```

The completion future streams the model, persists the turn, and dispatches at most one tool call via `execute_action`. When it resolves, the slice is done and the caller yields however its platform yields. The `cancel_tx` handle aborts the completion early: cancel, then feed the interrupting message as the next batch.

Everything a user might want to see streams through the `DisplayEvent` channel: text and reasoning chunks, tool calls and results with display segments, OAuth challenges, user-choice prompts. `DisplayEvent` is the low-level, non-`Clone` event stream; the high-level API translates it into the clonable `AgentEvent` type and the observer hooks described in [Observers](../agent-systems/observers.md). A headless embedding can simply drop the receiver.

## The manual embedding skeleton

Before the high-level API existed, an embedded runtime was a hand-written loop per thread over `process_batch`:

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

This skeleton, plus routing per `group_id`, interruption, deferral while a tool call is pending, idle teardown, and auto-compaction, is exactly what [`LocalAgentSystem::start`](../agent-systems/running-locally.md) packages. Write the loop yourself only when you need semantics the driver does not offer; the pieces on this page are the same either way.
