import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import { SqsEventSource } from 'aws-cdk-lib/aws-lambda-event-sources';
import { ToolSet, ToolSetConfig } from './tool-set';
import { InfinityAgents } from './infinity-agents';
import * as path from 'path';

export interface LambdaMCPToolSetProps {
  /**
   * Name of the MCP server (e.g., 'github', 'slack')
   */
  readonly name: string;

  /**
   * Command to run the MCP server (e.g., 'npx', 'uvx')
   */
  readonly command: string;

  /**
   * Arguments for the MCP server command
   */
  readonly args: string[];

  /**
   * Environment variables for the MCP server
   */
  readonly env?: Record<string, string | undefined>;

  /**
   * Optional: custom queue configuration
   */
  readonly queueProps?: Partial<sqs.QueueProps>;

  /**
   * Optional: custom Lambda configuration
   */
  readonly lambdaProps?: Partial<lambda.FunctionProps>;
}

/**
 * An MCP server that automatically creates the Lambda proxy, queue, and tool set configuration
 */
export class LambdaMCPToolSet extends ToolSet {
  public readonly queue: sqs.Queue;
  public readonly handler: lambda.Function;
  private readonly name: string;

  constructor(agent: InfinityAgents, id: string, props: LambdaMCPToolSetProps) {
    super(agent, id);
    this.name = props.name;

    // Create the MCP proxy Lambda function
    this.handler = new lambda.Function(this, 'Handler', {
      functionName: `infinity-agents-mcp-${props.name}`,
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, '../../../lambda/mcp-server-proxy')),
      timeout: cdk.Duration.seconds(60),
      memorySize: 512,
      environment: {
        MCP_SERVER_COMMAND: props.command,
        MCP_SERVER_ARGS: JSON.stringify(props.args),
        MCP_SERVER_ENV: JSON.stringify(props.env || {}),
      },
      ...props.lambdaProps,
    });

    // Create SQS queue for this MCP server
    this.queue = new sqs.Queue(this, 'Queue', {
      queueName: `infinity-agents-mcp-${props.name}`,
      visibilityTimeout: cdk.Duration.seconds(60),
      retentionPeriod: cdk.Duration.days(4),
      ...props.queueProps,
    });

    // Add queue as event source for the handler
    this.handler.addEventSource(
      new SqsEventSource(this.queue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Grant the agent permission to send to this queue
    agent.grantQueuePermissions(this.queue);

    // Grant the handler permission to send to the agent's input queue
    agent.inputQueue.grantSendMessages(this.handler);

    // Register this tool set with the agent
    agent.registerToolSet(this.toConfig());
  }

  toConfig(): ToolSetConfig {
    return {
      type: 'mcp',
      name: this.name,
      queue_url: this.queue.queueUrl,
    };
  }
}
