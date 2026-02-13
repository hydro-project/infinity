import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as path from 'path';
import { InfinityAgent } from '..';
import { RapToolSet } from '../tools';

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
   * Optional: custom Lambda configuration
   */
  readonly lambdaProps?: Partial<lambda.FunctionProps>;
}

/**
 * An MCP server that runs as a stdio subprocess inside a Lambda proxy.
 * Tool definitions are served via /.well-known/rap-toolset.
 */
export class LambdaMCPToolSet extends RapToolSet {
  public readonly handler: lambda.Function;

  constructor(agent: InfinityAgent, id: string, props: LambdaMCPToolSetProps) {
    const handler = new lambda.Function(agent, `${id}Handler`, {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'mcp-server-proxy')),
      timeout: cdk.Duration.seconds(60),
      memorySize: 512,
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        MCP_SERVER_COMMAND: JSON.stringify(props.command),
        MCP_SERVER_ENV: JSON.stringify(props.env || {}),
        MCP_SERVER_NAME: props.name,
      },
      ...props.lambdaProps,
    });

    super(agent, id, {
      handler,
    });

    this.handler = handler;
  }
}
