import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import * as events from 'aws-cdk-lib/aws-events';
import * as targets from 'aws-cdk-lib/aws-events-targets';
import { SqsEventSource } from 'aws-cdk-lib/aws-lambda-event-sources';
import { Construct } from 'constructs';
import * as path from 'path';

export class AgentZeroLeaderStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    // DynamoDB table for conversation history
    const historyTable = new dynamodb.Table(this, 'AgentZeroStateTable', {
      tableName: 'AgentZeroState',
      partitionKey: {
        name: 'session',
        type: dynamodb.AttributeType.STRING,
      },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
      pointInTimeRecovery: true,
    });

    // Dead Letter Queue for failed messages
    const deadLetterQueue = new sqs.Queue(this, 'AgentZeroDeadLetterQueue', {
      queueName: 'agentzero-leader-dlq',
      retentionPeriod: cdk.Duration.days(14),
    });

    // SQS Standard Queue for incoming messages (agent input)
    const messageQueue = new sqs.Queue(this, 'AgentZeroMessageQueue', {
      queueName: 'agentzero-leader',
      visibilityTimeout: cdk.Duration.minutes(15),
      retentionPeriod: cdk.Duration.days(4),
      deadLetterQueue: {
        queue: deadLetterQueue,
        maxReceiveCount: 3,
      },
    });

    // Dead Letter Queue for output messages
    const outputDeadLetterQueue = new sqs.Queue(this, 'AgentZeroOutputDeadLetterQueue', {
      queueName: 'agentzero-output-dlq',
      retentionPeriod: cdk.Duration.days(14),
    });

    // SQS Standard Queue for agent outputs
    const outputQueue = new sqs.Queue(this, 'AgentZeroOutputQueue', {
      queueName: 'agentzero-output',
      visibilityTimeout: cdk.Duration.minutes(5),
      retentionPeriod: cdk.Duration.days(4),
      deadLetterQueue: {
        queue: outputDeadLetterQueue,
        maxReceiveCount: 3,
      },
    });

    // IAM Role for EventBridge Scheduler to send messages to SQS
    const schedulerRole = new iam.Role(this, 'SchedulerRole', {
      assumedBy: new iam.ServicePrincipal('scheduler.amazonaws.com'),
    });
    messageQueue.grantSendMessages(schedulerRole);

    // Lambda function - using placeholder initially, then deploy with cargo lambda
    const lambdaFunction = new lambda.Function(this, 'AgentZeroLeaderFunction', {
      functionName: 'agentzero-leader',
      runtime: lambda.Runtime.PROVIDED_AL2023,
      handler: 'bootstrap',
      architecture: lambda.Architecture.ARM_64,
      code: lambda.Code.fromAsset(path.join(__dirname, '../../target/lambda/agentzero-leader')),
      timeout: cdk.Duration.minutes(15),
      memorySize: 512,
      environment: {
        DYNAMODB_TABLE_NAME: historyTable.tableName,
        OUTPUT_QUEUE_URL: outputQueue.queueUrl,
        INPUT_QUEUE_URL: messageQueue.queueUrl,
        INPUT_QUEUE_ARN: messageQueue.queueArn,
        SCHEDULER_ROLE_ARN: schedulerRole.roleArn,
        RUST_BACKTRACE: '1',
      },
    });

    // Grant Lambda permissions to access DynamoDB
    historyTable.grantReadWriteData(lambdaFunction);

    // Grant Lambda permissions to invoke Bedrock
    lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: [
          'bedrock:InvokeModel',
          'bedrock:InvokeModelWithResponseStream',
        ],
        resources: ['*'],
      })
    );

    // Grant Lambda permissions to create EventBridge Scheduler schedules
    lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: [
          'scheduler:CreateSchedule',
          'scheduler:DeleteSchedule',
        ],
        resources: ['*'],
      })
    );

    // Grant Lambda permission to pass the scheduler role
    lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['iam:PassRole'],
        resources: [schedulerRole.roleArn],
      })
    );

    // Add SQS as event source for Lambda
    lambdaFunction.addEventSource(
      new SqsEventSource(messageQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Grant Lambda permission to send to output queue
    outputQueue.grantSendMessages(lambdaFunction);

    // Grant Lambda permission to send to input queue (for sleep tool)
    messageQueue.grantSendMessages(lambdaFunction);

    // Slack Receiver Lambda (receives Slack events, sends to agent input queue)
    const slackReceiverFunction = new lambda.Function(this, 'SlackReceiverFunction', {
      functionName: 'agentzero-slack-receiver',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/slack-receiver')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        AGENT_INPUT_QUEUE_URL: messageQueue.queueUrl,
        SLACK_SIGNING_SECRET: process.env.SLACK_SIGNING_SECRET || '',
      },
    });

    // Grant Slack Receiver permission to send to agent input queue
    messageQueue.grantSendMessages(slackReceiverFunction);

    // API Gateway for Slack webhook
    const api = new apigateway.RestApi(this, 'SlackWebhookApi', {
      restApiName: 'AgentZero Slack Webhook',
      description: 'Receives Slack events and forwards to agent',
      deployOptions: {
        stageName: 'prod',
      },
    });

    const slackIntegration = new apigateway.LambdaIntegration(slackReceiverFunction);
    api.root.addResource('slack').addResource('events').addMethod('POST', slackIntegration);

    // Slack Responder Lambda (receives agent outputs, posts to Slack)
    const slackResponderFunction = new lambda.Function(this, 'SlackResponderFunction', {
      functionName: 'agentzero-slack-responder',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/slack-responder')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        SLACK_BOT_TOKEN: process.env.SLACK_BOT_TOKEN || '',
      },
    });

    // Add output queue as event source for Slack Responder
    slackResponderFunction.addEventSource(
      new SqsEventSource(outputQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Outputs
    new cdk.CfnOutput(this, 'QueueUrl', {
      value: messageQueue.queueUrl,
      description: 'SQS Queue URL',
    });

    new cdk.CfnOutput(this, 'QueueArn', {
      value: messageQueue.queueArn,
      description: 'SQS Queue ARN',
    });

    new cdk.CfnOutput(this, 'DynamoDBTableName', {
      value: historyTable.tableName,
      description: 'DynamoDB Table Name',
    });

    new cdk.CfnOutput(this, 'LambdaFunctionArn', {
      value: lambdaFunction.functionArn,
      description: 'Lambda Function ARN',
    });

    new cdk.CfnOutput(this, 'OutputQueueUrl', {
      value: outputQueue.queueUrl,
      description: 'Agent Output Queue URL',
    });

    new cdk.CfnOutput(this, 'SlackWebhookUrl', {
      value: api.url + 'slack/events',
      description: 'Slack Event Subscription URL',
    });

    new cdk.CfnOutput(this, 'SlackReceiverFunctionArn', {
      value: slackReceiverFunction.functionArn,
      description: 'Slack Receiver Lambda ARN',
    });

    new cdk.CfnOutput(this, 'SlackResponderFunctionArn', {
      value: slackResponderFunction.functionArn,
      description: 'Slack Responder Lambda ARN',
    });

    // Get Time Tool Queue
    const getTimeToolQueue = new sqs.Queue(this, 'GetTimeToolQueue', {
      queueName: 'agentzero-get-time-tool',
      visibilityTimeout: cdk.Duration.seconds(30),
      retentionPeriod: cdk.Duration.days(4),
    });

    // Get Time Tool Lambda
    const getTimeToolFunction = new lambda.Function(this, 'GetTimeToolFunction', {
      functionName: 'agentzero-get-time-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/get-time-tool')),
      timeout: cdk.Duration.seconds(30),
    });

    // Grant Get Time Tool Lambda permission to send to input queue
    messageQueue.grantSendMessages(getTimeToolFunction);

    // Add queue as event source for Get Time Tool Lambda
    getTimeToolFunction.addEventSource(
      new SqsEventSource(getTimeToolQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Update main Lambda environment with Get Time Tool queue URL
    lambdaFunction.addEnvironment('GET_TIME_TOOL_QUEUE_URL', getTimeToolQueue.queueUrl);

    // Grant main Lambda permission to send to Get Time Tool queue
    getTimeToolQueue.grantSendMessages(lambdaFunction);

    new cdk.CfnOutput(this, 'GetTimeToolQueueUrl', {
      value: getTimeToolQueue.queueUrl,
      description: 'Get Time Tool Queue URL',
    });

    // Create EC2 Tool Queue
    const createEc2ToolQueue = new sqs.Queue(this, 'CreateEc2ToolQueue', {
      queueName: 'agentzero-create-ec2-tool',
      visibilityTimeout: cdk.Duration.seconds(60),
      retentionPeriod: cdk.Duration.days(4),
    });

    // Create EC2 Tool Lambda
    const createEc2ToolFunction = new lambda.Function(this, 'CreateEc2ToolFunction', {
      functionName: 'agentzero-create-ec2-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/create-ec2-tool')),
      timeout: cdk.Duration.seconds(60),
    });

    // Grant Create EC2 Tool Lambda permissions to send to input queue
    messageQueue.grantSendMessages(createEc2ToolFunction);
    
    createEc2ToolFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: [
          'ec2:RunInstances',
          'ec2:CreateTags',
          'ec2:DescribeInstances',
        ],
        resources: ['*'],
      })
    );



    // Add queue as event source for Create EC2 Tool Lambda
    createEc2ToolFunction.addEventSource(
      new SqsEventSource(createEc2ToolQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Update main Lambda environment with Create EC2 Tool queue URL
    lambdaFunction.addEnvironment('CREATE_EC2_TOOL_QUEUE_URL', createEc2ToolQueue.queueUrl);

    // Grant main Lambda permission to send to Create EC2 Tool queue
    createEc2ToolQueue.grantSendMessages(lambdaFunction);

    new cdk.CfnOutput(this, 'CreateEc2ToolQueueUrl', {
      value: createEc2ToolQueue.queueUrl,
      description: 'Create EC2 Tool Queue URL',
    });

    // EC2 State Monitor Lambda - processes EventBridge EC2 state change events
    const ec2StateMonitorFunction = new lambda.Function(this, 'Ec2StateMonitorFunction', {
      functionName: 'agentzero-ec2-state-monitor',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/ec2-state-monitor')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        INPUT_QUEUE_URL: messageQueue.queueUrl,
      },
    });

    // Grant EC2 State Monitor permission to read EC2 tags and send to input queue
    messageQueue.grantSendMessages(ec2StateMonitorFunction);
    ec2StateMonitorFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['ec2:DescribeTags', 'ec2:DescribeInstances'],
        resources: ['*'],
      })
    );

    // EventBridge Rule for EC2 state changes to "running" (for AgentZero-created instances)
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

    // Add Lambda as target for the EventBridge rule
    ec2StateRule.addTarget(new targets.LambdaFunction(ec2StateMonitorFunction));

    new cdk.CfnOutput(this, 'Ec2StateMonitorFunctionArn', {
      value: ec2StateMonitorFunction.functionArn,
      description: 'EC2 State Monitor Lambda ARN',
    });

    // DynamoDB table for GitHub Actions check mappings
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

    // GitHub Actions Check Tool Queue
    const checkGithubActionsToolQueue = new sqs.Queue(this, 'CheckGithubActionsToolQueue', {
      queueName: 'agentzero-check-github-actions-tool',
      visibilityTimeout: cdk.Duration.seconds(30),
      retentionPeriod: cdk.Duration.days(4),
    });

    // GitHub Actions Check Tool Lambda
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

    // Grant GitHub Actions Check Tool Lambda permissions
    githubChecksTable.grantWriteData(checkGithubActionsToolFunction);
    messageQueue.grantSendMessages(checkGithubActionsToolFunction);

    // Add queue as event source for GitHub Actions Check Tool Lambda
    checkGithubActionsToolFunction.addEventSource(
      new SqsEventSource(checkGithubActionsToolQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Update main Lambda environment with GitHub Actions Check Tool queue URL
    lambdaFunction.addEnvironment('CHECK_GITHUB_ACTIONS_TOOL_QUEUE_URL', checkGithubActionsToolQueue.queueUrl);

    // Grant main Lambda permission to send to GitHub Actions Check Tool queue
    checkGithubActionsToolQueue.grantSendMessages(lambdaFunction);

    new cdk.CfnOutput(this, 'CheckGithubActionsToolQueueUrl', {
      value: checkGithubActionsToolQueue.queueUrl,
      description: 'GitHub Actions Check Tool Queue URL',
    });

    // GitHub Webhook Receiver Lambda
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

    // Grant GitHub Webhook Receiver permissions
    githubChecksTable.grantReadWriteData(githubWebhookReceiverFunction);
    messageQueue.grantSendMessages(githubWebhookReceiverFunction);

    // API Gateway endpoint for GitHub webhooks
    const githubWebhookIntegration = new apigateway.LambdaIntegration(githubWebhookReceiverFunction);
    api.root.addResource('github').addResource('webhook').addMethod('POST', githubWebhookIntegration);

    new cdk.CfnOutput(this, 'GithubWebhookUrl', {
      value: api.url + 'github/webhook',
      description: 'GitHub Webhook URL',
    });

    new cdk.CfnOutput(this, 'GithubWebhookReceiverFunctionArn', {
      value: githubWebhookReceiverFunction.functionArn,
      description: 'GitHub Webhook Receiver Lambda ARN',
    });

    new cdk.CfnOutput(this, 'GitHubChecksTableName', {
      value: githubChecksTable.tableName,
      description: 'GitHub Checks DynamoDB Table Name',
    });

    // GitHub MCP Server
    const mcpGithubQueue = new sqs.Queue(this, 'McpGithubQueue', {
      queueName: 'agentzero-mcp-github',
      visibilityTimeout: cdk.Duration.seconds(60),
      retentionPeriod: cdk.Duration.days(4),
    });

    const mcpGithubEnv: Record<string, string> = {};
    if (process.env.GITHUB_PERSONAL_ACCESS_TOKEN) {
      mcpGithubEnv.GITHUB_PERSONAL_ACCESS_TOKEN = process.env.GITHUB_PERSONAL_ACCESS_TOKEN;
    }

    const mcpGithubFunction = new lambda.Function(this, 'McpGithubFunction', {
      functionName: 'agentzero-mcp-github',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../lambda/mcp-server-proxy')),
      timeout: cdk.Duration.seconds(60),
      memorySize: 512,
      environment: {
        MCP_SERVER_COMMAND: 'npx',
        MCP_SERVER_ARGS: JSON.stringify(['-y', '@modelcontextprotocol/server-github']),
        MCP_SERVER_ENV: JSON.stringify(mcpGithubEnv),
      },
    });

    messageQueue.grantSendMessages(mcpGithubFunction);

    mcpGithubFunction.addEventSource(
      new SqsEventSource(mcpGithubQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    lambdaFunction.addEnvironment('MCP_GITHUB_QUEUE_URL', mcpGithubQueue.queueUrl);
    mcpGithubQueue.grantSendMessages(lambdaFunction);

    new cdk.CfnOutput(this, 'McpGithubQueueUrl', {
      value: mcpGithubQueue.queueUrl,
      description: 'GitHub MCP Server Queue URL',
    });
  }
}
