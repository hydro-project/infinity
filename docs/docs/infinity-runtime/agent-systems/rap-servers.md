---
sidebar_position: 4
title: Connecting RAP Servers
---

# Connecting RAP Servers
A **RAP tool server** publishes its tools over HTTP and delivers results back asynchronously. The `infinity-rap-bridge` crate packages discovery, callback conversion, and cancellation for a local agent system.

Bind the callback destination before discovery so every discovered tool receives its URL. After starting the system, serve callbacks into that system's sender:

```rust
use infinity_rap_bridge::{RapCallbackBridge, RapToolSet};

let bridge = RapCallbackBridge::bind().await?;
let server_urls = vec!["http://127.0.0.1:9000".to_owned()];
let rap = RapToolSet::connect(
    server_urls,
    "local-agent",
    bridge.callback_url().to_owned(),
)
.await?;

let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(rap.tools())
    .rap_notifier(rap.notifier())
    .build_local()
    .start();

let (mut views, callback_server_task) = bridge.serve_into(system.sender());
```

`connect` reads `/.well-known/rap-toolset` from each server and registers every tool in the returned manifests. When an agent invokes a tool, the invocation includes the explicit callback URL. `serve_into` receives later callbacks and sends converted agent inputs to the system. Keep `callback_server_task` alive while callbacks can arrive.

View updates are display state rather than agent history, so they are returned separately. Consume `views` in your display or persistence task:

```rust
while let Some(update) = views.recv().await {
    view_store.store(update).await?;
}
```

## Connect More Than One Server
Pass every server base URL in one call:

```rust
let rap = RapToolSet::connect(
    vec![
        "http://127.0.0.1:9000".to_owned(),
        "https://tools.example.com".to_owned(),
    ],
    "workspace-42",
    bridge.callback_url().to_owned(),
)
.await?;
```

The second argument identifies this connected tool set's application session. Use a stable value for the lifetime of the connection so its manifest entries share one cache scope. The third argument is the destination that all discovered tools include in their invocations.

Tool names from all manifests share one namespace. Give tools distinct names when connecting multiple servers.

## Results and Subscriptions
A RAP server acknowledges an invocation and later posts a `RapCallback` to the supplied callback URL. If the server rejects the invocation with a non-2xx status instead, the runtime records a descriptive error tool result naming the tool, endpoint, and status, because no callback will settle that call. The bridge handles tool results, OAuth requests, user choices, and subscription events. Tool results, OAuth requests, and user choices use stable deduplication IDs, so an HTTP retry does not enter history twice. Each subscription event receives its own ID because one subscription can emit many events.

A tool result with `subscription: true` keeps the tool call active. Later subscription events wake the thread, and an event with `final: true` removes the active subscription. See [Writing Custom Tools](./custom-tools.md#subscription-streams) for the same lifecycle implemented directly in Rust.

The bridge's `notifier()` configures lifecycle notifications for the same server URLs used during discovery. The built-in `cancel_subscription` and thread-closing tools use it to call `/cancel_tool_call` and `/close_thread`.

:::note

Lifecycle notifications currently go to every connected server because the runtime does not record which server owns each tool call. Each server should ignore IDs it does not own.

:::

## Control the Callback Destination
`RapCallbackBridge::bind` listens on `127.0.0.1`. This works when the RAP server runs in the same process, on the same host, or otherwise has access to the host loopback address. A RAP server in another container or host needs a network-reachable receiver instead. Pass that receiver's URL to `RapToolSet::connect`; platform receivers such as the Lambda RAP receiver use this pattern.

Callback routing is explicit and immutable. A tool set contains the callback URL but does not own a sender or callback task. `RapCallbackBridge::serve_into` binds the listener to one system sender, avoiding destination changes when tools are shared or invoked concurrently.

Admission does not require custom callback handling. The bridge forwards agent inputs into the system's input queue, where the local router consults the state store's [wake policy](./customizing-the-engine.md#persistence-providers) before spawning a driver. A callback for a thread the embedding refuses to wake is dropped there.

The Infinity Code daemon also uses `RapCallbackBridge`: its wake policy refuses callbacks for sessions the user shut down, and its callback loop persists view updates for connected clients. A platform receiver that cannot deliver into a local sender, such as the Lambda RAP receiver enqueueing to SQS, decodes callbacks with `prepare_callback` and forwards the resulting input and deduplication ID itself.

For MCP servers, use the lighter direct adapter in [Connecting MCP Servers](./mcp-servers.md). It calls MCP in process and does not require a RAP callback path.
