import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import * as path from 'path';

import { InfinityAgent } from '../../infinity-agents';
import { RapToolSet } from '../../infinity-agents/tools';

export interface GitHubToolSetProps {
  /**
   * API Gateway to add the webhook endpoint to
   */
  webhookGateway: apigateway.RestApi;
}

/**
 * GitHub Actions subscription tools.
 * Tool definitions are served via /.well-known/rap-toolset.
 */
export class GitHubEventToolSet extends RapToolSet {
  public readonly webhookUrl: string;

  constructor(agent: InfinityAgent, id: string, props: GitHubToolSetProps) {
    // DynamoDB tables
    const githubChecksTable = new dynamodb.Table(agent, 'GitHubChecksTable', {
      partitionKey: { name: 'pk', type: dynamodb.AttributeType.STRING },
      sortKey: { name: 'sk', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
      timeToLiveAttribute: 'ttl',
    });

    const subscriptionLookupTable = new dynamodb.Table(agent, 'SubscriptionLookupTable', {
      partitionKey: { name: 'subscriptionId', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
      timeToLiveAttribute: 'ttl',
    });

    // Tool handler Lambda (serves both .well-known and tool invocations)
    const checkGithubActionsToolFunction = new lambda.Function(agent, 'CheckActionsFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'check-github-actions-tool')),
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        GITHUB_CHECKS_TABLE: githubChecksTable.tableName,
        SUBSCRIPTION_LOOKUP_TABLE: subscriptionLookupTable.tableName,
      },
    });
    githubChecksTable.grantReadWriteData(checkGithubActionsToolFunction);
    subscriptionLookupTable.grantReadWriteData(checkGithubActionsToolFunction);

    // GitHub Webhook Receiver (not a tool — receives external webhooks)
    const githubWebhookReceiverFunction = new lambda.Function(agent, 'WebhookReceiverFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'github-webhook-receiver')),
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        GITHUB_CHECKS_TABLE: githubChecksTable.tableName,
        GITHUB_WEBHOOK_SECRET: process.env.GITHUB_WEBHOOK_SECRET || '',
      },
    });
    githubChecksTable.grantReadWriteData(githubWebhookReceiverFunction);
    agent.grantRapAccess(githubWebhookReceiverFunction);

    const githubWebhookIntegration = new apigateway.LambdaIntegration(githubWebhookReceiverFunction);
    props.webhookGateway.root.addResource('github').addResource('webhook').addMethod('POST', githubWebhookIntegration);

    super(agent, id, { serverUrl: '', handler: checkGithubActionsToolFunction });

    this.webhookUrl = props.webhookGateway.url + 'github/webhook';

    new cdk.CfnOutput(this, 'WebhookUrl', {
      value: this.webhookUrl,
      description: 'GitHub Events Webhook URL',
    });
  }
}
