---
sidebar_position: 1
title: Overview
---

# RAP Specification

This section defines the Reactive Agent Protocol — the message formats, lifecycle, and contracts between agent runtimes and tools.

RAP has two participants:

- **Agent Runtime** — orchestrates LLM completions, dispatches tool calls, manages conversation state and threads
- **Tool** — an independent HTTP service that receives invocations and returns results asynchronously via the RAP receiver

Communication is asynchronous. The runtime invokes tools via HTTP POST. Tools return results by POSTing to the RAP receiver endpoint, which enqueues them on the runtime's input queue.

## Protocol flow

```
Runtime                          Tool                         RAP Receiver
   │                              │                              │
   │──── POST invocation ────────▶│                              │
   │◀─── HTTP 200 (ack) ─────────│                              │
   │                              │                              │
   │  (runtime exits)             │  (tool processes async)      │
   │                              │                              │
   │                              │──── POST tool_result ───────▶│
   │                              │                              │──── enqueue ────▶ Input Queue
   │                              │                              │
   │◀──────────────────── message from input queue ──────────────│
   │  (runtime starts, continues conversation)                   │
```

## Message types

The RAP receiver accepts three message types:

| Type | Purpose | Required fields |
|---|---|---|
| `tool_result` | Result of a tool invocation | `group_id`, `id`, `text` |
| `subscription_event` | Event from an active subscription | `group_id`, `tool_call_id`, `text` |
| `oauth` | OAuth authorization URL for user | `group_id`, `id`, `auth_url` |

All messages include `group_id` (the thread ID) for routing to the correct conversation.
