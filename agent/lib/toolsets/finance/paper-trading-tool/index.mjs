import { DynamoDBClient, GetItemCommand, PutItemCommand, UpdateItemCommand } from '@aws-sdk/client-dynamodb';
import { sendToolResult } from '../../../infinity-agents/rap-js/index.mjs';

const dynamoClient = new DynamoDBClient({});

const TRADING_TABLE = process.env.TRADING_TABLE;

const TOOLSET_MANIFEST = {
  name: 'finance-trading',
  description: 'Paper trading and stock price tools',
  endpoint: '',
  tools: [
    {
      name: 'get_stock_price',
      description: 'Get the current market price of a stock.',
      inputSchema: {
        type: 'object',
        properties: {
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL, TSLA, MSFT)' },
        },
        required: ['symbol'],
      },
      annotations: { readOnly: true },
    },
    {
      name: 'create_trading_account',
      description: 'Create a paper trading account with an initial cash balance.',
      inputSchema: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Unique account name/ID' },
          initial_balance: { type: 'number', description: 'Starting cash balance in USD' },
        },
        required: ['account_id', 'initial_balance'],
      },
    },
    {
      name: 'buy_shares',
      description: 'Buy shares of a stock in a paper trading account. Uses real-time market prices.',
      inputSchema: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Trading account ID' },
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL)' },
          quantity: { type: 'number', description: 'Number of shares to buy' },
        },
        required: ['account_id', 'symbol', 'quantity'],
      },
    },
    {
      name: 'sell_shares',
      description: 'Sell shares of a stock from a paper trading account. Uses real-time market prices.',
      inputSchema: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Trading account ID' },
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL)' },
          quantity: { type: 'number', description: 'Number of shares to sell' },
        },
        required: ['account_id', 'symbol', 'quantity'],
      },
    },
    {
      name: 'get_portfolio',
      description: 'Get the current portfolio for a paper trading account, including cash balance, holdings with current market values, and P&L.',
      inputSchema: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Trading account ID' },
        },
        required: ['account_id'],
      },
      annotations: { readOnly: true },
    },
  ],
};

async function fetchPrice(symbol) {
  const url = `https://query2.finance.yahoo.com/v8/finance/chart/${encodeURIComponent(symbol)}?range=1d&interval=1d`;
  const res = await fetch(url, { headers: { 'User-Agent': 'Mozilla/5.0' } });
  if (!res.ok) throw new Error(`Yahoo Finance returned ${res.status} for ${symbol}`);
  const data = await res.json();
  const meta = data.chart?.result?.[0]?.meta;
  if (!meta) throw new Error(`No data for ${symbol}`);
  return meta.regularMarketPrice;
}

async function getAccount(accountId) {
  const result = await dynamoClient.send(new GetItemCommand({
    TableName: TRADING_TABLE,
    Key: { pk: { S: `ACCOUNT#${accountId}` }, sk: { S: 'META' } },
  }));
  return result.Item;
}

async function getHolding(accountId, symbol) {
  const result = await dynamoClient.send(new GetItemCommand({
    TableName: TRADING_TABLE,
    Key: { pk: { S: `ACCOUNT#${accountId}` }, sk: { S: `HOLDING#${symbol}` } },
  }));
  return result.Item;
}

async function handleCreateAccount(args) {
  const { account_id, initial_balance } = args;
  if (!account_id || initial_balance == null) return 'Error: account_id and initial_balance are required';

  const existing = await getAccount(account_id);
  if (existing) return `Error: Account "${account_id}" already exists`;

  await dynamoClient.send(new PutItemCommand({
    TableName: TRADING_TABLE,
    Item: {
      pk: { S: `ACCOUNT#${account_id}` },
      sk: { S: 'META' },
      balance: { N: String(initial_balance) },
      createdAt: { N: Date.now().toString() },
    },
  }));

  return `Created paper trading account "${account_id}" with ${initial_balance} balance.`;
}

async function handleBuyShares(args) {
  const { account_id, symbol, quantity } = args;
  if (!account_id || !symbol || !quantity) return 'Error: account_id, symbol, and quantity are required';

  const sym = symbol.toUpperCase();
  const account = await getAccount(account_id);
  if (!account) return `Error: Account "${account_id}" not found`;

  const price = await fetchPrice(sym);
  const cost = price * quantity;
  const balance = parseFloat(account.balance.N);

  if (cost > balance) {
    return `Insufficient funds. ${quantity} shares of ${sym} @ ${price.toFixed(2)} = ${cost.toFixed(2)}, but balance is ${balance.toFixed(2)}`;
  }

  const newBalance = balance - cost;

  await dynamoClient.send(new UpdateItemCommand({
    TableName: TRADING_TABLE,
    Key: { pk: { S: `ACCOUNT#${account_id}` }, sk: { S: 'META' } },
    UpdateExpression: 'SET balance = :b',
    ExpressionAttributeValues: { ':b': { N: String(newBalance) } },
  }));

  const holding = await getHolding(account_id, sym);
  const existingQty = holding ? parseFloat(holding.quantity.N) : 0;
  const existingCost = holding ? parseFloat(holding.totalCost.N) : 0;

  await dynamoClient.send(new PutItemCommand({
    TableName: TRADING_TABLE,
    Item: {
      pk: { S: `ACCOUNT#${account_id}` },
      sk: { S: `HOLDING#${sym}` },
      symbol: { S: sym },
      quantity: { N: String(existingQty + quantity) },
      totalCost: { N: String(existingCost + cost) },
    },
  }));

  return `Bought ${quantity} shares of ${sym} @ ${price.toFixed(2)} for ${cost.toFixed(2)}. New balance: ${newBalance.toFixed(2)}`;
}

