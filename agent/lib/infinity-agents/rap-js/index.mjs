/**
 * RAP (Reactive Agent Protocol) client helper.
 *
 * Provides a simple interface for tool Lambdas to send results back
 * to the agent via the RAP HTTP receiver endpoint.
 *
 * Uses AWS IAM SigV4 signing for Function URL auth.
 */

import { SignatureV4 } from '@smithy/signature-v4';
import { HttpRequest } from '@smithy/protocol-http';
import { Sha256 } from '@aws-crypto/sha256-js';
import { defaultProvider } from '@aws-sdk/credential-provider-node';

const signer = new SignatureV4({
  service: 'lambda',
  region: process.env.AWS_REGION || process.env.AWS_DEFAULT_REGION || 'us-east-1',
  credentials: defaultProvider(),
  sha256: Sha256,
});

/**
 * Send a tool result to the agent via the RAP receiver.
 *
 * @param {string} rapReceiverUrl - The RAP receiver Function URL
 * @param {string} groupId - Message group ID (thread ID)
 * @param {string} id - Tool call ID
 * @param {string|null} callId - Optional call ID
 * @param {string} text - Result text
 */
export async function sendToolResult(rapReceiverUrl, groupId, id, callId, text) {
  const body = {
    content: {
      type: 'toolresult',
      id,
      ...(callId && { call_id: callId }),
      content: [{ type: 'text', text }],
    },
    group_id: groupId,
  };

  await postToRap(rapReceiverUrl, body);
}

/**
 * Send an OAuth URL to the agent via the RAP receiver.
 */
export async function sendOAuthUrl(rapReceiverUrl, groupId, id, callId, authUrl) {
  const body = {
    content: {
      type: 'oauth_required',
      id,
      call_id: callId,
      auth_url: authUrl,
    },
    group_id: groupId,
  };

  await postToRap(rapReceiverUrl, body);
}

/**
 * Send a synthetic event (subscription notification) via the RAP receiver.
 */
export async function sendSyntheticEvent(rapReceiverUrl, groupId, toolCallId, text) {
  const body = {
    content: {
      type: 'toolresult',
      id: '',
      call_id: null,
      content: [{ type: 'text', text }],
    },
    group_id: groupId,
    synthetic: toolCallId,
  };

  await postToRap(rapReceiverUrl, body);
}

/**
 * Post a raw message body to the RAP receiver, signed with SigV4.
 */
async function postToRap(rapReceiverUrl, body) {
  const url = new URL(rapReceiverUrl);
  const bodyString = JSON.stringify(body);

  const request = new HttpRequest({
    method: 'POST',
    protocol: url.protocol,
    hostname: url.hostname,
    path: url.pathname,
    headers: {
      'Content-Type': 'application/json',
      host: url.hostname,
    },
    body: bodyString,
  });

  const signed = await signer.sign(request);

  const response = await fetch(rapReceiverUrl, {
    method: 'POST',
    headers: signed.headers,
    body: bodyString,
  });

  if (!response.ok) {
    const errorText = await response.text();
    throw new Error(`RAP receiver returned ${response.status}: ${errorText}`);
  }
}
