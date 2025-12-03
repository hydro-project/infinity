import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as path from 'path';
import { InfinityAgent } from '..';
import { MCPToolSet } from './mcp-tool-set';

export interface LambdaMCPToolSetProps {
  /**
   * Name of the MCP server (e.g., 'github', 'slack')
   */
  readonly name: string;

  /**
   * Command to run the MCP server (e.g., ['npx', '-y', '@modelcontextprotocol/server-github'])
   */
  readonly command: string[];

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
export class LambdaMCPToolSet extends MCPToolSet {
  public readonly handler: lambda.Function;

  constructor(agent: InfinityAgent, id: string, props: LambdaMCPToolSetProps) {
    // Create the MCP proxy Lambda function first
    const handler = new lambda.Function(agent, `${id}Handler`, {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'mcp-server-proxy')),
      timeout: cdk.Duration.seconds(60),
      memorySize: 512,
      environment: {
        MCP_SERVER_COMMAND: JSON.stringify(props.command),
        MCP_SERVER_ENV: JSON.stringify(props.env || {}),
      },
      ...props.lambdaProps,
    });

    super(agent, id, {
      name: props.name,
      handler,
      queueProps: props.queueProps,
    });

    this.handler = handler;
  }
}
