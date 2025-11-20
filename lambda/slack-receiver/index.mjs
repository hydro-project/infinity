import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const sqsClient = new SQSClient({});
const AGENT_INPUT_QUEUE_URL = process.env.AGENT_INPUT_QUEUE_URL;
const SLACK_SIGNING_SECRET = process.env.SLACK_SIGNING_SECRET;

export const handler = async (event) => {
  console.log('Received event:', JSON.stringify(event, null, 2));

  const body = JSON.parse(event.body || '{}');

  // Handle Slack URL verification challenge
  if (body.type === 'url_verification') {
    return {
      statusCode: 200,
      body: JSON.stringify({ challenge: body.challenge }),
    };
  }

  // Handle Slack event
  if (body.type === 'event_callback') {
    const slackEvent = body.event;

    // Ignore bot messages and message changes
    if (slackEvent.bot_id || slackEvent.subtype) {
      return { statusCode: 200, body: JSON.stringify({ ok: true }) };
    }

    // Handle app_mention or message events
    if (slackEvent.type === 'app_mention' || slackEvent.type === 'message') {
      const text = slackEvent.text;
      const threadTs = slackEvent.thread_ts || slackEvent.ts;
      const channel = slackEvent.channel;

      // Use thread_ts as the message group ID for conversation continuity
      const messageGroupId = `slack-${channel}-${threadTs}`;

      // Create the message for the agent
      const agentMessage = {
        content: {
          type: 'text',
          text: text,
        },
        group_id: group_id,
        metadata: {
          channel: channel,
          thread_ts: threadTs,
          user: slackEvent.user,
          ts: slackEvent.ts,
        },
      };

      // Send to agent input queue with conversation group ID as message attribute
      const command = new SendMessageCommand({
        QueueUrl: AGENT_INPUT_QUEUE_URL,
        MessageBody: JSON.stringify(agentMessage),
      });

      try {
        await sqsClient.send(command);
        console.log('Message sent to agent input queue:', messageGroupId);
      } catch (error) {
        console.error('Error sending to SQS:', error);
        return {
          statusCode: 500,
          body: JSON.stringify({ error: 'Failed to queue message' }),
        };
      }
    }
  }

  return {
    statusCode: 200,
    body: JSON.stringify({ ok: true }),
  };
};
