---
sidebar_position: 4
title: Slack Integration
---

# Slack Integration

The `SlackIntegration` construct turns Slack into the agent's chat frontend. It provisions two pieces: a webhook receiver and a responder. The receiver is wired to Slack's Events API (verified with `SLACK_SIGNING_SECRET`) and maps each Slack thread to a conversation `group_id` on the input queue. The responder authenticates with `SLACK_BOT_TOKEN` and posts output queue messages back into the originating Slack thread:

```typescript
import * as cdk from 'aws-cdk-lib';
import * as apigateway from 'aws-cdk-lib/aws-apigateway';
import { SlackIntegration } from 'infinity-agents-cdk';

const gateway = new apigateway.RestApi(this, 'WebhookApi');

const slack = new SlackIntegration(agent, 'SlackIntegration', { webhookGateway: gateway });
new cdk.CfnOutput(this, 'SlackWebhookUrl', { value: slack.webhookUrl });
```

To deploy, set `SLACK_SIGNING_SECRET` and `SLACK_BOT_TOKEN` in the deployment environment (you can get both from your app's settings at [api.slack.com/apps](https://api.slack.com/apps)) and point the Slack app's event subscription at the emitted webhook URL. Once configured, mentioning the app in a channel will start a durable agent conversation in that thread.

Because each Slack thread maps to its own `group_id`, every thread is an independent conversation with its own persisted history. This means that separate Slack threads will run concurrently on separate Lambda invocations.
