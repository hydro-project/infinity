---
sidebar_position: 3
title: The Completion Loop
---

# The Completion Loop
A slice, one unit of agent execution as described in [Architecture](../architecture.md), is a composition of three public functions in `infinity_agent_core::event_processor`. The high-level API's [`AgentSystem::step`](../agent-systems/step-mode.md) runs exactly this composition; this page documents the pieces for embeddings that need to arrange them differently.

## `prepare_input`: Absorb One Message
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

Many messages end at this phase: a slice pays for a model call only when an input warrants one.

## `run_completion`: Stream the Model
`run_completion` returns a `Stream` of `CompletionEvent`s: text chunks, reasoning start/stop/chunks, synchronous tool calls and their results, informational messages, and finally a terminal `Action`. It takes the provider and model ID, the [`HistoryManager`](./history-manager.md), the tool definitions and registry, an optional extra system prompt, and a `oneshot::Receiver<()>` cancellation handle. Dropping or firing the sender aborts the stream, which is how interruption is implemented.

As the stream runs, it buffers the turn into the history manager (`handle_completion`) and flushes at turn boundaries, retrying transient provider failures from clean committed history. Tools that opt into synchronous execution (`Tool::execute_synchronous`) are invoked inline and their results loop back into the completion without ending the slice. The stream's terminal event is a `CompletionAction`:

```rust
pub enum CompletionAction {
    /// Model produced text and is done (no tool call).
    Done(FinalResponse),
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

## `execute_action`: Dispatch and Stop
```rust
pub async fn execute_action<M>(
    action: CompletionAction,
    tool_registry: &HashMap<String, &dyn Tool<M>>,
    tool_context: &ToolContext<M>,
) -> Result<(), BoxError>
```

For `Done`, this is a no-op. For `ExecuteToolCall`, it calls the tool's `execute`, which dispatches the invocation (typically an HTTP POST to a RAP tool server) and returns without waiting for a result. **Call this only after `history.sync()` has persisted the turn.** Because the turn is durable before the dispatch, a crash between persist and dispatch is recoverable: the persisted history already contains the tool call, and the eventual result message has something to attach to. The result arrives later as a new `InputMessage` through your `InputSender`, starting the next slice.

## Composing a Slice
A full slice strings the three functions together: prepare every message in the batch, run one completion if anything was actionable, sync the history, and dispatch the resulting tool call. This composition is deliberately not a separate low-level entry point. The ordering between its phases carries the runtime's durability guarantees (the turn must be synced before the tool call goes out, and observers must see events at defined moments), so the composed slice lives in one place: the agent system's step pipeline. [`AgentSystem::step`](../agent-systems/step-mode.md) is that composition for platform-driven batches, and [`LocalAgentSystem::start`](../agent-systems/running-locally.md) wraps the same pipeline in a per-thread driver that adds batching, interruption, deferral while a tool call is pending, idle teardown, and auto-compaction.

Reach for the individual functions when you are building something those layers cannot express: a custom scheduler that interleaves preparation and completion differently, a replay tool that runs `prepare_input` without ever completing, or a new platform binding with its own notion of a batch. If you compose them yourself, preserve the ordering contract: `history.sync()` before `execute_action`, always. At the system layer, events reach your embedding through the [`ThreadObserver`](../agent-systems/observers.md); at this level you observe the `CompletionEvent` stream from `run_completion` directly.
