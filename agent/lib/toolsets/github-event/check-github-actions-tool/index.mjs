import { DynamoDBClient, PutItemCommand } from '@aws-sdk/client-dynamodb';

const dynamoClient = new DynamoDBClient({});

const GITHUB_CHECKS_TABLE = process.env.GITHUB_CHECKS_TABLE;

export const handler = async (event) => {
    console.log('Received event:', JSON.stringify(event, null, 2));

    for (const record of event.Records) {
        const request = JSON.parse(record.body);
        const { arguments: args, id, call_id, input_queue_url, group_id } = request;

        console.log('Processing subscribe_github_event request:', { args, id, call_id });

        try {
            // Extract parameters
            const owner = args.owner;
            const repo = args.repo;
            
            // Build filters object from optional parameters
            const filters = {};
            if (args.event_type) filters.eventType = args.event_type;
            if (args.sha) filters.sha = args.sha;
            if (args.pr_number) filters.prNumber = args.pr_number;
            if (args.issue_number) filters.issueNumber = args.issue_number;
            if (args.action) filters.action = args.action;
            if (args.branch) filters.branch = args.branch;
            if (args.actor) filters.actor = args.actor;

            // Build a filter key for the sort key - use a hash of filters or 'ALL' if no filters
            const filterKey = Object.keys(filters).length > 0 
                ? Object.entries(filters).map(([k, v]) => `${k}:${v}`).sort().join('|')
                : 'ALL';

            // Store mapping in DynamoDB
            // pk: owner/repo (to query all subscriptions for a repo)
            // sk: filterKey#toolCallId (to allow multiple subscriptions with same filters)
            const item = {
                pk: { S: `${owner}/${repo}` },
                sk: { S: `${filterKey}#${id}` },
                toolCallId: { S: id },
                callId: { S: call_id || '' },
                groupId: { S: group_id },
                inputQueueUrl: { S: input_queue_url },
                owner: { S: owner },
                repo: { S: repo },
                filters: { S: JSON.stringify(filters) },
                filterKey: { S: filterKey },
                createdAt: { N: Date.now().toString() },
                ttl: { N: Math.floor(Date.now() / 1000 + 86400).toString() }, // 24 hour TTL
            };

            const putCommand = new PutItemCommand({
                TableName: GITHUB_CHECKS_TABLE,
                Item: item,
            });

            await dynamoClient.send(putCommand);

            console.log('Stored GitHub event subscription in DynamoDB:', { filterKey, filters });

            // Send immediate subscription confirmation
            const { SQSClient, SendMessageCommand } = await import('@aws-sdk/client-sqs');
            const sqsClient = new SQSClient({});

            const filterDescription = Object.keys(filters).length > 0
                ? `Filters: ${JSON.stringify(filters)}`
                : 'No filters (will match all events)';

            const subscriptionContent = {
                type: 'toolresult',
                id: id,
                call_id: call_id,
                content: [
                    {
                        type: 'text',
                        text: `Subscription ID: ${id}\n${filterDescription}`,
                    },
                ],
            };

            const subscriptionMessage = {
                content: subscriptionContent,
                group_id: group_id,
            };

            const sendCommand = new SendMessageCommand({
                QueueUrl: input_queue_url,
                MessageBody: JSON.stringify(subscriptionMessage),
            });

            await sqsClient.send(sendCommand);
            console.log('Sent subscription confirmation to input queue');
        } catch (error) {
            console.error('Error storing GitHub event subscription:', error);

            // Send error message directly to input queue
            const { SQSClient, SendMessageCommand } = await import('@aws-sdk/client-sqs');
            const sqsClient = new SQSClient({});

            const errorContent = {
                type: 'toolresult',
                id: id,
                call_id: call_id,
                content: [
                    {
                        type: 'text',
                        text: `Failed to register GitHub event subscription: ${error.message}`,
                    },
                ],
            };

            const errorMessage = {
                content: errorContent,
                group_id: group_id,
            };

            const sendCommand = new SendMessageCommand({
                QueueUrl: input_queue_url,
                MessageBody: JSON.stringify(errorMessage),
            });

            await sqsClient.send(sendCommand);
            console.log('Sent error message to input queue');
        }
    }

    return {
        statusCode: 200,
        body: JSON.stringify({ ok: true }),
    };
};
