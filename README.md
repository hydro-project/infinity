# [Infinity](https://infinity.hydro.run)

Infinity is an open-source Rust framework for building agents, on a runtime efficient enough to fit fifty thousand of them in the memory of a Raspberry Pi. Infinity does for agents what async did for threads: instead of blocking on slow tools, Infinity agents run them concurrently, yield while they wait, and cost nothing until the next event arrives.

```rust
let system = AgentSystemBuilder::new_local(
    InMemoryConversationStore::new(),
    InMemoryStateStore::new(),
    StaticModel::new(provider, "claude-sonnet-4-5").await?,
)
.start();

let mut thread = system.thread_builder().launch().await;
thread.send_user_text("Write a haiku about Rust").await?;

while let Some(event) = thread.recv().await {
    println!("{event:?}");
}
```

Between turns, an agent is pure data, just its conversation history with no task, no stack, and no open connection, so a waiting agent hibernates for free and wakes on the next message. The same agent code runs unchanged in a resident process on your laptop and on serverless platforms like AWS Lambda, where scale to zero is the default.

**Get started today at [infinity.hydro.run](https://infinity.hydro.run)!**

## Learn More

- **[Infinity Runtime](https://infinity.hydro.run/docs/infinity-runtime/overview)**: Build your first agent with the [quickstart](https://infinity.hydro.run/docs/infinity-runtime/agent-systems/building-a-system), see how yielding turns and hibernation work in the [architecture guide](https://infinity.hydro.run/docs/infinity-runtime/architecture), and [deploy on AWS Lambda](https://infinity.hydro.run/docs/infinity-runtime/deploying-on-lambda) with the included CDK constructs.

- **[Reactive Agent Protocol (RAP)](https://infinity.hydro.run/docs/rap/what-is-rap)**: Serve tools over the network without holding a connection open: results and subscription events are delivered whenever they are ready, even to agents that currently have no process at all. Existing MCP servers run unchanged through a compatibility layer, and anyone can implement the open [specification](https://infinity.hydro.run/docs/rap/spec/overview).

- **[Infinity Code](https://infinity.hydro.run/docs/infinity-code/overview)**: A coding agent built on the runtime for concurrent work: it runs builds and tests in the background while their output streams in, edits in parallel threads with stacked sandboxes, and hands you each result as a diff to review.
