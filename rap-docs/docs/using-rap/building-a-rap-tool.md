---
sidebar_position: 2
title: Building a RAP Tool
---

# Building a RAP Tool

A RAP tool is any HTTP service that accepts a tool invocation, acknowledges immediately, and POSTs the result to the RAP receiver when done. You can build one in any language on any platform.

## The contract

Your tool receives a POST request:

```json
{
  "operation": "my_tool_name",
  "arguments": { "query": "AAPL" },
  "id": "call_abc123",
  "call_id": null,
  "rap_receiver_url": "https://rap-receiver.lambda-url.us-east-1.on.aws/",
  "group_id": "thread_xyz",
  "user_id": "user_42"
}
```

Your tool must:

1. Return HTTP 200 immediately (before doing any real work)
2. Process the request asynchronously
3. POST the result to `rap_receiver_url` when done

The result payload:

```json
{
  "type": "tool_result",
  "group_id": "thread_xyz",
  "id": "call_abc123",
  "text": "Current price of AAPL: $187.42"
}
```

That's it. The RAP receiver handles the rest — enqueuing the result, waking the agent, matching it to the pending tool call.

## Example: Lambda with response streaming

The reference implementation uses Lambda response streaming to acknowledge immediately:

```javascript
import { sendToolResult } from 'rap-js';

export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
  // Acknowledge immediately — the agent runtime doesn't block on this
  responseStream.write('OK');
  responseStream.end();

  // Now do the actual work
  const { arguments: args, id, call_id, rap_receiver_url, group_id } = 
    typeof event.body === 'string' ? JSON.parse(event.body) : event.body;

  const result = await doExpensiveWork(args);

  // Send result back via RAP
  await sendToolResult(rap_receiver_url, group_id, id, call_id, result);
});
```

The `rap-js` helper handles SigV4 signing for the RAP receiver's Lambda Function URL.

## Building a subscription tool

Subscription tools register an ongoing listener instead of returning a single result. The pattern:

1. On invocation, store the subscription in your database — include `rap_receiver_url`, `group_id`, and `id` (the tool call ID)
2. Return a confirmation as a normal tool result
3. When a matching event occurs, send a `subscription_event` to the stored `rap_receiver_url`

```javascript
import { sendToolResult, sendSubscriptionEvent } from 'rap-js';

// Called when the tool is invoked by the agent
async function handleSubscribe(args, id, callId, rapReceiverUrl, groupId) {
  await db.put({
    subscriptionId: id,
    filter: args.filter,
    rapReceiverUrl,
    groupId,
    toolCallId: id,
  });

  await sendToolResult(rapReceiverUrl, groupId, id, callId,
    `Subscribed with filter: ${args.filter}. Subscription ID: ${id}`
  );
}

// Called by your webhook handler when an event matches
async function handleEvent(subscription, eventData) {
  await sendSubscriptionEvent(
    subscription.rapReceiverUrl,
    subscription.groupId,
    subscription.toolCallId,
    JSON.stringify(eventData)
  );
}
```

The agent runtime automatically spawns a temporary child thread for each subscription event. The child processes the event and reports back to the parent. The subscription remains active.

## CDK integration

To wire your tool into an Infinity Agent, use `LambdaTool`:

```typescript
import { LambdaTool } from './infinity-agents/tools';

const myToolFunction = new lambda.Function(this, 'MyTool', {
  runtime: lambda.Runtime.NODEJS_24_X,
  handler: 'index.handler',
  code: lambda.Code.fromAsset(path.join(__dirname, 'my-tool')),
  timeout: cdk.Duration.seconds(30),
});

new LambdaTool(agent, 'MyTool', {
  name: 'my_tool',
  description: 'Does something useful',
  parameters: {
    type: 'object',
    properties: {
      query: { type: 'string', description: 'The query' },
    },
    required: ['query'],
  },
  handler: myToolFunction,
});
```

The framework creates a Function URL with IAM auth, grants the runtime permission to invoke it, and grants the tool permission to POST to the RAP receiver.
