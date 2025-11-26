import * as cdk from 'aws-cdk-lib';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import { Construct } from 'constructs';
import { InfinityAgents, LambdaMCPToolSet } from './tools';
import { MiscToolSet, Ec2ToolSet, GitHubToolSet } from './toolsets';

export class ExampleAgentStack extends cdk.Stack {
  private agent: InfinityAgents;
  private api: apigateway.RestApi;

  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    this.agent = new InfinityAgents(this, 'InfinityAgents');

    // API Gateway for webhooks
    this.api = new apigateway.RestApi(this, 'WebhookApi', {
      restApiName: 'InfinityAgents Webhooks',
      description: 'Receives webhook events and forwards to agent',
      deployOptions: {
        stageName: 'prod',
      },
    });

    const slackWebhookUrl = this.agent.setupSlackIntegration(this, this.api);

    // Tool sets
    new MiscToolSet(this.agent, 'MiscToolSet');
    new Ec2ToolSet(this.agent, 'Ec2ToolSet');
    const githubToolSet = new GitHubToolSet(this.agent, 'GitHubToolSet', { api: this.api });

    new LambdaMCPToolSet(this.agent, 'GithubMcp', {
      name: 'github',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
      env: {
        GITHUB_PERSONAL_ACCESS_TOKEN: process.env.GITHUB_PERSONAL_ACCESS_TOKEN
      },
    });

    new cdk.CfnOutput(this, 'SlackWebhookUrl', {
      value: slackWebhookUrl,
      description: 'Slack Event Subscription URL',
    });

    new cdk.CfnOutput(this, 'GithubWebhookUrl', {
      value: githubToolSet.webhookUrl,
      description: 'GitHub Webhook URL',
    });
  }
}
