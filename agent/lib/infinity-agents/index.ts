import { Construct } from 'constructs';
import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as ssm from 'aws-cdk-lib/aws-ssm';
import * as dsql from 'aws-cdk-lib/aws-dsql';

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
  public readonly lambdaFunction: lambda.Function;
  public readonly inputQueue: sqs.Queue;
  public readonly outputQueue: sqs.Queue;
  public readonly historyTable: dynamodb.Table;
  public readonly dsqlCluster: dsql.CfnCluster;
  private readonly schedulerRole: iam.Role;
  private readonly toolSetConfigs: ToolSetConfig[] = [];
  private toolsConfigParam?: ssm.StringParameter;

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
    });

    // SQS Standard Queue for incoming messages (agent input)
    this.inputQueue = new sqs.Queue(this, 'InputQueue', {
      visibilityTimeout: cdk.Duration.minutes(15),
      retentionPeriod: cdk.Duration.days(4),
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
      runtime: lambda.Runtime.PROVIDED_AL2023,
      handler: 'bootstrap',
      architecture: lambda.Architecture.ARM_64,
      code: lambda.Code.fromAsset(
        props.codePath || path.join(__dirname, '../../../target/lambda/infinity-agents-leader')
      ),
      timeout: cdk.Duration.minutes(15),
      memorySize: 128,
      reservedConcurrentExecutions: 1,
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        DYNAMODB_TABLE_NAME: this.historyTable.tableName,
        OUTPUT_QUEUE_URL: this.outputQueue.queueUrl,
        INPUT_QUEUE_URL: this.inputQueue.queueUrl,
        INPUT_QUEUE_ARN: this.inputQueue.queueArn,
        SCHEDULER_ROLE_ARN: this.schedulerRole.roleArn,
        DSQL_CLUSTER_ENDPOINT: this.dsqlCluster.attrEndpoint,
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
   * Stores config in SSM Parameter Store to avoid Lambda env var size limits
   */
  registerToolSet(config: ToolSetConfig): void {
    this.toolSetConfigs.push(config);

    const toolsConfig = {
      tool_sets: this.toolSetConfigs,
    };

    // Create or update SSM parameter with tools config
    // We use a single parameter that gets updated as tool sets are registered
    if (!this.toolsConfigParam) {
      this.toolsConfigParam = new ssm.StringParameter(this, 'ToolsConfigParam', {
        parameterName: `/infinity-agents/${this.node.id}/tools-config`,
        stringValue: JSON.stringify(toolsConfig),
        description: 'Tools configuration for InfinityAgent',
      });
      
      // Grant read access to the Lambda
      this.toolsConfigParam.grantRead(this.lambdaFunction);
      
      // Set env var with parameter name
      this.lambdaFunction.addEnvironment('TOOLS_CONFIG_SSM_PARAM', this.toolsConfigParam.parameterName);
    } else {
      // Update the parameter value using escape hatch since CDK doesn't support updating
      const cfnParam = this.toolsConfigParam.node.defaultChild as ssm.CfnParameter;
      cfnParam.value = JSON.stringify(toolsConfig);
    }
  }
}
