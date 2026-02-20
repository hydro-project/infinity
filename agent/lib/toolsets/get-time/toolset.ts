import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import { NodejsFunction } from 'aws-cdk-lib/aws-lambda-nodejs';
import * as path from 'path';

import { InfinityAgent, NODEJS_BUNDLING_DEFAULTS } from '../../infinity-agents';
import { RapToolSet } from '../../infinity-agents/tools';

/**
 * Utility tools for getting the current time.
 * Tool definitions are served via /.well-known/rap-toolset.
 */
export class GetTimeToolSet extends RapToolSet {
  constructor(agent: InfinityAgent, id: string) {
    const handler = new NodejsFunction(agent, 'GetTimeFunction', {
      entry: path.join(__dirname, 'get-time-tool', 'index.mjs'),
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'handler',
      bundling: NODEJS_BUNDLING_DEFAULTS,
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
    });

    super(agent, id, { handler });
  }
}
