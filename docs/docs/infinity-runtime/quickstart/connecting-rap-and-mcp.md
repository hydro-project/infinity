---
sidebar_position: 3
title: Connecting RAP & MCP Servers
---

# Connecting RAP & MCP Servers
In the final part of the quickstart, you will connect your agent to external tool servers over both supported protocols. A **RAP tool server** publishes its tools over HTTP and delivers results asynchronously to a callback URL; the `infinity-rap-bridge` crate packages the discovery, callback conversion, and cancellation this involves. An **MCP server** instead connects in process through the `infinity-mcp-bridge` crate, with no callback path at all.

Add the bridge crates to `Cargo.toml`:

```toml
infinity-rap-bridge = { git = "https://github.com/hydro-project/infinity" }
infinity-mcp-bridge = { git = "https://github.com/hydro-project/infinity" }
```

## Connecting a RAP Server
First, bind the callback destination, so that every tool discovered afterwards will receive its URL. After starting the system, serve callbacks into that system's sender:

```rust,no_run
# use std::sync::Arc;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
# use infinity_provider_bedrock::BedrockProvider;
use infinity_rap_bridge::{RapCallbackBridge, RapToolSet};

# async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let provider = Arc::new(BedrockProvider::from_env());
# let model = StaticModel::new(provider, "global.anthropic.claude-sonnet-4-6").await?;
# let (conversation_store, state_store) =
#     (InMemoryConversationStore::new(), InMemoryStateStore::new());
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
    .start();

let (mut views, callback_server_task) = bridge.serve_into(system.sender());
# let _ = (&mut views, callback_server_task);
# Ok(())
# }
```

`connect` reads `/.well-known/rap-toolset` from each server and registers every tool in the returned manifests. When an agent invokes a tool, the invocation will include the explicit callback URL. `serve_into` receives the later callbacks and sends converted agent inputs to the system, so keep `callback_server_task` alive while callbacks can still arrive.

View updates are display state rather than agent history, so they are returned separately. Consume `views` in your display or persistence task:

```rust,no_run
# use rap_protocol::RapViewUpdate;
# struct ViewStore;
# impl ViewStore {
#     async fn store(&self, _update: RapViewUpdate) -> Result<(), std::io::Error> {
#         Ok(())
#     }
# }
# async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let view_store = ViewStore;
# let (_view_tx, mut views) = tokio::sync::mpsc::unbounded_channel::<RapViewUpdate>();
while let Some(update) = views.recv().await {
    view_store.store(update).await?;
}
# Ok(())
# }
```

A single bridge can serve several servers at once. Pass every base URL in one call:

```rust,no_run
# use infinity_rap_bridge::{RapCallbackBridge, RapToolSet};
# async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let bridge = RapCallbackBridge::bind().await?;
let rap = RapToolSet::connect(
    vec![
        "http://127.0.0.1:9000".to_owned(),
        "https://tools.example.com".to_owned(),
    ],
    "workspace-42",
    bridge.callback_url().to_owned(),
)
.await?;
# let _ = rap;
# Ok(())
# }
```

The second argument identifies this connected tool set's application session; use a value that is stable for the lifetime of the connection, so that its manifest entries share one cache scope. The third argument is the callback destination that all discovered tools will include in their invocations.

Tool names from all manifests share one namespace, so give tools distinct names when connecting multiple servers.

### Results and Subscriptions
A RAP server acknowledges an invocation and later posts a `RapCallback` to the supplied callback URL. If the server instead rejects the invocation with a non-2xx status, the runtime records a descriptive error tool result naming the tool, endpoint, and status, because no callback will ever settle that call. The bridge handles tool results, OAuth requests, user choices, and subscription events. Tool results, OAuth requests, and user choices use stable deduplication IDs, so an HTTP retry will not enter history twice. Each subscription event receives its own ID, because one subscription can emit many events.

