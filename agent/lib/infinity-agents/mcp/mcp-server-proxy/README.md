# MCP Server Proxy

Lambda function that bridges MCP servers into the RAP ecosystem. It exposes a standard RAP toolset interface (`/.well-known/rap-toolset`) and translates between MCP's synchronous JSON-RPC protocol and RAP's async HTTP contract. From the agent's perspective, MCP tools are indistinguishable from native RAP tools.

## How it works

The proxy runs as a Lambda with response streaming. When the agent runtime invokes a tool:

1. The proxy writes `OK` to the response stream immediately (RAP acknowledgement) and closes it
2. The runtime persists state and exits — zero compute while the tool runs
3. The proxy connects to the MCP server (stdio subprocess or HTTP endpoint)
4. Sends the JSON-RPC request (`tools/list` or `tools/call`)
5. Waits for the synchronous MCP response
6. POSTs the result back to the agent's callback URL via RAP (SigV4-signed)

This turns a synchronous MCP call into an asynchronous RAP call. The agent hibernates at zero cost and resumes when the result arrives.

## Transport modes

The proxy selects its transport based on environment variables. If `MCP_SERVER_URL` is set, it uses HTTP. If `MCP_SERVER_COMMAND` is set, it uses stdio.

### Stdio

Spawns the MCP server as a child process and communicates over stdin/stdout using JSON-RPC. The server is started fresh for each invocation and terminated afterward. For `npx`-based servers, packages are pre-installed during Lambda cold start to avoid repeated downloads.

Used by `LambdaMCPToolSet`:

```typescript
new LambdaMCPToolSet(this, 'GitHub', {
  name: 'github',
  command: ['npx', '-y', '@modelcontextprotocol/server-github'],
  env: { GITHUB_PERSONAL_ACCESS_TOKEN: '...' },
});
```

### Streamable HTTP

Connects to a remote MCP server over HTTP. Sends JSON-RPC requests as POSTs and handles both direct JSON responses and SSE (Server-Sent Events) streaming. Supports `Mcp-Session-Id` for session continuity across invocations.

Used by `HTTPMCPToolSet`:

```typescript
new HTTPMCPToolSet(this, 'Remote', {
  name: 'remote',
  url: 'https://mcp-server.example.com/mcp',
  headers: { 'X-API-Key': '...' },
});
```

## RAP toolset interface

The proxy serves a toolset definition at `GET /.well-known/rap-toolset` that exposes two wrapper tools per MCP server:

| Tool | MCP operation | Description |
|---|---|---|
| `{name}_list_tools` | `tools/list` | Discovers available tools from the MCP server. Initiates OAuth if required. |
| `{name}_invoke_tool` | `tools/call` | Calls a specific MCP tool by name with arguments. |

The `{name}` prefix comes from the `MCP_SERVER_NAME` environment variable (e.g., `github_list_tools`, `github_invoke_tool`).

`invoke_tool` accepts two parameters:
- `tool_name` (string, required) — the MCP tool to call
- `arguments` (object, optional) — arguments to pass through to the MCP tool

## OAuth

The proxy handles the full OAuth 2.0 authorization code flow for MCP servers that require it. Enabled when `OAUTH_TOKEN_TABLE` and `OAUTH_CALLBACK_URL` are set (configured automatically by `HTTPMCPToolSet` with the `oauth` option).

The flow:

1. Proxy sends a request to the MCP server, gets back `401` with a `WWW-Authenticate` header containing a `resource_metadata` URL
2. Fetches the Protected Resource Metadata document, then the Authorization Server Metadata (tries RFC 8414, falls back to OpenID Connect discovery)
3. Gets client credentials — uses pre-configured `OAUTH_CLIENT_ID`/`OAUTH_CLIENT_SECRET` if available, otherwise performs Dynamic Client Registration (RFC 7591)
4. Generates a PKCE challenge (S256), builds the authorization URL, and sends it back to the agent via RAP so the user can authorize
5. User completes authorization, callback hits the proxy's API Gateway endpoint
6. Proxy exchanges the authorization code for tokens (supports both JSON and form-urlencoded responses), stores them in DynamoDB per user, and re-executes the original request with the new access token

Tokens are stored in a DynamoDB table keyed by `user_id` with TTL-based expiration. Client registrations are cached under `client:{resourceMetadataUrl}` keys.

## Environment variables

| Variable | Source | Description |
|---|---|---|
| `MCP_SERVER_COMMAND` | `LambdaMCPToolSet` | JSON array — command to spawn the MCP server (e.g., `["npx","-y","@modelcontextprotocol/server-github"]`) |
| `MCP_SERVER_ENV` | `LambdaMCPToolSet` | JSON object — environment variables passed to the subprocess |
| `MCP_SERVER_URL` | `HTTPMCPToolSet` | URL of the remote MCP HTTP endpoint |
| `MCP_SERVER_HEADERS` | `HTTPMCPToolSet` | JSON object — HTTP headers included with requests |
| `MCP_SERVER_NAME` | Both | Prefix for generated tool names (e.g., `github`) |
| `OAUTH_TOKEN_TABLE` | `HTTPMCPToolSet` (oauth) | DynamoDB table name for token and client registration storage |
| `OAUTH_CALLBACK_URL` | `HTTPMCPToolSet` (oauth) | API Gateway URL for the OAuth redirect callback |
| `OAUTH_CLIENT_ID` | `HTTPMCPToolSet` (oauth) | Pre-configured OAuth client ID (skips Dynamic Client Registration) |
| `OAUTH_CLIENT_SECRET` | `HTTPMCPToolSet` (oauth) | Pre-configured OAuth client secret |
