---
sidebar_position: 3
title: The Completion Loop
---

# The Completion Loop
A slice (one unit of agent execution, as described in [Architecture](../architecture.md)) is a composition of three public functions in `infinity_agent_core::event_processor`. The high-level API's [`AgentSystem::step`](../agent-systems/step-mode.md) runs exactly this composition, and applications that need to arrange the phases differently can call the functions directly.

## `prepare_input`: Absorb One Message
```rust
# use infinity_agent_core::event_processor::{HistoryManager, PrepareResult};
# use infinity_agent_core::message::InputMessage;
# use infinity_agent_core::traits::{ConversationStore, InputSender, StateStore};
# type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub async fn prepare_input<C, S, M>(
    input_msg: InputMessage,
    message_id: String,
    current_history: &HistoryManager<C, S>,
    conversation_store: &C,
    message_sender: &M,
) -> Result<PrepareResult, BoxError>
# where C: ConversationStore, S: StateStore, M: InputSender,
# { unimplemented!() }
```

`prepare_input` takes one raw `InputMessage` and decides what it means for this thread. It drops messages for closed threads and deduplicates redeliveries. It also handles the synthetic compaction messages (spawning a compaction child thread, or applying a finished compaction summary to the in-memory history), routes subscription events, and surfaces OAuth challenges and user-choice prompts. Actionable content is appended to the [`HistoryManager`](./history-manager.md), and the returned `PrepareResult` says what to do next:

- `Ready`: the message was appended to history; a completion should run.
- `Handled`: the message was fully absorbed (duplicate, closed thread, routed elsewhere); no completion needed.
- `OAuthRequired` / `UserChoiceRequired`: forward the challenge or prompt (identified by a `ChoiceId`) to the user; no completion.
- `CompactionApplied`: the in-memory history was compacted; no completion.

Many messages will end at this phase, so a slice only pays for a model call when an input warrants one.

## `run_completion`: Stream the Model
`run_completion` returns a `Stream` of `CompletionEvent`s: text chunks, reasoning start/stop/chunks, synchronous tool calls and their results, informational messages, and finally a terminal `Action`. It takes the provider, the model ID and whether that model accepts image input, the [`HistoryManager`](./history-manager.md), the tool definitions, registry, and context, the thread and message IDs, an optional extra system prompt, and a `oneshot::Receiver<()>` cancellation handle. Dropping or firing the sender will abort the stream, which is how interruption is implemented.

As the stream runs, it buffers the turn into the history manager (`handle_completion`) and flushes at turn boundaries. Provider failures are handled according to the [`ErrorClass`](../model-providers.md#writing-a-provider) the provider attached to them: `Transient` errors are retried after a short delay and `Throttled` errors after a longer backoff, both from clean committed history and up to a fixed retry limit, while `Fatal` errors end the stream. A `ContextOverflow` triggers recovery instead of a plain retry: if the only new input is a single tool result or subscription event, its body is replaced with a placeholder and the completion is retried; otherwise the new user input is dropped (and any pending tool results are settled as interrupted) and the stream ends, so that an oversized message can never wedge the thread. The runtime applies no timeouts of its own; stall detection, if any, belongs to the provider. Tools that opt into synchronous execution (`Tool::execute_synchronous`) are invoked inline, and their results loop back into the completion without ending the slice. The stream's terminal event is a `CompletionAction`:

```rust
# use infinity_provider_protocol::FinalResponse;
# use rap_protocol::{ProviderCallId, ToolCallId};
pub enum CompletionAction {
    /// Model produced text and is done (no tool call).
    Done(FinalResponse),
    /// Model wants to execute a tool call (fire-and-forget under RAP).
    ExecuteToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: ToolCallId,
        call_id: Option<ProviderCallId>,
        display_as: Option<String>,
    },
}
```

## `execute_action`: Dispatch and Stop
```rust
# use std::collections::HashMap;
# use infinity_agent_core::event_processor::CompletionAction;
# use infinity_agent_core::tools::{Tool, ToolContext};
# use infinity_agent_core::traits::InputSender;
# type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub async fn execute_action<M>(
    action: CompletionAction,
    tool_registry: &HashMap<String, &dyn Tool<M>>,
    tool_context: &ToolContext<M>,
) -> Result<(), BoxError>
# where M: InputSender + 'static,
# { unimplemented!() }
```

For `Done`, this is a no-op. For `ExecuteToolCall`, it calls the tool's `execute`, which dispatches the invocation (typically an HTTP POST to a RAP tool server) and returns without waiting for a result. **You must call this only after `history.sync()` has persisted the turn.** Because the turn is durable before the dispatch, a crash between persist and dispatch is recoverable: the persisted history already contains the tool call, so the eventual result message has something to attach to. The result will arrive later as a new `InputMessage` through your `InputSender`, which starts the next slice.

## Composing a Slice
A full slice strings the three functions together: it prepares every message in the batch, runs one completion if anything was actionable, syncs the history, and dispatches the resulting tool call. This composition is deliberately not a separate low-level entry point. The ordering between its phases carries the runtime's durability guarantees (the turn must be synced before the tool call goes out, and observers must see events at defined moments), so the composed slice lives in one place: the agent system's step pipeline. [`AgentSystem::step`](../agent-systems/step-mode.md) is that composition for platform-driven batches, and [`LocalAgentSystem::start`](../agent-systems/running-locally.md) wraps the same pipeline in a per-thread driver that adds batching, interruption, deferral while a tool call is pending, idle teardown, and auto-compaction.

You should reach for the individual functions when you are building something that those layers cannot express, such as a custom scheduler that interleaves preparation and completion differently, a replay tool that runs `prepare_input` without ever completing, or a new platform binding with its own notion of a batch. If you compose them yourself, you must preserve the ordering contract: `history.sync()` always comes before `execute_action`. At the system layer, events reach your application through the [`ThreadObserver`](../agent-systems/observers.md); at this level, you observe the `CompletionEvent` stream from `run_completion` directly.
