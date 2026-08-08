---
sidebar_position: 1
title: Overview
---

# The Low-Level API

The [Agent System API](../agent-systems/overview.md) is the recommended way to embed the Infinity Runtime: the builder, the step API, and the local driver package the machinery below into a few calls. This section documents the layer underneath, the pieces the high-level API is built from, all of which are public.

Drop down to this layer when the packaged behavior is not what you need:

- **Custom batch scheduling.** You want to decide yourself which messages form a batch, when a thread runs, or how concurrent threads are prioritized, rather than using the built-in per-thread driver or one-step-per-delivery semantics.
- **Embedding in an existing event loop.** Your process already has a reactor or actor framework, and you want the agent loop as a set of futures you compose rather than tasks the runtime spawns.
- **Custom deferral or replay semantics.** The high-level API defers subscription events while a tool call is pending and replays in-flight turns to new subscribers in one fixed way; at this layer you control both.
- **A new platform binding.** Porting the runtime to a new storage, transport, or serverless platform means implementing the platform traits below, and it helps to understand exactly what the loop does with them.

Everything here is the same code the high-level API runs. `Thread::step` is a composition of [`prepare_input`, `run_completion`, and `execute_action`](./completion-loop.md) over a [`HistoryManager`](./history-manager.md). Nothing is lost by starting high and dropping down only where needed.

## Crate map

| Crate | Role |
|---|---|
| `infinity-agent-core` | The agent loop: `HistoryManager`, `process_batch`, `run_completion`, built-in tools, the platform traits, and the [agent system API](../agent-systems/overview.md) on top |
| `infinity-provider-protocol` | The `ModelProvider` trait and the out-of-process provider transport. Deliberately lightweight so provider crates can depend on it alone |
| `rap-protocol` | RAP wire types: `RapInvocation`, toolset manifests, display segments |
| `rap-client` | Client-side RAP plumbing: the `HttpClient` trait, `ToolsetLoader` for discovery, `RapNotifier` for cancellations, and a local callback server |
| `infinity-agent-lambda` | The AWS binding: SQS handler, DSQL conversation store, DynamoDB state store |
| `infinity-daemon` | The embedded binding used by Infinity Code: one long-lived local agent system for the whole daemon, in-memory + JSON-file stores, lazily booted per-session RAP servers |

The last two are the production embeddings of the core. If you are writing your own, they are the reference material: `infinity-agent-lambda` is the minimal [step-mode](../agent-systems/step-mode.md) embedding, and `infinity-daemon` is the full interactive [local](../agent-systems/running-locally.md) one.

## The platform traits

The core talks to the outside world through four traits, defined in `infinity_agent_core::traits` and `rap_client::http`. The table shows what each production embedding plugs in:

| Trait | Responsibility | Lambda | Daemon |
|---|---|---|---|
| `ConversationStore` | Per-thread history, thread hierarchy, compaction summaries | Aurora DSQL | In-memory + JSON files |
| `StateStore` | Processed IDs, metadata, active subscriptions | DynamoDB | In-memory + JSON files |
| `InputSender` | Delivering messages to the input queue | SQS FIFO | `mpsc` channels |
| `HttpClient` | POST/GET to tool servers | SigV4-signed reqwest | Plain reqwest |

`InputSender` is the smallest and the most important, because it defines the yield boundary:

```rust
#[async_trait]
pub trait InputSender: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Send a message to the input queue for processing.
    async fn send_to_input_queue(
        &self,
        message: InputMessage,
        group_id: &str,
        dedup_id: &str,
    ) -> Result<(), Self::Error>;
}
```

Anything the runtime wants to happen later (a child thread's seed message, a report to a parent, a timer wake-up) goes through `send_to_input_queue` rather than a function call. The `group_id` selects the target thread and the `dedup_id` makes redelivery safe. Whatever ordering guarantee your transport gives per group is the concurrency control for the whole runtime, so it must be FIFO within a group. The core ships one implementation, `ChannelSender`, the in-process queue behind [local systems](../agent-systems/running-locally.md).

`ConversationStore` is the largest trait. Beyond appending and loading messages (`append_messages`, `load_history_up_to`) it models the thread tree (`spawn_thread`, `get_ancestor_chain`, `close_thread`) and compaction summaries (`save_compaction_summary`, `load_latest_compaction_summary_up_to`). The provided `load_history_with_ancestors` default method handles the subtle part, reconstructing a child thread's inherited history with the most recent compaction summary applied, so implementations only supply the primitive queries. The core also provides ready-made `InMemoryConversationStore` and `InMemoryStateStore` implementations, which are what the [agent system examples](../agent-systems/building-a-system.md) use.

`StateStore` keeps the bookkeeping that makes redelivery and wake-ups safe: processed message and tool-call IDs (`get_processed_ids`, `add_processed_message_ids`, `add_processed_tool_calls`), per-conversation metadata (`get_metadata`, `set_metadata`), and active subscriptions (`get_active_subscriptions`, `add_active_subscription`, `remove_active_subscription`).

Model access goes through a fifth trait, [`ModelProvider`](../model-providers.md), which streams completions and lists available models. The core never calls a model API directly.

## In this section

- **[The History Manager](./history-manager.md)**: the per-thread state object, covering committed history, the buffered in-flight turn, deduplication, and the `sync()` durability point.
- **[The Completion Loop](./completion-loop.md)**: `prepare_input`, `run_completion`, and `execute_action` (the three phases of a slice) and `process_batch`, which composes them over a batch of messages.
