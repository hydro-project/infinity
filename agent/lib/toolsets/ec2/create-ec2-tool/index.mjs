import { EC2Client, RunInstancesCommand } from '@aws-sdk/client-ec2';
import { sendToolResult } from 'rap-js';

const ec2Client = new EC2Client({});

export const handler = awslambda.streamifyResponse(async (event, responseStream) => {
    // Immediately signal OK to the invoker so the leader doesn't block
    responseStream.write('OK');
    responseStream.end();

    try {
        const body = typeof event.body === 'string' ? JSON.parse(event.body) : event.body;
        const { arguments: args, id, call_id, rap_receiver_url, group_id } = body;

        console.log('Processing create_ec2 request:', { args, id, call_id });

        try {
            const instanceType = args.instance_type;
            const amiId = args.ami_id;
            const instanceName = args.name;
            const keyName = args.key_name;

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
            await sendToolResult(rap_receiver_url, group_id, id, call_id,
                `Failed to create EC2 instance: ${error.message}`);
        }
    } catch (error) {
        console.error('Error parsing request:', error);
    }
});
