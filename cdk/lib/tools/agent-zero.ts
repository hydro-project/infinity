import { Construct } from 'constructs';
import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as iam from 'aws-cdk-lib/aws-iam';
import { SqsEventSource } from 'aws-cdk-lib/aws-lambda-event-sources';
import { ToolSetConfig } from './tool-set';
import * as path from 'path';

export interface AgentZeroProps {
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
 * The main AgentZero construct that manages the leader Lambda and tools
 */
export class AgentZero extends Construct {
  public readonly lambdaFunction: lambda.Function;
  public readonly inputQueue: sqs.Queue;
  public readonly outputQueue: sqs.Queue;
  public readonly historyTable: dynamodb.Table;
  private readonly schedulerRole: iam.Role;
  private readonly toolSetConfigs: ToolSetConfig[] = [];

  constructor(scope: Construct, id: string, props: AgentZeroProps = {}) {
    super(scope, id);

    // DynamoDB table for conversation history
    this.historyTable = new dynamodb.Table(this, 'StateTable', {
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
    const deadLetterQueue = new sqs.Queue(this, 'DeadLetterQueue', {
      queueName: 'agentzero-leader-dlq',
      retentionPeriod: cdk.Duration.days(14),
    });

    // SQS Standard Queue for incoming messages (agent input)
    this.inputQueue = new sqs.Queue(this, 'InputQueue', {
      queueName: 'agentzero-leader',
      visibilityTimeout: cdk.Duration.minutes(15),
      retentionPeriod: cdk.Duration.days(4),
      deadLetterQueue: {
        queue: deadLetterQueue,
        maxReceiveCount: 3,
      },
    });

    // Dead Letter Queue for output messages
    const outputDeadLetterQueue = new sqs.Queue(this, 'OutputDeadLetterQueue', {
      queueName: 'agentzero-output-dlq',
      retentionPeriod: cdk.Duration.days(14),
    });

    // SQS Standard Queue for agent outputs
    this.outputQueue = new sqs.Queue(this, 'OutputQueue', {
      queueName: 'agentzero-output',
      visibilityTimeout: cdk.Duration.minutes(5),
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

    // Create the leader Lambda function
    this.lambdaFunction = new lambda.Function(this, 'LeaderFunction', {
      functionName: 'agentzero-leader',
      runtime: lambda.Runtime.PROVIDED_AL2023,
      handler: 'bootstrap',
      architecture: lambda.Architecture.ARM_64,
      code: lambda.Code.fromAsset(
        props.codePath || path.join(__dirname, '../../../target/lambda/agentzero-leader')
      ),
      timeout: cdk.Duration.minutes(15),
      memorySize: 512,
      reservedConcurrentExecutions: 1,
      environment: {
        DYNAMODB_TABLE_NAME: this.historyTable.tableName,
        OUTPUT_QUEUE_URL: this.outputQueue.queueUrl,
        INPUT_QUEUE_URL: this.inputQueue.queueUrl,
        INPUT_QUEUE_ARN: this.inputQueue.queueArn,
        SCHEDULER_ROLE_ARN: this.schedulerRole.roleArn,
        RUST_BACKTRACE: '1',
      },
      ...props.lambdaProps,
    });

    // Grant permissions
    this.historyTable.grantReadWriteData(this.lambdaFunction);
    this.outputQueue.grantSendMessages(this.lambdaFunction);
    this.inputQueue.grantSendMessages(this.lambdaFunction);

    // Grant Bedrock permissions
    this.lambdaFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['bedrock:InvokeModel', 'bedrock:InvokeModelWithResponseStream'],
        resources: ['*'],
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
   * Automatically updates the TOOLS_CONFIG environment variable
   */
  registerToolSet(config: ToolSetConfig): void {
    this.toolSetConfigs.push(config);

    // Update the environment variable with the current config
    const toolsConfig = {
      tool_sets: this.toolSetConfigs,
    };

    this.lambdaFunction.addEnvironment('TOOLS_CONFIG', JSON.stringify(toolsConfig));
  }
}
