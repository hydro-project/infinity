import { Construct } from 'constructs';
import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import { SqsEventSource } from 'aws-cdk-lib/aws-lambda-event-sources';
import { Tool, ToolConfig } from './tool';
import { AgentZero } from './agent-zero';

export interface LambdaToolProps {
  /**
   * Tool name
   */
  readonly name: string;

  /**
   * Tool description
   */
  readonly description: string;

  /**
   * JSON Schema for tool parameters
   */
  readonly parameters: any;

  /**
   * Lambda function that implements this tool
   */
  readonly handler: lambda.IFunction;

  /**
   * Optional: custom queue configuration
   */
  readonly queueProps?: Partial<sqs.QueueProps>;
}

/**
 * A tool that forwards requests to a Lambda function via SQS
 */
export class LambdaTool extends Tool {
  public readonly queue: sqs.Queue;
  private readonly name: string;
  private readonly description: string;
  private readonly parameters: any;

  constructor(agent: AgentZero, id: string, props: LambdaToolProps) {
    super(agent, id);
    this.name = props.name;
    this.description = props.description;
    this.parameters = props.parameters;

    // Create SQS queue for this tool
    this.queue = new sqs.Queue(this, 'Queue', {
      queueName: `agentzero-${props.name.replace(/_/g, '-')}`,
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
  }

  toConfig(): ToolConfig {
    return {
      type: 'lambda',
      name: this.name,
      description: this.description,
      parameters: this.parameters,
      queue_url: this.queue.queueUrl,
    };
  }
}
