---
sidebar_position: 2
title: Tool Servers
---

# Tool Servers

In RAP, a tool is an independent HTTP service. It receives invocations from the agent runtime, does its work, and POSTs the result back to a callback URL. Tools don't know or care about the agent's LLM, conversation history, or internal state.

This is fundamentally different from MCP, where tools are child processes managed by the runtime. RAP tools are standalone services with their own lifecycle, scaling, and failure characteristics.

## Invocation

The runtime POSTs a JSON payload to the tool's HTTP endpoint:

```json
{
  "operation": "subscribe_github_events",
  "arguments": { "owner": "acme", "repo": "api", "event_type": "pull_request" },
  "id": "call_abc123",
  "call_id": null,
  "callback_url": "https://agent.example.com/callback",
  "group_id": "thread_xyz",
  "user_id": "user_42"
}
```

The tool must return HTTP 200 immediately, then process the request asynchronously. The `callback_url` is where the tool sends results when it's done. The `group_id` identifies the conversation thread so the result is routed correctly.

## Returning results

When the tool finishes, it POSTs to the `callback_url`:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "Subscribed to pull_request events on acme/api. Subscription ID: sub_def456"
}
```

The runtime wakes up, matches the result to the pending tool call via the `id` field, and continues the conversation.

## Subscription tools

Some tools register an ongoing subscription instead of returning a single result. See [Subscription Events](/docs/about/subscription-events) for how this works at the protocol level, including synthetic tool calls and threaded event processing.

## OAuth

Tools that need user authorization can trigger an OAuth flow by sending an `oauth` message instead of a tool result:

```json
{
  "type": "oauth",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "auth_url": "https://github.com/login/oauth/authorize?client_id=..."
}
```

The runtime surfaces this URL to the user. After authorization completes, the tool receives the callback, exchanges the code for a token, and retries the original operation — sending the actual result back through the normal flow.
