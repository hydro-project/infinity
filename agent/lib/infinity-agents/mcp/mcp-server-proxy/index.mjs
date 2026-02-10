import { spawn, execSync } from 'child_process';
import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs';
import { DynamoDBClient, GetItemCommand, PutItemCommand } from '@aws-sdk/client-dynamodb';

const sqsClient = new SQSClient({});
const dynamoClient = new DynamoDBClient({});

// MCP server configuration from environment
const MCP_SERVER_COMMAND = process.env.MCP_SERVER_COMMAND ? JSON.parse(process.env.MCP_SERVER_COMMAND) : [];
const MCP_SERVER_ENV = process.env.MCP_SERVER_ENV ? JSON.parse(process.env.MCP_SERVER_ENV) : {};
const MCP_SERVER_URL = process.env.MCP_SERVER_URL || null;
const MCP_SERVER_HEADERS = process.env.MCP_SERVER_HEADERS ? JSON.parse(process.env.MCP_SERVER_HEADERS) : {};

// OAuth configuration
const OAUTH_TOKEN_TABLE = process.env.OAUTH_TOKEN_TABLE || null;
const OAUTH_CALLBACK_URL = process.env.OAUTH_CALLBACK_URL || null;

// Pre-configured OAuth client (for providers that don't support Dynamic Client Registration)
const OAUTH_CLIENT_ID = process.env.OAUTH_CLIENT_ID || null;
const OAUTH_CLIENT_SECRET = process.env.OAUTH_CLIENT_SECRET || null;

// Pre-install MCP server during Lambda initialization (cold start)
let installationComplete = false;
let installationError = null;

async function ensureServerInstalled() {
    if (installationComplete) return;
    if (installationError) throw installationError;

    console.log('=== MCP Server Installation Starting ===');
    console.log('Command:', MCP_SERVER_COMMAND);

    try {
        if (MCP_SERVER_COMMAND[0] === 'npx') {
            const packageName = MCP_SERVER_COMMAND.slice(1).find(arg => !arg.startsWith('-'));
            if (packageName) {
                console.log(`Installing ${packageName}...`);
                const startTime = Date.now();
                const installCmd = `npm install -g --prefix /tmp ${packageName}`;
                console.log(`Running: ${installCmd}`);
                try {
                    execSync(installCmd, { stdio: 'inherit', env: { ...process.env, HOME: '/tmp' } });
                } catch (error) {
                    console.error('Install failed, but continuing (npx might still work)');
                }
                console.log(`Installation completed in ${Date.now() - startTime}ms`);
            }
        }
        installationComplete = true;
        console.log('=== MCP Server Installation Complete ===');
    } catch (error) {
        console.error('=== MCP Server Installation Failed ===');
        installationError = error;
        throw error;
    }
}

const installPromise = ensureServerInstalled().catch(err => {
    console.error('Background installation failed:', err);
});

// Token storage helpers
async function getStoredToken(userId) {
    if (!OAUTH_TOKEN_TABLE || !userId) return null;
    
    try {
        const result = await dynamoClient.send(new GetItemCommand({
            TableName: OAUTH_TOKEN_TABLE,
            Key: { user_id: { S: userId } },
        }));
        
        if (result.Item?.access_token?.S) {
            return {
                accessToken: result.Item.access_token.S,
                refreshToken: result.Item.refresh_token?.S,
                expiresAt: result.Item.expires_at?.N ? parseInt(result.Item.expires_at.N) : null,
            };
        }
    } catch (error) {
        console.error('Error fetching token:', error);
    }
    return null;
}

async function storeToken(userId, tokenData) {
    if (!OAUTH_TOKEN_TABLE || !userId) return;
    
    const item = {
        user_id: { S: userId },
        access_token: { S: tokenData.accessToken },
        updated_at: { N: String(Date.now()) },
    };
    
    if (tokenData.refreshToken) {
        item.refresh_token = { S: tokenData.refreshToken };
    }
    if (tokenData.expiresAt) {
        item.expires_at = { N: String(tokenData.expiresAt) };
    }
    
    await dynamoClient.send(new PutItemCommand({
        TableName: OAUTH_TOKEN_TABLE,
        Item: item,
    }));
}

