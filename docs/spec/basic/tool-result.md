---
sidebar_position: 4
title: Tool Result
---

# Tool Result

A tool result is a message sent from a tool to the runtime, delivering the outcome of a completed operation. The tool POSTs the result to the `callback_url` provided in the original [tool invocation](/spec/basic/tool-invocation).

## Request

The tool MUST send a tool result as an HTTP POST with `Content-Type: application/json`.

```http
POST https://agent.example.com/callback
Content-Type: application/json

{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "call_id": null,
  "text": "Deployment completed successfully. Instance i-0abc123 is running."
}
```

## Fields

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | `string` | Yes | MUST be `"tool_result"`. |
| `group_id` | `string` | Yes | Conversation thread identifier. MUST match the `group_id` from the original invocation. |
| `id` | `string` | Yes | Tool call identifier. MUST match the `id` from the original invocation. |
| `call_id` | `string \| null` | No | Secondary call identifier. If the original invocation included a `call_id`, it MUST be echoed here. |
| `text` | `string` | Yes | Result content. MAY be plain text or JSON-encoded structured data. |

## Response

The callback endpoint SHOULD return HTTP 200 on successful receipt. The tool does not need to interpret the response body.

```http
HTTP/1.1 200 OK
```

## Result Content

The `text` field carries the tool's output. The protocol does not prescribe a specific format for result content — implementations MAY use plain text for human-readable results, JSON-encoded strings for structured data, or error descriptions for failed operations. The LLM receives the `text` value as the tool's response and reasons about it in the context of the ongoing conversation.

### Structured Results

When returning structured data, tools SHOULD JSON-encode the data and place it in the `text` field:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "{\"instances\": [{\"id\": \"i-0abc123\", \"state\": \"running\"}], \"count\": 1}"
}
```

Runtimes MAY parse the `text` field as JSON if the tool's schema indicates structured output, but MUST be prepared to handle plain text.

## Error Handling

There is no separate error message type. If the tool encounters an error during processing, it MUST send a normal `tool_result` with the error description in the `text` field:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "Error: API rate limit exceeded. Retry after 60 seconds."
}
```

The LLM receives this as the tool's response and can decide how to handle it — retry, inform the user, or try a different approach.

Tools MUST NOT silently drop errors. Every invocation MUST eventually produce either a `tool_result` or an [`oauth`](/spec/server/oauth) message.

### Error Conventions

While the protocol does not mandate error formatting, tools SHOULD prefix error messages with `"Error: "` to help the LLM distinguish errors from successful results. Error messages SHOULD include actionable information — such as retry timing, missing permissions, or alternative approaches — so the LLM can attempt recovery. Tools SHOULD avoid exposing internal implementation details or stack traces in error messages, as these provide no value to the LLM and may leak sensitive information.

## Routing

The runtime routes incoming tool results using the `group_id` and `id` fields:

1. The `group_id` identifies the conversation thread
2. The `id` matches the result to the pending tool call within that thread

If the runtime receives a tool result with an unknown `group_id` or `id`, it SHOULD log the event and discard the message. It MUST NOT inject unmatched results into any conversation.

## Security Considerations

Runtimes MUST validate that the `group_id` and `id` in an incoming tool result correspond to an actual pending tool call. This prevents unauthorized parties from injecting fabricated results into conversations. Runtimes SHOULD also validate the `text` content before passing it to the LLM — while the LLM is generally resilient to unexpected input, sanitization reduces the risk of prompt injection through tool results.

Tools MUST NOT include sensitive data (credentials, tokens, internal identifiers) in results unless the operation explicitly requires it. Runtimes SHOULD implement idempotent result processing to handle duplicate deliveries gracefully, since network retries may cause the same result to arrive more than once.
