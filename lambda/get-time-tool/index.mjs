import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const sqsClient = new SQSClient({});

export const handler = async (event) => {
  console.log('Received event:', JSON.stringify(event, null, 2));

  for (const record of event.Records) {
    try {
      const request = JSON.parse(record.body);
      const { arguments: args, id, call_id, input_queue_url, group_id, metadata } = request;

      console.log('Processing get_time request:', { args, id, call_id });

      // Get the current time
      const now = new Date();
      const timeString = now.toISOString();
      const localTime = now.toLocaleString('en-US', { 
        timeZone: args.timezone || 'UTC',
        dateStyle: 'full',
        timeStyle: 'long'
      });

      const resultText = args.timezone 
        ? `Current time in ${args.timezone}: ${localTime}`
        : `Current UTC time: ${timeString}`;

      // Create tool result message in the same format as sleep tool
      const toolResultMessage = {
        content: {
          type: "toolresult",
          id: id,
          call_id: call_id,
          content: [
            {
              type: "text",
              text: resultText,
            }
          ]
        },
        group_id: group_id,
      };

      // Send result back to the agent input queue
      const command = new SendMessageCommand({
        QueueUrl: input_queue_url,
        MessageBody: JSON.stringify(toolResultMessage),
      });

      await sqsClient.send(command);
      console.log('Successfully sent tool result back to input queue');

    } catch (error) {
      console.error('Error processing message:', error);
      throw error; // Let SQS handle retry
    }
  }

  return {
    statusCode: 200,
    body: JSON.stringify({ ok: true }),
  };
};
