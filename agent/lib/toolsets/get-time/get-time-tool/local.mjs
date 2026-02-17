#!/usr/bin/env node
/**
 * Local standalone RAP tool server for get-time.
 *
 * Usage:
 *   node local.mjs [--port 3001]
 *
 * Results are POSTed back to the callback_url using plain HTTP (no SigV4).
 */

import { createServer } from 'node:http';
import { buildManifest, processGetTime } from './tool.mjs';

const PORT = parseInt(process.argv.find((_, i, a) => a[i - 1] === '--port') || '3001', 10);

async function sendResult(callbackUrl, groupId, id, callId, text) {
  const resp = await fetch(callbackUrl, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ type: 'tool_result', group_id: groupId, id, call_id: callId || null, text }),
  });
  if (!resp.ok) {
    console.error(`Callback returned ${resp.status}: ${await resp.text()}`);
  }
}

const server = createServer(async (req, res) => {
  // Discovery
  if (req.method === 'GET' && req.url?.includes('.well-known/rap-toolset')) {
    const host = req.headers.host || `localhost:${PORT}`;
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(buildManifest(`http://${host}`)));
    return;
  }

  // Tool invocation
  if (req.method === 'POST') {
    res.writeHead(200);
    res.end('OK');

    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString());
    const { arguments: args, id, call_id, callback_url, group_id } = body;

    console.log('Processing get_time:', { args, id, call_id });
    try {
      const resultText = processGetTime(args);
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
