# Slack Integration Lambdas

This directory contains the Node.js Lambda functions for Slack integration.

## Architecture

```
Slack Event → API Gateway → slack-receiver → Agent Input Queue (FIFO)
                                                      ↓
                                              Agent Lambda (Rust)
                                                      ↓
                                              Agent Output Queue (FIFO)
                                                      ↓
                                              slack-responder → Slack Thread
```

## slack-receiver

Receives Slack events via API Gateway and forwards them to the agent input queue.

**Input**: Slack event payload
**Output**: SQS message with format:
```json
{
  "type": "text",
  "text": "user message text",
  "metadata": {
    "channel": "C123456",
    "thread_ts": "1234567890.123456",
    "user": "U123456",
    "ts": "1234567890.123456"
  }
}
```

**Message Group ID**: `slack-{channel}-{thread_ts}` - ensures messages in the same thread are processed in order

## slack-responder

Receives agent outputs from the output queue and posts them to Slack threads.

**Input**: SQS message with format:
```json
{
  "text": "agent response text",
  "metadata": {
    "channel": "C123456",
    "thread_ts": "1234567890.123456"
  }
}
```

**Output**: Posts message to Slack using the Bot Token

## Setup

### 1. Install dependencies

```bash
cd lambda/slack-receiver
npm install

cd ../slack-responder
npm install
```

### 2. Set environment variables

Before deploying, set these environment variables:

```bash
export SLACK_SIGNING_SECRET=your_slack_signing_secret
export SLACK_BOT_TOKEN=xoxb-your-bot-token
```

### 3. Deploy

```bash
cd cdk
npx cdk deploy
```

### 4. Configure Slack App

1. Create a Slack App at https://api.slack.com/apps
2. Add Bot Token Scopes:
   - `app_mentions:read`
   - `chat:write`
   - `channels:history`
   - `groups:history`
   - `im:history`
   - `mpim:history`
3. Enable Event Subscriptions:
   - Request URL: Use the `SlackWebhookUrl` from CDK outputs
   - Subscribe to bot events: `app_mention`, `message.channels`, `message.groups`, `message.im`, `message.mpim`
4. Install the app to your workspace
5. Copy the Bot Token and Signing Secret to your environment variables

## Testing

Send a message in Slack:
- Mention your bot: `@YourBot what's the weather in Seattle?`
- Or send a DM to the bot

The bot will respond in a thread with the AI-generated response.

## Message Flow

1. User mentions bot or sends DM in Slack
2. Slack sends event to API Gateway webhook
3. `slack-receiver` Lambda:
   - Validates the event
   - Extracts message text and metadata
   - Sends to agent input queue with thread-based message group ID
4. Agent Lambda (Rust):
   - Processes message with conversation history (keyed by message group ID)
   - Streams response from Bedrock
   - Sends complete response to output queue
5. `slack-responder` Lambda:
   - Receives agent output
   - Posts response to Slack thread using metadata

## Conversation Continuity

- Each Slack thread has a unique message group ID: `slack-{channel}-{thread_ts}`
- DynamoDB stores conversation history keyed by this group ID
- Messages in the same thread are processed sequentially (FIFO queue)
- The agent maintains context across the entire thread
