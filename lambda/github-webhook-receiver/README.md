# GitHub Webhook Receiver

This Lambda function receives GitHub webhooks and completes pending GitHub Actions check tool calls.

## Architecture

```
GitHub → API Gateway → Lambda → DynamoDB (lookup) → SQS (agent input)
                                    ↓
                              (delete entry)
```

## Flow

1. GitHub sends webhook when check/workflow completes
2. Lambda verifies webhook signature
3. Lambda queries DynamoDB for matching tool calls
4. Lambda sends tool result to agent input queue
5. Lambda deletes the DynamoDB entry

## Webhook Events Handled

- `check_run` - Individual check runs
- `check_suite` - Check suite completions  
- `workflow_run` - GitHub Actions workflows
- `status` - Commit status updates

## Security

The webhook signature is verified using HMAC-SHA256 with the `GITHUB_WEBHOOK_SECRET` environment variable. This ensures webhooks are authentic and from GitHub.

## Environment Variables

- `GITHUB_CHECKS_TABLE` - DynamoDB table name for check mappings
- `GITHUB_WEBHOOK_SECRET` - Secret for verifying GitHub webhook signatures

## Matching Logic

The receiver matches webhooks to pending tool calls using:

1. Repository owner/name and git reference (commit SHA, branch, tag)
2. Check name (if specified) or "ALL" for any check

Multiple tool calls can wait for the same check, and all will be notified.
