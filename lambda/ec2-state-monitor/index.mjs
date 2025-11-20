import { EC2Client, DescribeTagsCommand } from '@aws-sdk/client-ec2';
import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const ec2Client = new EC2Client({});
const sqsClient = new SQSClient({});

const INPUT_QUEUE_URL = process.env.INPUT_QUEUE_URL;

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
        // Get tags for the instance
        const describeTagsCommand = new DescribeTagsCommand({
            Filters: [
                {
                    Name: 'resource-id',
                    Values: [instanceId],
                },
            ],
        });

        const tagsResult = await ec2Client.send(describeTagsCommand);
        const tags = {};
        
        for (const tag of tagsResult.Tags || []) {
            tags[tag.Key] = tag.Value;
        }

        console.log('Instance tags:', tags);

        // Check if this instance was created by AgentZero
        if (tags.CreatedBy !== 'AgentZero') {
            console.log('Instance not created by AgentZero, ignoring');
            return { statusCode: 200, body: 'OK' };
        }

        // Extract tool call metadata from tags
        const toolCallId = tags.ToolCallId;
        const callId = tags.CallId || null;
        const groupId = tags.GroupId;
        const instanceType = tags.InstanceType;
        const amiId = tags.AmiId;

        // Create tool result message
        const toolResultContent = {
            type: 'toolresult',
            id: toolCallId,
            call_id: callId,
            content: [
                {
                    type: 'text',
                    text: `EC2 instance is now running! Type: ${instanceType}, AMI: ${amiId}, Instance ID: ${instanceId}`,
                },
            ],
        };

        const toolResultMessage = {
            content: toolResultContent,
            group_id: groupId,
        };

        // Send result to agent input queue
        const sendCommand = new SendMessageCommand({
            QueueUrl: INPUT_QUEUE_URL,
            MessageBody: JSON.stringify(toolResultMessage),
        });

        await sqsClient.send(sendCommand);
        console.log('Sent tool result to input queue');

        return { statusCode: 200, body: 'OK' };
    } catch (error) {
        console.error('Error processing EC2 state change:', error);
        throw error;
    }
};
