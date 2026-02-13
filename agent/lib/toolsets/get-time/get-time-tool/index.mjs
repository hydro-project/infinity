import { sendToolResult } from 'rap-js';

const TOOLSET_MANIFEST = {
  name: 'get-time',
  description: 'Utility tools for getting the current time',
  endpoint: process.env.FUNCTION_URL || '',
  tools: [
    {
      name: 'get_time',
      description: 'Get the current time in a specified timezone or UTC.',
      inputSchema: {
        type: 'object',
        properties: {
          timezone: {
            type: 'string',
            description: "IANA timezone name (e.g., 'America/New_York', 'Europe/London'). Defaults to UTC if not specified.",
          },
        },
        required: [],
      },
    },
  ],
};

/**
 * Get-time tool Lambda handler.
 * Serves /.well-known/rap-toolset for discovery, and handles tool invocations via streaming.
 */
export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
  // Handle .well-known/rap-toolset discovery
  if (event.requestContext?.http?.method === 'GET' && event.rawPath?.includes('.well-known/rap-toolset')) {
    const manifest = { ...TOOLSET_MANIFEST };
    // Resolve endpoint from the Function URL at runtime
    if (!manifest.endpoint) {
      const host = event.requestContext?.domainName || '';
      manifest.endpoint = `https://${host}`;
    }
    responseStream.write(JSON.stringify(manifest));
    responseStream.end();
    return;
  }

  // Tool invocation — immediately signal OK
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
