---
sidebar_position: 2
title: The History Manager
---

# The History Manager
`HistoryManager` (in `infinity_agent_core::event_processor`) is the per-thread state object at the center of the loop. One instance represents one thread's view of the conversation: the committed history, the turn currently being streamed, and the deduplication sets that make redelivery safe. If you drive the loop yourself, it will be the first thing you construct and the value that you thread through every call, since everything in [the completion loop](./completion-loop.md) reads and writes through it.

## Construction
```rust
let history = HistoryManager::new_with_history(
    conversation_store.clone(),
    state_store.clone(),
    thread_id.clone(),
).await?;
```

Constructing the manager restores the thread's world from durable storage:

- The **ancestor chain** is loaded via `ConversationStore::get_ancestor_chain`, identifying the root thread and every parent between it and this thread.
- The **history** is loaded via `load_history_with_ancestors`, which reconstructs a child thread's inherited context: ancestor messages up to each spawn point, with the most recent compaction summary (from this thread or any ancestor) substituted for everything it covers. A freshly spawned child thread therefore sees its parent's conversation up to the moment of the spawn, and compaction transparently shortens what gets loaded.
- The **deduplication set** of processed message IDs comes from `StateStore::get_processed_ids`, along with per-conversation metadata.

Because everything is restored on load, an agent's full state can be rebuilt in any process, at any time, from the two stores. This is the property that makes the runtime serverless-capable.

## Committed History and the Turn Buffer
The manager keeps two layers of state:

- **Committed history** (`history` plus `pending_items`) holds messages that are part of the conversation and will be persisted by the next `sync()`. Inputs accepted by `handle_content` land here directly.
- **The turn buffer** holds assistant content for the turn that is currently streaming. `handle_completion` buffers each streamed chunk (coalescing consecutive text chunks into one message) without committing it.

The split exists because a streaming turn can fail or be interrupted halfway. At a **flush point** (meaning that the turn completed, or a turn-ending tool call arrived), `flush_turn` commits the buffer into history; `flush_turn_trimming_reasoning` additionally drops trailing reasoning, so that a committed turn never ends on a thinking block. On a mid-stream failure that will be retried, `discard_turn` drops the buffer so that the retry can rebuild its request from clean committed history.

`current_turn_view()` returns committed history followed by the in-flight buffer. This is what lets a client that attaches mid-stream see the partial assistant message, and the high-level API exposes it as [`ReplaySnapshot`](../agent-systems/observers.md).

## `sync()` and Deduplication
```rust
history.sync().await?;
```

`sync` writes everything that has been committed since the last sync: pending messages go through `ConversationStore::append_messages`, and the IDs that need durable deduplication go through the `StateStore`. It asserts that the turn buffer is empty, since calling it with un-flushed turn content is a bug (that content would be silently lost).

The persisted IDs are what make redelivery safe. Only inputs that are not naturally idempotent need them: user text and subscription events (a redelivered subscription event would mint a fresh injected invocation and be appended again). `handle_content(message, message_id)` is the single entry point for appending input to history: it consults the processed-ID set first, so a redelivered message (with the same `message_id`) will be skipped. Tool results do not need durable IDs at all, because they can be deduplicated against the history tail itself: the manager walks back across the trailing tool calls and results, and discards a result whose call is already answered (or has no live call). Because the IDs are persisted in the same `sync()` that persists the messages, a crash between processing and persistence will reprocess the message rather than duplicate it.

The loop's core ordering guarantee is built on this call: **a tool call is dispatched only after the turn that produced it has been synced.** If the process dies after dispatch, the persisted history already contains the tool call, so the eventual result message has something to attach to. The high-level API's step enforces this ordering (sync, then dispatch). If you drive [`execute_action`](./completion-loop.md) yourself, you must preserve it.

## Compaction and Threading Helpers
The manager also carries the state for the runtime's [threading](../built-in/threading.md) and compaction features. A custom driving loop will need to interact with each of them at a specific moment:

- When a compaction summary lands, call `apply_compaction()`. It replaces the covered prefix of the in-memory history with the latest summary from the store, and it tracks the absolute store index it covers so that a second compaction on top will compute the right split.
- When spawning a child thread (including a compaction thread), take `safe_spawn_point()` as the inheritance cutoff. It excludes trailing tool calls that have no result yet, so a child will never inherit a dangling call.
- After user text interrupts pending work, drain `take_interrupted_tool_calls()` and send best-effort cancellation notifications to the affected RAP tool servers.
- As subscription tools run, maintain the active-subscription set with `track_subscription` / `remove_subscription`. The set is persisted through the `StateStore`, where resource managers can consult it with `get_active_subscriptions` before releasing anything that a subscription still needs.
