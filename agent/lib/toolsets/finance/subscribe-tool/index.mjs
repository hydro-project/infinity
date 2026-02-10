import { DynamoDBClient, PutItemCommand, GetItemCommand, DeleteItemCommand } from '@aws-sdk/client-dynamodb';
import { sendToolResult } from 'rap-js';

const dynamoClient = new DynamoDBClient({});

const SUBSCRIPTIONS_TABLE = process.env.SUBSCRIPTIONS_TABLE;
const LOOKUP_TABLE = process.env.LOOKUP_TABLE;

async function handlePriceSubscription(args, id, callId, rapReceiverUrl, groupId) {
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
      rapReceiverUrl: { S: rapReceiverUrl },
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

async function handleNewsSubscription(args, id, callId, rapReceiverUrl, groupId) {
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
      rapReceiverUrl: { S: rapReceiverUrl },
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

export const handler = async (event) => {
  for (const record of event.Records) {
    const request = JSON.parse(record.body);
    const { arguments: args, id, call_id, rap_receiver_url, group_id, operation } = request;

    try {
      let result;
      if (operation === 'cancel_finance_subscription') {
        result = await handleCancel(args);
      } else if (operation === 'notify_price_change') {
        result = await handlePriceSubscription(args, id, call_id, rap_receiver_url, group_id);
      } else if (operation === 'notify_news') {
        result = await handleNewsSubscription(args, id, call_id, rap_receiver_url, group_id);
      } else {
        result = `Unknown tool: ${operation}`;
      }
      await sendToolResult(rap_receiver_url, group_id, id, call_id, result);
    } catch (err) {
      console.error('Error:', err);
      await sendToolResult(rap_receiver_url, group_id, id, call_id, `Error: ${err.message}`);
    }
  }
  return { statusCode: 200 };
};
