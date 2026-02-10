import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const sqsClient = new SQSClient({});
const INPUT_QUEUE_URL = process.env.INPUT_QUEUE_URL;

/**
 * RAP (Reactive Agent Protocol) HTTP receiver.
 *
 * Accepts tool call results via HTTP POST and forwards them to the
 * agent's input FIFO queue. This decouples tool implementors from
 * needing direct SQS access — they just POST JSON to a URL.
 *
 * Expected JSON body:
 * {
 *   content: { type: "toolresult"|"oauth_required", id, call_id?, content?, auth_url? },
 *   group_id: string,
 *   synthetic?: string | { type, tool_call_id }
 * }
 */
export const handler = async (event) => {
  // This Lambda is invoked via Function URL (HTTP)
  try {
    const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;

    if (!body || !body.group_id) {
      return {
        statusCode: 400,
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ error: 'Missing required field: group_id' }),
      };
    }

    // Derive a dedup ID from the content
    const dedupBase = body.content?.id || body.synthetic || 'unknown';
    const dedupId = `${dedupBase}-${Date.now()}`;

    await sqsClient.send(new SendMessageCommand({
      QueueUrl: INPUT_QUEUE_URL,
      MessageBody: JSON.stringify(body),
      MessageGroupId: body.group_id,
      MessageDeduplicationId: dedupId,
    }));

    return {
      statusCode: 200,
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ok: true }),
    };
  } catch (error) {
    console.error('RAP receiver error:', error);
    return {
      statusCode: 500,
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({}),
    };
  }
};
