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
  webhookGateway: apigateway.RestApi;
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

    // Subscription lookup table - maps subscription ID to pk/sk for fast cancellation
    const subscriptionLookupTable = new dynamodb.Table(agent, 'SubscriptionLookupTable', {
      tableName: 'InfinityAgentsSubscriptionLookup',
      partitionKey: {
        name: 'subscriptionId',
        type: dynamodb.AttributeType.STRING,
      },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
      timeToLiveAttribute: 'ttl',
    });

    const checkGithubActionsToolFunction = new lambda.Function(agent, 'CheckActionsFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'check-github-actions-tool')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        GITHUB_CHECKS_TABLE: githubChecksTable.tableName,
        SUBSCRIPTION_LOOKUP_TABLE: subscriptionLookupTable.tableName,
      },
    });
    githubChecksTable.grantReadWriteData(checkGithubActionsToolFunction);
    subscriptionLookupTable.grantReadWriteData(checkGithubActionsToolFunction);

    const subscribeGithubEventTool = new LambdaTool(agent, 'SubscribeTool', {
      name: 'subscribe_github_events',
      description:
        'Subscribes to GitHub webhook events on hydro-project/hydro. Use filters to match specific events. If there is nothing to do until an event arrives, you may want to use the sleep tool to hibernate until you are woken up by an event. DO NOT re-subscribe after an `interrupt`, the subscription remains active automatically.',
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
          event_type: {
            type: 'string',
            description:
              'Optional: GitHub event type to filter on (e.g., "pull_request", "issue_comment", "push", "check_run", "workflow_run", "issues", "pull_request_review", "pull_request_review_comment"). If omitted, matches all events.',
          },
          sha: {
            type: 'string',
            description:
              'Optional: Commit SHA to filter on. Matched against head_sha, after, or sha fields in webhook payloads.',
          },
          pr_number: {
            type: 'number',
            description:
              'Optional: Pull request number to filter on. Matches events related to this PR (comments, reviews, etc.).',
          },
          issue_number: {
            type: 'number',
            description:
              'Optional: Issue number to filter on. Matches events related to this issue (comments, state changes, etc.).',
          },
          action: {
            type: 'string',
            description:
              'Optional: Event action to filter on (e.g., "opened", "closed", "created", "completed"). Most GitHub events include an action field.',
          },
          branch: {
            type: 'string',
            description:
              'Optional: Branch name to filter on. Matched against ref, head_ref, or base_ref fields.',
          },
          actor: {
            type: 'string',
            description:
              'Optional: GitHub username to filter on. Matches the sender/actor of the event.',
          },
        },
        required: ['owner', 'repo'],
      },
      handler: checkGithubActionsToolFunction,
      queueProps: {
        visibilityTimeout: cdk.Duration.seconds(30),
      },
    });

    const cancelGithubSubscriptionTool = new LambdaTool(agent, 'CancelSubscriptionTool', {
      name: 'cancel_github_subscription',
      description:
        'Cancels an active GitHub webhook event subscription. Use this when you no longer need to receive events for a particular subscription.',
      parameters: {
        type: 'object',
        properties: {
          subscription_id: {
            type: 'string',
            description: 'The subscription ID returned when you created the subscription.',
          },
        },
        required: ['subscription_id'],
      },
      handler: checkGithubActionsToolFunction,
      queueProps: {
        visibilityTimeout: cdk.Duration.seconds(30),
      },
    });

    // GitHub Webhook Receiver
    const githubWebhookReceiverFunction = new lambda.Function(agent, 'WebhookReceiverFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
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
    props.webhookGateway.root.addResource('github').addResource('webhook').addMethod('POST', githubWebhookIntegration);

    super(agent, id, [subscribeGithubEventTool, cancelGithubSubscriptionTool]);

    this.webhookUrl = props.webhookGateway.url + 'github/webhook';

    new cdk.CfnOutput(this, 'WebhookUrl', {
      value: this.webhookUrl,
      description: 'GitHub Events Webhook URL',
    });
  }
}
