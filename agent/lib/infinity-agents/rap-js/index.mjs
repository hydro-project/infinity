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
 * Send a tool result to the agent.
 *
 * @param {string} rapReceiverUrl - The RAP receiver Function URL
 * @param {string} groupId - Message group ID (thread ID)
 * @param {string} id - Tool call ID
 * @param {string|null} callId - Optional call ID
 * @param {string} text - Result text
 */
export async function sendToolResult(rapReceiverUrl, groupId, id, callId, text) {
  await postToRap(rapReceiverUrl, {
    type: 'tool_result',
    group_id: groupId,
    id,
    call_id: callId || null,
    text,
  });
}

/**
 * Send an OAuth URL to the agent.
 */
export async function sendOAuthUrl(rapReceiverUrl, groupId, id, callId, authUrl) {
  await postToRap(rapReceiverUrl, {
    type: 'oauth',
    group_id: groupId,
    id,
    call_id: callId || null,
    auth_url: authUrl,
  });
}

/**
 * Send a subscription event notification.
 */
export async function sendSubscriptionEvent(rapReceiverUrl, groupId, toolCallId, text) {
  await postToRap(rapReceiverUrl, {
    type: 'subscription_event',
    group_id: groupId,
    tool_call_id: toolCallId,
    text,
  });
}

/**
 * Post a message to the RAP receiver, signed with SigV4.
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