// Store pending OAuth request for callback
async function storePendingOAuthRequest(requestId, requestData) {
    if (!OAUTH_TOKEN_TABLE) return;
    
    await dynamoClient.send(new PutItemCommand({
        TableName: OAUTH_TOKEN_TABLE,
        Item: {
            user_id: { S: `pending:${requestId}` },
            request_data: { S: JSON.stringify(requestData) },
            ttl: { N: String(Math.floor(Date.now() / 1000) + 600) }, // 10 min TTL
        },
    }));
}

async function getPendingOAuthRequest(requestId) {
    if (!OAUTH_TOKEN_TABLE) return null;
    
    try {
        const result = await dynamoClient.send(new GetItemCommand({
            TableName: OAUTH_TOKEN_TABLE,
            Key: { user_id: { S: `pending:${requestId}` } },
        }));
        
        if (result.Item?.request_data?.S) {
            return JSON.parse(result.Item.request_data.S);
        }
    } catch (error) {
        console.error('Error fetching pending request:', error);
    }
    return null;
}


/**
 * Custom error for OAuth required scenarios
 */
class OAuthRequiredError extends Error {
    constructor(resourceMetadataUrl, message = 'OAuth authorization required') {
        super(message);
        this.name = 'OAuthRequiredError';
        this.resourceMetadataUrl = resourceMetadataUrl;
    }
}

/**
 * Fetch and cache OAuth metadata from Protected Resource Metadata (PRM) document
 */
let cachedOAuthMetadata = null;

async function fetchOAuthMetadata(resourceMetadataUrl) {
    if (cachedOAuthMetadata?.resourceMetadataUrl === resourceMetadataUrl) {
        return cachedOAuthMetadata;
    }

    console.log('Fetching Protected Resource Metadata from:', resourceMetadataUrl);
    
    // Fetch the Protected Resource Metadata document
    const prmResponse = await fetch(resourceMetadataUrl);
    if (!prmResponse.ok) {
        throw new Error(`Failed to fetch PRM: ${prmResponse.status}`);
    }
    const prm = await prmResponse.json();
    console.log('Protected Resource Metadata:', JSON.stringify(prm));

    // Get the authorization server URL from PRM
    const authServerUrl = prm.authorization_servers?.[0];
    if (!authServerUrl) {
        throw new Error('No authorization server found in PRM');
    }

    // Fetch the Authorization Server Metadata
    // Try RFC 8414 first, then fall back to OpenID Connect discovery
    const authServerBase = authServerUrl.endsWith('/') ? authServerUrl : authServerUrl + '/';
    
    let asm = null;
    
    // Try OAuth 2.0 Authorization Server Metadata (RFC 8414)
    const oauthMetadataUrl = authServerBase + '.well-known/oauth-authorization-server';
    console.log('Trying OAuth Authorization Server Metadata:', oauthMetadataUrl);
    
    let asmResponse = await fetch(oauthMetadataUrl);
    if (asmResponse.ok) {
        asm = await asmResponse.json();
        console.log('Found OAuth Authorization Server Metadata');
    } else {
        // Fall back to OpenID Connect discovery
        const oidcMetadataUrl = authServerBase + '.well-known/openid-configuration';
        console.log('Trying OpenID Connect discovery:', oidcMetadataUrl);
        
        asmResponse = await fetch(oidcMetadataUrl);
        if (asmResponse.ok) {
            asm = await asmResponse.json();
            console.log('Found OpenID Connect configuration');
        }
    }
    
    if (!asm) {
        throw new Error(`Failed to fetch Authorization Server Metadata from either endpoint`);
    }
    
    console.log('Authorization Server Metadata:', JSON.stringify(asm));

    // Derive missing endpoints from issuer URL if not provided
    // This handles minimal OIDC configs that only have issuer
    const issuer = asm.issuer || authServerUrl;
    const issuerBase = issuer.endsWith('/') ? issuer.slice(0, -1) : issuer;
    
    // Standard OAuth/OIDC endpoint paths (used as fallback)
    const authorizationEndpoint = asm.authorization_endpoint || `${issuerBase}/login/oauth/authorize`;
    const tokenEndpoint = asm.token_endpoint || `${issuerBase}/login/oauth/access_token`;
    const registrationEndpoint = asm.registration_endpoint || null; // No standard fallback for DCR

    if (!asm.authorization_endpoint || !asm.token_endpoint) {
        console.log('Using derived endpoints:', { authorizationEndpoint, tokenEndpoint });
    }

    cachedOAuthMetadata = {
        resourceMetadataUrl,
        resource: prm.resource,
        authorizationEndpoint,
        tokenEndpoint,
        registrationEndpoint,
        scopes: prm.scopes_supported || asm.scopes_supported || [],
        codeChallengeMethodsSupported: asm.code_challenge_methods_supported || [],
    };

    return cachedOAuthMetadata;
}

