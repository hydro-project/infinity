import { DynamoDBClient, PutItemCommand, GetItemCommand, DeleteItemCommand } from '@aws-sdk/client-dynamodb';
import { sendToolResult } from '../../../infinity-agents/rap-js/index.mjs';

const dynamoClient = new DynamoDBClient({});

const SUBSCRIPTIONS_TABLE = process.env.SUBSCRIPTIONS_TABLE;
const LOOKUP_TABLE = process.env.LOOKUP_TABLE;

const TOOLSET_MANIFEST = {
  name: 'finance-subscriptions',
  description: 'Subscribe to stock price changes and news alerts',
  endpoint: '',
  tools: [
    {
      name: 'notify_price_change',
      description: 'Subscribe to be notified when a stock price changes by more than a given threshold (in dollars). The agent will receive a notification event when the price moves. If there is nothing to do until a notification arrives, use the sleep tool to hibernate.',
      inputSchema: {
        type: 'object',
        properties: {
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL, TSLA, MSFT)' },
          threshold: { type: 'number', description: 'Dollar amount of price change to trigger notification' },
        },
        required: ['symbol', 'threshold'],
      },
    },
    {
      name: 'notify_news',
      description: 'Subscribe to Google News RSS for a search query. The agent will receive notification events when new articles matching the query are published. If there is nothing to do until a notification arrives, use the sleep tool to hibernate.',
      inputSchema: {
        type: 'object',
        properties: {
          query: { type: 'string', description: 'Search query for Google News (e.g. "AAPL earnings", "Tesla stock")' },
        },
        required: ['query'],
      },
    },
    {
      name: 'cancel_finance_subscription',
      description: 'Cancel an active finance subscription (price change or news).',
      inputSchema: {
        type: 'object',
        properties: {
          subscription_id: { type: 'string', description: 'The subscription ID to cancel' },
        },
        required: ['subscription_id'],
      },
    },
  ],
};

async function handlePriceSubscription(args, id, callId, callbackUrl, groupId) {
  const { symbol, threshold } = args;
  if (!symbol || threshold == null) {
    return 'Error: symbol and threshold are required';
  }

  const pk = `PRICE#${symbol.toUpperCase()}`;
  const sk = `SUB#${id}`;

  await dynamoClient.send(new PutItemCommand({
    TableName: SUBSCRIPTIONS_TABLE,
    Item: {
      pk: { S: pk },
      sk: { S: sk },
      subType: { S: 'price' },
      symbol: { S: symbol.toUpperCase() },
      threshold: { N: String(threshold) },
      lastPrice: { N: '0' },
      toolCallId: { S: id },
      callId: { S: callId || '' },
      groupId: { S: groupId },
      rapReceiverUrl: { S: callbackUrl },
      createdAt: { N: Date.now().toString() },
    },
  }));

  await dynamoClient.send(new PutItemCommand({
    TableName: LOOKUP_TABLE,
    Item: {
      subscriptionId: { S: id },
      pk: { S: pk },
      sk: { S: sk },
    },
  }));

  return `Subscribed to price changes for ${symbol.toUpperCase()} with threshold ${threshold}. Subscription ID: ${id}`;
}

async function handleNewsSubscription(args, id, callId, callbackUrl, groupId) {
  const { query } = args;
  if (!query) {
    return 'Error: query is required';
  }

  const pk = `NEWS#${query}`;
  const sk = `SUB#${id}`;

  await dynamoClient.send(new PutItemCommand({
    TableName: SUBSCRIPTIONS_TABLE,
    Item: {
      pk: { S: pk },
      sk: { S: sk },
      subType: { S: 'news' },
      query: { S: query },
      lastArticleId: { S: '' },
      toolCallId: { S: id },
      callId: { S: callId || '' },
      groupId: { S: groupId },
      rapReceiverUrl: { S: callbackUrl },
      createdAt: { N: Date.now().toString() },
    },
  }));

  await dynamoClient.send(new PutItemCommand({
    TableName: LOOKUP_TABLE,
    Item: {
      subscriptionId: { S: id },
      pk: { S: pk },
      sk: { S: sk },
    },
  }));

  return `Subscribed to news for "${query}". Subscription ID: ${id}`;
}

async function handleCancel(args) {
  const { subscription_id } = args;
  if (!subscription_id) return 'Error: subscription_id is required';

  const lookup = await dynamoClient.send(new GetItemCommand({
    TableName: LOOKUP_TABLE,
    Key: { subscriptionId: { S: subscription_id } },
  }));

  if (!lookup.Item) {
    return `Subscription not found: ${subscription_id}`;
  }

  const pk = lookup.Item.pk.S;
  const sk = lookup.Item.sk.S;

  await dynamoClient.send(new DeleteItemCommand({
    TableName: SUBSCRIPTIONS_TABLE,
    Key: { pk: { S: pk }, sk: { S: sk } },
  }));
  await dynamoClient.send(new DeleteItemCommand({
    TableName: LOOKUP_TABLE,
    Key: { subscriptionId: { S: subscription_id } },
  }));

  return `Cancelled subscription: ${subscription_id}`;
}

export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
  // Handle .well-known/rap-toolset discovery
  if (event.requestContext?.http?.method === 'GET' && event.rawPath?.includes('.well-known/rap-toolset')) {
    const manifest = { ...TOOLSET_MANIFEST };
    if (!manifest.endpoint) {
      manifest.endpoint = `https://${event.requestContext?.domainName || ''}`;
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
    const { arguments: args, id, call_id, callback_url, group_id, operation } = body;

    let result;
    let isSubscription = false;
    if (operation === 'cancel_finance_subscription') {
      result = await handleCancel(args);
    } else if (operation === 'notify_price_change') {
      result = await handlePriceSubscription(args, id, call_id, callback_url, group_id);
      isSubscription = true;
    } else if (operation === 'notify_news') {
      result = await handleNewsSubscription(args, id, call_id, callback_url, group_id);
      isSubscription = true;
    } else {
      result = `Unknown tool: ${operation}`;
    }
    await sendToolResult(callback_url, group_id, id, call_id, result, isSubscription || undefined);
  } catch (err) {
    console.error('Error:', err);
    try {
      const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
      await sendToolResult(body.callback_url, body.group_id, body.id, body.call_id, `Error: ${err.message}`);
    } catch (e) {
      console.error('Failed to send error result:', e);
    }
  }
});
