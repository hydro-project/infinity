import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as path from 'path';

import { InfinityAgent } from '../../infinity-agents';  
import { CustomToolSet, LambdaTool } from '../../infinity-agents/tools';

/**
 * Miscellaneous utility tools
 */
export class GetTimeToolSet extends CustomToolSet {
  constructor(agent: InfinityAgent, id: string) {
    const getTimeToolFunction = new lambda.Function(agent, 'GetTimeFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'get-time-tool')),
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
    });

    const getTimeTool = new LambdaTool(agent, 'GetTimeTool', {
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
    });

    super(agent, id, [getTimeTool]);
  }
}
