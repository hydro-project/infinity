import { EC2Client, RunInstancesCommand } from '@aws-sdk/client-ec2';
import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const ec2Client = new EC2Client({});
const sqsClient = new SQSClient({});

export const handler = async (event) => {
    console.log('Received event:', JSON.stringify(event, null, 2));

    for (const record of event.Records) {
        const request = JSON.parse(record.body);
        const { arguments: args, id, call_id, input_queue_url, group_id } = request;

        console.log('Processing create_ec2 request:', { args, id, call_id });

        try {
            // Extract parameters
            const instanceType = args.instance_type;
            const amiId = args.ami_id;
            const instanceName = args.name;
            const keyName = args.key_name; // Optional

            // Create the EC2 instance with metadata tags
            const runCommand = new RunInstancesCommand({
                ImageId: amiId,
                InstanceType: instanceType,
                MinCount: 1,
                MaxCount: 1,
                KeyName: keyName || undefined,
                TagSpecifications: [
                    {
                        ResourceType: 'instance',
                        Tags: [
                            { Key: 'Name', Value: instanceName },
                            { Key: 'CreatedBy', Value: 'InfinityAgents' },
                            { Key: 'ToolCallId', Value: id },
                            ...(call_id ? [{ Key: 'CallId', Value: call_id }] : []),
                            { Key: 'GroupId', Value: group_id },
                            { Key: 'InstanceType', Value: instanceType },
                            { Key: 'AmiId', Value: amiId },
                        ],
                    },
                ],
            });

            const runResult = await ec2Client.send(runCommand);
            const instanceId = runResult.Instances[0].InstanceId;

            console.log('Created EC2 instance:', instanceId);
            console.log('EventBridge will notify when instance reaches running state');
        } catch (error) {
            console.error('Error creating EC2 instance:', error);

            // Send error message directly to input queue
            const errorContent = {
                type: 'toolresult',
                id: id,
                call_id: call_id,
                content: [
                    {
                        type: 'text',
                        text: `Failed to create EC2 instance: ${error.message}`,
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
                MessageGroupId: group_id,
                MessageDeduplicationId: `${id}-${Date.now()}`,
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
