#!/usr/bin/env node
/**
 * Local standalone RAP tool server for get-time.
 *
 * Usage:
 *   node local.mjs [--port 3001]
 *
 * Serves:
 *   GET  /.well-known/rap-toolset  — toolset discovery
 *   POST /                         — tool invocation
 *
 * Results are POSTed back to the callback_url using plain HTTP (no SigV4).
 */

import { createServer } from 'node:http';

const PORT = parseInt(process.argv.find((_, i, a) => a[i - 1] === '--port') || '3001', 10);

function getManifest(host) {
  return {
    name: 'get-time',
    description: 'Utility tools for getting the current time',
    endpoint: `http://${host}`,
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
}

async function sendResult(callbackUrl, groupId, id, callId, text) {
  const body = JSON.stringify({
    type: 'tool_result',
    group_id: groupId,
    id,
    call_id: callId || null,
    text,
  });
  const resp = await fetch(callbackUrl, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body,
  });
  if (!resp.ok) {
    console.error(`Callback returned ${resp.status}: ${await resp.text()}`);
  }
}

const server = createServer(async (req, res) => {
  // Discovery
  if (req.method === 'GET' && req.url?.includes('.well-known/rap-toolset')) {
    const manifest = getManifest(req.headers.host || `localhost:${PORT}`);
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(manifest));
    return;
  }

  // Tool invocation
  if (req.method === 'POST') {
    // Acknowledge immediately
    res.writeHead(200);
    res.end('OK');

    // Read body
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString());

    const { arguments: args, id, call_id, callback_url, group_id } = body;
    console.log('Processing get_time:', { args, id, call_id });

    try {
      const now = new Date();
      const timeString = now.toISOString();
      const localTime = now.toLocaleString('en-US', {
        timeZone: args?.timezone || 'UTC',
        dateStyle: 'full',
        timeStyle: 'long',
      });

      const resultText = args?.timezone
        ? `Current time in ${args.timezone}: ${localTime}`
        : `Current UTC time: ${timeString}`;

      await sendResult(callback_url, group_id, id, call_id, resultText);
      console.log('Sent tool result to', callback_url);
    } catch (error) {
      console.error('Error:', error);
      if (callback_url) {
        await sendResult(callback_url, group_id, id, call_id, `Error: ${error.message}`).catch(() => {});
      }
    }
    return;
  }

  res.writeHead(404);
  res.end('Not found');
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`get-time RAP tool server listening on http://127.0.0.1:${PORT}`);
  console.log(`Discovery: http://127.0.0.1:${PORT}/.well-known/rap-toolset`);
});
