import * as lambda from 'aws-cdk-lib/aws-lambda';
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
}

/**
 * An MCP server that automatically creates list_tools and invoke_tool methods.
 * The leader invokes the MCP proxy Lambda via HTTP (Function URL with IAM auth).
 * The Lambda uses response streaming to return OK immediately, then processes async.
 */
export class MCPToolSet extends ToolSet {
  public readonly functionUrl: lambda.FunctionUrl;
  private readonly name: string;

  constructor(agent: InfinityAgent, id: string, props: MCPToolSetProps) {
    super(agent, id);
    this.name = props.name;

    // Expose the MCP proxy handler via a Function URL with IAM auth (SigV4)
    this.functionUrl = props.handler.addFunctionUrl({
      authType: lambda.FunctionUrlAuthType.AWS_IAM,
      invokeMode: lambda.InvokeMode.RESPONSE_STREAM,
    });

    // Grant the leader permission to invoke this tool's Function URL
    agent.grantToolInvokeAccess(props.handler);

    // Grant the handler permission to invoke the RAP receiver (SigV4)
    agent.grantRapAccess(props.handler);

    // Register this tool set with the agent
    agent.registerToolSet(this.toConfig());
  }

  toConfig(): ToolSetConfig {
    return {
      type: 'mcp',
      name: this.name,
      function_url: this.functionUrl.url,
    };
  }
}
