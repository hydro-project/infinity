import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as events from 'aws-cdk-lib/aws-events';
import * as targets from 'aws-cdk-lib/aws-events-targets';
import * as path from 'path';

import { InfinityAgent } from '../../infinity-agents';
import { CustomToolSet, LambdaTool } from '../../infinity-agents/tools';

/**
 * Finance tools: price/news subscriptions + paper trading
 */
export class FinanceToolSet extends CustomToolSet {
  constructor(agent: InfinityAgent, id: string) {
    // --- DynamoDB tables ---

    const subscriptionsTable = new dynamodb.Table(agent, 'FinanceSubscriptionsTable', {
      tableName: 'InfinityAgentsFinanceSubscriptions',
      partitionKey: { name: 'pk', type: dynamodb.AttributeType.STRING },
      sortKey: { name: 'sk', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    const subscriptionLookupTable = new dynamodb.Table(agent, 'FinanceSubLookupTable', {
      tableName: 'InfinityAgentsFinanceSubLookup',
      partitionKey: { name: 'subscriptionId', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    const tradingTable = new dynamodb.Table(agent, 'PaperTradingTable', {
      tableName: 'InfinityAgentsPaperTrading',
      partitionKey: { name: 'pk', type: dynamodb.AttributeType.STRING },
      sortKey: { name: 'sk', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    // --- Subscribe tool Lambda ---

    const subscribeFunction = new lambda.Function(agent, 'FinanceSubscribeFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'subscribe-tool')),
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        SUBSCRIPTIONS_TABLE: subscriptionsTable.tableName,
        LOOKUP_TABLE: subscriptionLookupTable.tableName,
      },
    });
    subscriptionsTable.grantReadWriteData(subscribeFunction);
    subscriptionLookupTable.grantReadWriteData(subscribeFunction);

    const notifyPriceChangeTool = new LambdaTool(agent, 'NotifyPriceChangeTool', {
      name: 'notify_price_change',
      description:
        'Subscribe to be notified when a stock price changes by more than a given threshold (in dollars). ' +
        'The agent will receive a notification event when the price moves. ' +
        'If there is nothing to do until a notification arrives, use the sleep tool to hibernate.',
      parameters: {
        type: 'object',
        properties: {
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL, TSLA, MSFT)' },
          threshold: { type: 'number', description: 'Dollar amount of price change to trigger notification' },
        },
        required: ['symbol', 'threshold'],
      },
      handler: subscribeFunction,
    });

    const notifyNewsTool = new LambdaTool(agent, 'NotifyNewsTool', {
      name: 'notify_news',
      description:
        'Subscribe to Google News RSS for a search query. ' +
        'The agent will receive notification events when new articles matching the query are published. ' +
        'If there is nothing to do until a notification arrives, use the sleep tool to hibernate.',
      parameters: {
        type: 'object',
        properties: {
          query: { type: 'string', description: 'Search query for Google News (e.g. "AAPL earnings", "Tesla stock")' },
        },
        required: ['query'],
      },
      handler: subscribeFunction,
    });

    const cancelFinanceSubTool = new LambdaTool(agent, 'CancelFinanceSubTool', {
      name: 'cancel_finance_subscription',
      description: 'Cancel an active finance subscription (price change or news).',
      parameters: {
        type: 'object',
        properties: {
          subscription_id: { type: 'string', description: 'The subscription ID to cancel' },
        },
        required: ['subscription_id'],
      },
      handler: subscribeFunction,
    });

    // --- Poller Lambda (EventBridge scheduled) ---

    const pollerFunction = new lambda.Function(agent, 'FinancePollerFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'poller')),
      timeout: cdk.Duration.minutes(2),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        SUBSCRIPTIONS_TABLE: subscriptionsTable.tableName,
      },
    });
    subscriptionsTable.grantReadWriteData(pollerFunction);

    // Run every 2 minutes
    new events.Rule(agent, 'FinancePollerSchedule', {
      schedule: events.Schedule.rate(cdk.Duration.minutes(2)),
      targets: [new targets.LambdaFunction(pollerFunction)],
    });

    // Grant RAP receiver invoke permission (SigV4)
    agent.grantRapAccess(pollerFunction);

    // --- Paper trading Lambda ---

    const tradingFunction = new lambda.Function(agent, 'PaperTradingFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'paper-trading-tool')),
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        TRADING_TABLE: tradingTable.tableName,
      },
    });
    tradingTable.grantReadWriteData(tradingFunction);

    const getStockPriceTool = new LambdaTool(agent, 'GetStockPriceTool', {
      name: 'get_stock_price',
      description: 'Get the current market price of a stock.',
      parameters: {
        type: 'object',
        properties: {
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL, TSLA, MSFT)' },
        },
        required: ['symbol'],
      },
      handler: tradingFunction,
    });

    const createAccountTool = new LambdaTool(agent, 'CreateTradingAccountTool', {
      name: 'create_trading_account',
      description: 'Create a paper trading account with an initial cash balance.',
      parameters: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Unique account name/ID' },
          initial_balance: { type: 'number', description: 'Starting cash balance in USD' },
        },
        required: ['account_id', 'initial_balance'],
      },
      handler: tradingFunction,
    });

    const buySharesTool = new LambdaTool(agent, 'BuySharesTool', {
      name: 'buy_shares',
      description: 'Buy shares of a stock in a paper trading account. Uses real-time market prices.',
      parameters: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Trading account ID' },
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL)' },
          quantity: { type: 'number', description: 'Number of shares to buy' },
        },
        required: ['account_id', 'symbol', 'quantity'],
      },
      handler: tradingFunction,
    });

    const sellSharesTool = new LambdaTool(agent, 'SellSharesTool', {
      name: 'sell_shares',
      description: 'Sell shares of a stock from a paper trading account. Uses real-time market prices.',
      parameters: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Trading account ID' },
          symbol: { type: 'string', description: 'Stock ticker symbol (e.g. AAPL)' },
          quantity: { type: 'number', description: 'Number of shares to sell' },
        },
        required: ['account_id', 'symbol', 'quantity'],
      },
      handler: tradingFunction,
    });

    const getPortfolioTool = new LambdaTool(agent, 'GetPortfolioTool', {
      name: 'get_portfolio',
      description: 'Get the current portfolio for a paper trading account, including cash balance, holdings with current market values, and P&L.',
      parameters: {
        type: 'object',
        properties: {
          account_id: { type: 'string', description: 'Trading account ID' },
        },
        required: ['account_id'],
      },
      handler: tradingFunction,
    });

    super(agent, id, [
      notifyPriceChangeTool,
      notifyNewsTool,
      cancelFinanceSubTool,
      getStockPriceTool,
      createAccountTool,
      buySharesTool,
      sellSharesTool,
      getPortfolioTool,
    ]);
  }
}