A tool result with `subscription: true` keeps the tool call active. Later subscription events will wake the thread, and an event with `final: true` removes the active subscription. See [Adding Tools](./adding-tools.md#subscription-streams) for the same lifecycle implemented directly in Rust.

The bridge's `notifier()` configures lifecycle notifications for the same server URLs used during discovery. The built-in `cancel_subscription` and thread-closing tools use it to call `/cancel_tool_call` and `/close_thread`.

:::note

Lifecycle notifications currently go to every connected server because the runtime does not record which server owns each tool call. Each server should ignore IDs it does not own.

:::

### Controlling the Callback Destination
`RapCallbackBridge::bind` listens on `127.0.0.1`, which works when the RAP server runs in the same process, on the same host, or otherwise has access to the host loopback address. A RAP server in another container or on another host will need a network-reachable receiver instead; pass that receiver's URL to `RapToolSet::connect`. Platform receivers such as the Lambda RAP receiver use this pattern.

Callback routing is explicit and immutable: a tool set contains the callback URL, but it does not own a sender or a callback task. `RapCallbackBridge::serve_into` binds the listener to one system sender, which avoids destination changes when tools are shared or invoked concurrently.

Admission control does not require custom callback handling. The bridge forwards agent inputs into the system's input queue, where the local router consults the state store's [wake policy](../agent-systems/customizing-the-engine.md#persistence-providers) before spawning a driver. A callback for a thread that the application refuses to wake will be dropped there.

For example, the Infinity Code daemon uses `RapCallbackBridge` with a wake policy that refuses callbacks for sessions the user shut down, and its callback loop persists view updates for connected clients. A platform receiver that cannot deliver into a local sender, such as the Lambda RAP receiver enqueueing to SQS, decodes callbacks with `prepare_callback` and forwards the resulting input and deduplication ID itself.

## Connecting an MCP Server
An **`McpToolSet`** exposes an MCP server to a local agent system as two tools: one that discovers the server's tools, and one that invokes them. The toolset connects to the MCP server on first use, so setup will not start a subprocess or make a network request. For a stdio server, create the toolset from the launch command and then register its tools on the system:

```rust,no_run
use std::collections::HashMap;
# use std::sync::Arc;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
# use infinity_provider_bedrock::BedrockProvider;

use infinity_mcp_bridge::McpToolSet;

# async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let provider = Arc::new(BedrockProvider::from_env());
# let model = StaticModel::new(provider, "global.anthropic.claude-sonnet-4-6").await?;
# let (conversation_store, state_store) =
#     (InMemoryConversationStore::new(), InMemoryStateStore::new());
let filesystem = McpToolSet::stdio(
    "filesystem",
    vec![
        "npx".to_owned(),
        "-y".to_owned(),
        "@modelcontextprotocol/server-filesystem".to_owned(),
        "/workspace".to_owned(),
    ],
    HashMap::new(),
);

let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(filesystem.tools())
    .start();
# let _ = system;
# Ok(())
# }
```

The model sees `filesystem_list_tools` and `filesystem_invoke_tool`. It can call the first tool to read the MCP server's native tool definitions, and then pass a selected name and arguments to the second.

`McpToolSet` owns the lazy MCP connection, so keep it alive while creating tools; each returned tool holds the shared connection state. For stdio servers, dropping the last tool will terminate the child process.

To expose the server to one conversation instead of every thread, add the same tools through `thread_builder()`:

```rust,no_run
# use std::collections::HashMap;
# use std::sync::Arc;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
# use infinity_mcp_bridge::McpToolSet;
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
# let filesystem = McpToolSet::stdio("filesystem", vec![], HashMap::new());
let mut thread = system
    .thread_builder()
    .tools(filesystem.tools())
    .extra_system_prompt("Use the filesystem server only for /workspace.")
    .launch()
    .await;
# let _ = &mut thread;
# Ok(())
# }
```

The launched thread's subagents inherit both MCP tools. See [Launching Local Threads](../agent-systems/running-locally.md#configuring-a-new-thread) for the other launch-time options.

A remote MCP endpoint connects the same way through `McpToolSet::http`, which applies the supplied headers to initialization and tool requests:

```rust,no_run
# use std::collections::HashMap;
# use std::sync::Arc;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
# use infinity_mcp_bridge::McpToolSet;
# use infinity_provider_bedrock::BedrockProvider;
# async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let provider = Arc::new(BedrockProvider::from_env());
# let model = StaticModel::new(provider, "global.anthropic.claude-sonnet-4-6").await?;
# let (conversation_store, state_store) =
#     (InMemoryConversationStore::new(), InMemoryStateStore::new());
# let access_token = "example-token";
let github = McpToolSet::http(
    "github",
    "https://mcp.example.com/mcp",
    HashMap::from([(
        "Authorization".to_owned(),
        format!("Bearer {access_token}"),
    )]),
);

let system = AgentSystemBuilder::new_local(conversation_store, state_store, model)
    .tools(github.tools())
    .start();
# let _ = system;
# Ok(())
# }
```

The client retains the MCP session ID returned by the server and sends it on later requests. Credentials should be stored in transport headers rather than in prompts or tool arguments.

The bridge owns the complete adapter contract: MCP initialization and session handling, lazy stdio and Streamable HTTP connections, `tools/list` discovery and input-schema rendering, `tools/call` invocation with text, image, resource, and error formatting, and delivery of each result back to the originating local thread. Infinity Code's daemon proxy is built from the same definitions and dispatch path. Note that no RAP endpoint, toolset manifest loader, or callback server is involved: those components remain useful when an MCP server must be exposed across a process or network boundary, but a local agent system calls the shared adapter directly.

:::caution

The adapter serializes requests to one MCP server through its connection. Create separate `McpToolSet` values when conversations require isolated server processes, credentials, or MCP session state.

:::

This completes the quickstart. The [Agent Systems](../agent-systems/overview.md) section covers the full API for threads, observers, step mode, and engine customization; [Model Providers](../model-providers.md) documents the `ModelProvider` trait and the available backends. To deploy the same agent on AWS Lambda, where RAP and MCP servers connect through CDK constructs instead of in-process bridges, see [Serverless Deployments](../serverless/quickstart.mdx).
