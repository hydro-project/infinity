---
sidebar_position: 2
---

# RAP Servers

A RAP server is an independent HTTP service that offers a set of tools. It receives invocations from the agent runtime, does its work, and POSTs the result back to a callback URL. Just like MCP, these tools are abstracted away from agent runtime specifics such as the model or durable store, so a single RAP server can be used with any agent runtime that supports the RAP protocol.

Unlike MCP servers, which spin up a separate processes or connections per-client, a single RAP server can concurrently process requests from *several* clients. This multienant approach makes it easy to scale RAP servers without running into operating system restrictions or fault-tolerance issues. RAP tools are standalone services with their own lifecycle, scaling, and failure characteristics.

## Invocation

All tool calls are issues via HTTP requests to the RAP server. The agent runtime POSTs a JSON payload to the tool's HTTP endpoint:

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

The tool must return HTTP 200 immediately, then process the request asynchronously. This allows the agent runtime to shut down while a tool call is processing because there are no long-lived connections.

## Returning results

When the RAP tool call is complete, it POSTs the tool call result to the `callback_url` provided in the request:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "Subscribed to pull_request events on acme/api. Subscription ID: sub_def456"
}
```

This API request wakes up the agent runtime, which will load the session state from durable storage, matches the result to the pending tool call via the `id` field, and continues the conversation. Again, because this is a separate HTTP request from the tool invocation, the agent runtime does not need to maintain any long-lived TCP connections and is free to shut down after processing the tool call result.

## Subscription tools

Some tools register an ongoing subscription instead of returning a single result. A GitHub webhook listener, a stock price monitor, a Slack channel watcher: these tools deliver events over time, each one waking the agent.

Whenever a matching event occurs, the RAP server POSTs a `subscription_event` to the callback URL associated with the subscription:

```json
{
  "type": "subscription_event",
  "group_id": "thread_xyz",
  "tool_call_id": "call_abc123",
  "text": "{\"event_type\": \"pull_request\", \"action\": \"opened\", \"number\": 42, \"title\": \"Fix auth bug\"}"
}
```

Note that `subscription_event` uses `tool_call_id` (referencing the original subscription call), not `id` like a normal `tool_result`. This tells the runtime the message is a new event from an ongoing subscription, not the final result of a one-off call.

The runtime presents each event to the LLM using _synthetic tool calls_, which enable subscription support for existing LLMs without additional training. See [Subscription Events](/docs/rap/about/subscription-events) for more detail, including synthetic tool calls, threaded processing, and cancellation.

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
