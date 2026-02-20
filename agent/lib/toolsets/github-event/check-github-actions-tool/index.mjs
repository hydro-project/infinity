import { DynamoDBClient, PutItemCommand, GetItemCommand, DeleteItemCommand } from '@aws-sdk/client-dynamodb';
import { sendToolResult } from '../../../infinity-agents/rap-js/index.mjs';

const dynamoClient = new DynamoDBClient({});

const GITHUB_CHECKS_TABLE = process.env.GITHUB_CHECKS_TABLE;
const SUBSCRIPTION_LOOKUP_TABLE = process.env.SUBSCRIPTION_LOOKUP_TABLE;

const TOOLSET_MANIFEST = {
  name: 'github-events',
  description: 'Subscribe to and manage GitHub webhook event notifications',
  endpoint: '',
  tools: [
    {
      name: 'subscribe_github_events',
      description:
        'Subscribes to GitHub webhook events on a repository. Use filters to match specific events. If there is nothing to do until an event arrives, you may want to use the sleep tool to hibernate until you are woken up by an event. DO NOT re-subscribe after an `interrupt`, the subscription remains active automatically.',
      inputSchema: {
        type: 'object',
        properties: {
          owner: { type: 'string', description: 'GitHub repository owner (username or organization).' },
          repo: { type: 'string', description: 'GitHub repository name.' },
          event_type: { type: 'string', description: 'Optional: GitHub event type to filter on (e.g., "pull_request", "push", "issues").' },
          sha: { type: 'string', description: 'Optional: Commit SHA to filter on.' },
          pr_number: { type: 'number', description: 'Optional: Pull request number to filter on.' },
          issue_number: { type: 'number', description: 'Optional: Issue number to filter on.' },
          action: { type: 'string', description: 'Optional: Event action to filter on (e.g., "opened", "closed").' },
          branch: { type: 'string', description: 'Optional: Branch name to filter on.' },
          actor: { type: 'string', description: 'Optional: GitHub username to filter on.' },
        },
        required: ['owner', 'repo'],
      },
      annotations: { subscription: true },
    },
    {
      name: 'cancel_github_subscription',
      description: 'Cancels an active GitHub webhook event subscription.',
      inputSchema: {
        type: 'object',
        properties: {
          subscription_id: { type: 'string', description: 'The subscription ID returned when you created the subscription.' },
        },
        required: ['subscription_id'],
      },
    },
  ],
};

async function handleSubscribe(args, id, call_id, callback_url, group_id) {
    const owner = args.owner;
    const repo = args.repo;
    
    const filters = {};
    if (args.event_type) filters.eventType = args.event_type;
    if (args.sha) filters.sha = args.sha;
    if (args.pr_number) filters.prNumber = args.pr_number;
    if (args.issue_number) filters.issueNumber = args.issue_number;
    if (args.action) filters.action = args.action;
    if (args.branch) filters.branch = args.branch;
    if (args.actor) filters.actor = args.actor;

    const filterKey = Object.keys(filters).length > 0 
        ? Object.entries(filters).map(([k, v]) => `${k}:${v}`).sort().join('|')
        : 'ALL';

    const pk = `${owner}/${repo}`;
    const sk = `${filterKey}#${id}`;

    const subscriptionItem = {
        pk: { S: pk },
        sk: { S: sk },
        toolCallId: { S: id },
        callId: { S: call_id || '' },
        groupId: { S: group_id },
        rapReceiverUrl: { S: callback_url },
        owner: { S: owner },
        repo: { S: repo },
        filters: { S: JSON.stringify(filters) },
        filterKey: { S: filterKey },
        createdAt: { N: Date.now().toString() },
    };

    await dynamoClient.send(new PutItemCommand({
        TableName: GITHUB_CHECKS_TABLE,
        Item: subscriptionItem,
    }));

    await dynamoClient.send(new PutItemCommand({
        TableName: SUBSCRIPTION_LOOKUP_TABLE,
        Item: {
            subscriptionId: { S: id },
            pk: { S: pk },
            sk: { S: sk },
        },
    }));

    console.log('Stored GitHub event subscription:', { pk, sk, filters });

    const filterDescription = Object.keys(filters).length > 0
        ? `Filters: ${JSON.stringify(filters)}`
        : 'No filters (will match all events)';

    return `Subscription ID: ${id}\n${filterDescription}`;
}

async function handleCancelSubscription(args) {
    const subscriptionId = args.subscription_id;
    
    if (!subscriptionId) {
        return 'Error: subscription_id is required';
    }

    const lookupResult = await dynamoClient.send(new GetItemCommand({
        TableName: SUBSCRIPTION_LOOKUP_TABLE,
        Key: { subscriptionId: { S: subscriptionId } },
    }));

    if (!lookupResult.Item) {
        return `Subscription not found: ${subscriptionId}. It may have already been cancelled or expired.`;
    }

    const pk = lookupResult.Item.pk.S;
    const sk = lookupResult.Item.sk.S;

    await dynamoClient.send(new DeleteItemCommand({
        TableName: GITHUB_CHECKS_TABLE,
        Key: { pk: { S: pk }, sk: { S: sk } },
    }));

    await dynamoClient.send(new DeleteItemCommand({
        TableName: SUBSCRIPTION_LOOKUP_TABLE,
        Key: { subscriptionId: { S: subscriptionId } },
    }));

    console.log('Cancelled subscription:', { subscriptionId, pk, sk });
    return `Successfully cancelled subscription: ${subscriptionId}`;
}

export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
    // Handle .well-known/rap-toolset discovery
    if (event.requestContext?.http?.method === 'GET' && event.rawPath?.includes('.well-known/rap-toolset')) {
        const manifest = { ...TOOLSET_MANIFEST };
        if (!manifest.endpoint) {
            manifest.endpoint = `https://${event.requestContext?.domainName || ''}`;
        }
        responseStream.write(JSON.stringify(manifest));
        responseStream.end();
        return;
    }

    // Tool invocation
    responseStream.write('OK');
    responseStream.end();

    try {
        const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
        const { arguments: args, id, call_id, callback_url, group_id, tool_name } = body;

        console.log('Processing request:', { tool_name, args, id, call_id });

        try {
            let resultText;

            if (tool_name === 'cancel_github_subscription' || args.subscription_id) {
                resultText = await handleCancelSubscription(args);
            } else {
                resultText = await handleSubscribe(args, id, call_id, callback_url, group_id);
            }

            await sendToolResult(callback_url, group_id, id, call_id, resultText);
            console.log('Sent response via RAP');
        } catch (error) {
            console.error('Error processing request:', error);
            await sendToolResult(callback_url, group_id, id, call_id, `Error: ${error.message}`);
        }
    } catch (error) {
        console.error('Error parsing request:', error);
    }
});
