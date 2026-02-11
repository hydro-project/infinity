---
sidebar_position: 3
title: Tool Result
---

# Tool Result

When a tool finishes processing, it POSTs the result to the `callback_url` from the original invocation.

## Request

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

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | string | yes | Must be `"tool_result"` |
| `group_id` | string | yes | Thread ID from the original invocation |
| `id` | string | yes | Tool call ID from the original invocation |
| `call_id` | string \| null | no | Secondary call ID from the original invocation, if provided |
| `text` | string | yes | Result content — plain text or JSON-encoded structured data |

## Response

The callback endpoint should return HTTP 200 on success. The tool does not need to interpret the response body.

## Error handling

There is no separate error message type. If the tool encounters an error, it sends a normal `tool_result` with the error description as the `text` field:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "Error: API rate limit exceeded. Retry after 60 seconds."
}
```

The LLM receives this as the tool's response and can decide how to handle it — retry, inform the user, or try a different approach.
