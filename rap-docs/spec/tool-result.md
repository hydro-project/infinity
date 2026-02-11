---
sidebar_position: 4
title: Tool Result
---

# Tool Result

When a tool finishes processing, it POSTs the result to the RAP receiver URL provided in the original invocation.

## Request format

```http
POST https://rap-receiver.lambda-url.us-east-1.on.aws/
Content-Type: application/json
Authorization: <SigV4 signature>

{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "call_id": null,
  "text": "Deployment completed successfully. Instance i-0abc123 is running."
}
```

| Field | Type | Description |
|---|---|---|
| `type` | string | Must be `"tool_result"` |
| `group_id` | string | Thread ID from the original invocation |
| `id` | string | Tool call ID from the original invocation |
| `call_id` | string \| null | Optional secondary call ID from the original invocation |
| `text` | string | The result content. Can be plain text or JSON-encoded structured data. |

## RAP receiver processing

The RAP receiver transforms the payload into the internal message format and enqueues it on the input FIFO queue:

```json
{
  "content": {
    "type": "toolresult",
    "id": "call_abc123",
    "content": [{ "type": "text", "text": "Deployment completed successfully..." }]
  },
  "group_id": "thread_xyz"
}
```

The message is enqueued with:
- `MessageGroupId`: the `group_id` (ensures ordering within the thread)
- `MessageDeduplicationId`: `{id}-{timestamp}` (prevents duplicate processing)

## Runtime processing

When the runtime receives a tool result, it appends it to conversation history as a user message containing a `ToolResult`. The LLM sees this as the response to its earlier tool call and continues the conversation.

If the agent was interrupted (a user message arrived while waiting for this result), the tool result is still appended normally. The LLM sees both the interruption and the eventual result in its context.

## Error handling

If the tool encounters an error, it should still send a tool result with the error message:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "Error: API rate limit exceeded. Retry after 60 seconds."
}
```

The LLM can then decide how to handle the error — retry, inform the user, or try a different approach.
