---
sidebar_position: 1
title: Overview
---

# The Agent System API

The high-level entry point to the Infinity Runtime is the **agent system**: a configured collection of stores, tools, and a model source that runs any number of conversation threads. If an actor system is a set of actors plus a mail delivery mechanism, an agent system is exactly that with threads as the actors. Every root agent and every subagent it spawns is a thread, and all communication between them (user input, tool results, child reports, subscription events, timer wake-ups) is a message delivered to a thread's inbox.

The API lives in `infinity_agent_core::system` and is what both production embeddings are built on: the AWS Lambda handler and the Infinity Code daemon each construct one `AgentSystem` and differ only in how slices get driven.

```rust
use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};

let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(my_tools)
    .with_tokio_sleep_tools()
    .build_local();

let running = system.start_with_handles();
let mut thread = running.thread_handle("thread-1").await.expect("system running");
thread.send_user_text("hello").await?;
while let Some(event) = thread.recv().await {
    // TextChunk, ToolCall, CompletionFinished, ...
}
```

## Two ways to drive it

An agent system executes work in [slices](../architecture.md): load a thread, run one completion, dispatch at most one tool call, persist, yield. The builder offers two construction modes that decide *who* schedules those slices:

**Local mode** (`AgentSystemBuilder::new_local` → `build_local()` → `start()`). The system runs itself: an internal in-process queue (`ChannelSender`), a router task, and one driver task per active thread. Drivers batch inputs, handle mid-completion interruption, defer subscription events while a tool call is pending, auto-compact long histories, and idle out when a thread has nothing to wait for. This is the mode for a long-lived process; the Infinity Code daemon runs a single local system for its entire lifetime, serving every session. See [Running Locally](./running-locally.md).

**Step mode** (`AgentSystemBuilder::new(…, sender)` → `build()`). You bring your own transport (an [`InputSender`](../low-level/overview.md#the-platform-traits), e.g. SQS on Lambda) and your own scheduler, and call `AgentSystem::step` with each batch of messages the transport delivers for a thread. Nothing survives between steps; the thread handle is loaded from the stores each time, which is what makes this mode fit serverless platforms. See [Step Mode](./step-mode.md).

| | Local mode | Step mode |
|---|---|---|
| Constructor | `new_local(stores…, model)` | `new(stores…, model, sender)` |
| Message transport | Built-in in-process queue | Your `InputSender` (e.g. SQS FIFO) |
| Scheduling | Built-in router + per-thread drivers | You call `step` per delivered batch |
| Yield | Driver parks on its channel; idles out | The call returns; the process may exit |
| Interruption | Automatic (user text cancels the in-flight completion) | Redeliver as the next batch |
| Used by | Infinity Code daemon | `infinity-agent-lambda` |

The execution model is identical in both: same tools, same slices, same durability guarantees.

## The pieces of a system

Beyond the driving mode, configuring a system means choosing implementations for a handful of well-separated concerns. Each has a dedicated page in this section:

| Component | Role | Where it's covered |
|---|---|---|
| `ConversationStore` / `StateStore` | Durable history, thread hierarchy, dedup state. In-memory implementations ship with the core | [Building a System](./building-a-system.md) |
| `ModelSource` | Resolves which `ModelProvider` and model a thread uses, per completion round | [Building a System](./building-a-system.md#the-model-source) |
| Tools | `Tool` implementations, registered statically or resolved per thread via `ThreadConfigSource` | [Building a System](./building-a-system.md#tools) |
| `ThreadObserver` | Where events stream out and durability hooks fire | [Observers](./observers.md) |
| Built-in tools | Threading, subscriptions, and sleep tools added automatically | [Built-in Tools](../built-in-tools.md) |

The agent system API packages the loop that `infinity-agent-core` exposes piece by piece underneath: `HistoryManager`, `prepare_input`, `run_completion`, `execute_action`. Drop down to [the low-level API](../low-level/overview.md) when you need something the builder doesn't model: a custom batching policy, embedding a single completion into an existing event loop, or a new platform binding with its own replay semantics. Everything the high-level API does is built from those public pieces, so the two levels interoperate: a `Thread` handle exposes its `HistoryManager`, and the platform traits are shared.
