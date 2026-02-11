---
sidebar_position: 4
title: Subscription Events
---

# Subscription Events

Subscription events allow tools to send multiple results over time for a single tool call. This is how agents react to external events — webhooks, price changes, monitoring alerts — without polling.

## Lifecycle

1. The runtime invokes a subscription tool with a normal [tool invocation](/spec/tool-invocation)
2. The tool stores the `callback_url`, `group_id`, and `id` from the invocation
3. The tool returns a confirmation as a normal [`tool_result`](/spec/tool-result)
4. When a matching event occurs (now or in the future), the tool POSTs a `subscription_event` to the stored `callback_url`
5. The runtime processes the event (typically by injecting a [synthetic tool call](/docs/about/subscription-events#synthetic-tool-calls))
6. Steps 4–5 repeat for each matching event until the subscription is cancelled

## Notification Request

```http
POST https://agent.example.com/callback
Content-Type: application/json

{
  "type": "subscription_event",
  "group_id": "thread_xyz",
  "tool_call_id": "call_abc123",
  "text": "{\"event_type\": \"pull_request\", \"action\": \"opened\", \"number\": 42}"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | string | yes | Must be `"subscription_event"` |
| `group_id` | string | yes | Thread ID of the subscribing conversation |
| `tool_call_id` | string | yes | The tool call ID from the original invocation that created the subscription |
| `text` | string | yes | Event payload, typically JSON-encoded |

Note that `subscription_event` uses `tool_call_id` (referencing the original subscription call), while `tool_result` uses `id`. This distinction tells the runtime that the message is a new event from an ongoing subscription, not the final result of a one-off call.

## Response
The callback endpoint should return HTTP 200 on success. The tool does not need to interpret the response body.

## Cancellation

Cancellation is tool-specific. The tool should expose a separate cancellation operation (e.g. `cancel_subscription`) that accepts the subscription ID and removes it from storage. Once cancelled, no more events are sent.

The runtime does not automatically cancel subscriptions when a thread closes. Agents should cancel subscriptions explicitly before shutting down.
