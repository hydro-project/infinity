import crypto from 'crypto';
import { DynamoDBClient, QueryCommand } from '@aws-sdk/client-dynamodb';
import { sendSyntheticEvent } from 'rap-js';

const dynamoClient = new DynamoDBClient({});

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

function extractEventData(body, eventType) {
    const data = { eventType, action: body.action, actor: body.sender?.login };

    if (body.head_sha) data.sha = body.head_sha;
    else if (body.sha) data.sha = body.sha;
    else if (body.after) data.sha = body.after;
    else if (body.pull_request?.head?.sha) data.sha = body.pull_request.head.sha;
    else if (body.check_run?.head_sha) data.sha = body.check_run.head_sha;
    else if (body.check_suite?.head_sha) data.sha = body.check_suite.head_sha;
    else if (body.workflow_run?.head_sha) data.sha = body.workflow_run.head_sha;

    if (body.pull_request?.number) data.prNumber = body.pull_request.number;
    else if (body.issue?.pull_request && body.issue?.number) data.prNumber = body.issue.number;
    else if (body.number && eventType === 'pull_request') data.prNumber = body.number;

    if (body.issue?.number && !body.issue?.pull_request) data.issueNumber = body.issue.number;
    else if (body.number && eventType === 'issues') data.issueNumber = body.number;

    if (body.ref) data.branch = body.ref.replace('refs/heads/', '');
    if (body.pull_request?.head?.ref) data.headBranch = body.pull_request.head.ref;
    if (body.pull_request?.base?.ref) data.baseBranch = body.pull_request.base.ref;

    return data;
}

function matchesFilters(filters, eventData) {
    if (Object.keys(filters).length === 0) return true;

    for (const [key, value] of Object.entries(filters)) {
        switch (key) {
            case 'eventType': if (eventData.eventType !== value) return false; break;
            case 'sha': if (eventData.sha !== value) return false; break;
            case 'prNumber': if (eventData.prNumber !== value) return false; break;
            case 'issueNumber': if (eventData.issueNumber !== value) return false; break;
            case 'action': if (eventData.action !== value) return false; break;
            case 'branch':
                if (eventData.branch !== value && eventData.headBranch !== value && eventData.baseBranch !== value) return false;
                break;
            case 'actor': if (eventData.actor !== value) return false; break;
        }
    }
    return true;
}

function formatEventResult(eventType, body) {
    return JSON.stringify({ event_type: eventType, payload: body }, null, 2);
}

export const handler = async (event) => {
    console.log('Received GitHub webhook:', JSON.stringify(event, null, 2));

    try {
        const signature = event.headers['x-hub-signature-256'] || event.headers['X-Hub-Signature-256'];
        const payload = event.body;

        if (signature && !verifyGitHubSignature(payload, signature)) {
            console.error('Invalid GitHub signature');
            return { statusCode: 401, body: JSON.stringify({ error: 'Invalid signature' }) };
        }

        const body = JSON.parse(payload);
        const eventType = event.headers['x-github-event'] || event.headers['X-GitHub-Event'];
        const repository = body.repository;

        if (!repository) {
            console.log('No repository in payload, ignoring');
            return { statusCode: 200, body: JSON.stringify({ ok: true }) };
        }

        const owner = repository.owner.login;
        const repo = repository.name;

        console.log(`GitHub event: ${eventType} for ${owner}/${repo}`);

        const eventData = extractEventData(body, eventType);
        console.log('Extracted event data:', eventData);

        const queryResult = await dynamoClient.send(new QueryCommand({
            TableName: GITHUB_CHECKS_TABLE,
            KeyConditionExpression: 'pk = :pk',
            ExpressionAttributeValues: { ':pk': { S: `${owner}/${repo}` } },
        }));

        const items = queryResult.Items || [];
        console.log(`Found ${items.length} subscriptions for ${owner}/${repo}`);

        for (const item of items) {
            const filters = JSON.parse(item.filters?.S || '{}');
            const filterKey = item.filterKey?.S || 'ALL';

            console.log(`Checking subscription with filters:`, filters);

            if (matchesFilters(filters, eventData)) {
                console.log(`Matched subscription: ${filterKey}`);

                const toolCallId = item.toolCallId.S;
                const groupId = item.groupId.S;
                const rapReceiverUrl = item.rapReceiverUrl.S;

                const resultText = formatEventResult(eventType, body);
                await sendSyntheticEvent(rapReceiverUrl, groupId, toolCallId, resultText);
                console.log('Sent event notification via RAP');
            }
        }

        return { statusCode: 200, body: JSON.stringify({ ok: true }) };
    } catch (error) {
        console.error('Error processing GitHub webhook:', error);
        return { statusCode: 500, body: JSON.stringify({ error: error.message }) };
    }
};
