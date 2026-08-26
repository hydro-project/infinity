---
sidebar_position: 10
title: Customizing the Engine
---

# Customizing the Engine
The quickstart chooses in-memory stores, one model, and local scheduling. Replace those engine defaults when the process must share durable state, select models at runtime, or let another platform own message delivery.

| Requirement | Extension point |
|---|---|
| Persist conversations across processes | `ConversationStore` and `StateStore` |
| Change the model between completion rounds | `ModelSource` |
| Let an external platform schedule work | `AgentSystemBuilder::new` and `AgentSystem::step` |
| Replace the completion pipeline | [Low-Level API](../low-level/overview.md) |

Tools and protocol integrations have dedicated guides: [Dynamic Thread Configuration](./dynamic-configuration.md), [Connecting RAP Servers](./rap-servers.md), [Connecting MCP Servers](./mcp-servers.md), and [Writing Custom Tools](./custom-tools.md).

## Persistence Providers
An agent system uses two stores with separate responsibilities:

- **`ConversationStore`** persists messages, thread relationships, and compaction summaries.
- **`StateStore`** persists processed IDs, thread metadata, and active subscriptions.

The runtime can resume a thread when both stores retain their state. In-memory implementations are complete stores, but their contents belong to one process. A service with multiple workers should provide shared implementations:

```rust
let system = AgentSystemBuilder::new_local(
    PostgresConversationStore::connect(&database_url).await?,
    RedisStateStore::connect(&redis_url).await?,
    model,
)
.start();
```

Store methods define the runtime's ordering, deduplication, and thread-tree contract. Implement both traits directly when adding a persistence provider, and test interrupted turns, duplicate inputs, child threads, compaction, and active subscriptions. The [platform traits](../low-level/overview.md#the-platform-traits) document the complete interfaces.

`StateStore` also carries the local router's wake policy. Before an event-style input (a tool result, subscription event, OAuth completion, or user choice) wakes an idle thread, the router calls `should_wake_thread_for_event`. Returning `false` drops the input before any driver is spawned, so a stale callback cannot create thread records for a conversation that does not exist. The default admits everything. `InMemoryStateStore::for_conversations` links the in-memory pair so events are refused for threads the conversation store has never seen, and the Infinity Code daemon's stores refuse sessions the user shut down. User text is never gated, because first input is how threads are created.

:::caution

Conversation history and runtime state must describe the same logical deployment. Restoring only one store can lose deduplication records, subscription ownership, or messages needed by the thread history.

:::

## Model Resolution
A **`ModelSource`** chooses one `ResolvedModel` at the start of each completion round:

```rust
#[async_trait(?Send)]
impl ModelSource for SelectedModelSource {
    async fn resolve(&self, thread_id: &str) -> Result<ResolvedModel, BoxError> {
        let selection = self.selections.load(thread_id).await?;
        self.catalog.resolve(&selection)
    }
}
```

`ResolvedModel` contains the provider, model ID, context window, and image-input support. The context window controls when automatic compaction begins. Updating the selection affects the next completion; a completion already in progress keeps the model it started with.

Use `StaticModel` when every round uses one model. Use a root-aware source when subagents should inherit their root conversation's persisted selection. [Dynamic Thread Configuration](./dynamic-configuration.md#choosing-a-model-per-thread) shows that pattern.

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

The builder preserves Infinity's standard slice ordering, history synchronization, tool dispatch, interruption, and replay behavior. Drop to the [Low-Level API](../low-level/overview.md) only when an embedding must replace one of those policies, such as integrating a custom scheduler or controlling preparation and completion separately. At that layer, the embedding composes `HistoryManager`, `prepare_input`, `run_completion`, and `execute_action`, and must preserve the documented durability order, including syncing history before dispatching a tool call.
