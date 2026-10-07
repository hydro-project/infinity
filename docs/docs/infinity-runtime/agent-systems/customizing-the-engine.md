---
sidebar_position: 6
title: Customizing the Engine
---

# Customizing the Engine
`AgentSystemBuilder` accepts implementations of the engine's extension points in place of the defaults. Custom `ConversationStore` and `StateStore` implementations can persist conversations across processes, a `ModelSource` can change the model between completion rounds, and `AgentSystemBuilder::new` with `AgentSystem::step` hands scheduling to an external platform. An application that must replace the completion pipeline itself can drop to the [low-level API](../low-level/overview.md).

Tools and protocol integrations have dedicated pages: [Dynamic Thread Configuration](./dynamic-configuration.md), [Connecting RAP & MCP Servers](../quickstart/connecting-rap-and-mcp.md), and [Adding Tools](../quickstart/adding-tools.md).

## Persistence Providers
An agent system uses two stores with separate responsibilities:

- **`ConversationStore`** persists messages, thread relationships, and compaction summaries.
- **`StateStore`** persists processed IDs, thread metadata, active subscriptions, and pending user choices.

The runtime can resume a thread whenever both stores retain their state. The in-memory implementations are complete stores, but their contents belong to a single process, so a service with multiple workers should provide shared implementations:

```rust
let system = AgentSystemBuilder::new_local(
    PostgresConversationStore::connect(&database_url).await?,
    RedisStateStore::connect(&redis_url).await?,
    model,
)
.start();
```

The store methods define the runtime's ordering, deduplication, and thread-tree contract. When adding a persistence provider, implement both traits directly, and make sure to test interrupted turns, duplicate inputs, child threads, compaction, and active subscriptions. The [platform traits](../low-level/overview.md#the-platform-traits) document the complete interfaces.

The stores also carry the local router's wake policy. Before an event-style input (a tool result, subscription event, OAuth completion, or user choice) can wake an idle thread, the router checks `ConversationStore::thread_exists` and `StateStore::is_thread_stopped`. An event for a thread the conversation store has never seen, or for one the state store reports as stopped, will be dropped before any driver is spawned. This means that a stale callback cannot create thread records for a conversation that does not exist. `is_thread_stopped` defaults to `false` (admitting everything); the Infinity Code daemon's store implements it to refuse events for sessions the user shut down. User text is never gated, because first input is how threads are created and how stopped threads are resumed.

:::caution

Conversation history and runtime state must describe the same logical deployment. Restoring only one store can lose deduplication records, subscription ownership, or messages needed by the thread history.

:::

## Model Resolution
A **`ModelSource`** chooses one `ResolvedModel` at the start of each completion round:

```rust
#[async_trait(?Send)]
impl ModelSource for SelectedModelSource {
    async fn resolve(&self, thread_id: &ThreadId<str>) -> Result<ResolvedModel, BoxError> {
        let selection = self.selections.load(thread_id).await?;
        self.catalog.resolve(&selection)
    }
}
```

`ResolvedModel` contains the provider, model ID, context window, and image-input support; the context window controls when automatic compaction begins. Updating the selection will affect the next completion, while a completion that is already in progress keeps the model it started with.

Use `StaticModel` when every round uses one model, or a root-aware source when subagents should inherit their root conversation's persisted selection. [Dynamic Thread Configuration](./dynamic-configuration.md#choosing-a-model-per-thread) shows that pattern.

## Platform-Managed Scheduling
`new_local` owns an in-process queue and drives threads for the life of the process. Use `AgentSystemBuilder::new` when a platform already provides the input queue and scheduler:

```rust
let mut system = AgentSystemBuilder::new(
    conversation_store,
    state_store,
    model_source,
    platform_sender,
)
.build();
```

The platform then calls `AgentSystem::step` for each delivered batch. This mode fits SQS and Lambda because all thread state is reloaded from the stores for each call. [Step Mode](./step-mode.md) covers batching, observers, deferral, and outcomes.

In both modes, the builder preserves Infinity's standard slice ordering, history synchronization, tool dispatch, interruption, and replay behavior. You should drop to the [Low-Level API](../low-level/overview.md) only when an application must replace one of those policies, for example to integrate a custom scheduler or to control preparation and completion separately. At that layer, the application composes `HistoryManager`, `prepare_input`, `run_completion`, and `execute_action` itself, and it must preserve the documented durability order, including syncing history before dispatching a tool call.
