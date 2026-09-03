---
sidebar_position: 1
title: Overview
---

# The Agent System API
An **agent system** runs conversation threads against shared stores, tools, and model providers. A root thread holds one user-facing conversation. Subagents, compaction workers, and subscription-event workers are child threads in the same conversation tree.

The API lives in `infinity_agent_core::system`. Start with [Build Your First Agent](./building-a-system.md). It uses the in-memory stores, one model, and `thread_builder()` to run a conversation before introducing any engine customization.

Everything that wakes an agent is an `InputMessage`: user text, a tool result, a report from a child, a subscription event, or a timer. Its `group_id` selects the destination thread. Messages for different threads can run concurrently, while messages for one thread are processed in order.

Each processing **slice** loads a thread, prepares its new messages, runs at most one model completion, persists the turn, dispatches at most one tool call, and yields. A tool result starts another slice when it returns through the input queue. This boundary lets a thread wait on tools without holding a model request or worker open. See [Architecture](../architecture.md) for the complete flow.

After the quickstart, continue with [Launching Local Threads](./running-locally.md) for the complete resident-process workflow. Add capabilities as the application needs them through [RAP servers](./rap-servers.md), [MCP servers](./mcp-servers.md), and [custom tools](./custom-tools.md). The later guides cover observers, external scheduling, and deeper engine configuration.

## Choosing an Execution Mode
<a id="two-ways-to-drive-it"></a>
The builder constructor selects who schedules slices.

**Local mode** creates an in-process queue and runs threads as messages arrive:

```rust
let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(shared_tools)
    .start();
```

Use local mode for a daemon, desktop application, or service that stays alive. User text interrupts an in-progress completion, active threads compact automatically, and an idle thread releases its driver until another message arrives. [Launch Local Threads](./running-locally.md) covers the `thread_builder()` and `ThreadHandle` APIs.

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

Use step mode when SQS, a job runner, or another scheduler decides when code runs. The handler calls `AgentSystem::step`, stores no resident task, and can exit after the call returns. See [Step Mode](./step-mode.md).

| | Local mode | Step mode |
|---|---|---|
| Constructor | `new_local(stores..., model)` | `new(stores..., model, sender)` |
| Input queue | In process | Supplied by the platform |
| Scheduling | Managed by the system | One `step` call per delivered batch |
| Best fit | Resident applications | Serverless and external schedulers |

## System and Thread Lifecycle
A local `RunningSystem` or `LaunchingSystem` remains available until `shutdown()` consumes it. Shutdown interrupts active completions, flushes their pending history, stops every thread, and waits for the local runtime to finish.

Individual threads have a shorter active lifetime. A thread driver starts when a message arrives and exits once nothing is queued and no tool result is on its way. Active subscriptions do not keep a driver resident: a subscription event respawns the driver when it arrives. The conversation remains in the stores, so the next message loads it and continues from the persisted history.

`next_lifecycle_event()` reports a `ThreadLifecycleEvent` for each driver transition: `ThreadLifecycleState::Live` when a driver spawns and `ThreadLifecycleState::Idle` when it exits, and `is_idle()` reports whether any driver remains. Use these signals to track conversation activity and release per-conversation resources such as command-based RAP servers or caches:

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

For one driver, `Live` always arrives before its matching `Idle`, and an idle thread that receives another message reports a fresh pair. A child thread can still be active after its root becomes idle. Resource managers scoped to a conversation should map the reported thread to its root, treat a thread as active while `StateStore::get_active_subscriptions` reports subscriptions for it (its events still need the resources that deliver them), and release resources only when no thread in that tree remains active. Applications that need custom fan-out, replay rendering, or durability hooks instead of a `ThreadHandle` event channel can implement an [observer](./observers.md).

The system API composes the public `HistoryManager`, `prepare_input`, `run_completion`, and `execute_action` primitives. Embeddings that need a different scheduler or replay policy can use [the low-level API](../low-level/overview.md).
