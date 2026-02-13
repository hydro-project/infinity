import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as path from 'path';

import { InfinityAgent } from '../../infinity-agents';
import { RapToolSet } from '../../infinity-agents/tools';

/**
 * Utility tools for getting the current time.
 * Tool definitions are served via /.well-known/rap-toolset.
 */
export class GetTimeToolSet extends RapToolSet {
  constructor(agent: InfinityAgent, id: string) {
    const handler = new lambda.Function(agent, 'GetTimeFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'get-time-tool')),
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
    });

    super(agent, id, { handler });
  }
}
