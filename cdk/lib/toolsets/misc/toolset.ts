import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as path from 'path';
import { CustomToolSet, LambdaTool, InfinityAgents } from '../../tools';

/**
 * Miscellaneous utility tools
 */
export class MiscToolSet extends CustomToolSet {
  constructor(agent: InfinityAgents, id: string) {
    const getTimeToolFunction = new lambda.Function(agent, `${id}GetTimeFunction`, {
      functionName: 'infinity-agents-get-time-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'get-time-tool')),
      timeout: cdk.Duration.seconds(30),
    });

    const getTimeTool = new LambdaTool(agent, `${id}GetTimeTool`, {
      name: 'get_time',
      description: 'Get the current time in a specified timezone or UTC.',
      parameters: {
        type: 'object',
        properties: {
          timezone: {
            type: 'string',
            description:
              "IANA timezone name (e.g., 'America/New_York', 'Europe/London'). Defaults to UTC if not specified.",
          },
        },
        required: [],
      },
      handler: getTimeToolFunction,
      queueProps: {
        visibilityTimeout: cdk.Duration.seconds(30),
      },
    });

    super(agent, id, [getTimeTool]);
  }
}
