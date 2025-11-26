#!/usr/bin/env node
import 'source-map-support/register';
import * as cdk from 'aws-cdk-lib';
import { ExampleAgentStack } from '../lib/example-agent';

const app = new cdk.App();
new ExampleAgentStack(app, 'AgentZeroLeaderStack', {
  env: {
    account: process.env.CDK_DEFAULT_ACCOUNT,
    region: process.env.CDK_DEFAULT_REGION,
  },
});
