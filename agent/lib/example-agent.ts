import * as cdk from 'aws-cdk-lib';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import { Construct } from 'constructs';

import { InfinityAgent } from './infinity-agents';
import { HTTPMCPToolSet } from './infinity-agents/mcp';
import { SlackIntegration } from './infinity-agents/slack';
import { GetTimeToolSet, Ec2ToolSet, GitHubEventToolSet } from './toolsets';

export class ExampleAgent extends InfinityAgent {
  constructor(scope: Construct, id: string, gateway: apigateway.RestApi) {
    super(scope, id);

    new HTTPMCPToolSet(this, 'GithubMcp', {
      name: 'github',
      url: 'https://api.githubcopilot.com/mcp/',
      oauth: {
        callbackGateway: gateway,
        stageName: 'prod',
        clientId: process.env.GITHUB_OAUTH_CLIENT_ID,
        clientSecret: process.env.GITHUB_OAUTH_CLIENT_SECRET,
      },
    });

    // Custom tool sets
    new GetTimeToolSet(this, 'GetTimeToolSet');
    new Ec2ToolSet(this, 'Ec2ToolSet');

    // Event subscriptions
    new GitHubEventToolSet(this, 'GitHubEventToolSet', { webhookGateway: gateway });
  }
}


export class ExampleAgentStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    // API Gateway for webhooks
    const gateway = new apigateway.RestApi(this, 'WebhookApi');

    const agent = new ExampleAgent(this, 'ExampleAgent', gateway);

    const slack = new SlackIntegration(agent, 'SlackIntegration', { webhookGateway: gateway });
    new cdk.CfnOutput(this, 'SlackWebhookUrl', {
      value: slack.webhookUrl,
      description: 'Slack Event Subscription URL',
    });
  }
}
