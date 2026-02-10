import { Construct } from 'constructs';
import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as dsql from 'aws-cdk-lib/aws-dsql';
import * as cr from 'aws-cdk-lib/custom-resources';
import { RustFunction } from 'cargo-lambda-cdk';

import { SqsEventSource } from 'aws-cdk-lib/aws-lambda-event-sources';
import { ToolSetConfig } from './tools/tool-set';
import * as path from 'path';

export interface InfinityAgentsProps {
  /**
   * Path to the Lambda code
   */
  readonly codePath?: string;

  /**
   * Lambda function configuration
   */
  readonly lambdaProps?: Partial<lambda.FunctionProps>;
}

/**
 * The main InfinityAgent construct that manages the leader Lambda and tools
 */
export class InfinityAgent extends Construct {
  public readonly lambdaFunction: RustFunction;
  public readonly inputQueue: sqs.Queue;
  public readonly outputQueue: sqs.Queue;
  public readonly delayQueue: sqs.Queue;
  public readonly historyTable: dynamodb.Table;
  public readonly dsqlCluster: dsql.CfnCluster;
  private readonly schedulerRole: iam.Role;
  private readonly toolSetConfigs: ToolSetConfig[] = [];
  private toolsConfigResource?: cr.AwsCustomResource;

  constructor(scope: Construct, id: string, props: InfinityAgentsProps = {}) {
    super(scope, id);

    // DynamoDB table for metadata and processed IDs (no longer stores conversation history)
    this.historyTable = new dynamodb.Table(this, 'StateTable', {
      tableName: 'InfinityAgentsState',
      partitionKey: {
        name: 'session',
        type: dynamodb.AttributeType.STRING,
      },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
      pointInTimeRecovery: true,
    });

    // Aurora DSQL cluster for conversation history
    this.dsqlCluster = new dsql.CfnCluster(this, 'ConversationHistoryCluster', {
      deletionProtectionEnabled: true,
    });

    // Dead Letter Queue for failed messages
    const deadLetterQueue = new sqs.Queue(this, 'DeadLetterQueue', {
      retentionPeriod: cdk.Duration.days(14),
      fifo: true,
    });

    // SQS FIFO Queue for incoming messages (agent input)
    // MessageGroupId = thread ID, so messages for different threads process concurrently
    this.inputQueue = new sqs.Queue(this, 'InputQueue', {
      fifo: true,
      contentBasedDeduplication: false,
      retentionPeriod: cdk.Duration.days(4),
      visibilityTimeout: cdk.Duration.minutes(15),
      deadLetterQueue: {
        queue: deadLetterQueue,
        maxReceiveCount: 3,
      },
    });

    // Dead Letter Queue for output messages
    const outputDeadLetterQueue = new sqs.Queue(this, 'OutputDeadLetterQueue', {
      retentionPeriod: cdk.Duration.days(14),
    });

    // SQS Standard Queue for agent outputs
    this.outputQueue = new sqs.Queue(this, 'OutputQueue', {
      retentionPeriod: cdk.Duration.days(4),
      deadLetterQueue: {
        queue: outputDeadLetterQueue,
        maxReceiveCount: 3,
      },
    });

    // IAM Role for EventBridge Scheduler to send messages to SQS
    this.schedulerRole = new iam.Role(this, 'SchedulerRole', {
      assumedBy: new iam.ServicePrincipal('scheduler.amazonaws.com'),
    });
    this.inputQueue.grantSendMessages(this.schedulerRole);

    // Standard SQS queue for short delays (supports per-message DelaySeconds up to 900s).
    // A relay Lambda forwards messages to the FIFO input queue after the delay expires.
    this.delayQueue = new sqs.Queue(this, 'DelayQueue', {
      retentionPeriod: cdk.Duration.days(4),
      visibilityTimeout: cdk.Duration.seconds(30),
    });

    const delayRelayFunction = new lambda.Function(this, 'DelayRelayFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'delay-relay')),
      timeout: cdk.Duration.seconds(30),
      memorySize: 128,
      environment: {
        INPUT_QUEUE_URL: this.inputQueue.queueUrl,
      },
    });

    this.inputQueue.grantSendMessages(delayRelayFunction);

