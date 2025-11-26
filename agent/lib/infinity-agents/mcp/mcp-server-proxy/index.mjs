import { spawn, execSync } from 'child_process';
import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';

const sqsClient = new SQSClient({});

// MCP server configuration from environment
const MCP_SERVER_COMMAND = process.env.MCP_SERVER_COMMAND ? JSON.parse(process.env.MCP_SERVER_COMMAND) : [];
const MCP_SERVER_ENV = process.env.MCP_SERVER_ENV ? JSON.parse(process.env.MCP_SERVER_ENV) : {};

// Pre-install MCP server during Lambda initialization (cold start)
// This happens outside the handler so it doesn't count against request timeout
let installationComplete = false;
let installationError = null;

async function ensureServerInstalled() {
    if (installationComplete) {
        return;
    }

    if (installationError) {
        throw installationError;
    }

    console.log('=== MCP Server Installation Starting ===');
    console.log('Command:', MCP_SERVER_COMMAND);

    try {
        // If using npx, pre-install the package
        if (MCP_SERVER_COMMAND[0] === 'npx') {
            // Extract package name from args (skip -y flag)
            const packageName = MCP_SERVER_COMMAND.slice(1).find(arg => !arg.startsWith('-'));
            
            if (packageName) {
                console.log(`Installing ${packageName}...`);
                const startTime = Date.now();
                
                // Run npm install globally in /tmp to cache the package
                const installCmd = `npm install -g --prefix /tmp ${packageName}`;
                console.log(`Running: ${installCmd}`);
                
                try {
                    execSync(installCmd, {
                        stdio: 'inherit',
                        env: { ...process.env, HOME: '/tmp' }
                    });
                } catch (error) {
                    console.error('Install failed, but continuing (npx might still work)');
                    // Don't throw - npx might still work
                }
                
                const duration = Date.now() - startTime;
                console.log(`Installation completed in ${duration}ms`);
            }
        }

        installationComplete = true;
        console.log('=== MCP Server Installation Complete ===');
    } catch (error) {
        console.error('=== MCP Server Installation Failed ===');
        console.error('Error:', error.message);
        installationError = error;
        throw error;
    }
}

// Start installation immediately on cold start
const installPromise = ensureServerInstalled().catch(err => {
    console.error('Background installation failed:', err);
});

class MCPClient {
    constructor() {
        this.process = null;
        this.messageId = 0;
        this.pendingRequests = new Map();
        this.buffer = '';
    }

    async start() {
        await installPromise;
        return new Promise((resolve, reject) => {
            const [cmd, ...args] = MCP_SERVER_COMMAND;
            console.log('Starting MCP server:', cmd, args);
            
            this.process = spawn(cmd, args, {
                env: {
                    ...process.env,
                    ...MCP_SERVER_ENV,
                    HOME: '/tmp',
                    TMPDIR: '/tmp',
                },
                stdio: ['pipe', 'pipe', 'pipe'],
            });

            this.process.stdout.on('data', (data) => {
                this.buffer += data.toString();
                this.processBuffer();
            });

            this.process.stderr.on('data', (data) => {
                console.error('MCP server stderr:', data.toString());
            });

            this.process.on('error', (error) => {
                console.error('MCP server process error:', error);
                reject(error);
            });

            this.process.on('exit', (code) => {
                console.log('MCP server exited with code:', code);
            });

            // Initialize the MCP connection
            this.sendRequest('initialize', {
                protocolVersion: '2024-11-05',
                capabilities: {
                    roots: { listChanged: false },
                    sampling: {},
                },
                clientInfo: {
                    name: 'infinity-agents-mcp-proxy',
                    version: '1.0.0',
                },
            }).then(() => {
                console.log('MCP server initialized');
                resolve();
            }).catch(reject);
        });
    }

    processBuffer() {
        const lines = this.buffer.split('\n');
        this.buffer = lines.pop() || '';

        for (const line of lines) {
            if (line.trim()) {
                try {
                    const message = JSON.parse(line);
                    this.handleMessage(message);
                } catch (error) {
                    console.error('Failed to parse MCP message:', line, error);
                }
            }
        }
    }

    handleMessage(message) {
        console.log('Received MCP message:', JSON.stringify(message));

        if (message.id !== undefined && this.pendingRequests.has(message.id)) {
            const { resolve, reject } = this.pendingRequests.get(message.id);
            this.pendingRequests.delete(message.id);

            if (message.error) {
                reject(new Error(message.error.message || 'MCP error'));
            } else {
                resolve(message.result);
            }
        }
    }