/**
 * Get OAuth client credentials
 * Uses pre-configured credentials if available, otherwise tries Dynamic Client Registration
 */
async function getOrCreateClientRegistration(oauthMetadata) {
    // If pre-configured client credentials are provided, use them
    if (OAUTH_CLIENT_ID) {
        console.log('Using pre-configured OAuth client');
        return {
            clientId: OAUTH_CLIENT_ID,
            clientSecret: OAUTH_CLIENT_SECRET,
        };
    }

    // Otherwise, try Dynamic Client Registration
    if (!OAUTH_TOKEN_TABLE) {
        throw new Error('OAUTH_TOKEN_TABLE required for Dynamic Client Registration');
    }
    
    // Use a consistent key for the client registration based on the auth server
    const registrationKey = `client:${oauthMetadata.resourceMetadataUrl}`;
    
    // Try to get existing registration
    try {
        const result = await dynamoClient.send(new GetItemCommand({
            TableName: OAUTH_TOKEN_TABLE,
            Key: { user_id: { S: registrationKey } },
        }));
        
        if (result.Item?.client_id?.S) {
            console.log('Using existing client registration from DynamoDB');
            return {
                clientId: result.Item.client_id.S,
                clientSecret: result.Item.client_secret?.S || null,
            };
        }
    } catch (error) {
        console.error('Error fetching client registration:', error);
    }
    
    // No existing registration, perform Dynamic Client Registration
    if (!oauthMetadata.registrationEndpoint) {
        throw new Error('Authorization server does not support Dynamic Client Registration. Please provide OAUTH_CLIENT_ID and OAUTH_CLIENT_SECRET environment variables.');
    }
    
    console.log('Performing Dynamic Client Registration at:', oauthMetadata.registrationEndpoint);
    
    // Build redirect URIs
    const redirectUris = OAUTH_CALLBACK_URL ? [OAUTH_CALLBACK_URL] : [];
    
    // Client metadata for registration (RFC 7591)
    const clientMetadata = {
        client_name: 'Infinity Agents MCP Proxy',
        redirect_uris: redirectUris,
        grant_types: ['authorization_code', 'refresh_token'],
        response_types: ['code'],
        token_endpoint_auth_method: 'none', // Public client (PKCE)
        application_type: 'web',
        // Request scopes if available
        scope: oauthMetadata.scopes?.join(' ') || undefined,
    };
    
    const response = await fetch(oauthMetadata.registrationEndpoint, {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
        },
        body: JSON.stringify(clientMetadata),
    });
    
    if (!response.ok) {
        const errorText = await response.text();
        console.error('Dynamic Client Registration failed:', response.status, errorText);
        throw new Error(`Dynamic Client Registration failed: ${response.status} - ${errorText}`);
    }
    
    const registration = await response.json();
    console.log('Client registered successfully, client_id:', registration.client_id);
    
    // Store the registration for future use
    const item = {
        user_id: { S: registrationKey },
        client_id: { S: registration.client_id },
        registration_response: { S: JSON.stringify(registration) },
        created_at: { N: String(Date.now()) },
    };
    
    if (registration.client_secret) {
        item.client_secret = { S: registration.client_secret };
    }
    
    await dynamoClient.send(new PutItemCommand({
        TableName: OAUTH_TOKEN_TABLE,
        Item: item,
    }));
    
    return {
        clientId: registration.client_id,
        clientSecret: registration.client_secret || null,
    };
}

