import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import { SqsEventSource } from 'aws-cdk-lib/aws-lambda-event-sources';
import { ToolSet, ToolSetConfig } from '../tools/tool-set';
import { InfinityAgent } from '..';

export interface MCPToolSetProps {
  /**
   * Name of the MCP server (e.g., 'github', 'slack')
   */
  readonly name: string;

  /**
   * Lambda function that proxies to the MCP server
   */
  readonly handler: lambda.IFunction;

  /**
   * Optional: custom queue configuration
   */
  readonly queueProps?: Partial<sqs.QueueProps>;
}

/**
 * An MCP server that automatically creates list_tools and invoke_tool methods
 */
export class MCPToolSet extends ToolSet {
  public readonly queue: sqs.Queue;
  private readonly name: string;

  constructor(agent: InfinityAgent, id: string, props: MCPToolSetProps) {
    super(agent, id);
    this.name = props.name;

    // Create SQS queue for this MCP server
    this.queue = new sqs.Queue(this, 'Queue', {
      queueName: `infinity-agents-mcp-${props.name}`,
      visibilityTimeout: cdk.Duration.seconds(60),
      retentionPeriod: cdk.Duration.days(4),
      ...props.queueProps,
    });

    // Add queue as event source for the handler
    props.handler.addEventSource(
      new SqsEventSource(this.queue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    // Grant the agent permission to send to this queue
    agent.grantQueuePermissions(this.queue);

    // Grant the handler permission to send to the agent's input queue
    agent.inputQueue.grantSendMessages(props.handler);

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
