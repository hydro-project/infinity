import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
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
      queueName: 'agentzero-leader-dlq.fifo',
      fifo: true,
      retentionPeriod: cdk.Duration.days(14),
    });

    // SQS FIFO Queue for incoming messages (agent input)
    const messageQueue = new sqs.Queue(this, 'AgentZeroMessageQueue', {
      queueName: 'agentzero-leader.fifo',
      fifo: true,
      contentBasedDeduplication: false,
      deduplicationScope: sqs.DeduplicationScope.MESSAGE_GROUP,
      fifoThroughputLimit: sqs.FifoThroughputLimit.PER_MESSAGE_GROUP_ID,
      visibilityTimeout: cdk.Duration.minutes(15),
      retentionPeriod: cdk.Duration.days(4),
      deadLetterQueue: {
        queue: deadLetterQueue,
        maxReceiveCount: 3,
      },
    });

    // Dead Letter Queue for output messages
    const outputDeadLetterQueue = new sqs.Queue(this, 'AgentZeroOutputDeadLetterQueue', {
      queueName: 'agentzero-output-dlq.fifo',
      fifo: true,
      retentionPeriod: cdk.Duration.days(14),
    });

    // SQS FIFO Queue for agent outputs
    const outputQueue = new sqs.Queue(this, 'AgentZeroOutputQueue', {
      queueName: 'agentzero-output.fifo',
      fifo: true,
      contentBasedDeduplication: false,
      deduplicationScope: sqs.DeduplicationScope.MESSAGE_GROUP,
      fifoThroughputLimit: sqs.FifoThroughputLimit.PER_MESSAGE_GROUP_ID,
      visibilityTimeout: cdk.Duration.minutes(5),
      retentionPeriod: cdk.Duration.days(4),
      deadLetterQueue: {
        queue: outputDeadLetterQueue,
        maxReceiveCount: 3,
      },
    });

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

    // Add SQS as event source for Lambda
    lambdaFunction.addEventSource(
      new SqsEventSource(messageQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Grant Lambda permission to send to output queue
    outputQueue.grantSendMessages(lambdaFunction);

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

    // IAM user for cargo lambda deployment (optional - comment out if not needed)
    // const deployUser = new iam.User(this, 'lambda-deploy-user');
    // const accessKey = new iam.AccessKey(this, 'lambda-deploy-access-key', {
    //   user: deployUser,
    // });

    // const deployPolicy = new iam.Policy(this, 'lambda-deploy-policy', {
    //   statements: [
    //     new iam.PolicyStatement({
    //       sid: 'EnableLambdaDeployPermissions',
    //       effect: iam.Effect.ALLOW,
    //       actions: [
    //         'lambda:GetFunction',
    //         'lambda:GetLayerVersion',
    //         'lambda:CreateFunction',
    //         'lambda:UpdateFunctionCode',
    //         'lambda:UpdateFunctionConfiguration',
    //         'lambda:PublishVersion',
    //         'lambda:TagResource',
    //       ],
    //       resources: [lambdaFunction.functionArn],
    //     }),
    //     new iam.PolicyStatement({
    //       sid: 'EnableIAMPassRole',
    //       effect: iam.Effect.ALLOW,
    //       actions: ['iam:PassRole'],
    //       resources: [lambdaFunction.role!.roleArn],
    //     }),
    //   ],
    // });
    // deployPolicy.attachToUser(deployUser);

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

    // new cdk.CfnOutput(this, 'DeployAccessKeyId', {
    //   value: accessKey.accessKeyId,
    //   description: 'Access Key ID for cargo lambda deployment',
    // });

    // new cdk.CfnOutput(this, 'DeploySecretAccessKey', {
    //   value: accessKey.secretAccessKey.unsafeUnwrap(),
    //   description: 'Secret Access Key for cargo lambda deployment',
    // });

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
  }
}
