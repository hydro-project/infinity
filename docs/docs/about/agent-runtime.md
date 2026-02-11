---
sidebar_position: 4
title: Agent Runtime
---

# Agent Runtime

The agent runtime is the host process that connects an LLM to RAP tools. It's the equivalent of an MCP client, but designed for a world where tool calls are asynchronous and the runtime doesn't stay alive between them.

The runtime manages the conversation lifecycle: it receives inputs, maintains conversation state, runs LLM completions, dispatches tool calls, and delivers output. Tools never interact with the LLM directly — the runtime mediates everything.

## Core responsibilities

**Conversation state.** The runtime owns the conversation history. Because the runtime is ephemeral (it exits between tool calls), state must be persisted to durable storage and reloaded on each invocation. The runtime is the single source of truth for what the LLM has seen and said.

**Tool dispatch.** When the LLM produces a tool call, the runtime resolves it to an HTTP endpoint and POSTs the invocation. The tool acknowledges immediately. The runtime does not wait for the result — it persists state and exits. This fire-and-forget dispatch is what enables hibernation.

**Result routing.** Tool results, subscription events, and user messages all arrive through the same input channel. The runtime doesn't distinguish between them at the transport level — it loads state, appends the new message, and runs the LLM again. The `group_id` field routes messages to the correct conversation thread.

**Tool definitions.** The runtime maintains the set of available tools and their JSON Schema definitions. These are passed to the LLM on each completion request so it knows what tools it can call. How definitions are stored and loaded is implementation-specific.

## Capabilities provided to tools

The invocation payload includes context that tools need to operate:

| Field | Purpose |
|---|---|
| `callback_url` | Where the tool should POST results. The runtime provides this so tools can return results asynchronously without knowing the runtime's internal architecture. |
| `group_id` | The conversation thread ID. Tools include this when returning results so the runtime routes them to the correct conversation. |
| `user_id` | The end user's identity, if available. Tools can use this for authorization, personalization, or audit logging. |

Tools are fully decoupled from the runtime. They don't know what LLM is being used, what the conversation history looks like, or whether the runtime is a Lambda function or a CLI process. They receive a request, do their work, and POST the result to the callback URL.

## Interruption model

Because the runtime is stateless and message-driven, interruptions are handled naturally. If a user sends a message while the agent is waiting for a tool result, the runtime simply starts, loads state, and processes the user message. When the tool result arrives later, it's processed as a separate invocation. The LLM sees both in its context and can reason about the sequence of events.

This means agents are always responsive to users, even while waiting for long-running operations.

Runtimes need to be careful about concurrency within a single conversation thread. If two messages for the same thread arrive close together (e.g. a user message and a tool result), processing them simultaneously could corrupt conversation state. The Infinity Runtime handles this by using a FIFO queue with message group IDs — messages within a thread are serialized, while different threads process concurrently. Other implementations might use database locks, optimistic concurrency control, or single-threaded processing per thread. The key invariant: within a thread, messages must be processed one at a time.

## MCP compatibility

The runtime can integrate MCP servers through a proxy layer. See [MCP Compatibility](/docs/about/mcp-compatibility) for how this works.

## Implementation flexibility

The protocol doesn't prescribe how the runtime is built. The reference implementation (the [Infinity Runtime](/docs/infinity-runtime/overview)) is a Rust Lambda function using Amazon Bedrock, but any process that can receive messages, call an LLM, POST HTTP requests, and persist state can serve as a RAP runtime. See [Building a Runtime](/docs/using-rap/building-a-runtime) for implementation guidance.
