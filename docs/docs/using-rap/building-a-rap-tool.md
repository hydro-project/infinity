---
sidebar_position: 2
title: Building a RAP Tool
---

# Building a RAP Tool

A RAP tool is any HTTP service that accepts an invocation, acknowledges immediately, and POSTs the result to a callback URL when done. You can build one in any language on any platform.

## The contract

Your tool receives a POST with a JSON body containing `operation`, `arguments`, `id`, `callback_url`, and `group_id`. See [The Tool Role](/docs/about/tool-servers) for the full schema.

Your tool must:

1. Return HTTP 200 immediately (before doing any real work)
2. Process the request asynchronously
3. POST the result to `callback_url` when done

That's it. The runtime handles the rest — matching the result to the pending tool call, waking the agent, continuing the conversation.

## Example: Lambda with response streaming

The reference implementation uses Lambda response streaming to acknowledge before the handler finishes:

```javascript
import { sendToolResult } from 'rap-js';

export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
  // Acknowledge immediately
  responseStream.write('OK');
  responseStream.end();

  // Parse the invocation
  const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
  const { arguments: args, id, call_id, callback_url, group_id } = body;

  // Do the actual work
  const result = await doExpensiveWork(args);

  // Send result back
  await sendToolResult(callback_url, group_id, id, call_id, result);
});
```

The `rap-js` helper handles SigV4 signing for Lambda Function URLs. If your callback endpoint uses a different auth mechanism, you can POST directly.

## Building a subscription tool

Subscription tools register an ongoing listener instead of returning a single result:

1. On invocation, store the subscription in your database — include `callback_url`, `group_id`, and `id`
2. Return a confirmation as a normal tool result
3. When a matching event occurs, send a `subscription_event` to the stored `callback_url`

```javascript
import { sendToolResult, sendSubscriptionEvent } from 'rap-js';

// Invoked by the agent runtime
async function handleSubscribe(args, id, callId, callbackUrl, groupId) {
  await db.put({
    subscriptionId: id,
    filter: args.filter,
    callbackUrl,
    groupId,
    toolCallId: id,
  });

  await sendToolResult(callbackUrl, groupId, id, callId,
    `Subscribed with filter: ${args.filter}. Subscription ID: ${id}`
  );
}

// Invoked by your webhook handler when an event matches
async function handleEvent(subscription, eventData) {
  await sendSubscriptionEvent(
    subscription.callbackUrl,
    subscription.groupId,
    subscription.toolCallId,
    JSON.stringify(eventData)
  );
}
```

The runtime automatically spawns a child thread for each subscription event. The child processes the event and can report back to the parent. The subscription remains active.

## CDK integration

If you're using the Infinity Runtime, wire your tool in with `LambdaTool`:

```typescript
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

The framework creates a Function URL with IAM auth, grants the runtime permission to invoke it, and grants the tool permission to POST results back.