    sendRequest(method, params) {
        return new Promise((resolve, reject) => {
            const id = ++this.messageId;
            const request = {
                jsonrpc: '2.0',
                id,
                method,
                params,
            };

            this.pendingRequests.set(id, { resolve, reject });

            const requestStr = JSON.stringify(request) + '\n';
            console.log('Sending MCP request:', requestStr.trim());
            this.process.stdin.write(requestStr);

            // Timeout after 45 seconds (give more time for first request)
            setTimeout(() => {
                if (this.pendingRequests.has(id)) {
                    this.pendingRequests.delete(id);
                    reject(new Error('MCP request timeout'));
                }
            }, 45000);
        });
    }

    async listTools() {
        return this.sendRequest('tools/list', {});
    }

    async invokeTool(toolName, args) {
        return this.sendRequest('tools/call', {
            name: toolName,
            arguments: args,
        });
    }

    stop() {
        if (this.process) {
            this.process.kill();
            this.process = null;
        }
    }
}

export const handler = async (event) => {
    console.log('Received event:', JSON.stringify(event, null, 2));

    // Ensure installation is complete before processing
    await ensureServerInstalled();

    const mcpClient = new MCPClient();

    try {
        await mcpClient.start();

        for (const record of event.Records) {
            const request = JSON.parse(record.body);
            const { arguments: args, id, call_id, input_queue_url, group_id, operation } = request;

            console.log('Processing MCP request:', { operation, args, id, call_id });

            try {
                let result;

                // Determine operation from tool name suffix
                if (operation && operation.endsWith('_list_tools')) {
                    // list_tools operation
                    console.log('Listing tools');
                    const toolsResult = await mcpClient.listTools();
                    result = formatToolsList(toolsResult);
                } else if (operation && operation.endsWith('_invoke_tool')) {
                    // invoke_tool operation
                    const toolName = args.tool_name;
                    const toolArgs = args.arguments || {};

                    console.log('Invoking tool:', toolName, 'with args:', toolArgs);
                    const invokeResult = await mcpClient.invokeTool(toolName, toolArgs);
                    result = formatToolResult(toolName, invokeResult);
                } else {
                    throw new Error(`Unknown operation: ${operation}`);
                }

                // Send success result to agent input queue
                const toolResultContent = {
                    type: 'toolresult',
                    id: id,
                    call_id: call_id,
                    content: [
                        {
                            type: 'text',
                            text: result,
                        },
                    ],
                };

                const toolResultMessage = {
                    content: toolResultContent,
                    group_id: group_id,
                };

                const sendCommand = new SendMessageCommand({
                    QueueUrl: input_queue_url,
                    MessageBody: JSON.stringify(toolResultMessage),
                    MessageAttributes: {
                        ConversationGroupId: {
                            DataType: 'String',
                            StringValue: group_id,
                        },
                    },
                });

                await sqsClient.send(sendCommand);
                console.log('Sent tool result to input queue');
            } catch (error) {
                console.error('Error processing MCP request:', error);

                // Send error message to agent input queue
                const errorContent = {
                    type: 'toolresult',
                    id: id,
                    call_id: call_id,
                    content: [
                        {
                            type: 'text',
                            text: `MCP tool error: ${error.message}`,
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
                    MessageAttributes: {
                        ConversationGroupId: {
                            DataType: 'String',
                            StringValue: group_id,
                        },
                    },
                });

                await sqsClient.send(sendCommand);
                console.log('Sent error message to input queue');
            }
        }

        return {
            statusCode: 200,
            body: JSON.stringify({ ok: true }),
        };
    } finally {
        mcpClient.stop();
    }
};

function formatToolsList(toolsResult) {
    const tools = toolsResult.tools || [];
    
    if (tools.length === 0) {
        return 'No tools available from this MCP server.';
    }

    let result = `Available tools (${tools.length}):\n\n`;
    
    for (const tool of tools) {
        result += `**${tool.name}**\n`;
        result += `${tool.description || 'No description'}\n`;
        
        if (tool.inputSchema) {
            result += `Parameters: ${JSON.stringify(tool.inputSchema, null, 2)}\n`;
        }
        
        result += '\n';
    }

    return result;
}

function formatToolResult(toolName, invokeResult) {
    let result = `Tool "${toolName}" completed.\n\n`;

    if (invokeResult.content) {
        for (const item of invokeResult.content) {
            if (item.type === 'text') {
                result += item.text + '\n';
            } else if (item.type === 'image') {
                result += `[Image: ${item.mimeType}]\n`;
            } else if (item.type === 'resource') {
                result += `[Resource: ${item.resource?.uri}]\n`;
            }
        }
    }

    if (invokeResult.isError) {
        result = `Tool "${toolName}" failed: ${result}`;
    }

    return result;
}
