import { EC2Client, RunInstancesCommand } from '@aws-sdk/client-ec2';
import { sendToolResult } from 'rap-js';

const ec2Client = new EC2Client({});

const TOOLSET_MANIFEST = {
  name: 'ec2',
  description: 'EC2 instance management tools',
  endpoint: '',
  tools: [
    {
      name: 'create_ec2',
      description: 'Create an EC2 instance. You will be notified when the instance is running.',
      inputSchema: {
        type: 'object',
        properties: {
          instance_type: { type: 'string', description: "EC2 instance type (e.g., 't3.micro', 't3.small')." },
          ami_id: { type: 'string', description: 'AMI ID to use for the instance.' },
          name: { type: 'string', description: 'Name tag for the instance.' },
          key_name: { type: 'string', description: 'SSH key pair name for accessing the instance. Optional.' },
        },
        required: ['instance_type', 'ami_id', 'name'],
      },
    },
  ],
};

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
    const { arguments: args, id, call_id, rap_receiver_url, group_id } = body;

    console.log('Processing create_ec2 request:', { args, id, call_id });

    try {
      const runCommand = new RunInstancesCommand({
        ImageId: args.ami_id,
        InstanceType: args.instance_type,
        MinCount: 1,
        MaxCount: 1,
        KeyName: args.key_name || undefined,
        TagSpecifications: [{
          ResourceType: 'instance',
          Tags: [
            { Key: 'Name', Value: args.name },
            { Key: 'CreatedBy', Value: 'InfinityAgents' },
            { Key: 'ToolCallId', Value: id },
            ...(call_id ? [{ Key: 'CallId', Value: call_id }] : []),
            { Key: 'GroupId', Value: group_id },
            { Key: 'InstanceType', Value: args.instance_type },
            { Key: 'AmiId', Value: args.ami_id },
          ],
        }],
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
