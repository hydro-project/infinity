---
sidebar_position: 3
title: The Tool Role
---

# The Tool Role

In RAP, a tool is an independent service that receives invocations via HTTP and returns results asynchronously. Tools don't know or care about the agent's LLM, conversation history, or internal state. They receive a request, do their work, and POST the result to the RAP receiver when done.

This is the fundamental difference from MCP, where tools are processes managed by the agent runtime. In RAP, tools are standalone services with their own lifecycle, scaling, and failure characteristics.

## Invocation

The agent runtime POSTs a JSON payload to the tool's HTTP endpoint:

```json
{
  "operation": "subscribe_github_events",
  "arguments": { "owner": "acme", "repo": "api", "event_type": "pull_request" },
  "id": "call_abc123",
  "call_id": null,
  "rap_receiver_url": "https://rap-receiver.example.com",
  "group_id": "thread_xyz",
  "user_id": "user_42"
}
```

The tool must acknowledge immediately (HTTP 200) and then process the request asynchronously. In the reference implementation, tools use Lambda response streaming to return `OK` before the handler finishes.

## Returning results

When the tool finishes, it POSTs the result to the `rap_receiver_url` from the original request:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "Subscribed to pull_request events on acme/api. Subscription ID: sub_def456"
}
```

The RAP receiver enqueues this on the agent's input queue, which triggers the runtime to wake up and continue the conversation.

## Subscription tools

Some tools don't return a single result — they register an ongoing subscription. The tool records the subscription in its own storage (e.g. DynamoDB), including the `rap_receiver_url`, `group_id`, and `id` from the original invocation.

When a matching event occurs (a webhook fires, a price threshold is crossed), the tool sends a subscription event:

```json
{
  "type": "subscription_event",
  "group_id": "thread_xyz",
  "tool_call_id": "call_abc123",
  "text": "{\"event_type\": \"pull_request\", \"action\": \"opened\", ...}"
}
```

The agent runtime receives this as a synthetic tool result tied to the original subscription call. It spawns a temporary child thread to process the event, while the parent thread's subscription remains active.

## OAuth

Tools that need user authorization can trigger an OAuth flow. Instead of returning a tool result, the tool sends an OAuth message:

```json
{
  "type": "oauth",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "auth_url": "https://github.com/login/oauth/authorize?client_id=..."
}
```

The agent runtime surfaces this URL to the user. After the user completes authorization, the OAuth callback hits the tool, which exchanges the code for a token and retries the original operation.
