import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import * as events from 'aws-cdk-lib/aws-events';
import * as targets from 'aws-cdk-lib/aws-events-targets';
import { Construct } from 'constructs';
import * as path from 'path';
import { AgentZero, LambdaTool, CustomToolSet, LambdaMCPToolSet } from './tools';

export class ExampleAgentStack extends cdk.Stack {
  private agent: AgentZero;
  private api: apigateway.RestApi;

  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    this.agent = new AgentZero(this, 'AgentZero');

    // API Gateway for webhooks
    this.api = new apigateway.RestApi(this, 'WebhookApi', {
      restApiName: 'AgentZero Webhooks',
      description: 'Receives webhook events and forwards to agent',
      deployOptions: {
        stageName: 'prod',
      },
    });

    const slackWebhookUrl = this.agent.setupSlackIntegration(this, this.api);

    this.setupMiscTools();
    this.setupEc2Tools();
    const githubWebhookUrl = this.githubSubscriptionTool();

    new LambdaMCPToolSet(this.agent, 'GithubMcp', {
      name: 'github',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
      env: {
        GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN
      },
    });

    new cdk.CfnOutput(this, 'SlackWebhookUrl', {
      value: slackWebhookUrl,
      description: 'Slack Event Subscription URL',
    });

    new cdk.CfnOutput(this, 'GithubWebhookUrl', {
      value: githubWebhookUrl,
      description: 'GitHub Webhook URL',
    });
  }

  private setupMiscTools(): void {
    const getTimeToolFunction = new lambda.Function(this, 'GetTimeToolFunction', {
      functionName: 'agentzero-get-time-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/get-time-tool')),
      timeout: cdk.Duration.seconds(30),
    });

    const getTimeTool = new LambdaTool(this.agent, 'GetTimeTool', {
      name: 'get_time',
      description: 'Get the current time in a specified timezone or UTC.',
      parameters: {
        type: 'object',
        properties: {
          timezone: {
            type: 'string',
            description:
              "IANA timezone name (e.g., 'America/New_York', 'Europe/London'). Defaults to UTC if not specified.",
          },
        },
        required: [],
      },
      handler: getTimeToolFunction,
      queueProps: {
        visibilityTimeout: cdk.Duration.seconds(30),
      },
    });

    new CustomToolSet(this.agent, 'MiscToolSet', [getTimeTool]);
  }

  private setupEc2Tools(): void {
    // Create EC2 Tool Lambda
    const createEc2ToolFunction = new lambda.Function(this, 'CreateEc2ToolFunction', {
      functionName: 'agentzero-create-ec2-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/create-ec2-tool')),
      timeout: cdk.Duration.seconds(60),
    });
    createEc2ToolFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['ec2:RunInstances', 'ec2:CreateTags', 'ec2:DescribeInstances'],
        resources: ['*'],
      })
    );

    const createEc2Tool = new LambdaTool(this.agent, 'CreateEc2Tool', {
      name: 'create_ec2',
      description: 'Create an EC2 instance. You will be notified when the instance is running.',
      parameters: {
        type: 'object',
        properties: {
          instance_type: {
            type: 'string',
            description: "EC2 instance type (e.g., 't3.micro', 't3.small').",
          },
          ami_id: {
            type: 'string',
            description: 'AMI ID to use for the instance.',
          },
          name: {
            type: 'string',
            description: 'Name tag for the instance.',
          },
          key_name: {
            type: 'string',
            description: 'SSH key pair name for accessing the instance. Optional.',
          },
        },
        required: ['instance_type', 'ami_id', 'name'],
      },
      handler: createEc2ToolFunction,
    });

    // EC2 State Monitor Lambda
    const ec2StateMonitorFunction = new lambda.Function(this, 'Ec2StateMonitorFunction', {
      functionName: 'agentzero-ec2-state-monitor',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/ec2-state-monitor')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        INPUT_QUEUE_URL: this.agent.inputQueue.queueUrl,
      },
    });
    this.agent.inputQueue.grantSendMessages(ec2StateMonitorFunction);
    ec2StateMonitorFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['ec2:DescribeTags', 'ec2:DescribeInstances'],
        resources: ['*'],
      })
    );

    const ec2StateRule = new events.Rule(this, 'Ec2StateChangeRule', {
      ruleName: 'agentzero-ec2-running',
      description: 'Monitors EC2 instances created by AgentZero reaching running state',
      eventPattern: {
        source: ['aws.ec2'],
        detailType: ['EC2 Instance State-change Notification'],
        detail: {
          state: ['running'],
        },
      },
    });
    ec2StateRule.addTarget(new targets.LambdaFunction(ec2StateMonitorFunction));

    new CustomToolSet(this.agent, 'Ec2ToolSet', [createEc2Tool]);
  }

  private githubSubscriptionTool(): string {
    // GitHub Actions Check Tool
    const githubChecksTable = new dynamodb.Table(this, 'GitHubChecksTable', {
      tableName: 'AgentZeroGitHubChecks',
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

    const checkGithubActionsToolFunction = new lambda.Function(this, 'CheckGithubActionsToolFunction', {
      functionName: 'agentzero-check-github-actions-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/check-github-actions-tool')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        GITHUB_CHECKS_TABLE: githubChecksTable.tableName,
      },
    });
    githubChecksTable.grantWriteData(checkGithubActionsToolFunction);

    const subscribeGithubActionsTool = new LambdaTool(this.agent, 'SubscribeGithubActionsTool', {
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
    const githubWebhookReceiverFunction = new lambda.Function(this, 'GithubWebhookReceiverFunction', {
      functionName: 'agentzero-github-webhook-receiver',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/github-webhook-receiver')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        GITHUB_CHECKS_TABLE: githubChecksTable.tableName,
        GITHUB_WEBHOOK_SECRET: process.env.GITHUB_WEBHOOK_SECRET || '',
      },
    });
    githubChecksTable.grantReadWriteData(githubWebhookReceiverFunction);
    this.agent.inputQueue.grantSendMessages(githubWebhookReceiverFunction);

    const githubWebhookIntegration = new apigateway.LambdaIntegration(githubWebhookReceiverFunction);
    this.api.root.addResource('github').addResource('webhook').addMethod('POST', githubWebhookIntegration);

    new CustomToolSet(this.agent, 'GithubActionsToolSet', [subscribeGithubActionsTool]);

    return this.api.url + 'github/webhook';
  }
}
