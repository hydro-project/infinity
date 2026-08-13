---
sidebar_position: 5
title: Connecting MCP Servers
---

# Connecting MCP Servers
An **`McpToolSet`** from the `infinity-mcp-bridge` crate exposes an MCP server to a local agent system as two tools: one for discovering the server's tools and one for invoking them. It connects to the MCP server on first use, so setup does not start a subprocess or make a network request.

## Connect a stdio Server
Create the toolset, then register its tools on the system or a single thread:

```rust
use std::collections::HashMap;

use infinity_mcp_bridge::McpToolSet;

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
    .build_local()
    .start();
```

The model sees `filesystem_list_tools` and `filesystem_invoke_tool`. It calls the first tool to read the MCP server's native tool definitions, then passes a selected name and arguments to the second.

`McpToolSet` owns the lazy MCP connection. Keep it alive while creating tools; each returned tool holds the shared connection state. For stdio servers, dropping the last tool terminates the child process.

To expose the server to one conversation instead of every thread, add the same tools through `thread_builder()`:

```rust
let mut thread = system
    .thread_builder()
    .tools(filesystem.tools())
    .extra_system_prompt("Use the filesystem server only for /workspace.")
    .launch()
    .await;
```

The launched thread's subagents inherit both MCP tools. See [Launching Local Threads](./running-locally.md#configuring-a-new-thread) for the other launch-time options.

## Connect a Streamable HTTP Server
`McpToolSet::http` connects to a remote MCP endpoint and applies the supplied headers to initialization and tool requests:

```rust
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
    .build_local();
```

The client retains the MCP session ID returned by the server and sends it on later requests. Store credentials in transport headers rather than prompts or tool arguments.

## What the Adapter Handles
The bridge owns the complete adapter contract, and Infinity Code's daemon proxy is built from the same definitions and dispatch path. It covers:

- MCP initialization and session handling
- lazy stdio and Streamable HTTP connections
- `tools/list` discovery and input-schema rendering
- `tools/call` invocation and text, image, resource, and error formatting
- delivery of each result back to the originating local thread

No RAP endpoint, toolset manifest loader, or callback server is involved. Those components remain useful when an MCP server must be exposed across a process or network boundary, but a local agent system can call the shared adapter directly.

:::caution

The adapter serializes requests to one MCP server through its connection. Create separate `McpToolSet` values when conversations require isolated server processes, credentials, or MCP session state.

:::
