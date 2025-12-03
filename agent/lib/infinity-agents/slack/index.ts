import { Construct } from 'constructs';
import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import { SqsEventSource } from 'aws-cdk-lib/aws-lambda-event-sources';
import * as path from 'path';
import { InfinityAgent } from '../index';

export interface SlackIntegrationProps {
  /**
   * API Gateway to add the Slack webhook endpoint to
   */
  readonly webhookGateway: apigateway.RestApi;
}

/**
 * Slack integration for InfinityAgent
 * Creates receiver and responder Lambda functions and API Gateway endpoint
 */
export class SlackIntegration extends Construct {
  public readonly webhookUrl: string;

  constructor(agent: InfinityAgent, id: string, props: SlackIntegrationProps) {
    super(agent, id);

    // Slack Receiver Lambda (receives Slack events, sends to agent input queue)
    const slackReceiverFunction = new lambda.Function(this, 'ReceiverFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'slack-receiver')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        AGENT_INPUT_QUEUE_URL: agent.inputQueue.queueUrl,
        SLACK_SIGNING_SECRET: process.env.SLACK_SIGNING_SECRET || '',
      },
    });

    agent.inputQueue.grantSendMessages(slackReceiverFunction);

    // Add Slack webhook endpoint to API Gateway
    const slackIntegration = new apigateway.LambdaIntegration(slackReceiverFunction);
    props.webhookGateway.root.addResource('slack').addResource('events').addMethod('POST', slackIntegration);

    // Slack Responder Lambda (receives agent outputs, posts to Slack)
    const slackResponderFunction = new lambda.Function(this, 'ResponderFunction', {
      runtime: lambda.Runtime.NODEJS_24_X,
      handler: 'index.handler',
      code: lambda.Code.fromAsset(path.join(__dirname, 'slack-responder')),
      timeout: cdk.Duration.seconds(30),
      environment: {
        SLACK_BOT_TOKEN: process.env.SLACK_BOT_TOKEN || '',
      },
    });

    slackResponderFunction.addEventSource(
      new SqsEventSource(agent.outputQueue, {
        batchSize: 1,
        reportBatchItemFailures: true,
      })
    );

    this.webhookUrl = props.webhookGateway.url + 'slack/events';
  }
}
