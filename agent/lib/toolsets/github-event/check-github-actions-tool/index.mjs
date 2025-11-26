import { DynamoDBClient, PutItemCommand } from '@aws-sdk/client-dynamodb';

const dynamoClient = new DynamoDBClient({});

const GITHUB_CHECKS_TABLE = process.env.GITHUB_CHECKS_TABLE;

export const handler = async (event) => {
    console.log('Received event:', JSON.stringify(event, null, 2));

    for (const record of event.Records) {
        const request = JSON.parse(record.body);
        const { arguments: args, id, call_id, input_queue_url, group_id } = request;

        console.log('Processing check_github_actions request:', { args, id, call_id });

        try {
            // Extract parameters
            const owner = args.owner;
            const repo = args.repo;
            const sha = args.sha; // commit SHA to match against head_sha from webhook
            const checkName = args.check_name; // optional: specific check/workflow name to wait for

            // Store mapping in DynamoDB
            // Use toolCallId in the sort key to allow multiple listeners for the same sha/check combination
            const item = {
                pk: { S: `${owner}/${repo}/${sha}` },
                sk: { S: `${checkName || 'ALL'}#${id}` },
                toolCallId: { S: id },
                callId: { S: call_id || '' },
                groupId: { S: group_id },
                inputQueueUrl: { S: input_queue_url },
                owner: { S: owner },
                repo: { S: repo },
                sha: { S: sha },
                checkName: { S: checkName || '' },
                createdAt: { N: Date.now().toString() },
                ttl: { N: Math.floor(Date.now() / 1000 + 86400).toString() }, // 24 hour TTL
            };

            const putCommand = new PutItemCommand({
                TableName: GITHUB_CHECKS_TABLE,
                Item: item,
            });

            await dynamoClient.send(putCommand);

            console.log('Stored GitHub check mapping in DynamoDB');
            console.log('Waiting for GitHub webhook to notify when check completes');

            // Send immediate subscription confirmation
            const { SQSClient, SendMessageCommand } = await import('@aws-sdk/client-sqs');
            const sqsClient = new SQSClient({});

            const subscriptionContent = {
                type: 'toolresult',
                id: id,
                call_id: call_id,
                content: [
                    {
                        type: 'text',
                        text: `Subscription ID: ${id}`,
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
            console.error('Error storing GitHub check mapping:', error);

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
                        text: `Failed to register GitHub Actions check: ${error.message}`,
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
