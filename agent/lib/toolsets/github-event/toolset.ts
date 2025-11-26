import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import * as path from 'path';

import { InfinityAgent } from '../../infinity-agents';  
import { CustomToolSet, LambdaTool } from '../../infinity-agents/tools';

export interface GitHubToolSetProps {
  /**
   * API Gateway to add the webhook endpoint to
   */
  api: apigateway.RestApi;
}

/**
 * GitHub Actions subscription tools
 */
export class GitHubEventToolSet extends CustomToolSet {
  public readonly webhookUrl: string;

  constructor(agent: InfinityAgent, id: string, props: GitHubToolSetProps) {
    // GitHub Actions Check Tool
    const githubChecksTable = new dynamodb.Table(agent, 'GitHubChecksTable', {
      tableName: 'InfinityAgentsGitHubChecks',
      partitionKey: {
        name: 'pk',
        type: dynamodb.AttributeType.STRING,
      },
      sortKey: {
        name: 'sk',
        type: dynamodb.AttributeType.STRING,
      },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
      timeToLiveAttribute: 'ttl',
    });

    const checkGithubActionsToolFunction = new lambda.Function(agent, 'CheckActionsFunction', {
      functionName: 'infinity-agents-check-github-actions-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'check-github-actions-tool')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        GITHUB_CHECKS_TABLE: githubChecksTable.tableName,
      },
    });
    githubChecksTable.grantWriteData(checkGithubActionsToolFunction);

    const subscribeGithubActionsTool = new LambdaTool(agent, 'SubscribeTool', {
      name: 'subscribe_github_actions_result',
      description:
        'Subscribes to GitHub actions events. The SHA is compared against head_sha from GitHub webhook events. If there is nothing to do until an event arrives, you may want to use the sleep tool to hibernate until you are woken up by an event. DO NOT re-subscribe after an `interrupt`, the subscription remains active automatically.',
      parameters: {
        type: 'object',
        properties: {
          owner: {
            type: 'string',
            description: 'GitHub repository owner (username or organization).',
          },
          repo: {
            type: 'string',
            description: 'GitHub repository name.',
          },
          sha: {
            type: 'string',
            description:
              'Commit SHA to monitor. This must be a full commit SHA (not a branch or tag) as it will be matched against head_sha from GitHub webhook events.',
          },
          check_name: {
            type: 'string',
            description:
              'Optional: specific check/workflow name to wait for. If omitted, waits for the next event for any check related to that commit.',
          },
          kind: {
            type: 'string',
            description:
              'The invocation style: `subscribe` when subscribing to events and `interrupt` when an event arrives.',
          },
        },
        required: ['owner', 'repo', 'sha'],
      },
      handler: checkGithubActionsToolFunction,
      queueProps: {
        visibilityTimeout: cdk.Duration.seconds(30),
      },
    });

    // GitHub Webhook Receiver
    const githubWebhookReceiverFunction = new lambda.Function(agent, 'WebhookReceiverFunction', {
      functionName: 'infinity-agents-github-webhook-receiver',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'github-webhook-receiver')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        GITHUB_CHECKS_TABLE: githubChecksTable.tableName,
        GITHUB_WEBHOOK_SECRET: process.env.GITHUB_WEBHOOK_SECRET || '',
      },
    });
    githubChecksTable.grantReadWriteData(githubWebhookReceiverFunction);
    agent.inputQueue.grantSendMessages(githubWebhookReceiverFunction);

    const githubWebhookIntegration = new apigateway.LambdaIntegration(githubWebhookReceiverFunction);
    props.api.root.addResource('github').addResource('webhook').addMethod('POST', githubWebhookIntegration);

    super(agent, id, [subscribeGithubActionsTool]);

    this.webhookUrl = props.api.url + 'github/webhook';
  }
}
