# GitHub Actions Check Tool

This tool allows the agent to monitor GitHub Actions workflow runs and check statuses. When invoked, it stores a mapping in DynamoDB and waits for a GitHub webhook to notify when the check completes.

## How It Works

1. **Tool Invocation**: Agent calls `check_github_actions` with repository info and a git reference (commit SHA, branch, or tag)
2. **Storage**: The tool stores a mapping in DynamoDB with:
   - Primary key: `owner/repo/ref`
   - Sort key: check name (or "ALL" to match any check)
   - Tool call metadata (id, group_id, input_queue_url)
3. **Webhook**: GitHub sends webhooks to the receiver Lambda when checks complete
4. **Completion**: The webhook receiver finds matching entries, sends results to the agent input queue, and deletes the DynamoDB entry

## Setup

### 1. Configure GitHub Webhook Secret

Set the `GITHUB_WEBHOOK_SECRET` environment variable in your CDK deployment:

```bash
export GITHUB_WEBHOOK_SECRET="your-secret-here"
```

### 2. Deploy the Stack

```bash
cd cdk
npm install
cdk deploy
```

Note the `GithubWebhookUrl` output value.

### 3. Configure GitHub Webhook

In your GitHub repository settings:

1. Go to Settings → Webhooks → Add webhook
2. Set Payload URL to the `GithubWebhookUrl` from CDK output
3. Set Content type to `application/json`
4. Set Secret to your `GITHUB_WEBHOOK_SECRET`
5. Select individual events:
   - Check runs
   - Check suites
   - Workflow runs
   - Statuses
6. Save the webhook

## Supported Events

The tool monitors these GitHub webhook events:

- **check_run**: Individual check runs (e.g., from GitHub Apps)
- **check_suite**: Check suite completions
- **workflow_run**: GitHub Actions workflow runs
- **status**: Commit status updates (legacy API)

## Usage Example

```json
{
  "owner": "myorg",
  "repo": "myrepo",
  "ref": "abc123def456",
  "check_name": "CI Tests"
}
```

Or wait for any check to complete:

```json
{
  "owner": "myorg",
  "repo": "myrepo",
  "ref": "main"
}
```

## DynamoDB Schema

Table: `AgentZeroGitHubChecks`

- **pk** (String): `owner/repo/ref`
- **sk** (String): check name or "ALL"
- **toolCallId** (String): Tool call ID for response
- **callId** (String): Optional call ID
- **groupId** (String): Conversation group ID
- **inputQueueUrl** (String): SQS queue URL for responses
- **ttl** (Number): 24-hour TTL for automatic cleanup
