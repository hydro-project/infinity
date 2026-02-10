import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
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
   * Optional: Enable OAuth support. When enabled, creates a DynamoDB table for token storage
   * and an API Gateway endpoint for OAuth callbacks.
   */
  readonly oauth?: {
    /**
     * API Gateway to add the OAuth callback endpoint to
     */
    callbackGateway: apigateway.RestApi;
    /**
     * Stage name (must match the gateway's deployOptions.stageName)
     */
    stageName: string;
    /**
     * Pre-configured OAuth client ID (for providers that don't support Dynamic Client Registration)
     */
    clientId?: string;
    /**
     * Pre-configured OAuth client secret
     */
    clientSecret?: string;
  };

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
  public readonly tokenTable?: dynamodb.Table;
  public readonly oauthCallbackUrl?: string;

  constructor(agent: InfinityAgent, id: string, props: HTTPMCPToolSetProps) {
    // Create token table if OAuth is enabled
    let tokenTable: dynamodb.Table | undefined;

    if (props.oauth) {
      tokenTable = new dynamodb.Table(agent, `${id}TokenTable`, {
        partitionKey: { name: 'user_id', type: dynamodb.AttributeType.STRING },
        billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
        removalPolicy: cdk.RemovalPolicy.RETAIN,
        timeToLiveAttribute: 'ttl',
      });
    }

    // Build the callback URL upfront (before creating Lambda) to avoid circular dependency
    let oauthCallbackUrl: string | undefined;
    if (props.oauth) {
      const stack = cdk.Stack.of(agent);
      oauthCallbackUrl = cdk.Fn.sub(
        'https://${RestApiId}.execute-api.${Region}.${UrlSuffix}/${StageName}/oauth/${McpName}',
        {
          RestApiId: props.oauth.callbackGateway.restApiId,
          Region: stack.region,
          UrlSuffix: stack.urlSuffix,
          StageName: props.oauth.stageName,
          McpName: props.name,
        }
      );
    }

    // Create the MCP proxy Lambda function
    const handler = new lambda.Function(agent, `${id}Handler`, {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'mcp-server-proxy')),
      timeout: cdk.Duration.seconds(60),
      memorySize: 256,
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        MCP_SERVER_URL: props.url,
        MCP_SERVER_HEADERS: JSON.stringify(props.headers || {}),
        ...(tokenTable && { OAUTH_TOKEN_TABLE: tokenTable.tableName }),
        ...(oauthCallbackUrl && { OAUTH_CALLBACK_URL: oauthCallbackUrl }),
        ...(props.oauth?.clientId && { OAUTH_CLIENT_ID: props.oauth.clientId }),
        ...(props.oauth?.clientSecret && { OAUTH_CLIENT_SECRET: props.oauth.clientSecret }),
      },
      ...props.lambdaProps,
    });

    // Grant DynamoDB permissions if OAuth is enabled
    if (tokenTable) {
      tokenTable.grantReadWriteData(handler);
    }

    // Add OAuth callback endpoint to the gateway
    if (props.oauth) {
      let oauthResource = props.oauth.callbackGateway.root.getResource('oauth');
      if (!oauthResource) {
        oauthResource = props.oauth.callbackGateway.root.addResource('oauth');
      }
      const mcpResource = oauthResource.addResource(props.name);
      mcpResource.addMethod('GET', new apigateway.LambdaIntegration(handler));
    }

    super(agent, id, {
      name: props.name,
      handler,
      queueProps: props.queueProps,
    });

    this.handler = handler;
    this.tokenTable = tokenTable;
    this.oauthCallbackUrl = oauthCallbackUrl;

    if (this.oauthCallbackUrl) {
      new cdk.CfnOutput(this, 'OAuthCallbackUrl', {
        value: this.oauthCallbackUrl,
        description: 'MCP OAuth Callback URL',
      });
    }
  }
}