async function handleSellShares(args) {
  const { account_id, symbol, quantity } = args;
  if (!account_id || !symbol || !quantity) return 'Error: account_id, symbol, and quantity are required';

  const sym = symbol.toUpperCase();
  const account = await getAccount(account_id);
  if (!account) return `Error: Account "${account_id}" not found`;

  const holding = await getHolding(account_id, sym);
  if (!holding) return `Error: No holdings of ${sym} in account "${account_id}"`;

  const heldQty = parseFloat(holding.quantity.N);
  if (quantity > heldQty) return `Error: Only ${heldQty} shares of ${sym} held, cannot sell ${quantity}`;

  const price = await fetchPrice(sym);
  const proceeds = price * quantity;
  const balance = parseFloat(account.balance.N);
  const newBalance = balance + proceeds;

  await dynamoClient.send(new UpdateItemCommand({
    TableName: TRADING_TABLE,
    Key: { pk: { S: `ACCOUNT#${account_id}` }, sk: { S: 'META' } },
    UpdateExpression: 'SET balance = :b',
    ExpressionAttributeValues: { ':b': { N: String(newBalance) } },
  }));

  const newQty = heldQty - quantity;
  const costBasis = parseFloat(holding.totalCost.N);
  const newCost = costBasis * (newQty / heldQty);

  if (newQty === 0) {
    const { DeleteItemCommand } = await import('@aws-sdk/client-dynamodb');
    await dynamoClient.send(new DeleteItemCommand({
      TableName: TRADING_TABLE,
      Key: { pk: { S: `ACCOUNT#${account_id}` }, sk: { S: `HOLDING#${sym}` } },
    }));
  } else {
    await dynamoClient.send(new PutItemCommand({
      TableName: TRADING_TABLE,
      Item: {
        pk: { S: `ACCOUNT#${account_id}` },
        sk: { S: `HOLDING#${sym}` },
        symbol: { S: sym },
        quantity: { N: String(newQty) },
        totalCost: { N: String(newCost) },
      },
    }));
  }

  const pnl = proceeds - (costBasis * (quantity / heldQty));
  return `Sold ${quantity} shares of ${sym} @ ${price.toFixed(2)} for ${proceeds.toFixed(2)}. P&L: ${pnl >= 0 ? '+' : ''}${pnl.toFixed(2)}. New balance: ${newBalance.toFixed(2)}`;
}

async function handleGetPrice(args) {
  const { symbol } = args;
  if (!symbol) return 'Error: symbol is required';
  const sym = symbol.toUpperCase();
  const price = await fetchPrice(sym);
  return JSON.stringify({ symbol: sym, price: parseFloat(price.toFixed(2)) });
}

async function handleGetPortfolio(args) {
  const { account_id } = args;
  if (!account_id) return 'Error: account_id is required';

  const account = await getAccount(account_id);
  if (!account) return `Error: Account "${account_id}" not found`;

  const balance = parseFloat(account.balance.N);

  const { QueryCommand } = await import('@aws-sdk/client-dynamodb');
  const result = await dynamoClient.send(new QueryCommand({
    TableName: TRADING_TABLE,
    KeyConditionExpression: 'pk = :pk AND begins_with(sk, :prefix)',
    ExpressionAttributeValues: {
      ':pk': { S: `ACCOUNT#${account_id}` },
      ':prefix': { S: 'HOLDING#' },
    },
  }));

  const holdings = [];
  let totalValue = balance;

  for (const item of (result.Items || [])) {
    const sym = item.symbol.S;
    const qty = parseFloat(item.quantity.N);
    const costBasis = parseFloat(item.totalCost.N);
    let currentPrice = 0;
    let marketValue = 0;

    try {
      currentPrice = await fetchPrice(sym);
      marketValue = currentPrice * qty;
    } catch (err) {
      marketValue = costBasis;
    }

    totalValue += marketValue;
    const pnl = marketValue - costBasis;

    holdings.push({
      symbol: sym,
      quantity: qty,
      avg_cost: parseFloat((costBasis / qty).toFixed(2)),
      current_price: parseFloat(currentPrice.toFixed(2)),
      market_value: parseFloat(marketValue.toFixed(2)),
      pnl: parseFloat(pnl.toFixed(2)),
    });
  }

  return JSON.stringify({
    account_id,
    cash_balance: parseFloat(balance.toFixed(2)),
    holdings,
    total_portfolio_value: parseFloat(totalValue.toFixed(2)),
  }, null, 2);
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
    switch (operation) {
      case 'get_stock_price': result = await handleGetPrice(args); break;
      case 'create_trading_account': result = await handleCreateAccount(args); break;
      case 'buy_shares': result = await handleBuyShares(args); break;
      case 'sell_shares': result = await handleSellShares(args); break;
      case 'get_portfolio': result = await handleGetPortfolio(args); break;
      default: result = `Unknown tool: ${operation}`;
    }
    await sendToolResult(callback_url, group_id, id, call_id, result);
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
