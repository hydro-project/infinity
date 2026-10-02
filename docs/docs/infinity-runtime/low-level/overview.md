---
sidebar_position: 1
title: Overview
---

# The Low-Level API
Underneath the [Agent System API](../agent-systems/overview.md) sits a public layer that consists of a per-thread state object, three loop functions, and four platform traits. This is the same code that the high-level API runs: [`AgentSystem::step`](../agent-systems/step-mode.md) is a composition of [`prepare_input`, `run_completion`, and `execute_action`](./completion-loop.md) over a [`HistoryManager`](./history-manager.md). This means that nothing is lost by starting high and dropping down only where needed.

You will need to drop down to this layer when the packaged behavior is not what you need:

- **Custom batch scheduling.** You want to decide which messages form a batch, when a thread runs, or how concurrent threads are prioritized, instead of using the built-in per-thread driver or one-step-per-delivery semantics.
- **Embedding in an existing event loop.** Your process already has a reactor or actor framework, and you want the agent loop to be futures that you compose, rather than tasks that the runtime spawns.
- **Custom deferral or replay semantics.** The high-level API defers subscription events while a tool call is pending and replays in-flight turns to new subscribers in one fixed way; at this layer, you can control both.
- **A new platform binding.** Porting the runtime to a new storage, transport, or serverless platform means implementing the platform traits below, and it is easier to follow how the loop uses them at this level.

## Crate Map
The loop itself lives in one crate, with the protocol and platform pieces factored around it:

- **`infinity-agent-core`** is the agent loop: `HistoryManager`, `prepare_input`/`run_completion`/`execute_action`, built-in tools, the platform traits, and the [agent system API](../agent-systems/overview.md) on top.
- **`infinity-provider-protocol`** defines the `ModelProvider` trait and the out-of-process provider transport, deliberately lightweight so provider crates can depend on it alone.
- **`rap-protocol`** holds the RAP wire types: `RapInvocation`, toolset manifests, display segments.
- **`rap-client`** is the client-side RAP plumbing: the `HttpClient` trait, `ToolsetLoader` for discovery, `RapNotifier` for cancellations, and a local callback server.
- **`infinity-agent-lambda`** is the AWS binding: SQS handler, DSQL conversation store, DynamoDB state store.
- **`infinity-daemon`** is the resident-process binding used by Infinity Code: it runs one long-lived local agent system for the whole daemon, with in-memory + JSON-file stores and lazily booted per-session RAP servers.

The last two are the production applications of the core. If you are writing your own, they are the reference material: `infinity-agent-lambda` is the minimal [step-mode](../agent-systems/step-mode.md) application, and `infinity-daemon` is the full interactive [local](../agent-systems/running-locally.md) one.

## The Platform Traits
Porting the runtime to a new platform means implementing four traits, which are defined in `infinity_agent_core::traits` and `rap_client::http`. Everything else (turn management, yielding, threading, subscriptions, compaction, and deduplication) comes with the core:

- **`ConversationStore`** holds per-thread history, the thread hierarchy, and compaction summaries. Lambda backs it with Aurora DSQL; the daemon uses in-memory state persisted to JSON files.
- **`StateStore`** holds processed IDs, metadata, active subscriptions, and pending user choices. Lambda uses DynamoDB; the daemon uses the same in-memory JSON-file store.
- **`InputSender`** delivers messages to the input queue: an SQS FIFO queue on Lambda, `mpsc` channels in the daemon.
- **`HttpClient`** performs the POSTs and GETs to tool servers: SigV4-signed reqwest on Lambda, plain reqwest in the daemon.

You should start with `InputSender`. It is the smallest trait but the most consequential, because it defines the yield boundary:

```rust
#[async_trait]
pub trait InputSender: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Send a message to the input queue for processing.
    async fn send_to_input_queue(
        &self,
        message: InputMessage,
        dedup_id: &str,
    ) -> Result<(), Self::Error>;
}
```

Anything that the runtime wants to happen later (such as a child thread's seed message, a report to a parent, or a timer wake-up) goes through `send_to_input_queue` rather than a function call. The message's `group_id` selects the target thread, and the `dedup_id` makes redelivery safe. Your implementation must guarantee one property: delivery has to be FIFO within a group, because per-group ordering is the concurrency control for the whole runtime. The core ships with one implementation, `ChannelSender`, which is the in-process queue behind [local systems](../agent-systems/running-locally.md).

`ConversationStore` is the largest trait, but the subtle part is provided for you. You only need to supply the primitive queries: appending and loading messages (`append_messages`, `load_history_up_to`), the thread tree (`spawn_thread`, `get_ancestor_chain`, `close_thread`), and compaction summaries (`save_compaction_summary`, `load_latest_compaction_summary_up_to`). The `load_history_with_ancestors` default method will then reconstruct a child thread's inherited history with the most recent compaction summary applied, which is the part that is easy to get wrong. The core's `InMemoryConversationStore` and `InMemoryStateStore` are complete implementations (the daemon runs on them), so you will only need a custom store for durable multi-process storage.

`StateStore` keeps the bookkeeping that makes redelivery and wake-ups safe: processed message and tool-call IDs, per-conversation metadata, and active subscriptions. Its implementations are typically thin key-value mappings, since the semantics that matter (which IDs get recorded when) are driven by the [`HistoryManager`](./history-manager.md), not by your store.

Model access goes through a fifth trait, [`ModelProvider`](../model-providers.md), which streams completions and lists available models. The core never calls a model API directly.

The remaining pieces are the loop itself: [the history manager](./history-manager.md) covers the per-thread state object, its turn buffer, and the `sync()` durability point, and [the completion loop](./completion-loop.md) covers the three phases of a slice and the ordering contract for composing them.
