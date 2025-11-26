# Lambda Functions

This directory contains Lambda functions for InfinityAgents tools and integrations.

## Tool Lambdas

### get-time-tool
Simple tool that returns the current time in a specified timezone.

### create-ec2-tool
Creates an EC2 instance and waits for it to reach running state via EventBridge.

**Flow**: Tool → EC2 API → EventBridge → ec2-state-monitor → Agent

### check-github-actions-tool
Monitors GitHub Actions workflow/check status and waits for completion via webhooks.

**Flow**: Tool → DynamoDB → GitHub Webhook → github-webhook-receiver → Agent

## Integration Lambdas

### slack-receiver
Receives Slack events via webhook and forwards to agent input queue.

### slack-responder
Receives agent outputs from SQS and posts to Slack.

### ec2-state-monitor
Monitors EC2 state changes via EventBridge and notifies agent when instances reach running state.

### github-webhook-receiver
Receives GitHub webhooks and completes pending GitHub Actions check tool calls.

## Pattern: Async Tool with External Event

Both EC2 and GitHub Actions tools follow the same pattern for async operations:

1. **Tool Lambda**: Initiates operation and stores metadata
   - EC2: Tags on instance
   - GitHub: DynamoDB entry
2. **Event Source**: External system sends events
   - EC2: EventBridge state changes
   - GitHub: Webhooks
3. **Monitor Lambda**: Receives events and completes tool calls
   - Looks up metadata
   - Sends result to agent input queue
   - Cleans up metadata

This pattern allows the agent to continue processing while waiting for long-running operations.