    delayRelayFunction.addEventSource(
      new SqsEventSource(this.delayQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Create the leader Lambda function using cargo-lambda-cdk
    this.lambdaFunction = new RustFunction(this, 'LeaderFunction', {
      manifestPath: props.codePath || path.join(__dirname, '../../..'),
      binaryName: 'infinity-agents-leader',
      architecture: lambda.Architecture.ARM_64,
      timeout: cdk.Duration.minutes(15),
      memorySize: 128,
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        DYNAMODB_TABLE_NAME: this.historyTable.tableName,
        OUTPUT_QUEUE_URL: this.outputQueue.queueUrl,
        INPUT_QUEUE_URL: this.inputQueue.queueUrl,
        INPUT_QUEUE_ARN: this.inputQueue.queueArn,
        SCHEDULER_ROLE_ARN: this.schedulerRole.roleArn,
        DELAY_QUEUE_URL: this.delayQueue.queueUrl,
        DSQL_CLUSTER_ENDPOINT: this.dsqlCluster.attrEndpoint,
        RUST_BACKTRACE: '1',
      },
    });

    // Grant permissions
    this.historyTable.grantReadWriteData(this.lambdaFunction);
    this.outputQueue.grantSendMessages(this.lambdaFunction);
    this.inputQueue.grantSendMessages(this.lambdaFunction);
    this.delayQueue.grantSendMessages(this.lambdaFunction);

    // Grant Bedrock permissions
    this.lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['bedrock:InvokeModel', 'bedrock:InvokeModelWithResponseStream'],
        resources: ['*'],
      })
    );

    // Grant DSQL permissions
    this.lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: [
          'dsql:DbConnectAdmin',
          'dsql:GenerateDbConnectAuthToken',
        ],
        resources: [this.dsqlCluster.attrResourceArn],
      })
    );

    // Grant EventBridge Scheduler permissions
    this.lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['scheduler:CreateSchedule', 'scheduler:DeleteSchedule'],
        resources: ['*'],
      })
    );

    // Grant permission to pass the scheduler role
    this.lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['iam:PassRole'],
        resources: [this.schedulerRole.roleArn],
      })
    );

    // Add SQS as event source
    this.lambdaFunction.addEventSource(
      new SqsEventSource(this.inputQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );
  }

  /**
   * Grant a queue permission to send messages to the agent's input queue
   * and grant the agent permission to send messages to the queue
   */
  grantQueuePermissions(queue: sqs.IQueue): void {
    queue.grantSendMessages(this.lambdaFunction);
  }

  /**
   * Register a tool set configuration (called by tool sets during construction)
   * Stores config in a DynamoDB item via a custom resource at deploy time.
   * No size limits beyond DynamoDB's 400KB item limit.
   */
  registerToolSet(config: ToolSetConfig): void {
    this.toolSetConfigs.push(config);

    const toolsConfig = {
      tool_sets: this.toolSetConfigs,
    };

    const configKey = `__tools_config__`;

    // Remove previous custom resource if it exists, so we can recreate with updated config
    if (this.toolsConfigResource) {
      this.node.tryRemoveChild('ToolsConfigWriter');
    }

    this.toolsConfigResource = new cr.AwsCustomResource(this, 'ToolsConfigWriter', {
      onCreate: {
        service: 'DynamoDB',
        action: 'putItem',
        parameters: {
          TableName: this.historyTable.tableName,
          Item: {
            session: { S: configKey },
            config: { S: JSON.stringify(toolsConfig) },
          },
        },
        physicalResourceId: cr.PhysicalResourceId.of('tools-config'),
      },
      onUpdate: {
        service: 'DynamoDB',
        action: 'putItem',
        parameters: {
          TableName: this.historyTable.tableName,
          Item: {
            session: { S: configKey },
            config: { S: JSON.stringify(toolsConfig) },
          },
        },
        physicalResourceId: cr.PhysicalResourceId.of('tools-config'),
      },
      policy: cr.AwsCustomResourcePolicy.fromSdkCalls({
        resources: [this.historyTable.tableArn],
      }),
    });

    this.lambdaFunction.addEnvironment('TOOLS_CONFIG_DDB_KEY', configKey);
  }
}
