---
sidebar_position: 2
title: Building a System
---

# Building a System

`AgentSystemBuilder` assembles the four things every agent system needs (a conversation store, a state store, a model source, and a message transport) and lets you layer tools and per-thread configuration on top. This page walks through each piece.

```rust
use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};

let model = StaticModel::new(provider, "claude-sonnet-4-5").await?;

let system = AgentSystemBuilder::new_local(
    InMemoryConversationStore::new(),
    InMemoryStateStore::new(),
    model,
)
.tool(Box::new(MyTool))
.extra_system_prompt("You are a helpful assistant for the Foo project.")
.with_tokio_sleep_tools()
.build_local();
```

The constructor decides the [driving mode](./overview.md#two-ways-to-drive-it): `new_local` creates an internal in-process queue and yields a `LocalAgentSystem` from `build_local()`; `new` takes your own [`InputSender`](../low-level/overview.md#the-platform-traits) and yields a step-mode `AgentSystem` from `build()`.

## Stores

The system persists everything through two traits: `ConversationStore` (per-thread history, the thread tree, compaction summaries) and `StateStore` (processed message IDs, thread metadata, active subscriptions). The Lambda embedding plugs in Aurora DSQL and DynamoDB; for local systems and tests, the core ships functional in-memory implementations in `infinity_agent_core::stores` (`InMemoryConversationStore`, `InMemoryStateStore`). The Infinity Code daemon wraps these with JSON-file persistence.

Because all state lives in the stores, a system holds no per-thread memory of its own: threads are loaded on demand and any thread can resume after a process restart.

## The model source

Models are resolved through the `ModelSource` trait, once per completion round, per thread:

```rust
#[async_trait(?Send)]
pub trait ModelSource {
    async fn resolve(&self, thread_id: &str) -> Result<ResolvedModel, BoxError>;
}
```

`ResolvedModel` carries the [`ModelProvider`](../model-providers.md), the model ID, the context window size (used for the auto-compaction threshold), and whether the model accepts image input. For the common case there is `StaticModel::new(provider, model_id)`, which looks the model up in the provider's catalog once and always resolves to it.

Per-round resolution is what makes mid-session model switching work: the Infinity Code daemon's `ModelSource` reads each thread's persisted model selection, so switching models takes effect on the next completion without restarting anything, and a spawned child thread inherits its parent's selection at resolve time.

## Tools

Tools implement the `Tool` trait from `infinity_agent_core::tools`, generic over the `InputSender` so the same tool runs on any transport:

```rust
#[async_trait]
pub trait Tool<M: InputSender>: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    /// Fire-and-forget execution: dispatch the call and return.
    /// The result arrives later as an InputMessage.
    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<M>,
    ) -> Result<(), BoxError>;

    /// Opt into synchronous execution: return Some(result) to have it
    /// injected into history immediately, looping back into the
    /// completion instead of yielding.
    async fn execute_synchronous(/* … */) -> Option<ToolResult> { None }

    /// Waiting rather than working (sleep tools). Pending calls to passive
    /// tools never hold back deferred events.
    fn is_passive(&self) -> bool { false }
}
```

Note what `execute` does *not* return: a result. Dispatching is the whole job, which is what lets the slice end immediately after. The `ToolContext` carries the `callback_url` results should be POSTed to (set with the builder's `.callback_url(url)`; local systems run a callback server from `rap_client::callback_server` that converts each POST into an `InputMessage` on the system's sender), the thread stack, and the `InputSender` for injecting messages directly. Platform-specific configuration does not go through the context: a tool that needs platform resources carries them as its own fields (the Lambda sleep tools hold their scheduler client, role ARN, and queue ARNs, for example), so the core API stays platform-neutral.

Most agents need no custom `Tool` implementations at all. `RapTool` (in `infinity_agent_core::tools::rap_tool`) is a generic implementation that POSTs a `RapInvocation` to any RAP tool server endpoint, and `rap_client`'s `ToolsetLoader` builds the definitions from a server's `/.well-known/rap-toolset` manifest.

Register tools statically with `.tool(…)` / `.tools(…)`, so every thread sees the same set, alongside `.extra_system_prompt(…)` and `.rap_notifier(…)` (best-effort cancellation/closure notifications to RAP servers).

The [built-in thread and subscription tools](../built-in-tools.md) (`spawn_thread`, `report_to_parent`, `close_thread`, `send_message_to_child`, `cancel_subscription`, `sleep_until_event_or_input`) are added automatically on top of whatever you register; opt out with `.without_builtin_tools()`. The timed sleep tools are platform-specific because they need durable timers: on Lambda they're backed by SQS delays and EventBridge Scheduler, while local systems call `.with_tokio_sleep_tools()` to get the core's tokio-timer versions.

## Dynamic per-thread configuration

Static registration gives every thread the same toolset. When different conversations need different tools, implement `ThreadConfigSource` instead. The motivating case is the Infinity Code daemon, where each session has its own working directory and therefore its own RAP tool servers:

```rust
#[async_trait(?Send)]
pub trait ThreadConfigSource<M: InputSender, H: HttpClient> {
    async fn resolve(&self, thread_id: &str) -> Result<ThreadConfig<M, H>, BoxError>;
}
```

and pass it with `.thread_config(source)` (which replaces the static `tools`/`extra_system_prompt`/`rap_notifier` configuration). `ThreadConfig` is the same triple of tools, extra system prompt, and RAP notifier, but resolved per thread, lazily, the first time a thread actually runs a step. Lazy resolution matters: loading a thread to inspect or replay its history never boots a tool server.

The daemon's implementation is a good reference (`SessionRapManager` in `infinity-daemon`): it maps a thread to its root session, reads the session's RAP config on first resolve, boots the servers, and caches the toolset. When the session goes idle it shuts the servers down but keeps the cached config, so the next message transparently reboots them. The lazy-restart pattern falls out of resolving configuration per thread instead of per process.
