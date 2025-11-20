#!/usr/bin/env node
import 'source-map-support/register';
import * as cdk from 'aws-cdk-lib';
import { AgentZeroLeaderStack } from '../lib/agentzero-leader-stack';

const app = new cdk.App();
new AgentZeroLeaderStack(app, 'AgentZeroLeaderStack', {
  env: {
    account: process.env.CDK_DEFAULT_ACCOUNT,
    region: process.env.CDK_DEFAULT_REGION,
  },
});
