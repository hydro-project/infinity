---
sidebar_position: 3
title: Subscriptions
---

# Subscriptions

Subscriptions are how RAP agents react to the outside world without polling. A tool registers an ongoing subscription — "notify me when a GitHub PR is opened," "alert me when AAPL moves more than $5" — and sends events to the agent over time. Each event wakes the agent, which processes it and goes back to sleep.

## How subscriptions work

A subscription starts as a normal tool call. The agent invokes a tool like `subscribe_github_events`, and the tool does two things:

1. Records the subscription in its own storage, saving the `callback_url`, `group_id`, and `id` from the invocation
2. Returns a confirmation as a normal `tool_result` — "Subscribed to pull_request events. Subscription ID: sub_abc"

The agent sees the confirmation and knows the subscription is active. At this point it can hibernate (e.g. call `sleep_until_event_or_input`) and wait for events to arrive.

When a matching event occurs — a webhook fires, a price threshold is crossed, a new article is published — the tool (or its webhook handler) POSTs a `subscription_event` to the stored `callback_url`. See the [Subscription Events spec](/spec/server/subscription-events) for the exact payload format.

## The problem: presenting events to the LLM

When a subscription event arrives, the runtime needs to show it to the LLM in a way that makes sense in the conversation. The challenge is that the original subscription tool call already has a result (the confirmation message). You can't just append a second result for the same tool call — the LLM would be confused.

RAP solves this with synthetic tool calls.

## Synthetic tool calls

A synthetic tool call is a fabricated assistant message that the runtime injects into conversation history. It looks like the LLM called the subscription tool again, but it was actually created by the runtime to carry the event data.

When a subscription event arrives, the runtime:

1. Looks up the original subscription tool call in history (using the `tool_call_id` from the event)
2. Creates a synthetic assistant message that echoes the original tool call, annotated with `kind: "interrupt"` to signal that this is an event, not a new invocation
3. Appends the event content as the tool result for this synthetic call

The LLM sees what looks like a natural tool call / result pair in its history:

```
[tool_call] subscribe_github_events({ owner: "acme", repo: "api", kind: "interrupt:call_abc123 (subscription remains active)" })
[tool_result] {"event_type": "pull_request", "action": "opened", "number": 42, ...}
```

The `kind` annotation tells the LLM that this is an event from an existing subscription, not a new subscription request. The LLM can then reason about the event and decide what to do.

## Inline vs. threaded processing

There are two strategies for where the synthetic tool call gets injected:

**Inline processing.** The synthetic call is appended directly to the subscribing thread's history. The LLM sees the event in its main conversation and processes it there. This is simpler but means every event accumulates in the same context window — after many events, the conversation history grows large and the LLM loses focus on the original task.

**Threaded processing.** The runtime spawns a new child thread to process each event. The child gets a clean context: it inherits the parent's history up to the spawn point, plus the event data. It processes the event in isolation and can report results back to the parent via `close_thread`. The parent's context stays focused on its original task, and each event gets a fresh, minimal context window to work with.

The Infinity Runtime uses threaded processing by default. This is the recommended approach for production runtimes, since subscriptions can generate many events and you don't want them to block the parent conversation.

## Cancellation

Cancellation is tool-specific — the tool exposes a separate operation (e.g. `cancel_subscription`) that accepts the subscription ID and removes it from storage. Once cancelled, no more events are sent.

The runtime does not automatically cancel subscriptions when a thread closes. Agents should cancel subscriptions explicitly before shutting down. If a subscription isn't cancelled and the subscribing thread is closed, events will still arrive at the callback URL but the runtime may not have a valid thread to process them in.

:::warning

This is an active area of development and subject to change. Future versions of RAP will include a standard protocol for cancelling subscriptions to enable auto-cleanup.

:::
