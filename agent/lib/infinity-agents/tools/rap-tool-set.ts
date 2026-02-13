import * as lambda from 'aws-cdk-lib/aws-lambda';
import { ToolSet, ToolSetConfig } from './tool-set';
import { InfinityAgent } from '..';

export interface RapToolSetProps {
  /**
   * Base URL of the RAP tool server.
   * The runtime will fetch the toolset definition from `{serverUrl}/.well-known/rap-toolset`.
   */
  readonly serverUrl: string;

  /**
   * Lambda function that implements the tool server (optional).
   * If provided, the construct will create a Function URL and use it as the server URL,
   * and grant the leader Lambda permission to invoke it.
   */
  readonly handler?: lambda.IFunction;
}

/**
 * A RAP toolset that loads tool definitions from a `.well-known/rap-toolset` endpoint.
 * The leader Lambda fetches and caches the toolset definition at runtime.
 */
export class RapToolSet extends ToolSet {
  public readonly serverUrl: string;

  constructor(agent: InfinityAgent, id: string, props: RapToolSetProps) {
    super(agent, id);

    if (props.handler) {
      // Create a Function URL for the handler and use it as the server URL
      const fnUrl = props.handler.addFunctionUrl({
        authType: lambda.FunctionUrlAuthType.AWS_IAM,
        invokeMode: lambda.InvokeMode.RESPONSE_STREAM,
      });
      this.serverUrl = fnUrl.url;

      // Grant the leader permission to invoke this tool's Function URL
      agent.grantToolInvokeAccess(props.handler);

      // Grant the handler permission to invoke the RAP receiver (SigV4)
      agent.grantRapAccess(props.handler);
    } else {
      this.serverUrl = props.serverUrl;
    }

    // Register this tool set with the agent
    agent.registerToolSet(this.toConfig());
  }

  toConfig(): ToolSetConfig {
    return {
      type: 'toolset_server',
      server_url: this.serverUrl,
    };
  }
}
