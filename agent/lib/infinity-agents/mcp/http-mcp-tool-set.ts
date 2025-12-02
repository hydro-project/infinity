import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as path from 'path';
import { InfinityAgent } from '..';
import { MCPToolSet } from './mcp-tool-set';

export interface HTTPMCPToolSetProps {
  /**
   * Name of the MCP server (e.g., 'github', 'slack')
   */
  readonly name: string;

  /**
   * URL of the HTTP MCP server endpoint
   */
  readonly url: string;

  /**
   * Optional: HTTP headers to include with requests (e.g., for authentication)
   */
  readonly headers?: Record<string, string>;

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
 * An MCP server that connects to an HTTP endpoint using Streamable HTTP transport
 */
export class HTTPMCPToolSet extends MCPToolSet {
  public readonly handler: lambda.Function;

  constructor(agent: InfinityAgent, id: string, props: HTTPMCPToolSetProps) {
    // Create the MCP proxy Lambda function
    const handler = new lambda.Function(agent, `${id}Handler`, {
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'mcp-server-proxy')),
      timeout: cdk.Duration.seconds(60),
      memorySize: 256, // HTTP client needs less memory than stdio
      environment: {
        MCP_SERVER_URL: props.url,
        MCP_SERVER_HEADERS: JSON.stringify(props.headers || {}),
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
