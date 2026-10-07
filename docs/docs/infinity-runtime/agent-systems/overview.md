---
sidebar_position: 1
title: Overview
---

# The Agent System API
An **agent system** is a pool of conversation threads that runs against shared stores, tools, and model providers. Each root thread holds one user-facing conversation, while subagents, compaction workers, and subscription-event workers run as child threads in the same conversation tree. The API lives in `infinity_agent_core::system`.

Everything that wakes an agent is an `InputMessage`, whether it carries user text, a tool result, a report from a child, a subscription event, or a timer. Each message has a `group_id` that selects the destination thread. Messages for different threads can be processed concurrently, but messages for the same thread will always be processed in order.

When a message arrives, the system processes it in a **slice**: it loads the thread, prepares the new messages, runs at most one model completion, persists the turn, dispatches at most one tool call, and yields. When the tool result later returns through the input queue, it will start another slice. This means that a thread can wait on a tool without holding a model request or a worker open. See [Architecture](../architecture.md) for the complete flow.

[Launching Local Threads](./running-locally.md) covers the resident-process workflow. Tools can be [implemented in Rust in the same process](../quickstart/adding-tools.md) or provided by [RAP and MCP servers](../quickstart/connecting-rap-and-mcp.md). The remaining pages cover observers, external scheduling, and engine configuration.

## Choosing an Execution Mode
The choice of builder constructor determines who schedules slices.

**Local mode** creates an in-process queue and runs threads as messages arrive:

```rust
let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(shared_tools)
    .start();
```

Local mode is suited to a daemon, desktop application, or any service that stays alive. In this mode, user text will interrupt an in-progress completion, active threads are compacted automatically, and an idle thread releases its driver until another message arrives. [Launching Local Threads](./running-locally.md) covers the `thread_builder()` and `ThreadHandle` APIs.

**Step mode** accepts your platform's `InputSender` and processes an explicit batch:

```rust
let mut system = AgentSystemBuilder::new(
    conversation_store,
    state_store,
    model,
    platform_sender,
)
.build();
```

Step mode is for platforms where a scheduler such as SQS or a job runner decides when code runs. Your handler calls `AgentSystem::step` once per delivery, and because no resident task is kept, the process can exit as soon as the call returns. See [Step Mode](./step-mode.md).

## System and Thread Lifecycle
A local `RunningSystem` or `LaunchingSystem` remains available until `shutdown()` consumes it. Shutting down will interrupt any active completions, flush their pending history, stop every thread, and wait for the local runtime to finish.

Individual threads have a shorter active lifetime. A thread driver starts when a message arrives, and it exits once nothing is queued and no tool result is on its way. Active subscriptions do not keep a driver resident; instead, a subscription event will respawn the driver when it arrives. Because the conversation remains in the stores, the next message loads it and continues from the persisted history.

You can observe these transitions with `next_lifecycle_event()`, which reports a `ThreadLifecycleEvent` for each one: `ThreadLifecycleState::Live` when a driver spawns and `ThreadLifecycleState::Idle` when it exits. `is_idle()` reports whether any driver remains. These signals can be used to track conversation activity and to release per-conversation resources such as command-based RAP servers or caches:

```rust
while let Some(event) = system.next_lifecycle_event().await {
    match event.state {
        ThreadLifecycleState::Live => {
            status.mark_conversation_active(&event.thread_id).await?;
        }
        ThreadLifecycleState::Idle => {
            resources.release_if_conversation_is_idle(&event.thread_id).await?;
        }
    }
}
```

For a single driver, `Live` will always arrive before its matching `Idle`, and an idle thread that receives another message will report a fresh pair. Note that a child thread can still be active after its root becomes idle. A resource manager scoped to a conversation should therefore map each reported thread to its root, and it should treat a thread as active while `StateStore::get_active_subscriptions` reports subscriptions for it, because those events still need the resources that deliver them. Resources should be released only when no thread in the tree remains active. Applications that need custom fan-out, replay rendering, or durability hooks instead of a `ThreadHandle` event channel can implement an [observer](./observers.md).

The system API is a composition of the public `HistoryManager`, `prepare_input`, `run_completion`, and `execute_action` primitives. If your application needs a different scheduler or replay policy, you can use [the low-level API](../low-level/overview.md) directly.
