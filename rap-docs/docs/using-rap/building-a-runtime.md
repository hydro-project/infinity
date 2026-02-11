---
sidebar_position: 3
title: Building a Runtime
---

# Building a Runtime

The agent runtime is the component that orchestrates LLM completions and tool dispatch. The reference implementation is a Rust Lambda, but you can build a RAP-compatible runtime in any language on any platform. Here's what it needs to do.

## The runtime loop

```
receive message from input queue
  → load conversation history from durable storage
  → append the new message to history
  → loop:
      → send history + tool definitions to LLM
      → if LLM produces text → accumulate as response
      → if LLM calls tools → for each tool call:
          → POST invocation to tool's HTTP endpoint (fire-and-forget)
          → do NOT wait for the tool to finish
      → if LLM stops (no more tool calls) → break
  → persist conversation history
  → send accumulated text to output
  → exit
```

The critical detail: after dispatching tool calls, the runtime **exits**. It does not wait for results. When a tool result arrives on the input queue, the runtime starts fresh, loads history, and continues.

## Handling tool results

Tool results arrive as messages on the input queue. They look like user messages containing a `ToolResult` — the tool call ID, an optional call ID, and the result text. The runtime appends this to conversation history and runs the LLM again, which sees the result and decides what to do next.

## Handling subscription events

Subscription events arrive with a `synthetic` field containing the original tool call ID. The runtime should:

1. Look up the original tool call in conversation history
2. Spawn a new child thread (new message group ID)
3. Seed the child thread with: the subscription tool call (with an `interrupt` annotation), the event content as a tool result, and a `spawn_thread` call instructing the child to process the event
4. The child thread processes the event independently. The parent's subscription remains active.

This is the most complex part of the protocol. See the [Subscriptions](/spec/subscriptions) spec for the exact message format.

## Handling hibernation

The runtime needs to support three sleep patterns:

**Timed sleep.** The `sleep` and `sleep_until` tools schedule a future message on the input queue. For short delays, use a message delay mechanism. For long delays, use a scheduler (EventBridge Scheduler, cron, etc.) to enqueue a message at the target time. The runtime exits immediately after scheduling.

**Indefinite sleep.** The `sleep_until_event_or_input` tool is a no-op — the runtime simply exits without scheduling anything. It wakes when the next message arrives naturally (user input, subscription event, or tool result from another pending call).

**Interruption.** If a user message arrives while the agent is sleeping (waiting for a timed wake-up), the runtime processes it immediately. The pending sleep result will arrive later and be appended to history normally.

## Thread management

The runtime should support:

- `spawn_thread` — create a child thread with a new message group ID. The child inherits the parent's conversation history truncated at the spawn point.
- `report_to_parent` — send a synthetic message to the parent thread with intermediate results.
- `close_thread` — mark the thread as closed, optionally sending a final report to the parent.

Thread hierarchy is stored in the conversation database. When loading history for a child thread, the runtime walks the ancestor chain, concatenating each ancestor's history (truncated at the spawn point) to build the full context.

## Durable state

The runtime must persist conversation history to survive across invocations. The reference implementation uses Aurora DSQL with a simple schema:

- `conversation_history` table: `(session_id, message_order, message_id, message_data)`
- `thread_hierarchy` table: `(thread_id, parent_thread_id, root_thread_id, spawn_message_order, closed)`

Any durable store works — Postgres, DynamoDB, Redis, a file on disk. The key requirement is that history loads are fast and writes are durable before the runtime exits.
