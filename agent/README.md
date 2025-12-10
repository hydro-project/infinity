# InfinityAgents Leader CDK Stack

This CDK stack deploys the infrastructure for the InfinityAgents Leader Lambda function.

## Infrastructure

- **DynamoDB Table**: `InfinityAgentsState` - Stores conversation history with deduplication
- **SQS FIFO Queue**: `infinity-agents-leader.fifo` - Receives incoming messages with message group IDs
- **Dead Letter Queue**: `infinity-agents-leader-dlq.fifo` - Captures failed messages
- **Lambda Function**: `infinity-agents-leader` - Processes messages and manages conversations
- **IAM User**: For cargo lambda deployment

## Setup

1. Configure environment variables:
```bash
# Copy the example file
cp .env.example .env

# Edit .env and add your Slack credentials
# Get these from https://api.slack.com/apps
```

2. Install dependencies:
```bash
cd cdk
npm install
```

3. Deploy the stack:

**For bash/zsh:**
```bash
cd cdk
npx cdk bootstrap  # Only needed once per account/region
source ../.env && npx cdk deploy
```

**For fish shell:**
```bash
cd cdk
npx cdk bootstrap  # Only needed once per account/region
chmod +x deploy.fish
./deploy.fish
```

## Deployment

Configure AWS credentials from the CDK outputs:
```bash
export AWS_ACCESS_KEY_ID=<DeployAccessKeyId>
export AWS_SECRET_ACCESS_KEY=<DeploySecretAccessKey>
```
