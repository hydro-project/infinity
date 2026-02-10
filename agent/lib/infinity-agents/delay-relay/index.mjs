import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const sqsClient = new SQSClient({});
const INPUT_QUEUE_URL = process.env.INPUT_QUEUE_URL;

/**
 * Delay relay Lambda: receives messages from a standard SQS queue
 * (which supports per-message DelaySeconds) and forwards them to
 * the FIFO input queue with the correct MessageGroupId and dedup ID.
 */
export const handler = async (event) => {
  for (const record of event.Records) {
    const body = JSON.parse(record.body);
    const { message, group_id, dedup_id } = body;

    await sqsClient.send(new SendMessageCommand({
      QueueUrl: INPUT_QUEUE_URL,
      MessageBody: message,
      MessageGroupId: group_id,
      MessageDeduplicationId: `${dedup_id}-${Date.now()}`,
    }));

    console.log(`Relayed delayed message to FIFO queue (group: ${group_id})`);
  }
};
