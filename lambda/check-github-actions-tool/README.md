# GitHub Actions Check Tool

This tool allows the agent to monitor GitHub Actions workflow runs and check statuses. When invoked, it stores a mapping in DynamoDB and waits for a GitHub webhook to notify when the check completes.

## How It Works

1. **Tool Invocation**: Agent calls `wait_github_actions_result` with repository info and a commit SHA
2. **Storage**: The tool stores a mapping in DynamoDB with:
   - Primary key: `owner/repo/sha`
   - Sort key: check name (or "ALL" to match any check)
   - Tool call metadata (id, group_id, input_queue_url)
3. **Webhook**: GitHub sends webhooks to the receiver Lambda when checks complete
4. **Matching**: The webhook receiver compares the `head_sha` from GitHub events against the stored SHA
5. **Completion**: The webhook receiver finds matching entries, sends results to the agent input queue, and deletes the DynamoDB entry

**Important**: The `sha` parameter must be a full commit SHA (not a branch or tag name) because it's matched against the `head_sha` field from GitHub webhook events.

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

Wait for a specific check on a commit:

```json
{
  "owner": "myorg",
  "repo": "myrepo",
  "sha": "abc123def456789",
  "check_name": "CI Tests"
}
```

Or wait for any check to complete on a commit:

```json
{
  "owner": "myorg",
  "repo": "myrepo",
  "sha": "abc123def456789"
}
```

**Note**: The `sha` must be a full commit SHA, not a branch name or tag.

## DynamoDB Schema

Table: `AgentZeroGitHubChecks`

- **pk** (String): `owner/repo/sha` (where sha is the commit SHA)
- **sk** (String): `checkName#toolCallId` (e.g., "CI Tests#abc123" or "ALL#abc123")
  - This allows multiple tool calls to listen for the same sha/check combination
- **toolCallId** (String): Tool call ID for response
- **callId** (String): Optional call ID
- **groupId** (String): Conversation group ID
- **inputQueueUrl** (String): SQS queue URL for responses
- **sha** (String): The commit SHA being monitored (matched against head_sha from webhooks)
- **checkName** (String): The check name being monitored (or empty for ALL)
- **ttl** (Number): 24-hour TTL for automatic cleanup
