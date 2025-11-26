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

3. Build the Rust Lambda function:
```bash
cd ..
cargo lambda build --release --arm64
```

4. Deploy the stack:

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

After the initial CDK deployment, you can update just the Lambda function code using cargo lambda:

```bash
cargo lambda build --release --arm64
cargo lambda deploy infinity-agents-leader
```

Configure AWS credentials from the CDK outputs:
```bash
export AWS_ACCESS_KEY_ID=<DeployAccessKeyId>
export AWS_SECRET_ACCESS_KEY=<DeploySecretAccessKey>
```

## Testing

Send a message to the SQS queue:
```bash
aws sqs send-message \
  --queue-url <QueueUrl from outputs> \
  --message-body '{"text":"Hello, what is the weather in Seattle?"}' \
  --message-group-id "user-123" \
  --message-deduplication-id "$(uuidgen)"
```

## Configuration

The Lambda function uses these environment variables:
- `DYNAMODB_TABLE_NAME`: Set automatically to `InfinityAgentsState`
- `RUST_BACKTRACE`: Set to `1` for debugging

## Architecture

1. Messages arrive in the SQS FIFO queue with a `MessageGroupId`
2. Lambda processes messages one at a time per message group
3. Conversation history is loaded from DynamoDB using the group ID
4. Messages are deduplicated using SQS message IDs
5. AI responses are streamed from Bedrock
6. History is appended to DynamoDB after each interaction
