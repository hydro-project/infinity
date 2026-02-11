---
sidebar_position: 5
title: Subscriptions
---

# Subscriptions

Subscriptions allow tools to send multiple results over time for a single tool call. This is how agents react to external events — GitHub webhooks, price changes, monitoring alerts — without polling.

## Creating a subscription

A subscription tool receives a normal tool invocation. Instead of doing work and returning, it:

1. Records the subscription in its own storage, saving `rap_receiver_url`, `group_id`, and `id` (the tool call ID)
2. Returns a confirmation as a normal `tool_result`

The agent sees the confirmation and knows the subscription is active. It can then hibernate with `sleep_until_event_or_input`.

## Sending subscription events

When a matching event occurs, the tool (or its webhook handler) POSTs to the stored RAP receiver URL:

```json
{
  "type": "subscription_event",
  "group_id": "thread_xyz",
  "tool_call_id": "call_abc123",
  "text": "{\"event_type\": \"pull_request\", \"action\": \"opened\", \"number\": 42}"
}
```

| Field | Type | Description |
|---|---|---|
| `type` | string | Must be `"subscription_event"` |
| `group_id` | string | Thread ID of the subscribing conversation |
| `tool_call_id` | string | The original tool call ID that created the subscription |
| `text` | string | Event payload (typically JSON-encoded) |

## Runtime handling

The runtime processes subscription events differently from normal tool results:

1. The event arrives on the input queue with a `synthetic` field containing the `tool_call_id`
2. The runtime looks up the original tool call in conversation history
3. It spawns a new child thread (new `group_id`) to process the event
4. The child thread is seeded with four messages:
   - A synthetic tool call echoing the original subscription call (with `kind: "interrupt"` annotation)
   - The event content as a tool result for that synthetic call
   - A `spawn_thread` call instructing the child to process the event
   - The spawn result confirming the child is active
5. The child processes the event independently and can `close_thread` with a report when done
6. The parent thread's subscription remains active — future events spawn new child threads

This design means the parent thread is never blocked by event processing. Multiple events can be processed concurrently in separate child threads.

## Cancellation

To cancel a subscription, the agent calls a cancellation tool (e.g. `cancel_finance_subscription`) with the subscription ID. The tool deletes the subscription from its storage. No more events will be sent.

Subscriptions should be cancelled before closing a thread. The runtime does not automatically cancel subscriptions when a thread closes.

## Synthetic message format

The `synthetic` field on the input queue message can be either:

**A plain string** (backward compatibility) — treated as a subscription event with the string as the `tool_call_id`:
```json
{ "synthetic": "call_abc123" }
```

**A tagged object** — explicitly typed:
```json
{ "synthetic": { "type": "subscription_event", "tool_call_id": "call_abc123" } }
```

Thread reports use the same mechanism:
```json
{ "synthetic": { "type": "thread_report", "tool_call_id": "spawn_call_xyz" } }
```
