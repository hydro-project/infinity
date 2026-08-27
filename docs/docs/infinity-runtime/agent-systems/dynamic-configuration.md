---
sidebar_position: 7
title: Dynamic Thread Configuration
---

# Dynamic Per-Thread Configuration
A long-lived agent system often serves conversations with different credentials, tools, instructions, and model selections. There are two ways to supply those differences:

- A **`ThreadConfigSource`** derives tools, an extra system prompt, and a RAP notifier from a thread ID. Use it when the configuration must be reconstructed after a process restart.
- A **`ModelSource`** chooses a model at the start of each completion round. Use it when model selection can differ by conversation or change while a conversation is active.

For configuration chosen when a process creates a new thread, local systems also provide [`thread_builder()`](./running-locally.md#configuring-a-new-thread). That configuration is convenient but process-local. The sources on this page are the durable option because they can resolve their output from stores, a tenant registry, or another external system.

## Resolving Tools and Instructions
**`ThreadConfigSource`** returns the configuration used when a thread is loaded:

```rust
#[async_trait(?Send)]
pub trait ThreadConfigSource<M: InputSender, H: HttpClient> {
    async fn resolve(
        &self,
        thread_id: &str,
    ) -> Result<ThreadConfig<M, H>, BoxError>;
}
```

`ThreadConfig` contains three values:

- `tools`: the tools available to the model, in addition to the runtime's built-in tools
- `extra_system_prompt`: instructions appended to the built-in system prompt
- `rap_notifier`: a client for notifying the thread's RAP servers about cancellation and thread closure

The following source gives every root conversation and its subagents the same tenant configuration. `ConversationStore::get_ancestor_chain` maps a child back to its root before the tenant registry is queried:

```rust
use std::rc::Rc;

use async_trait::async_trait;
use infinity_agent_core::system::{ThreadConfig, ThreadConfigSource};
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::tools::Tool;
use infinity_agent_core::traits::ConversationStore;
use rap_client::http::SimpleHttpClient;
use rap_client::notifier::RapNotifier;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct TenantConfig {
    name: String,
    tools: Vec<Rc<dyn Tool<ChannelSender>>>,
    notifier: Option<RapNotifier<SimpleHttpClient>>,
}

struct TenantThreadConfig<C> {
    conversations: C,
    tenants: TenantRegistry,
}

#[async_trait(?Send)]
impl<C> ThreadConfigSource<ChannelSender, SimpleHttpClient>
    for TenantThreadConfig<C>
where
    C: ConversationStore + 'static,
{
    async fn resolve(
        &self,
        thread_id: &str,
    ) -> Result<ThreadConfig<ChannelSender, SimpleHttpClient>, BoxError> {
        let ancestors = self.conversations.get_ancestor_chain(thread_id).await?;
        let root_id = ancestors
            .first()
            .map(|(id, _)| id.as_str())
            .unwrap_or(thread_id);
        let tenant = self.tenants.for_root_thread(root_id).await?;

        Ok(ThreadConfig {
            tools: tenant.tools,
            extra_system_prompt: Some(format!(
                "You are assisting tenant {}. Only access that tenant's resources.",
                tenant.name,
            )),
            rap_notifier: tenant.notifier,
        })
    }
}
```

Register the source when building the system:

```rust
let system = AgentSystemBuilder::new_local(
    conversation_store.clone(),
    state_store,
    model_source,
)
.thread_config(TenantThreadConfig {
    conversations: conversation_store,
    tenants,
})
.start();
```

:::note

`.thread_config(...)` replaces `.tool(...)`, `.tools(...)`, `.extra_system_prompt(...)`, and `.rap_notifier(...)`. Put all four forms of configuration in the returned `ThreadConfig`. The runtime still adds its [built-in tools](../built-in-tools.md).

:::

### When Resolution Runs
A local system resolves `ThreadConfig` before a thread begins processing its first input. The resolved value remains in use while that thread is active. If the thread becomes idle and later receives another message, the system loads it again and calls `resolve` again.

This boundary is useful for resources that should exist only while a conversation is active. A source can load a RAP manifest, start a tenant-specific tool server, and cache the resulting tools when `resolve` runs. An idle watcher can release those resources after the system reports that every thread belonging to the root conversation has exited. The next message calls `resolve` and reconstructs them. See [System and Thread Lifecycle](./overview.md#system-and-thread-lifecycle) for the local idle signals.

`ThreadConfig` itself is not stored in conversation history. Persist the information needed to reconstruct it, such as a tenant ID, repository path, or server configuration. A resumed thread can then receive the same configuration in a new process.

Resolution receives the ID of the thread that is about to run. For a root conversation this is the user-facing thread ID. For `spawn_thread`, compaction, and subscription-event work, it can be a child ID. Map child IDs to the root when configuration belongs to the whole conversation, as in the example above. Keep the leaf ID when each subagent needs a distinct capability set. This choice affects both access control and agent behavior, so do not infer a tenant from the shape of a generated child ID.

## Choosing a Model per Thread
**`ModelSource`** resolves one `ResolvedModel` at the beginning of every completion round:

```rust
#[async_trait(?Send)]
pub trait ModelSource {
    async fn resolve(&self, thread_id: &str) -> Result<ResolvedModel, BoxError>;
}
```

The constructor accepts the source directly. A fixed deployment can pass `StaticModel`; a multi-tenant deployment can read a persisted selection:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use infinity_agent_core::system::{ModelSource, ResolvedModel};
use infinity_agent_core::traits::ConversationStore;
use infinity_provider_protocol::ModelProvider;

struct SelectedModel {
    provider: Arc<dyn ModelProvider>,
    model_id: String,
    context_window: usize,
    supports_image_input: bool,
}

struct TenantModelSource<C> {
    conversations: C,
    selections: ModelSelections,
    catalog: ModelCatalog,
}

#[async_trait(?Send)]
impl<C> ModelSource for TenantModelSource<C>
where
    C: ConversationStore + 'static,
{
    async fn resolve(&self, thread_id: &str) -> Result<ResolvedModel, BoxError> {
        let ancestors = self.conversations.get_ancestor_chain(thread_id).await?;
        let root_id = ancestors
            .first()
            .map(|(id, _)| id.as_str())
            .unwrap_or(thread_id);
        let selection = self.selections.for_root_thread(root_id).await?;
        let model = self.catalog.resolve(&selection)?;

        Ok(ResolvedModel {
            provider: model.provider,
            model_id: model.model_id,
            context_window: model.context_window,
            supports_image_input: model.supports_image_input,
        })
    }
}
```

To switch models, update the record read by `ModelSelections`. The next completion round resolves the new value. A completion already in progress continues with the model it started with. Keeping the selection in durable storage also means a restarted process makes the same choice.

The same root-versus-leaf decision applies here. Resolve through the root when subagents should inherit the conversation's model. Resolve through the leaf when child threads have their own persisted selection. Validate every stored selection against a catalog and define a fallback for models that have been removed.

## Launch-Time Configuration
**Launch-time configuration** belongs to a root thread created through `thread_builder()`. It is suited to process-local jobs where the caller already has the tool instances and model source:

```rust
let mut reviewer = system
    .thread_builder()
    .tools(review_tools)
    .extra_system_prompt("Review changes for the payments repository.")
    .model(review_model)
    .launch()
    .await;
```

The launched tools and prompt are added to the system-wide configuration, while the launched model replaces the system-wide model for that root. Subagents inherit all three from the launched root.

Launch configuration is retained in process memory. Conversation history remains durable, but a process that resumes the thread cannot reconstruct its launch configuration from history. Use a `ThreadConfigSource` and a root-aware `ModelSource` when tools, instructions, or model choices must survive restarts. Use [`thread_builder()`](./running-locally.md#configuring-a-new-thread) when creating and configuring a thread are one process-local operation.
