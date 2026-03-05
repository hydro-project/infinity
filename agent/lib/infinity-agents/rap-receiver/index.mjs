import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const sqsClient = new SQSClient({});
const INPUT_QUEUE_URL = process.env.INPUT_QUEUE_URL;

/**
 * RAP (Reactive Agent Protocol) HTTP receiver.
 *
 * Accepts clean JSON payloads via HTTP POST and transforms them into
 * the legacy InputMessage format before forwarding to the agent's
 * input FIFO queue.
 *
 * Accepted payload types:
 *
 * Tool result:
 *   { type: "tool_result", group_id, id, call_id?, text }
 *
 * OAuth redirect:
 *   { type: "oauth", group_id, id, call_id?, auth_url }
 *
 * Subscription event:
 *   { type: "subscription_event", group_id, tool_call_id, text }
 */
export const handler = async (event) => {
  try {
    const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
    console.log("Received body", body);

    if (!body || !body.type || !body.group_id) {
      return respond(400, { error: 'Missing required fields: type, group_id' });
    }

    let sqsMessage;
    let dedupBase;

    switch (body.type) {
      case 'tool_result':
        if (!body.id && body.id !== '') {
          return respond(400, { error: 'tool_result requires: id, text' });
        }
        sqsMessage = {
          content: {
            type: 'toolresult',
            id: body.id,
            ...(body.call_id && { call_id: body.call_id }),
            content: [{ type: 'text', text: body.text || '' }],
          },
          group_id: body.group_id,
          ...(body.subscription && { subscription: true }),
        };
        dedupBase = body.id;
        break;

      case 'oauth':
        if (!body.id || !body.auth_url) {
          return respond(400, { error: 'oauth requires: id, auth_url' });
        }
        sqsMessage = {
          content: {
            type: 'oauth_required',
            id: body.id,
            call_id: body.call_id || null,
            auth_url: body.auth_url,
          },
          group_id: body.group_id,
        };
        dedupBase = body.id;
        break;

      case 'subscription_event':
        if (!body.tool_call_id) {
          return respond(400, { error: 'subscription_event requires: tool_call_id, text' });
        }
        sqsMessage = {
          content: {
            type: 'toolresult',
            id: '',
            call_id: null,
            content: [{ type: 'text', text: body.text || '' }],
          },
          group_id: body.group_id,
          synthetic: body.associative
            ? { type: "subscription_event", tool_call_id: body.tool_call_id, associative: true }
            : body.tool_call_id,
        };
        dedupBase = body.tool_call_id;
        break;

      default:
        return respond(400, { error: `Unknown type: ${body.type}` });
    }

    await sqsClient.send(new SendMessageCommand({
      QueueUrl: INPUT_QUEUE_URL,
      MessageBody: JSON.stringify(sqsMessage),
      MessageGroupId: body.group_id,
      MessageDeduplicationId: `${dedupBase}-${Date.now()}`,
    }));

    return respond(200, { ok: true });
  } catch (error) {
    console.error('RAP receiver error:', error);
    return respond(500, { error: 'Internal error' });
  }
};

function respond(statusCode, body) {
  return {
    statusCode,
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  };
}
