---
sidebar_position: 2
title: The History Manager
---

# The History Manager
`HistoryManager` (in `infinity_agent_core::event_processor`) is the per-thread state object at the center of the loop. One instance represents one thread's view of the conversation: the committed history, the turn currently being streamed, and the deduplication sets that make redelivery safe. If you drive the loop yourself, it will be the first thing you construct and the value that you thread through every call, since everything in [the completion loop](./completion-loop.md) reads and writes through it.

## Construction
```rust
# use infinity_agent_core::ThreadId;
# use infinity_agent_core::event_processor::HistoryManager;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let conversation_store = InMemoryConversationStore::new();
# let state_store = InMemoryStateStore::new();
# let thread_id = ThreadId::from("thread-1".to_owned());
let history = HistoryManager::new_with_history(
    conversation_store.clone(),
    state_store.clone(),
    thread_id.clone(),
).await?;
# history.sync().await?;
# Ok(())
# }
```

Constructing the manager restores the thread's world from durable storage:

- The **ancestor chain** is loaded via `ConversationStore::get_ancestor_chain`, identifying the root thread and every parent between it and this thread.
- The **history** is loaded via `load_history_with_ancestors`, which reconstructs a child thread's inherited context: ancestor messages up to each spawn point, with the most recent compaction summary (from this thread or any ancestor) substituted for everything it covers. A freshly spawned child thread therefore sees its parent's conversation up to the moment of the spawn, and compaction transparently shortens what gets loaded.
- The **per-conversation metadata** comes from the `StateStore`. Nothing else is preloaded: whether an input was already processed is answered on demand by `ConversationStore::is_message_processed`, a point lookup against the persisted history (which stores each message together with the dedup ID it arrived under).

Because everything is restored on load, an agent's full state can be rebuilt in any process, at any time, from the two stores. This is the property that makes the runtime serverless-capable.

## Committed History and the Turn Buffer
The manager keeps three layers of unpersisted state on top of the in-memory `history`:

- **Unvalidated inputs** are inputs (user text, tool results, injected synthetic results) that the model has not produced any output for yet. Inputs accepted by `handle_input` land here. They are part of the in-memory history, so the next completion includes them, but `sync()` will not persist them: one of them could be the oversized message that overflows the model's context window, and persisting it would wedge the thread on that message forever. As soon as the model streams any output for a request, `mark_inputs_model_validated` promotes them, since output proves that the context fit.
- **Known-safe items** (`pending_items`) are promoted inputs and model-produced content that will be persisted by the next `sync()`. Model output can only be committed after the inputs it answers have been promoted, so known-safe items always precede unvalidated ones in history order.

  Interrupt markers are the one kind of input that can skip validation. When **user text** arrives while the history ends on an unanswered tool call, the manager records an `Interrupt` entry that settles the call (rendered to the model as an "interrupted by user" tool result). Because a tool call at the tail is committed model output, nothing unvalidated can precede it, so the marker is committed known-safe, and the step's commit barrier (an unconditional `sync()` between the prepare and completion phases) persists it before the interrupted tool is cancelled or the model is invoked — regardless of what later happens to the text that caused it. A marker that arrives in any other position (user text cutting off a streaming completion whose inputs have not yet been validated) must stay unvalidated to preserve the ordering above, but it is never dropped by overflow recovery.

  An **event** (subscription event or child report) that interrupts a tool call records no marker. The event itself is the record — it is never dropped, only placeholder-replaced on overflow — and `render_history` infers the interruption from it, answering the call with an "interrupted by an incoming event" result. This keeps the persisted history honest for a future resume: if the process dies before the event is validated, the log ends on the bare tool call and reads as "in flight", not as a deliberate stop.
- **The turn buffer** holds assistant content for the turn that is currently streaming. `handle_completion` buffers each streamed chunk (coalescing consecutive text chunks into one message) without committing it.

The turn buffer exists because a streaming turn can fail or be interrupted halfway. At a **flush point** (meaning that the turn completed, or a turn-ending tool call arrived), `flush_turn` commits the buffer into history; `flush_turn_trimming_reasoning` additionally drops trailing reasoning, so that a committed turn never ends on a thinking block. On a mid-stream failure that will be retried, `discard_turn` drops the buffer so that the retry can rebuild its request from clean committed history.

`current_turn_view()` returns committed history followed by the in-flight buffer. This is what lets a client that attaches mid-stream see the partial assistant message, and the high-level API exposes it as [`ReplaySnapshot`](../agent-systems/observers.md).

## `sync()` and Deduplication
```rust
# use infinity_agent_core::ThreadId;
# use infinity_agent_core::event_processor::HistoryManager;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let history = HistoryManager::new_with_history(
#     InMemoryConversationStore::new(),
#     InMemoryStateStore::new(),
#     ThreadId::from("thread-1".to_owned()),
# ).await?;
history.sync().await?;
# Ok(())
# }
```

`sync` writes the known-safe items committed since the last sync through `ConversationStore::append_messages`, each paired with its dedup ID. Unvalidated inputs stay in memory until the model has answered them. It asserts that the turn buffer is empty, since calling it with un-flushed turn content is a bug (that content would be silently lost).

The persisted IDs are what make redelivery safe. `handle_input(message, message_id)` is the single entry point for appending an incoming input to history: it first checks the IDs handled in this process, then asks the `ConversationStore` whether the ID was persisted by an earlier incarnation of the thread, and skips the message if so. Tool results are additionally deduplicated against the history tail itself: the manager walks back across the trailing tool calls and results, and discards a result whose call is already answered (or has no live call). Because each ID is stored with its message in the same `append_messages` call, there is no separate write that can be lost: a crash between processing and persistence will reprocess the message rather than duplicate it, and a message can never be persisted without its ID.

The loop's core ordering guarantee is built on this call: **a tool call is dispatched only after the turn that produced it has been synced.** If the process dies after dispatch, the persisted history already contains the tool call, so the eventual result message has something to attach to. The high-level API's step enforces this ordering (sync, then dispatch). If you drive [`execute_action`](./completion-loop.md) yourself, you must preserve it.

## Compaction and Threading Helpers
The manager also carries the state for the runtime's [threading](../built-in/threading.md) and compaction features. A custom driving loop will need to interact with each of them at a specific moment:

- When a compaction summary lands, call `apply_compaction()`. It replaces the covered prefix of the in-memory history with the latest summary from the store, and it tracks the absolute store index it covers so that a second compaction on top will compute the right split.
- When spawning a child thread (including a compaction thread), take `safe_spawn_point()` as the inheritance cutoff. It excludes trailing tool calls that have no result yet, so a child will never inherit a dangling call, and it stays before any unvalidated inputs, since children inherit history from the store and those inputs have not been persisted. Pass it to `ConversationStore::spawn_thread` as `SpawnContext::InheritUpTo(point)`.
- After user text interrupts pending work, drain `take_interrupted_tool_calls()` and send best-effort cancellation notifications to the affected RAP tool servers.
- As subscription tools run, maintain the active-subscription set with `track_subscription` / `remove_subscription`. The set is persisted through the `StateStore`, where resource managers can consult it with `get_active_subscriptions` before releasing anything that a subscription still needs.
