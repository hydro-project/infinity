---
sidebar_position: 3
title: Subscriptions
---

# Subscriptions

Subscriptions are how RAP agents react to the outside world without polling. A tool registers an ongoing subscription ("notify me when a GitHub PR is opened," "alert me when AAPL moves more than $5") and sends events to the agent over time. Each event wakes the agent, which processes it and goes back to sleep.

## How subscriptions work

A subscription starts as a normal tool call. The agent invokes a tool like `subscribe_github_events`, and the tool does two things:

1. Records the subscription in its own storage, saving the `callback_url`, `group_id`, and `id` from the invocation
2. Returns a confirmation as a normal `tool_result` ("Subscribed to pull_request events. Subscription ID: sub_abc") with `"subscription": true`, which tells the runtime to track the tool call as an active subscription in the thread

The agent sees the confirmation and knows the subscription is active. At this point it can hibernate (e.g. call `sleep_until_event_or_input`) and wait for events to arrive.

When a matching event occurs (a webhook fires, a price threshold is crossed, a new article is published), the tool or its webhook handler POSTs a `subscription_event` to the stored `callback_url`. See the [Subscription Events spec](/docs/rap/spec/server/subscription-events) for the exact payload format.

## Synthetic tool calls

When a subscription event arrives, the runtime needs to show it to the LLM in a way that makes sense in the conversation. The challenge is that the original subscription tool call already has a result (the confirmation message). You can't just append a second result for the same tool call; the LLM would be confused.

RAP solves this with synthetic tool calls. A synthetic tool call is a fabricated assistant message that the runtime injects into conversation history. Instead of mimicking the original tool call, the Infinity Runtime uses a dedicated `receive_event__injected` tool name so the LLM can clearly distinguish events from real invocations. The `__injected` suffix marks the call as harness-injected: it is not in the toolset offered to the model, and if the model tries to invoke it directly the runtime rejects the call with an error.

When a subscription event arrives, the runtime:

1. Looks up the original subscription tool call in history (using the `tool_call_id` from the event)
2. Creates a synthetic assistant message with the tool name `receive_event__injected`, including `original_tool_name`, `original_tool_call_id`, and `original_args` in the arguments so the LLM knows which subscription produced the event
3. Appends the event content as the tool result for this synthetic call

The LLM sees what looks like a natural tool call / result pair in its history:

```
[tool_call] receive_event__injected({ original_tool_name: "subscribe_github_events", original_tool_call_id: "call_abc123", original_args: { owner: "acme", repo: "api" } })
[tool_result] {"event_type": "pull_request", "action": "opened", "number": 42, ...}
```

The synthetic tool name and `original_tool_call_id` tell the LLM that this is an event from an existing subscription, not a new tool invocation. The `original_args` provide context about which subscription produced the event. The LLM can then reason about the event and decide what to do.

The same mechanism carries reports from child threads back to their parents: the report is injected as a `receive_event__injected` call referencing the original `spawn_thread` invocation. See [Threading](/docs/infinity-runtime/threading).

## Inline vs. threaded processing

There are two strategies for where the synthetic tool call gets injected. The tool chooses between them with the `associative` field on each event.

**Inline processing** (`"associative": true`). The synthetic call is appended directly to the subscribing thread's history. The LLM sees the event in its main conversation and processes it there. This fits events that are incremental updates to an ongoing operation, such as streamed log lines from a long-running command, where the agent needs to see the updates in place. The cost is that every event accumulates in the same context window.

**Threaded processing** (the default). The runtime spawns a new child thread to process each event. The child gets a clean context: it inherits the parent's history up to the spawn point, plus the event data. It processes the event in isolation and can report results back to the parent via `close_thread`. The parent's context stays focused on its original task, and each event gets a fresh, minimal context window to work with.

The Infinity Runtime uses threaded processing whenever `associative` is not set. This is the recommended approach for independent events (webhooks, alerts, price changes), since subscriptions can generate many events and you don't want them to crowd out the parent conversation.

A tool can also mark an event with `"final": true` to signal that the subscription has ended (for example, a monitored command exited). The runtime removes the subscription from its active tracking automatically, and the tool must not send further events.

## Cancellation

The runtime tracks each thread's active subscriptions, recorded when a tool result arrives with `"subscription": true`. To cancel one, the agent calls the runtime's built-in `cancel_subscription(tool_call_id)` tool, which sends a [cancellation notification](/docs/rap/spec/basic/tool-cancellation) (`POST /cancel_tool_call`) to the tool servers and removes the subscription from tracking. Only the thread that created a subscription can cancel it.

Subscriptions can also end without an explicit cancellation:

- The tool sends an event with `"final": true`, and the runtime drops the subscription from tracking.
- The subscribing thread closes. The runtime sends a [thread closure notification](/docs/rap/spec/basic/thread-closure) (`POST /close_thread`) to all tool servers, which should clean up any subscriptions and other resources scoped to that thread.

Tools may additionally expose their own domain-specific cancellation operations, but the notification endpoints above are the standard mechanism runtimes rely on.