/**
 * Generate PKCE code verifier and challenge
 */
function generatePKCE() {
    // Generate a random code verifier (43-128 characters)
    const array = new Uint8Array(32);
    crypto.getRandomValues(array);
    const codeVerifier = base64UrlEncode(array);
    
    // Generate code challenge using SHA-256
    const encoder = new TextEncoder();
    const data = encoder.encode(codeVerifier);
    return crypto.subtle.digest('SHA-256', data).then(hash => {
        const codeChallenge = base64UrlEncode(new Uint8Array(hash));
        return { codeVerifier, codeChallenge };
    });
}

function base64UrlEncode(buffer) {
    const base64 = btoa(String.fromCharCode(...buffer));
    return base64.replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

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
                env: { ...process.env, ...MCP_SERVER_ENV, HOME: '/tmp', TMPDIR: '/tmp' },
                stdio: ['pipe', 'pipe', 'pipe'],
            });

            this.process.stdout.on('data', (data) => {
                this.buffer += data.toString();
                this.processBuffer();
            });

            this.process.stderr.on('data', (data) => {
                console.error('MCP server stderr:', data.toString());
            });

            this.process.on('error', reject);
            this.process.on('exit', (code) => console.log('MCP server exited with code:', code));

            this.sendRequest('initialize', {
                protocolVersion: '2024-11-05',
                capabilities: { roots: { listChanged: false }, sampling: {} },
                clientInfo: { name: 'infinity-agents-mcp-proxy', version: '1.0.0' },
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
                    this.handleMessage(JSON.parse(line));
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
            const request = { jsonrpc: '2.0', id, method, params };

            this.pendingRequests.set(id, { resolve, reject });
            const requestStr = JSON.stringify(request) + '\n';
            console.log('Sending MCP request:', requestStr.trim());
            this.process.stdin.write(requestStr);

            setTimeout(() => {
                if (this.pendingRequests.has(id)) {
                    this.pendingRequests.delete(id);
                    reject(new Error('MCP request timeout'));
                }
            }, 45000);
        });
    }

    async listTools() { return this.sendRequest('tools/list', {}); }
    async invokeTool(toolName, args) { return this.sendRequest('tools/call', { name: toolName, arguments: args }); }
    stop() { if (this.process) { this.process.kill(); this.process = null; } }
}


/**
 * HTTP-based MCP client with OAuth support
 */
class HTTPMCPClient {
    constructor(url, headers = {}, accessToken = null) {
        this.url = url;
        this.baseHeaders = headers;
        this.accessToken = accessToken;
        this.messageId = 0;
        this.sessionId = null;
    }

    get headers() {
        const h = { ...this.baseHeaders };
        if (this.accessToken) {
            h['Authorization'] = `Bearer ${this.accessToken}`;
        }
        return h;
    }

    async start() {
        const result = await this.sendRequest('initialize', {
            protocolVersion: '2024-11-05',
            capabilities: { roots: { listChanged: false }, sampling: {} },
            clientInfo: { name: 'infinity-agents-mcp-proxy', version: '1.0.0' },
        });
        console.log('HTTP MCP server initialized:', result);
        await this.sendNotification('notifications/initialized', {});
        return result;
    }

