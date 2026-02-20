import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import { NodejsFunction } from 'aws-cdk-lib/aws-lambda-nodejs';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as events from 'aws-cdk-lib/aws-events';
import * as targets from 'aws-cdk-lib/aws-events-targets';
import * as path from 'path';
import { Construct } from 'constructs';

import { InfinityAgent, NODEJS_BUNDLING_DEFAULTS } from '../../infinity-agents';
import { RapToolSet } from '../../infinity-agents/tools';

/**
 * Finance tools: two separate RAP toolsets (subscriptions + paper trading).
 * This is a Construct that creates both toolsets as children.
 */
export class FinanceToolSet extends Construct {
  constructor(agent: InfinityAgent, id: string) {
    super(agent, id);

    // --- DynamoDB tables ---

    const subscriptionsTable = new dynamodb.Table(this, 'SubscriptionsTable', {
      partitionKey: { name: 'pk', type: dynamodb.AttributeType.STRING },
      sortKey: { name: 'sk', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    const subscriptionLookupTable = new dynamodb.Table(this, 'SubLookupTable', {
      partitionKey: { name: 'subscriptionId', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    const tradingTable = new dynamodb.Table(this, 'PaperTradingTable', {
      partitionKey: { name: 'pk', type: dynamodb.AttributeType.STRING },
      sortKey: { name: 'sk', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    // --- Subscriptions toolset ---

    const subscribeFunction = new NodejsFunction(this, 'SubscribeFunction', {
      entry: path.join(__dirname, 'subscribe-tool', 'index.mjs'),
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'handler',
      bundling: NODEJS_BUNDLING_DEFAULTS,
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        SUBSCRIPTIONS_TABLE: subscriptionsTable.tableName,
        LOOKUP_TABLE: subscriptionLookupTable.tableName,
      },
    });
    subscriptionsTable.grantReadWriteData(subscribeFunction);
    subscriptionLookupTable.grantReadWriteData(subscribeFunction);

    new RapToolSet(agent, 'FinanceSubscriptions', {
      handler: subscribeFunction,
    });

    // --- Poller Lambda (EventBridge scheduled, not a tool) ---

    const pollerFunction = new NodejsFunction(this, 'PollerFunction', {
      entry: path.join(__dirname, 'poller', 'index.mjs'),
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'handler',
      bundling: NODEJS_BUNDLING_DEFAULTS,
      timeout: cdk.Duration.minutes(2),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        SUBSCRIPTIONS_TABLE: subscriptionsTable.tableName,
      },
    });
    subscriptionsTable.grantReadWriteData(pollerFunction);
    agent.grantRapAccess(pollerFunction);

    new events.Rule(this, 'PollerSchedule', {
      schedule: events.Schedule.rate(cdk.Duration.minutes(2)),
      targets: [new targets.LambdaFunction(pollerFunction)],
    });

    // --- Paper trading toolset ---

    const tradingFunction = new NodejsFunction(this, 'TradingFunction', {
      entry: path.join(__dirname, 'paper-trading-tool', 'index.mjs'),
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'handler',
      bundling: NODEJS_BUNDLING_DEFAULTS,
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        TRADING_TABLE: tradingTable.tableName,
      },
    });
    tradingTable.grantReadWriteData(tradingFunction);

    new RapToolSet(agent, 'FinanceTrading', {
      handler: tradingFunction,
    });
  }
}
