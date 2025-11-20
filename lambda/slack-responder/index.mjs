import { SQSClient, DeleteMessageCommand } from '@aws-sdk/client-sqs';

const sqsClient = new SQSClient({});
const SLACK_BOT_TOKEN = process.env.SLACK_BOT_TOKEN;

export const handler = async (event) => {
  console.log('Received output event:', JSON.stringify(event, null, 2));

  for (const record of event.Records) {
    try {
      const message = JSON.parse(record.body);
      const { text, metadata } = message;

      if (!metadata || !metadata.channel || !metadata.thread_ts) {
        console.error('Missing metadata in message:', message);
        continue;
      }

      // Post message to Slack thread
      // If thread_ts equals the original message ts, it means the message wasn't in a thread
      // In that case, we create a thread by replying to that message
      const response = await fetch('https://slack.com/api/chat.postMessage', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${SLACK_BOT_TOKEN}`,
        },
        body: JSON.stringify({
          channel: metadata.channel,
          thread_ts: metadata.thread_ts,
          text: text,
        }),
      });

      const result = await response.json();

      if (!result.ok) {
        console.error('Slack API error:', result);
        throw new Error(`Slack API error: ${result.error}`);
      }

      console.log('Successfully posted to Slack:', result.ts);
    } catch (error) {
      console.error('Error processing message:', error);
      // Let SQS handle retry logic
      throw error;
    }
  }

  return {
    statusCode: 200,
    body: JSON.stringify({ ok: true }),
  };
};
