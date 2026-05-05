---
sidebar_position: 2
---

# Request Transport

RAP uses HTTP as its transport layer. All messages are JSON-encoded and delivered via HTTP POST requests. The protocol defines two directions of communication, each using a distinct HTTP endpoint.

## Message Encoding

All RAP messages MUST be encoded as JSON objects with `Content-Type: application/json`. Messages MUST be UTF-8 encoded.

## Runtime → Tool

The runtime invokes a tool by sending an HTTP POST request to the tool's registered endpoint URL. This is a [Tool Invocation](/docs/rap/spec/basic/tool-invocation).

```
POST https://tool.example.com/invoke
Content-Type: application/json
```

The tool MUST respond with HTTP 200 to acknowledge receipt. The response body is not read by the runtime — it exists only to confirm delivery. The tool then processes the request asynchronously.

### Delivery Requirements

The runtime MUST send invocations as HTTP POST requests and MUST include `Content-Type: application/json` in the request headers. If the tool endpoint is unreachable or returns an HTTP 5xx error, the runtime SHOULD implement retry logic with exponential backoff for transient failures. The runtime MUST NOT retry on HTTP 4xx responses, as these indicate client-side errors (malformed payload, unknown operation, authentication failure) that will not succeed on retry.

### Tool Acknowledgement

The tool MUST return HTTP 200 immediately upon receiving a valid invocation. The tool MUST NOT block the HTTP response while processing the invocation — the acknowledgement confirms only that the message was received and will be processed asynchronously. If the `toolset_version` in the invocation is stale, the tool SHOULD return HTTP 409 Conflict instead. For all other error conditions, the tool SHOULD still acknowledge with HTTP 200 and deliver the error asynchronously via a [tool result](/docs/rap/spec/basic/tool-result).

### Thread Closure Notification

In addition to tool invocations, runtimes send a best-effort [thread closure](/docs/rap/spec/basic/thread-closure) notification when a conversation thread is closed:

```
POST https://tool.example.com/close_thread
Content-Type: application/json
```

This is a fire-and-forget lifecycle signal — the runtime MUST NOT retry on failure, and the tool server MUST always respond with HTTP 200 regardless of whether it acted on the notification. Tool servers MAY use this to clean up thread-specific resources. See [Thread Closure](/docs/rap/spec/basic/thread-closure) for the full specification.

## Tool → Runtime

Tools send messages to the runtime by POSTing to the `callback_url` provided in the original invocation. Three message types are supported:

| Message Type | Description |
|---|---|
| [`tool_result`](/docs/rap/spec/basic/tool-result) | The result of a completed tool operation |
| [`subscription_event`](/docs/rap/spec/server/subscription-events) | An event from an active subscription |
| [`oauth`](/docs/rap/spec/server/oauth) | An authorization request requiring user interaction |

```
POST https://agent.example.com/callback
Content-Type: application/json
```

### Delivery Requirements

Tools MUST send callback messages as HTTP POST requests and MUST include `Content-Type: application/json` in the request headers. Every callback message MUST include the `group_id` from the original invocation so the runtime can route it to the correct conversation thread. The callback endpoint SHOULD return HTTP 200 on successful receipt.

Because callback delivery may fail due to transient network issues or runtime unavailability, tools SHOULD implement retry logic with exponential backoff for transient failures (HTTP 5xx, connection timeouts). Tools SHOULD NOT retry on HTTP 4xx responses, as these indicate the callback was rejected for a reason that will not change on retry (e.g., unknown `group_id`, expired callback URL).

### Callback URL

The `callback_url` is an opaque string provided by the runtime in each tool invocation. Tools MUST treat it as an opaque endpoint and MUST NOT make assumptions about its structure, lifetime, or hosting infrastructure.

The callback URL MAY be scoped to a single tool call, a conversation thread, or an entire runtime instance — the scoping strategy is an implementation decision. The URL MAY also expire after a configurable period, which is particularly relevant for subscription tools that need to deliver events over extended timeframes. Runtimes SHOULD document their callback URL lifetime and scoping behavior so that tool authors can design accordingly. Tools that need to send messages after the initial result (e.g., [subscription events](/docs/rap/spec/server/subscription-events)) MUST store the callback URL durably.

## Authentication

The authentication mechanism between runtime and tool is implementation-specific. The protocol does not mandate any particular authentication scheme.

Implementations SHOULD use established authentication mechanisms. Common approaches include:

- **AWS SigV4**: Request signing for Lambda Function URLs and API Gateway endpoints
- **Bearer tokens**: OAuth 2.0 access tokens or API keys in the `Authorization` header
- **Mutual TLS**: Client certificate authentication for high-security environments
- **No authentication**: Acceptable for internal services within a trusted network boundary

Callback URLs SHOULD be authenticated to prevent unauthorized message injection. Implementations MAY embed authentication tokens in the callback URL itself (e.g., as query parameters or path segments) or require tools to present credentials when POSTing to the callback.

## Security Considerations

Runtimes MUST validate all incoming callback messages against the expected schema and SHOULD verify that callback messages originate from a tool that was actually invoked — for example, by checking that the `group_id` and `id` correspond to a pending tool call. Tools MUST validate invocation payloads before processing to guard against injection attacks or malformed input.

All communication SHOULD use HTTPS in production environments. Implementations SHOULD protect against replay attacks on callback URLs, particularly for subscription tools where the callback URL persists over extended periods. Strategies for replay protection include embedding single-use tokens in callback URLs, requiring timestamps in callback payloads, or maintaining a log of processed message IDs.
