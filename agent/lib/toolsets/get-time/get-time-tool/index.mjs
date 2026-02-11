import { sendToolResult } from 'rap-js';

/**
 * Get-time tool Lambda handler.
 * Invoked via Function URL with response streaming.
 * Immediately streams back OK, then processes the request and sends results via RAP.
 */
export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
  // Immediately signal OK to the invoker so the leader doesn't block
  responseStream.write('OK');
  responseStream.end();

  try {
    const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
    const { arguments: args, id, call_id, rap_receiver_url, group_id } = body;

    console.log('Processing get_time request:', { args, id, call_id });

    const now = new Date();
    const timeString = now.toISOString();
    const localTime = now.toLocaleString('en-US', {
      timeZone: args.timezone || 'UTC',
      dateStyle: 'full',
      timeStyle: 'long',
    });

    const resultText = args.timezone
      ? `Current time in ${args.timezone}: ${localTime}`
      : `Current UTC time: ${timeString}`;

    await sendToolResult(rap_receiver_url, group_id, id, call_id, resultText);
    console.log('Successfully sent tool result via RAP');
  } catch (error) {
    console.error('Error processing request:', error);
  }
});
