---
sidebar_position: 3
title: Running Locally
---

# Running Locally

A **local agent system** is the resident, self-driving mode: the system owns an in-process input queue and runs a small actor runtime on top of it. Build one with `AgentSystemBuilder::new_local(…)` and `build_local()`, then start it. The simplest way to start is `start_with_handles()`, which runs the system with a built-in channel-based observer:

```rust
let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .thread_config(config_source)
    .with_tokio_sleep_tools()
    .build_local();

let sender = system.sender();          // for the RAP callback server
let running = system.start_with_handles();

let mut thread = running.thread_handle("session-1").await.expect("system running");
thread.send_user_text("hello").await?;
while let Some(event) = thread.recv().await {
    // TextChunk, ToolCall, CompletionFinished, ...
}
```

Embeddings that need custom fan-out or durability hooks call `start(|thread_id| MyObserver::new(thread_id))` with their own [observer](./observers.md) instead. Either way, `start` spawns the **router** on the current tokio [`LocalSet`] and returns a `RunningSystem`, the handle you keep for the life of the process.

## The router and the drivers

The router owns the receiving end of the internal queue and maintains one **driver task** per active thread. When a message arrives for a thread with no driver, the router spawns one; the driver then owns that thread exclusively. The same thread never has two drivers, which is the per-thread serialization the whole runtime relies on (the local equivalent of an SQS FIFO message group).

Each driver loop does considerably more than call `Thread::step` in a loop:

- **Batching.** Everything queued for the thread is drained into one batch per step, so a burst of subscription events costs one completion, not five.
- **Interruption.** If user text arrives while a completion is streaming, the driver cancels the in-flight step (the partial turn is flushed to the store), prefixes the message with `<interrupt>` so the model knows what happened, and starts the next step immediately.
- **Deferral.** While the thread has a pending call to a non-passive tool, deferrable synthetic events (subscription events, child-thread reports) are held in a queue rather than interrupting the running call; they're flushed as soon as the call settles or user text interrupts it. Calls to *passive* tools (the sleep tools) never hold anything back.
- **Auto-compaction.** When a completion reports token usage above 75% of the model's context window, the driver injects a compaction request; when the summary comes back, the in-memory history is compacted and the trigger resets.
- **Idling.** When the thread's queue is empty and the thread expects no wake-up (no pending tool call, no active subscription, or it just called `close_thread`), the driver exits. An idle thread costs nothing; the router respawns a driver on the next message. Idle-exit is race-free: an exiting driver closes its channels and hands any message or subscribe request that raced in back to the router, so nothing is lost.

## The `RunningSystem` handle

| Member | Purpose |
|---|---|
| `sender()` | The system's `InputSender`; hand it to the RAP callback server and any other message injectors |
| `send(message, dedup_id)` / `send_user_text(thread_id, text)` | Deliver input to a thread |
| `thread_handle(thread_id)` | Attach to a thread and get a send/receive handle for it (with `start_with_handles()`; see [Thread handles](#thread-handles)) |
| `subscribe(thread_id, request)` / `subscribe_handle()` | Attach a live subscriber to a running thread (see [Observers](./observers.md#live-attach-and-replay)); resolves once the subscriber is installed, so a subsequent `send` is guaranteed to be observed |
| `thread_exits` | A receiver that yields a thread ID each time a driver idles out |
| `active_threads()` / `is_idle()` | Which threads currently have a live driver |
| `begin_shutdown()` + `task` | Whole-process wind-down: every driver flushes its in-flight turn and exits; await `task` for completion |

A local system is designed to run for the **lifetime of the process**. Individual threads idle out and respawn constantly, but the router keeps running, so senders never race a teardown. `begin_shutdown` exists only for process exit.

## Thread handles

`RunningSystem::thread_handle(thread_id)` (available when the system was started with `start_with_handles()`) packages attach, send, and receive into one object. It subscribes to the thread and resolves once the subscription is installed, returning a `ThreadHandle`:

- `replay()` is the thread's state at the moment of attach: committed history plus any in-flight turn and in-progress reasoning. Render this first to catch up.
- `events` (also exposed through `recv()`) is an unbounded queue of the thread's `AgentEvent`s from that moment on. Every event lands in exactly one of the two: either it is reflected in the replay or it is delivered to the queue.
- `send_user_text(text)` / `send(message, dedup_id)` deliver input to the thread.

The thread does not need to exist yet. Attaching to a fresh ID spawns a driver just long enough to install the subscription (the replay is empty), and the driver idles right back out; when you later send the first message, events flow to the handle from the very first `UserInput`. Subscriptions live in a registry shared across driver respawns, so a handle keeps receiving events across any number of idle/respawn cycles. Multiple handles can attach to the same thread, each receiving every event, and dropping a handle simply detaches it.

## The idle pattern: `thread_exits`

Per-conversation resources (tool server processes, file handles, caches) shouldn't live longer than the conversation is active, but "active" is something only the system knows. The `thread_exits` channel is the hook: every time a driver idles out, the embedding gets the thread ID and can decide whether anything it owns is now unused.

The Infinity Code daemon is the reference implementation of this pattern. Each daemon *session* is a root thread (plus any subagent threads it spawned), and each session owns RAP tool server processes booted lazily by its [`ThreadConfigSource`](./building-a-system.md#dynamic-per-thread-configuration). A watcher task consumes `thread_exits`, maps each exiting thread to its root session, and shuts the session's servers down once **no** thread of that session has a live driver and no keep-alive client is attached. Because the config source caches the session's toolset and reboots servers on demand, shutting down early is always safe: the next message just pays a server restart.

```text
thread_exits ──▶ map to root session ──▶ session fully idle? ──▶ shut down its tool servers
                                              │
new message ──▶ router respawns driver ──▶ config source lazily reboots servers
```

For a complete picture of a production local embedding, the daemon (`crates/infinity-daemon`) is worth reading top to bottom: one `SessionManager` builds one local system at startup with a catalog-backed `ModelSource` (per-thread model switching), a `SessionRapManager` as the `ThreadConfigSource` (per-session tools), and a `DaemonObserver` per thread that fans events out to attached terminal and web clients, including live attach with full replay of a mid-stream turn. Every one of those integration points is a trait described in [Building a System](./building-a-system.md) and [Observers](./observers.md).

[`LocalSet`]: https://docs.rs/tokio/latest/tokio/task/struct.LocalSet.html
