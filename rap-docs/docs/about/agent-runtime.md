---
sidebar_position: 2
title: Agent Runtime
---

# Agent Runtime

The agent runtime is the process that orchestrates LLM completions and tool execution. In RAP, the runtime is designed to be ephemeral — it starts, processes one or more messages, and exits.

The runtime's job on each invocation:

1. Pull messages from the input queue
2. Load conversation history from durable storage
3. Run the LLM completion loop — the model reads history, produces text or tool calls
4. For each tool call, POST the invocation to the tool's HTTP endpoint. The tool acknowledges immediately. The runtime does **not** wait for the tool to finish.
5. Persist updated conversation history
6. Exit

When a tool result arrives later, it lands on the input queue, the runtime starts again, and the cycle repeats. The runtime never blocks on a tool.

**Thread management.** The runtime supports spawning child threads via the `spawn_thread` tool. A child thread gets its own message group on the input queue, so it processes concurrently with the parent. The child inherits the parent's conversation history up to the spawn point. When it finishes, it can report results back to the parent via a synthetic message.

**Interruption handling.** If a user message arrives while the agent is waiting for a tool result, the runtime processes it immediately. When the original tool result eventually arrives, the runtime injects a synthetic "interrupted" result into the conversation so the LLM understands what happened.

**MCP compatibility.** MCP servers are wrapped in a Lambda proxy that spawns the MCP process, forwards JSON-RPC requests, and returns results via RAP. From the runtime's perspective, an MCP tool looks like any other RAP tool — it's invoked via HTTP and returns results asynchronously through the RAP receiver.

The reference implementation is a Rust Lambda function using Amazon Bedrock for completions and Aurora DSQL for conversation state. But the protocol is runtime-agnostic — any process that speaks RAP can fill this role.