    async sendRequest(method, params) {
        const id = ++this.messageId;
        const request = { jsonrpc: '2.0', id, method, params };

        console.log('Sending HTTP MCP request:', JSON.stringify(request));

        const headers = {
            'Content-Type': 'application/json',
            'Accept': 'application/json, text/event-stream',
            ...this.headers,
        };

        if (this.sessionId) {
            headers['Mcp-Session-Id'] = this.sessionId;
        }

        const response = await fetch(this.url, {
            method: 'POST',
            headers,
            body: JSON.stringify(request),
        });

        const newSessionId = response.headers.get('Mcp-Session-Id');
        if (newSessionId) this.sessionId = newSessionId;

        // Check for OAuth required (401 with WWW-Authenticate header)
        if (response.status === 401) {
            const authHeader = response.headers.get('WWW-Authenticate');
            const body = await response.text();
            
            // Extract resource_metadata URL from WWW-Authenticate header
            // Format: Bearer resource_metadata="https://..."
            let resourceMetadataUrl = null;
            if (authHeader) {
                const match = authHeader.match(/resource_metadata="([^"]+)"/);
                if (match) {
                    resourceMetadataUrl = match[1];
                }
            }
            
            if (resourceMetadataUrl) {
                throw new OAuthRequiredError(resourceMetadataUrl);
            }
            throw new Error(`Unauthorized: ${body}`);
        }

        if (!response.ok) {
            const errorText = await response.text();
            throw new Error(`HTTP MCP request failed: ${response.status} ${response.statusText} - ${errorText}`);
        }

        const contentType = response.headers.get('Content-Type') || '';
        if (contentType.includes('text/event-stream')) {
            return this.handleSSEResponse(response, id);
        }

        const result = await response.json();
        console.log('Received HTTP MCP response:', JSON.stringify(result));

        if (result.error) {
            throw new Error(result.error.message || 'MCP error');
        }
        return result.result;
    }

    async handleSSEResponse(response, expectedId) {
        const text = await response.text();
        const lines = text.split('\n');
        let result = null;
        let currentData = '';

        for (const line of lines) {
            if (line.startsWith('data: ')) {
                currentData += line.slice(6);
            } else if (line === '' && currentData) {
                try {
                    const message = JSON.parse(currentData);
                    console.log('Received SSE message:', JSON.stringify(message));
                    if (message.id === expectedId) {
                        if (message.error) throw new Error(message.error.message || 'MCP error');
                        result = message.result;
                    }
                } catch (e) {
                    if (e.message.includes('MCP error')) throw e;
                    console.error('Failed to parse SSE data:', currentData, e);
                }
                currentData = '';
            }
        }

        if (result === null) throw new Error('No response received from SSE stream');
        return result;
    }

    async sendNotification(method, params) {
        const notification = { jsonrpc: '2.0', method, params };
        console.log('Sending HTTP MCP notification:', JSON.stringify(notification));

        const headers = { 'Content-Type': 'application/json', ...this.headers };
        if (this.sessionId) headers['Mcp-Session-Id'] = this.sessionId;

        const response = await fetch(this.url, {
            method: 'POST',
            headers,
            body: JSON.stringify(notification),
        });

        if (!response.ok && response.status !== 202 && response.status !== 204) {
            console.warn('Notification response:', response.status, await response.text());
        }
    }

    async listTools() { return this.sendRequest('tools/list', {}); }
    async invokeTool(toolName, args) { return this.sendRequest('tools/call', { name: toolName, arguments: args }); }
    stop() {}
}

function createMCPClient(accessToken = null) {
    if (MCP_SERVER_URL) {
        console.log('Using HTTP MCP client with URL:', MCP_SERVER_URL);
        return new HTTPMCPClient(MCP_SERVER_URL, MCP_SERVER_HEADERS, accessToken);
    } else {
        console.log('Using stdio MCP client with command:', MCP_SERVER_COMMAND);
        return new MCPClient();
    }
}


/**
 * Send a tool result back to the agent
 */
async function sendToolResult(inputQueueUrl, groupId, id, callId, text) {
    const toolResultMessage = {
        content: {
            type: 'toolresult',
            id,
            call_id: callId,
            content: [{ type: 'text', text }],
        },
        group_id: groupId,
    };

    await sqsClient.send(new SendMessageCommand({
        QueueUrl: inputQueueUrl,
        MessageBody: JSON.stringify(toolResultMessage),
        MessageGroupId: groupId,
        MessageDeduplicationId: `${id}-${Date.now()}`,
    }));
    console.log('Sent tool result to input queue');
}

