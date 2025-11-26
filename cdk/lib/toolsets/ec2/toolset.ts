import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as events from 'aws-cdk-lib/aws-events';
import * as targets from 'aws-cdk-lib/aws-events-targets';
import * as path from 'path';
import { CustomToolSet, LambdaTool, InfinityAgents } from '../../tools';

/**
 * EC2 management tools
 */
export class Ec2ToolSet extends CustomToolSet {
  constructor(agent: InfinityAgents, id: string) {
    // Create EC2 Tool Lambda
    const createEc2ToolFunction = new lambda.Function(agent, `${id}CreateEc2Function`, {
      functionName: 'infinity-agents-create-ec2-tool',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'create-ec2-tool')),
      timeout: cdk.Duration.seconds(60),
    });
    createEc2ToolFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['ec2:RunInstances', 'ec2:CreateTags', 'ec2:DescribeInstances'],
        resources: ['*'],
      })
    );

    const createEc2Tool = new LambdaTool(agent, `${id}CreateEc2Tool`, {
      name: 'create_ec2',
      description: 'Create an EC2 instance. You will be notified when the instance is running.',
      parameters: {
        type: 'object',
        properties: {
          instance_type: {
            type: 'string',
            description: "EC2 instance type (e.g., 't3.micro', 't3.small').",
          },
          ami_id: {
            type: 'string',
            description: 'AMI ID to use for the instance.',
          },
          name: {
            type: 'string',
            description: 'Name tag for the instance.',
          },
          key_name: {
            type: 'string',
            description: 'SSH key pair name for accessing the instance. Optional.',
          },
        },
        required: ['instance_type', 'ami_id', 'name'],
      },
      handler: createEc2ToolFunction,
    });

    // EC2 State Monitor Lambda
    const ec2StateMonitorFunction = new lambda.Function(agent, `${id}StateMonitorFunction`, {
      functionName: 'infinity-agents-ec2-state-monitor',
      runtime: lambda.Runtime.NODEJS_20_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'ec2-state-monitor')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        INPUT_QUEUE_URL: agent.inputQueue.queueUrl,
      },
    });
    agent.inputQueue.grantSendMessages(ec2StateMonitorFunction);
    ec2StateMonitorFunction.addToRolePolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['ec2:DescribeTags', 'ec2:DescribeInstances'],
        resources: ['*'],
      })
    );

    const ec2StateRule = new events.Rule(agent, `${id}StateChangeRule`, {
      ruleName: 'infinity-agents-ec2-running',
      description: 'Monitors EC2 instances created by InfinityAgents reaching running state',
      eventPattern: {
        source: ['aws.ec2'],
        detailType: ['EC2 Instance State-change Notification'],
        detail: {
          state: ['running'],
        },
      },
    });
    ec2StateRule.addTarget(new targets.LambdaFunction(ec2StateMonitorFunction));

    super(agent, id, [createEc2Tool]);
  }
}
