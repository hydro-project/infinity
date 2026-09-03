---
sidebar_position: 1
title: Overview
---

# The Low-Level API
The [Agent System API](../agent-systems/overview.md) is the recommended way to embed the Infinity Runtime: the builder, the step API, and the local driver package the machinery on this page into a few calls. This section documents the layer underneath, the pieces the high-level API is built from, all of which are public.

Drop down to this layer when the packaged behavior is not what you need:

- **Custom batch scheduling.** You decide which messages form a batch, when a thread runs, or how concurrent threads are prioritized, instead of using the built-in per-thread driver or one-step-per-delivery semantics.
- **Embedding in an existing event loop.** Your process already has a reactor or actor framework, and you want the agent loop as futures you compose rather than tasks the runtime spawns.
- **Custom deferral or replay semantics.** The high-level API defers subscription events while a tool call is pending and replays in-flight turns to new subscribers in one fixed way; at this layer you control both.
- **A new platform binding.** Porting the runtime to a new storage, transport, or serverless platform means implementing the platform traits below, and the loop's use of them is easier to follow at this level.

Everything here is the same code the high-level API runs: [`AgentSystem::step`](../agent-systems/step-mode.md) is a composition of [`prepare_input`, `run_completion`, and `execute_action`](./completion-loop.md) over a [`HistoryManager`](./history-manager.md). Nothing is lost by starting high and dropping down only where needed.

## Crate Map
| Crate | Role |
|---|---|
| `infinity-agent-core` | The agent loop: `HistoryManager`, `prepare_input`/`run_completion`/`execute_action`, built-in tools, the platform traits, and the [agent system API](../agent-systems/overview.md) on top |
| `infinity-provider-protocol` | The `ModelProvider` trait and the out-of-process provider transport. Deliberately lightweight so provider crates can depend on it alone |
| `rap-protocol` | RAP wire types: `RapInvocation`, toolset manifests, display segments |
| `rap-client` | Client-side RAP plumbing: the `HttpClient` trait, `ToolsetLoader` for discovery, `RapNotifier` for cancellations, and a local callback server |
| `infinity-agent-lambda` | The AWS binding: SQS handler, DSQL conversation store, DynamoDB state store |
| `infinity-daemon` | The embedded binding used by Infinity Code: one long-lived local agent system for the whole daemon, in-memory + JSON-file stores, lazily booted per-session RAP servers |

The last two are the production embeddings of the core. If you are writing your own, they are the reference material: `infinity-agent-lambda` is the minimal [step-mode](../agent-systems/step-mode.md) embedding, and `infinity-daemon` is the full interactive [local](../agent-systems/running-locally.md) one.

## The Platform Traits
Porting the runtime to a new platform means implementing four traits, defined in `infinity_agent_core::traits` and `rap_client::http`. Everything else (turn management, yielding, threading, subscriptions, compaction, deduplication) comes with the core. The table shows what each production embedding plugs in:

| Trait | Responsibility | Lambda | Daemon |
|---|---|---|---|
| `ConversationStore` | Per-thread history, thread hierarchy, compaction summaries | Aurora DSQL | In-memory + JSON files |
| `StateStore` | Processed IDs, metadata, active subscriptions, pending user choices | DynamoDB | In-memory + JSON files |
| `InputSender` | Delivering messages to the input queue | SQS FIFO | `mpsc` channels |
| `HttpClient` | POST/GET to tool servers | SigV4-signed reqwest | Plain reqwest |

Start with `InputSender`. It is the smallest trait and the most consequential, because it defines the yield boundary:

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

Anything the runtime wants to happen later (a child thread's seed message, a report to a parent, a timer wake-up) goes through `send_to_input_queue` rather than a function call. The message's `group_id` selects the target thread and the `dedup_id` makes redelivery safe. Your implementation must guarantee one property: delivery is FIFO within a group, because per-group ordering is the concurrency control for the whole runtime. The core ships one implementation, `ChannelSender`, the in-process queue behind [local systems](../agent-systems/running-locally.md).

`ConversationStore` is the largest trait, but the subtle part is provided. You supply the primitive queries: appending and loading messages (`append_messages`, `load_history_up_to`), the thread tree (`spawn_thread`, `get_ancestor_chain`, `close_thread`), and compaction summaries (`save_compaction_summary`, `load_latest_compaction_summary_up_to`). The `load_history_with_ancestors` default method then reconstructs a child thread's inherited history with the most recent compaction summary applied, which is the part that is easy to get wrong. The core's `InMemoryConversationStore` and `InMemoryStateStore` are complete implementations (the daemon runs on them), so a custom store is needed only for durable multi-process storage.

`StateStore` keeps the bookkeeping that makes redelivery and wake-ups safe: processed message and tool-call IDs, per-conversation metadata, and active subscriptions. Its implementations are typically thin key-value mappings; the semantics that matter (which IDs get recorded when) are driven by the [`HistoryManager`](./history-manager.md), not by your store.

Model access goes through a fifth trait, [`ModelProvider`](../model-providers.md), which streams completions and lists available models. The core never calls a model API directly.

## In This Section
- **[The History Manager](./history-manager.md)**: the per-thread state object, covering committed history, the buffered in-flight turn, deduplication, and the `sync()` durability point.
- **[The Completion Loop](./completion-loop.md)**: `prepare_input`, `run_completion`, and `execute_action`, the three phases of a slice, and how to compose them while preserving the durability ordering.
