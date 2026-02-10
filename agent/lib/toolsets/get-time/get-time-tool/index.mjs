import { sendToolResult } from 'rap-js';

export const handler = async (event) => {
  console.log('Received event:', JSON.stringify(event, null, 2));

  for (const record of event.Records) {
    try {
      const request = JSON.parse(record.body);
      const { arguments: args, id, call_id, rap_receiver_url, group_id } = request;

      console.log('Processing get_time request:', { args, id, call_id });

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

      await sendToolResult(rap_receiver_url, group_id, id, call_id, resultText);
      console.log('Successfully sent tool result via RAP');

    } catch (error) {
      console.error('Error processing message:', error);
      throw error;
    }
  }

  return { statusCode: 200, body: JSON.stringify({ ok: true }) };
};