/**
 * Send an OAuth URL to the agent (special message type that bypasses history)
 */
async function sendOAuthUrl(inputQueueUrl, groupId, id, callId, authUrl) {
    const oauthMessage = {
        content: {
            type: 'oauth_required',
            id,
            call_id: callId,
            auth_url: authUrl,
        },
        group_id: groupId,
    };

    await sqsClient.send(new SendMessageCommand({
        QueueUrl: inputQueueUrl,
        MessageBody: JSON.stringify(oauthMessage),
        MessageGroupId: groupId,
        MessageDeduplicationId: `${id}-${Date.now()}`,
    }));
    console.log('Sent OAuth URL to input queue');
}

/**
 * Unified handler for both SQS events (tool requests) and HTTP requests (OAuth callback)
 */
export const handler = async (event) => {
    console.log('Received event:', JSON.stringify(event, null, 2));

    // Detect if this is an HTTP request (API Gateway or Function URL) or SQS event
    // API Gateway REST API has httpMethod, API Gateway HTTP API has requestContext.http
    if (event.httpMethod || event.requestContext?.http || event.rawPath) {
        return handleOAuthCallback(event);
    }

    // Otherwise, handle as SQS event
    if (!MCP_SERVER_URL) {
        await ensureServerInstalled();
    }

    for (const record of event.Records) {
        const request = JSON.parse(record.body);
        const { arguments: args, id, call_id, input_queue_url, group_id, operation, user_id } = request;

        console.log('Processing MCP request:', { operation, args, id, call_id, user_id });

        try {
            // Get stored token for this user
            const tokenData = await getStoredToken(user_id);
            const mcpClient = createMCPClient(tokenData?.accessToken);

            try {
                await mcpClient.start();

                let result;
                if (operation?.endsWith('_list_tools')) {
                    console.log('Listing tools');
                    const toolsResult = await mcpClient.listTools();
                    result = formatToolsList(toolsResult);
                } else if (operation?.endsWith('_invoke_tool')) {
                    const toolName = args.tool_name;
                    const toolArgs = args.arguments || {};
                    console.log('Invoking tool:', toolName, 'with args:', toolArgs);
                    const invokeResult = await mcpClient.invokeTool(toolName, toolArgs);
                    result = formatToolResult(toolName, invokeResult);
                } else {
                    throw new Error(`Unknown operation: ${operation}`);
                }

                await sendToolResult(input_queue_url, group_id, id, call_id, result);
            } catch (error) {
                if (error instanceof OAuthRequiredError) {
                    // For list_tools, initiate OAuth flow
                    if (operation?.endsWith('_list_tools')) {
                        // Fetch OAuth metadata from the PRM document
                        const oauthMetadata = await fetchOAuthMetadata(error.resourceMetadataUrl);
                        
                        // Get or create client registration (Dynamic Client Registration)
                        const clientRegistration = await getOrCreateClientRegistration(oauthMetadata);
                        
                        // Generate PKCE challenge
                        const pkce = await generatePKCE();
                        
                        // Store the pending request with PKCE verifier, metadata, and client info
                        await storePendingOAuthRequest(id, {
                            operation, args, id, call_id, input_queue_url, group_id, user_id,
                            codeVerifier: pkce.codeVerifier,
                            tokenEndpoint: oauthMetadata.tokenEndpoint,
                            resource: oauthMetadata.resource,
                            clientId: clientRegistration.clientId,
                            clientSecret: clientRegistration.clientSecret,
                        });

                        // Build the authorization URL with the registered client ID
                        const authUrl = buildAuthorizationUrl(oauthMetadata, clientRegistration, id, user_id, pkce.codeChallenge);
                        await sendOAuthUrl(input_queue_url, group_id, id, call_id, authUrl);
                    } else {
                        // For other operations, tell the agent to call list_tools first
                        await sendToolResult(
                            input_queue_url, group_id, id, call_id,
                            'Authorization required. Please call the list_tools operation first to complete the OAuth flow.'
                        );
                    }
                } else {
                    throw error;
                }
            } finally {
                mcpClient.stop();
            }
        } catch (error) {
            console.error('Error processing MCP request:', error);
            await sendToolResult(input_queue_url, group_id, id, call_id, `MCP tool error: ${error.message}`);
        }
    }

    return { statusCode: 200, body: JSON.stringify({ ok: true }) };
};

