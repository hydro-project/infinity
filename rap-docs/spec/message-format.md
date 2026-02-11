---
sidebar_position: 2
title: Message Format
---

# Message Format

All messages on the input queue use a common envelope:

```json
{
  "content": { ... },
  "group_id": "thread_xyz",
  "metadata": { "user_id": "user_42", "channel": "C0123" },
  "synthetic": null
}
```

| Field | Type | Description |
|---|---|---|
| `content` | object | The message payload — either a user message or tool result |
| `group_id` | string | Thread ID. Used as the FIFO message group ID for ordering. |
| `metadata` | object \| null | Arbitrary metadata (user ID, channel info, etc.). Stored with the root thread. |
| `synthetic` | string \| object \| null | Present on subscription events and thread reports. See [Subscriptions](/spec/subscriptions). |

## Content types

**User text message:**
```json
{
  "content": {
    "type": "text",
    "text": "What's the status of PR #42?"
  }
}
```

**Tool result:**
```json
{
  "content": {
    "type": "toolresult",
    "id": "call_abc123",
    "call_id": null,
    "content": [{ "type": "text", "text": "PR #42 is approved and ready to merge." }]
  }
}
```

**OAuth required:**
```json
{
  "content": {
    "type": "oauth_required",
    "id": "call_abc123",
    "call_id": null,
    "auth_url": "https://github.com/login/oauth/authorize?..."
  }
}
```

## Queue semantics

The input queue is FIFO with message group IDs. Each `group_id` (thread) is an independent message group, so:

- Messages within a thread are processed in order
- Different threads process concurrently
- Deduplication IDs prevent duplicate processing (format: `{tool_call_id}-{timestamp}`)

Visibility timeout should be set high enough for the runtime to complete a full LLM completion cycle (15 minutes in the reference implementation).
