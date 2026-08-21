---
sidebar_position: 2
title: Quickstart
---

# Build Your First Agent
This quickstart creates a local agent with in-memory state, launches one conversation thread, and streams its events. You need a Tokio `LocalSet` because local agent tasks do not require `Send`.

```rust
use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
use infinity_agent_core::system::{AgentEvent, AgentSystemBuilder, StaticModel};
use tokio::task::LocalSet;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    LocalSet::new()
        .run_until(async {
            let model = StaticModel::new(provider, "claude-sonnet-4-5").await?;

            let system = AgentSystemBuilder::new_local(
                InMemoryConversationStore::new(),
                InMemoryStateStore::new(),
                model,
            )
            .with_tokio_sleep_tools()
            .build_local()
            .start();

            let mut thread = system.thread_builder().launch().await;
            thread.send_user_text("Write a haiku about distributed systems").await?;

            while let Some(event) = thread.recv().await {
                println!("{event:?}");
                if matches!(event, AgentEvent::CompletionFinished { .. }) {
                    break;
                }
            }

            system.shutdown().await;
            Ok(())
        })
        .await
}
```

Replace `provider` and the model ID with one of the supported [model providers](../model-providers.md). `StaticModel` keeps every completion on that model.

The two in-memory stores retain conversation and runtime state for the life of this process. They implement the full runtime semantics, so no other persistence setup is needed to run an agent.

## Launch a Configured Thread
`thread_builder()` adds tools and instructions to one new conversation before it starts:

```rust
let mut thread = system
    .thread_builder()
    .tool(Box::new(ReadIssue { github: github.clone() }))
    .extra_system_prompt("You are maintaining the Infinity repository.")
    .launch()
    .await;

thread.send_user_text("Summarize issue #42").await?;
```

`launch()` generates the thread ID and returns its `ThreadHandle`. Tools and instructions added here also apply to subagents that this thread spawns.

Continue with [Launching Local Threads](./running-locally.md) for replay, existing-thread attachment, per-thread models, and the complete `ThreadBuilder` API.

## Add Integrations When You Need Them
The initial system already includes tools for spawning and communicating with subagents, cancelling subscriptions, and waiting for input. `.with_tokio_sleep_tools()` adds timed sleep tools for this resident process.

Add external capabilities through focused guides:

- [Connect RAP Servers](./rap-servers.md)
- [Connect MCP Servers](./mcp-servers.md)
- [Write Custom Tools](./custom-tools.md)

For durable stores, changing models between turns, or changing how work is scheduled, see [Customizing the Engine](./customizing-the-engine.md). For tools and models selected by tenant or conversation, see [Dynamic Thread Configuration](./dynamic-configuration.md).