/**
 * Build the OAuth authorization URL with all required parameters
 */
function buildAuthorizationUrl(oauthMetadata, clientRegistration, requestId, userId, codeChallenge) {
    const url = new URL(oauthMetadata.authorizationEndpoint);
    
    // Build redirect URI with our callback URL
    let redirectUri = OAUTH_CALLBACK_URL;
    if (redirectUri) {
        const callbackUrl = new URL(redirectUri);
        callbackUrl.searchParams.set('request_id', requestId);
        if (userId) callbackUrl.searchParams.set('user_id', userId);
        redirectUri = callbackUrl.toString();
    }
    
    // Standard OAuth 2.0 parameters
    url.searchParams.set('response_type', 'code');
    url.searchParams.set('client_id', clientRegistration.clientId);
    if (redirectUri) url.searchParams.set('redirect_uri', redirectUri);
    url.searchParams.set('state', requestId);
    
    // PKCE parameters (RFC 7636)
    url.searchParams.set('code_challenge', codeChallenge);
    url.searchParams.set('code_challenge_method', 'S256');
    
    // Request scopes if available
    if (oauthMetadata.scopes?.length > 0) {
        url.searchParams.set('scope', oauthMetadata.scopes.join(' '));
    }
    
    // Resource indicator (RFC 8707) if available
    if (oauthMetadata.resource) {
        url.searchParams.set('resource', oauthMetadata.resource);
    }
    
    return url.toString();
}


/**
 * Handle OAuth callback (invoked via Function URL)
 */
async function handleOAuthCallback(event) {
    console.log('OAuth callback received:', JSON.stringify(event, null, 2));

    const params = event.queryStringParameters || {};
    const { code, state, request_id, user_id, error, error_description } = params;

    // Use state as request_id if not provided directly
    const effectiveRequestId = request_id || state;

    if (error) {
        console.error('OAuth error:', error, error_description);
        return {
            statusCode: 400,
            headers: { 'Content-Type': 'text/html' },
            body: `<html><body><h1>Authorization Failed</h1><p>${error}: ${error_description || 'Unknown error'}</p></body></html>`,
        };
    }

    if (!code || !effectiveRequestId) {
        return {
            statusCode: 400,
            headers: { 'Content-Type': 'text/html' },
            body: '<html><body><h1>Invalid Request</h1><p>Missing authorization code or request ID.</p></body></html>',
        };
    }

    try {
        // Get the pending request
        const pendingRequest = await getPendingOAuthRequest(effectiveRequestId);
        if (!pendingRequest) {
            return {
                statusCode: 400,
                headers: { 'Content-Type': 'text/html' },
                body: '<html><body><h1>Request Expired</h1><p>The authorization request has expired. Please try again.</p></body></html>',
            };
        }

        // Exchange authorization code for access token
        const tokenData = await exchangeCodeForToken(code, pendingRequest);
        
        // Store the token
        const effectiveUserId = user_id || pendingRequest.user_id;
        if (effectiveUserId) {
            await storeToken(effectiveUserId, tokenData);
        }

        // Re-execute the original request with the new token
        const mcpClient = createMCPClient(tokenData.accessToken);
        
        try {
            await mcpClient.start();

            let result;
            if (pendingRequest.operation?.endsWith('_list_tools')) {
                const toolsResult = await mcpClient.listTools();
                result = formatToolsList(toolsResult);
            } else if (pendingRequest.operation?.endsWith('_invoke_tool')) {
                const toolName = pendingRequest.args.tool_name;
                const toolArgs = pendingRequest.args.arguments || {};
                const invokeResult = await mcpClient.invokeTool(toolName, toolArgs);
                result = formatToolResult(toolName, invokeResult);
            }

            // Send the result back to the agent
            await sendToolResult(
                pendingRequest.input_queue_url,
                pendingRequest.group_id,
                pendingRequest.id,
                pendingRequest.call_id,
                result
            );
        } finally {
            mcpClient.stop();
        }

        return {
            statusCode: 200,
            headers: { 'Content-Type': 'text/html' },
            body: '<html><body><h1>Authorization Successful</h1><p>You can close this window and return to the conversation.</p></body></html>',
        };
    } catch (error) {
        console.error('OAuth callback error:', error);
        return {
            statusCode: 500,
            headers: { 'Content-Type': 'text/html' },
            body: `<html><body><h1>Error</h1><p>${error.message}</p></body></html>`,
        };
    }
};

