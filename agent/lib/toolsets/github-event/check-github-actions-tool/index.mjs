import { DynamoDBClient, PutItemCommand, GetItemCommand, DeleteItemCommand } from '@aws-sdk/client-dynamodb';
import { sendToolResult } from 'rap-js';

const dynamoClient = new DynamoDBClient({});

const GITHUB_CHECKS_TABLE = process.env.GITHUB_CHECKS_TABLE;
const SUBSCRIPTION_LOOKUP_TABLE = process.env.SUBSCRIPTION_LOOKUP_TABLE;

async function handleSubscribe(args, id, call_id, rap_receiver_url, group_id) {
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
        rapReceiverUrl: { S: rap_receiver_url },
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
    // Immediately signal OK to the invoker so the leader doesn't block
    responseStream.write('OK');
    responseStream.end();

    try {
        const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
        const { arguments: args, id, call_id, rap_receiver_url, group_id, tool_name } = body;

        console.log('Processing request:', { tool_name, args, id, call_id });

        try {
            let resultText;

            if (tool_name === 'cancel_github_subscription' || args.subscription_id) {
                resultText = await handleCancelSubscription(args);
            } else {
                resultText = await handleSubscribe(args, id, call_id, rap_receiver_url, group_id);
            }

            await sendToolResult(rap_receiver_url, group_id, id, call_id, resultText);
            console.log('Sent response via RAP');
        } catch (error) {
            console.error('Error processing request:', error);
            await sendToolResult(rap_receiver_url, group_id, id, call_id, `Error: ${error.message}`);
        }
    } catch (error) {
        console.error('Error parsing request:', error);
    }
});
