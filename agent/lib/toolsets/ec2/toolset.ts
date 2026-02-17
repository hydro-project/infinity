import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as events from 'aws-cdk-lib/aws-events';
import * as targets from 'aws-cdk-lib/aws-events-targets';
import * as path from 'path';

import { InfinityAgent } from '../../infinity-agents';
import { RapToolSet } from '../../infinity-agents/tools';

/**
 * EC2 management tools.
 * Tool definitions are served via /.well-known/rap-toolset.
 */
export class Ec2ToolSet extends RapToolSet {
  constructor(agent: InfinityAgent, id: string) {
    const createEc2Function = new lambda.Function(agent, 'CreateEc2Function', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'create-ec2-tool')),
      timeout: cdk.Duration.seconds(60),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
    });
    createEc2Function.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['ec2:RunInstances', 'ec2:CreateTags', 'ec2:DescribeInstances'],
        resources: ['*'],
      })
    );

    // EC2 State Monitor — EventBridge listener, not a tool the LLM calls
    const ec2StateMonitorFunction = new lambda.Function(agent, 'StateMonitorFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'ec2-state-monitor')),
      timeout: cdk.Duration.seconds(30),
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      environment: {
        RAP_CALLBACK_URL: agent.rapReceiverUrl,
      },
    });
    ec2StateMonitorFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['ec2:DescribeTags', 'ec2:DescribeInstances'],
        resources: ['*'],
      })
    );

    new events.Rule(agent, 'StateChangeRule', {
      description: 'Monitors EC2 instances created by InfinityAgents reaching running state',
      eventPattern: {
        source: ['aws.ec2'],
        detailType: ['EC2 Instance State-change Notification'],
        detail: { state: ['running'] },
      },
    }).addTarget(new targets.LambdaFunction(ec2StateMonitorFunction));

    agent.grantRapAccess(ec2StateMonitorFunction);

    super(agent, id, { handler: createEc2Function });
  }
}
