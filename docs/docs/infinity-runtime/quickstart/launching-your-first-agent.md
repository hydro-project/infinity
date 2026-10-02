---
sidebar_position: 1
title: Launching Your First Agent
---

# Launching Your First Agent
In this tutorial, you will create a Rust project, launch an agent backed by Amazon Bedrock, and stream its events. The next two pages extend the same project with [custom tools](./adding-tools.md) and [external tool servers](./connecting-rap-and-mcp.md).

## Creating a Project
Infinity agents are ordinary Rust programs, so you will manage dependencies, build, and run them with Cargo. Start by creating a new binary project:

```bash
cargo new my-agent
cd my-agent
```

The project needs two Infinity crates: `infinity-agent-core`, which is the runtime itself, and `infinity-provider-bedrock`, which is the model provider used in this tutorial. Neither is published to crates.io yet, so you will add both as git dependencies. The runtime is async, so you will also need Tokio:

```toml
[dependencies]
infinity-agent-core = { git = "https://github.com/hydro-project/infinity" }
infinity-provider-bedrock = { git = "https://github.com/hydro-project/infinity" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Writing the Agent
An agent runs inside an **agent system**, which owns the stores, tools, and model provider that its conversation threads share. Each conversation is a thread, and each thread has a handle that can be used to send input and receive events. Replace `src/main.rs` with a program that builds a system, launches one thread, and prints each event the thread produces:

```rust,no_run
use std::sync::Arc;

use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
use infinity_agent_core::system::{AgentEvent, AgentSystemBuilder, StaticModel};
use infinity_provider_bedrock::BedrockProvider;
use tokio::task::LocalSet;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    LocalSet::new()
        .run_until(async {
            let provider = Arc::new(BedrockProvider::from_env());
            let model = StaticModel::new(provider, "global.anthropic.claude-sonnet-4-6").await?;

            let system = AgentSystemBuilder::new_local(
                InMemoryConversationStore::new(),
                InMemoryStateStore::new(),
                model,
            )
            .with_tokio_sleep_tools()
            .start();

            let mut thread = system.thread_builder().launch().await;
            thread
                .send_user_text("Write a haiku about distributed systems")
                .await?;

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

Let's walk through the pieces:

- `BedrockProvider::from_env()` reads AWS configuration from the environment. Any other [model provider](../model-providers.md) works the same way: you can construct the provider and then pick a model from its catalog. `StaticModel` looks the model id up in the catalog and will keep every completion on that model.
- `AgentSystemBuilder::new_local` builds a system that runs inside this process. The two in-memory stores hold conversation history and runtime state for the life of the process; they implement the full runtime semantics, so nothing else is needed to run an agent.
- `.with_tokio_sleep_tools()` gives agents `sleep` and `sleep_until` tools backed by Tokio timers. The system already includes tools for spawning subagents, cancelling subscriptions, and waiting for input.
- Local agent tasks are not `Send`, so the system has to run inside a `LocalSet`.

The provider uses your existing AWS credentials, so you can now run the agent with `cargo run`. The thread will emit `UserInput` for the accepted message, then `CompletionStarted`, a series of `TextChunk`s carrying the haiku, and finally `CompletionFinished` with token usage. The loop breaks on `CompletionFinished` and shuts the system down. If the model calls a tool (for example `sleep`), you will also see a `ToolCall` event, followed by a `ToolResult` and a second completion round.

:::info

Bedrock requires [model access](https://docs.aws.amazon.com/bedrock/latest/userguide/model-access.html) to be enabled for the Anthropic models in your region.

:::

## Configuring the Thread
The first thread used the system defaults, but `thread_builder()` can also configure each conversation before it starts. The most common option is an extra system prompt, which is appended to the runtime's base instructions:

```rust,no_run
# use std::sync::Arc;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
# use infinity_provider_bedrock::BedrockProvider;
# async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let provider = Arc::new(BedrockProvider::from_env());
# let model = StaticModel::new(provider, "global.anthropic.claude-sonnet-4-6").await?;
# let system = AgentSystemBuilder::new_local(
#     InMemoryConversationStore::new(),
#     InMemoryStateStore::new(),
#     model,
# )
# .start();
let mut thread = system
    .thread_builder()
    .extra_system_prompt("You are maintaining the Infinity repository.")
    .launch()
    .await;

thread.send_user_text("Summarize the open release blockers").await?;
# Ok(())
# }
```

`launch()` generates the thread ID and returns its `ThreadHandle`. Instructions and tools that are set here will also apply to any subagents the thread spawns. [Launching Local Threads](../agent-systems/running-locally.md) covers the rest of the `ThreadBuilder` API, including replay, attaching to existing threads, and per-thread models.

Next, we will [give the agent a custom tool](./adding-tools.md).
