import { EC2Client, DescribeTagsCommand } from '@aws-sdk/client-ec2';
import { sendToolResult } from 'rap-js';

const ec2Client = new EC2Client({});

const RAP_RECEIVER_URL = process.env.RAP_RECEIVER_URL;

export const handler = async (event) => {
    console.log('Received EC2 state change event:', JSON.stringify(event, null, 2));

    const instanceId = event.detail['instance-id'];
    const state = event.detail.state;

    console.log(`Instance ${instanceId} changed to state: ${state}`);

    if (state !== 'running') {
        console.log('Ignoring non-running state');
        return { statusCode: 200, body: 'OK' };
    }

    try {
        const describeTagsCommand = new DescribeTagsCommand({
            Filters: [{ Name: 'resource-id', Values: [instanceId] }],
        });

        const tagsResult = await ec2Client.send(describeTagsCommand);
        const tags = {};
        for (const tag of tagsResult.Tags || []) {
            tags[tag.Key] = tag.Value;
        }

        console.log('Instance tags:', tags);

        if (tags.CreatedBy !== 'InfinityAgents') {
            console.log('Instance not created by InfinityAgents, ignoring');
            return { statusCode: 200, body: 'OK' };
        }

        const toolCallId = tags.ToolCallId;
        const callId = tags.CallId || null;
        const groupId = tags.GroupId;
        const instanceType = tags.InstanceType;
        const amiId = tags.AmiId;

        await sendToolResult(RAP_RECEIVER_URL, groupId, toolCallId, callId,
            `EC2 instance is now running! Type: ${instanceType}, AMI: ${amiId}, Instance ID: ${instanceId}`);

        console.log('Sent tool result via RAP');
        return { statusCode: 200, body: 'OK' };
    } catch (error) {
        console.error('Error processing EC2 state change:', error);
        throw error;
    }
};
