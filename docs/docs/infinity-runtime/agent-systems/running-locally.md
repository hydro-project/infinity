---
sidebar_position: 3
title: Launching Local Threads
---

# Launching Local Threads
A **local agent system** runs for the lifetime of a Tokio process and exposes each conversation through a `ThreadHandle`. Launcher mode creates a new root thread and its configuration in one operation:

```rust
let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(shared_tools)
    .build_local()
    .with_thread_launcher()
    .start();

let mut reviewer = system
    .thread_builder()
    .tool(Box::new(FetchDiffTool { repo: repo.clone() }))
    .extra_system_prompt("Review changes in the Infinity repository.")
    .model(review_model)
    .launch()
    .await;

reviewer.send_user_text("Review PR #92").await?;
while let Some(event) = reviewer.recv().await {
    render_event(event);
}
```

`with_thread_launcher()` changes `start()` to return a `LaunchingSystem`. Its `thread_builder()` generates a thread ID, records the launch configuration, attaches to the new thread, and returns its handle.

## Configuring a New Thread
`ThreadBuilder` accepts four kinds of launch-time configuration:

```rust
let mut thread = system
    .thread_builder()
    .tool(Box::new(ReadIssue { client: github.clone() }))
    .tools(repository_tools)
    .extra_system_prompt("Work only in the payments repository.")
    .model(payments_model)
    .launch()
    .await;
```

`tool` and `tools` add capabilities to the tools registered on the system. `extra_system_prompt` appends instructions after the system-wide extra prompt. `model` replaces the system-wide `ModelSource` for this root thread.

Subagents spawned by the thread inherit its added tools, prompt, and model. This keeps a root conversation and its child work on the same repository or tenant configuration.

:::caution

Launch configuration is stored in the current process. Conversation history remains in the stores, but a new process cannot recover the launch-time tools, prompt, or model from that history. Use [Dynamic Thread Configuration](./dynamic-configuration.md) when the configuration must survive a restart.

:::

## Sending Input and Receiving Events
<a id="talking-to-threads"></a>
A **`ThreadHandle`** combines the thread ID, a replay snapshot, an event receiver, and methods for sending input:

```rust
let thread_id = thread.thread_id().to_owned();
render_history(thread.replay());

thread.send_user_text("Summarize the highest-risk change").await?;

while let Some(event) = thread.recv().await {
    match event {
        AgentEvent::TextChunk { text } => render_text(text),
        AgentEvent::ToolCall { name, .. } => render_tool_call(name),
        AgentEvent::CompletionFinished { .. } => break,
        other => render_event(other),
    }
}
```

`replay()` contains the committed conversation plus any partial turn that was active when the handle attached. `recv()` begins at the same boundary. Rendering replay first and then live events produces each event once.

`send_user_text` marks the input as user-driven. If a completion is already streaming, the local system interrupts it, persists the partial turn, and begins a new completion with the latest text. `send` accepts a complete `InputMessage` for tool results, timers, and other synthetic inputs.

A handle can outlive the `LaunchingSystem`. Its send methods therefore return `ChannelSendError` if the system has shut down.

## Attaching to an Existing Thread
Store a thread's ID when another client or a later part of the application must attach:

```rust
let Some(mut thread) = system.thread_handle(&saved_thread_id).await else {
    return Err("conversation does not exist".into());
};

render_history(thread.replay());
while let Some(event) = thread.recv().await {
    render_event(event);
}
```

`LaunchingSystem::thread_handle` returns `None` when the ID was not launched in this process and has no stored history. This prevents a mistyped ID from creating a new unconfigured conversation. New threads should always come from `thread_builder()`.

Multiple handles may attach to one thread. Each receives its own replay boundary and every later event. Dropping a handle detaches that receiver without stopping the thread.

## Running Without Launcher Mode
If thread IDs come from an external system and every thread uses the system-wide configuration, start the local system directly:

```rust
let running = local_system.start();
let mut thread = running.thread_handle("customer-42").await;
thread.send_user_text("hello").await?;
```

`RunningSystem::thread_handle` accepts an ID even when no history exists. The first input creates that conversation. Prefer launcher mode when the application creates threads itself or supplies per-thread tools.

Use [Connecting MCP Servers](./mcp-servers.md) to expose a stdio or HTTP MCP server as tools before launching a thread. Use [Writing Custom Tools](./custom-tools.md) for local Rust tools, including tools that emit subscription streams. Applications that need protocol-specific replay or fan-out can replace handles with [Observers](./observers.md).
