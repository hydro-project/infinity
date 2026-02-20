import { sendToolResult } from '../../../infinity-agents/rap-js/index.mjs';
import { buildManifest, processGetTime } from './tool.mjs';

/**
 * Get-time tool Lambda handler.
 * Serves /.well-known/rap-toolset for discovery, and handles tool invocations via streaming.
 */
export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
  // Discovery
  if (event.requestContext?.http?.method === 'GET' && event.rawPath?.includes('.well-known/rap-toolset')) {
    const endpoint = process.env.FUNCTION_URL || `https://${event.requestContext?.domainName || ''}`;
    responseStream.write(JSON.stringify(buildManifest(endpoint)));
    responseStream.end();
    return;
  }

  // Acknowledge immediately
  responseStream.write('OK');
  responseStream.end();

  try {
    const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
    const { arguments: args, id, call_id, callback_url, group_id } = body;
    console.log('Processing get_time request:', { args, id, call_id });

    const resultText = processGetTime(args);
    await sendToolResult(callback_url, group_id, id, call_id, resultText);
    console.log('Successfully sent tool result via RAP');
  } catch (error) {
    console.error('Error processing request:', error);
  }
});
