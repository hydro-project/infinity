---
sidebar_position: 5
title: OAuth
---

# OAuth

Tools that require user authorization can initiate an OAuth flow by sending an `oauth` message to the callback URL instead of a tool result.

## Request

```http
POST https://agent.example.com/callback
Content-Type: application/json

{
  "type": "oauth",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "call_id": null,
  "auth_url": "https://github.com/login/oauth/authorize?client_id=abc&redirect_uri=..."
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | string | yes | Must be `"oauth"` |
| `group_id` | string | yes | Thread ID from the original invocation |
| `id` | string | yes | Tool call ID from the original invocation |
| `call_id` | string \| null | no | Secondary call ID from the original invocation, if provided |
| `auth_url` | string | yes | The full OAuth authorization URL the user should visit |

## Flow

1. The runtime invokes a tool that requires authorization
2. The tool detects that no valid token exists for the user
3. Instead of a `tool_result`, the tool sends an `oauth` message with the authorization URL
4. The runtime surfaces the URL to the user (e.g. via Slack message, CLI prompt)
5. The user visits the URL and completes authorization
6. The OAuth callback hits the tool's redirect endpoint
7. The tool exchanges the authorization code for a token, stores it, and retries the original operation
8. The tool sends the actual `tool_result` to the callback URL

The runtime treats the `oauth` message as a special tool result that requires user interaction. The original tool call remains pending until the tool sends a real `tool_result` after authorization completes.
