import * as lambda from 'aws-cdk-lib/aws-lambda';
import { Tool, ToolConfig } from './tool';
import { InfinityAgent } from '..';

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
}

// Track Function URLs per handler so multiple LambdaTools sharing a handler
// don't call addFunctionUrl twice (Lambda only allows one Function URL).
const handlerFunctionUrls = new Map<lambda.IFunction, lambda.FunctionUrl>();

/**
 * A tool that the leader invokes via HTTP (Lambda Function URL with IAM auth).
 * The tool Lambda uses response streaming to return OK immediately,
 * then processes the request asynchronously and sends results via RAP.
 */
export class LambdaTool extends Tool {
  public readonly functionUrl: lambda.FunctionUrl;
  private readonly name: string;
  private readonly description: string;
  private readonly parameters: any;

  constructor(agent: InfinityAgent, id: string, props: LambdaToolProps) {
    super(agent, id);
    this.name = props.name;
    this.description = props.description;
    this.parameters = props.parameters;

    // Reuse existing Function URL if this handler already has one
    let fnUrl = handlerFunctionUrls.get(props.handler);
    if (!fnUrl) {
      fnUrl = props.handler.addFunctionUrl({
        authType: lambda.FunctionUrlAuthType.AWS_IAM,
        invokeMode: lambda.InvokeMode.RESPONSE_STREAM,
      });
      handlerFunctionUrls.set(props.handler, fnUrl);

      // Only grant permissions once per handler
      agent.grantToolInvokeAccess(props.handler);
      agent.grantRapAccess(props.handler);
    }

    this.functionUrl = fnUrl;
  }

  toConfig(): ToolConfig {
    return {
      type: 'lambda',
      name: this.name,
      description: this.description,
      parameters: this.parameters,
      function_url: this.functionUrl.url,
    };
  }
}
