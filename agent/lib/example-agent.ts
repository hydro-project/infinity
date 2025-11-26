import * as cdk from 'aws-cdk-lib';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import { Construct } from 'constructs';

import { InfinityAgent } from './infinity-agents';
import { LambdaMCPToolSet } from './infinity-agents/mcp';
import { SlackIntegration } from './infinity-agents/slack';
import { GetTimeToolSet, Ec2ToolSet, GitHubEventToolSet } from './toolsets';

export class ExampleAgent extends InfinityAgent {
  constructor(scope: Construct, id: string) {
    super(scope, id);

    // API Gateway for webhooks
    const gateway = new apigateway.RestApi(this, 'WebhookApi', {
      restApiName: 'InfinityAgents Webhooks',
      deployOptions: {
        stageName: 'prod',
      },
    });

    // MCP tool sets
    new LambdaMCPToolSet(this, 'GithubMcp', {
      name: 'github',
      command: ['npx', '-y', '@modelcontextprotocol/server-github'],
      env: {
        GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN,
      },
    });

    // Custom tool sets
    new GetTimeToolSet(this, 'GetTimeToolSet');
    new Ec2ToolSet(this, 'Ec2ToolSet');

    const githubToolSet = new GitHubEventToolSet(this, 'GitHubEventToolSet', { webhookGateway: gateway });
    new cdk.CfnOutput(this, 'GithubWebhookUrl', {
      value: githubToolSet.webhookUrl,
      description: 'GitHub Webhook URL',
    });

    const slack = new SlackIntegration(this, 'SlackIntegration', { webhookGateway: gateway });
    new cdk.CfnOutput(this, 'SlackWebhookUrl', {
      value: slack.webhookUrl,
      description: 'Slack Event Subscription URL',
    });
  }
}

export class ExampleAgentStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    new ExampleAgent(this, 'ExampleAgent');
  }
}
