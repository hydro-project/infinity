import crypto from 'crypto';
import { DynamoDBClient, QueryCommand, DeleteItemCommand } from '@aws-sdk/client-dynamodb';
import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const dynamoClient = new DynamoDBClient({});
const sqsClient = new SQSClient({});

const GITHUB_CHECKS_TABLE = process.env.GITHUB_CHECKS_TABLE;
const GITHUB_WEBHOOK_SECRET = process.env.GITHUB_WEBHOOK_SECRET;

function verifyGitHubSignature(payload, signature) {
    if (!GITHUB_WEBHOOK_SECRET) {
        console.warn('GITHUB_WEBHOOK_SECRET not set, skipping signature verification');
        return true;
    }

    const hmac = crypto.createHmac('sha256', GITHUB_WEBHOOK_SECRET);
    const digest = 'sha256=' + hmac.update(payload).digest('hex');
    return crypto.timingSafeEqual(Buffer.from(signature), Buffer.from(digest));
}

export const handler = async (event) => {
    console.log('Received GitHub webhook:', JSON.stringify(event, null, 2));

    try {
        // Verify GitHub signature
        const signature = event.headers['x-hub-signature-256'] || event.headers['X-Hub-Signature-256'];
        const payload = event.body;

        if (signature && !verifyGitHubSignature(payload, signature)) {
            console.error('Invalid GitHub signature');
            return {
                statusCode: 401,
                body: JSON.stringify({ error: 'Invalid signature' }),
            };
        }

        const body = JSON.parse(payload);
        const eventType = event.headers['x-github-event'] || event.headers['X-GitHub-Event'];

        console.log('GitHub event type:', eventType);

        // Handle check_run and check_suite events
        if (eventType === 'check_run' || eventType === 'check_suite') {
            await handleCheckEvent(body, eventType);
        } else if (eventType === 'workflow_run') {
            await handleWorkflowRunEvent(body);
        } else if (eventType === 'status') {
            await handleStatusEvent(body);
        } else {
            console.log('Ignoring event type:', eventType);
        }

        return {
            statusCode: 200,
            body: JSON.stringify({ ok: true }),
        };
    } catch (error) {
        console.error('Error processing GitHub webhook:', error);
        return {
            statusCode: 500,
            body: JSON.stringify({ error: error.message }),
        };
    }
};

async function handleCheckEvent(body, eventType) {
    const action = body.action;
    const check = eventType === 'check_run' ? body.check_run : body.check_suite;
    const repository = body.repository;
    const owner = repository.owner.login;
    const repo = repository.name;
    const headSha = check.head_sha;
    const checkName = check.name || check.app?.name || '';
    const conclusion = check.conclusion;
    const status = check.status;

    console.log(`Check event: ${eventType} - ${action}, status: ${status}, conclusion: ${conclusion}`);

    // Only process completed checks
    if (status !== 'completed') {
        console.log('Check not completed yet, ignoring');
        return;
    }

    await processCompletedCheck(owner, repo, headSha, checkName, conclusion, check);
}

async function handleWorkflowRunEvent(body) {
    const action = body.action;
    const workflowRun = body.workflow_run;
    const repository = body.repository;
    const owner = repository.owner.login;
    const repo = repository.name;
    const headSha = workflowRun.head_sha;
    const workflowName = workflowRun.name;
    const conclusion = workflowRun.conclusion;
    const status = workflowRun.status;

    console.log(`Workflow run: ${action}, status: ${status}, conclusion: ${conclusion}`);

    // Only process completed workflows
    if (status !== 'completed') {
        console.log('Workflow not completed yet, ignoring');
        return;
    }

    await processCompletedCheck(owner, repo, headSha, workflowName, conclusion, workflowRun);
}

async function handleStatusEvent(body) {
    const state = body.state;
    const repository = body.repository;
    const owner = repository.owner.login;
    const repo = repository.name;
    const sha = body.sha;
    const context = body.context || '';
    const description = body.description || '';

    console.log(`Status event: ${state}, context: ${context}`);

    // Only process success/failure/error states
    if (!['success', 'failure', 'error'].includes(state)) {
        console.log('Status not final, ignoring');
        return;
    }

    await processCompletedCheck(owner, repo, sha, context, state, body);
}

async function processCompletedCheck(owner, repo, ref, checkName, conclusion, checkData) {
    const pk = `${owner}/${repo}/${ref}`;

    // Query for matching entries (both specific check name and ALL)
    const queryCommand = new QueryCommand({
        TableName: GITHUB_CHECKS_TABLE,
        KeyConditionExpression: 'pk = :pk',
        ExpressionAttributeValues: {
            ':pk': { S: pk },
        },
    });

    const queryResult = await dynamoClient.send(queryCommand);
    const items = queryResult.Items || [];

    console.log(`Found ${items.length} matching entries in DynamoDB`);

    for (const item of items) {
        const sk = item.sk.S;
        const storedCheckName = item.checkName?.S || '';

        // Match if waiting for ALL checks or specific check name matches
        if (sk === 'ALL' || sk === checkName || storedCheckName === checkName) {
            console.log(`Matched check: ${checkName}, sending result to agent`);

            const toolCallId = item.toolCallId.S;
            const callId = item.callId?.S || null;
            const groupId = item.groupId.S;
            const inputQueueUrl = item.inputQueueUrl.S;

            // Create tool result message
            const resultText = formatCheckResult(owner, repo, ref, checkName, conclusion, checkData);

            const toolResultContent = {
                type: 'toolresult',
                id: toolCallId,
                call_id: callId,
                content: [
                    {
                        type: 'text',
                        text: resultText,
                    },
                ],
            };

            const toolResultMessage = {
                content: toolResultContent,
                group_id: groupId,
            };

            // Send result to agent input queue
            const sendCommand = new SendMessageCommand({
                QueueUrl: inputQueueUrl,
                MessageBody: JSON.stringify(toolResultMessage),
            });

            await sqsClient.send(sendCommand);
            console.log('Sent tool result to input queue');

            // Delete the entry from DynamoDB
            const deleteCommand = new DeleteItemCommand({
                TableName: GITHUB_CHECKS_TABLE,
                Key: {
                    pk: { S: pk },
                    sk: { S: sk },
                },
            });

            await dynamoClient.send(deleteCommand);
            console.log('Deleted entry from DynamoDB');
        }
    }
}

function formatCheckResult(owner, repo, ref, checkName, conclusion, checkData) {
    const repoUrl = `https://github.com/${owner}/${repo}`;
    const commitUrl = `${repoUrl}/commit/${ref}`;
    
    let status = conclusion;
    if (conclusion === 'success') {
        status = '✅ SUCCESS';
    } else if (conclusion === 'failure') {
        status = '❌ FAILURE';
    } else if (conclusion === 'error') {
        status = '⚠️ ERROR';
    } else if (conclusion === 'cancelled') {
        status = '🚫 CANCELLED';
    }

    let result = `GitHub Actions check completed!\n\n`;
    result += `Repository: ${owner}/${repo}\n`;
    result += `Commit: ${ref}\n`;
    result += `Check: ${checkName}\n`;
    result += `Status: ${status}\n`;
    
    if (checkData.html_url) {
        result += `Details: ${checkData.html_url}\n`;
    } else {
        result += `Commit: ${commitUrl}\n`;
    }

    return result;
}
