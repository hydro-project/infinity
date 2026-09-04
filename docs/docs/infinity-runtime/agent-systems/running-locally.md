---
sidebar_position: 2
title: Launching Local Threads
---

# Launching Local Threads
A **local agent system** runs for the lifetime of a Tokio process and exposes each conversation through a `ThreadHandle`. To create a new root thread and configure it in one operation, use a thread builder:

```rust
let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(shared_tools)
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

`start()` returns a `LaunchingSystem`. Calling `thread_builder()` on it will generate a thread ID, record the launch configuration, attach to the new thread, and return its handle.

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

`tool` and `tools` add capabilities on top of the tools registered on the system. `extra_system_prompt` appends instructions after the system-wide extra prompt, and `model` replaces the system-wide `ModelSource` for this root thread.

Subagents spawned by the thread will inherit its added tools, prompt, and model, so a root conversation and its child work stay on the same repository or tenant configuration.

:::caution

Launch configuration is stored in the current process. Conversation history remains in the stores, but a new process cannot recover the launch-time tools, prompt, or model from that history. Use [Dynamic Thread Configuration](./dynamic-configuration.md) when the configuration must survive a restart.

:::

## Sending Input and Receiving Events
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

`replay()` contains the committed conversation plus any partial turn that was active when the handle attached, and `recv()` begins at the same boundary. This means that if you render the replay first and then the live events, each event will be rendered exactly once.

`send_user_text` marks the input as user-driven. If a completion is already streaming, the local system will interrupt it, persist the partial turn, and begin a new completion with the latest text. For tool results, timers, and other synthetic inputs, `send` accepts a complete `InputMessage`.

A handle can outlive the `LaunchingSystem`, so its send methods will return `ChannelSendError` if the system has already shut down.

## Attaching to an Existing Thread
If another client or a later part of the application must attach to a conversation, store the thread's ID:

```rust
let Some(mut thread) = system.thread_handle(&saved_thread_id).await? else {
    return Err("conversation does not exist".into());
};

render_history(thread.replay());
while let Some(event) = thread.recv().await {
    render_event(event);
}
```

`LaunchingSystem::thread_handle` returns an error when the conversation store lookup fails, and it returns `None` when the ID was not launched in this process and does not exist in the store. This prevents a mistyped ID from creating a new unconfigured conversation; new threads always come from `thread_builder()`.

Multiple handles may attach to one thread, and each will receive its own replay boundary and every later event. Dropping a handle detaches that receiver without stopping the thread. When handles are not enough, for example when replay must be rendered in a protocol-specific format or events must fan out to many clients, you can implement an [observer](./observers.md) instead.