/**
 * Exchange authorization code for access token using the token endpoint
 */
async function exchangeCodeForToken(code, pendingRequest) {
    const { tokenEndpoint, codeVerifier, resource, clientId, clientSecret } = pendingRequest;
    
    if (!tokenEndpoint) {
        throw new Error('No token endpoint available');
    }
    
    if (!clientId) {
        throw new Error('No client_id available');
    }
    
    console.log('Exchanging code for token at:', tokenEndpoint);
    
    // Build redirect URI (must match what was used in authorization request)
    let redirectUri = OAUTH_CALLBACK_URL;
    if (redirectUri) {
        const callbackUrl = new URL(redirectUri);
        callbackUrl.searchParams.set('request_id', pendingRequest.id);
        if (pendingRequest.user_id) callbackUrl.searchParams.set('user_id', pendingRequest.user_id);
        redirectUri = callbackUrl.toString();
    }
    
    // Build token request body
    const params = new URLSearchParams();
    params.set('grant_type', 'authorization_code');
    params.set('code', code);
    params.set('client_id', clientId);
    if (redirectUri) params.set('redirect_uri', redirectUri);
    if (codeVerifier) params.set('code_verifier', codeVerifier);
    if (resource) params.set('resource', resource);
    
    // Build headers - include client secret if we have one (confidential client)
    const headers = {
        'Content-Type': 'application/x-www-form-urlencoded',
        'Accept': 'application/json', // Request JSON response (GitHub needs this)
    };
    
    // If we have a client secret, use HTTP Basic auth or include in body
    if (clientSecret) {
        // Use HTTP Basic authentication for confidential clients
        const credentials = btoa(`${clientId}:${clientSecret}`);
        headers['Authorization'] = `Basic ${credentials}`;
    }
    
    const response = await fetch(tokenEndpoint, {
        method: 'POST',
        headers,
        body: params.toString(),
    });
    
    if (!response.ok) {
        const errorText = await response.text();
        console.error('Token exchange failed:', response.status, errorText);
        throw new Error(`Token exchange failed: ${response.status} - ${errorText}`);
    }
    
    // Parse response - handle both JSON and form-urlencoded responses
    const contentType = response.headers.get('Content-Type') || '';
    let tokenResponse;
    
    if (contentType.includes('application/json')) {
        tokenResponse = await response.json();
    } else {
        // Parse as form-urlencoded (GitHub's default without Accept header)
        const text = await response.text();
        tokenResponse = Object.fromEntries(new URLSearchParams(text));
    }
    
    console.log('Token response received');
    
    return {
        accessToken: tokenResponse.access_token,
        refreshToken: tokenResponse.refresh_token || null,
        expiresAt: tokenResponse.expires_in 
            ? Date.now() + (tokenResponse.expires_in * 1000) 
            : null,
    };
}

function formatToolsList(toolsResult) {
    const tools = toolsResult.tools || [];
    if (tools.length === 0) return 'No tools available from this MCP server.';

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
            if (item.type === 'text') result += item.text + '\n';
            else if (item.type === 'image') result += `[Image: ${item.mimeType}]\n`;
            else if (item.type === 'resource') result += `[Resource: ${item.resource?.uri}]\n`;
        }
    }
    if (invokeResult.isError) result = `Tool "${toolName}" failed: ${result}`;
    return result;
}
