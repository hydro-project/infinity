---
sidebar_position: 3
---

# In-Memory Local CLI

The Infinity Agent CLI lets you run a RAP agent locally from your terminal. It connects directly to Amazon Bedrock, keeps conversation history in memory, and gives you a live streaming interface to interact with the agent — no AWS infrastructure required.

## Quick Start

```bash
# From the repo root
cargo run -p infinity-agent-cli
```

The CLI needs Bedrock credentials in your environment. The standard `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` variables work, or any credential chain that the AWS SDK picks up (SSO, instance profile, etc.).

## How It Works

The CLI runs two concurrent tasks:

- **Agent loop** — reads messages from an in-memory channel, runs the prepare → completion → execute cycle against Bedrock, and emits display events.
- **Terminal UI** — captures keystrokes, renders streaming model output in the terminal's native scrollback, and keeps a fixed input bar at the bottom separated by a horizontal rule.

User input and tool-spawned messages (thread spawns, subscription events) all feed into the same `mpsc` channel, so the agent processes them in order just like the Lambda runtime processes SQS messages.

On startup, the CLI also launches a local HTTP callback server on a random port. This is the `callback_url` that gets passed to RAP tool servers, allowing them to POST results back to the agent.

### Terminal Interface

The terminal uses a VT100 scroll region so output scrolls naturally in your terminal's scrollback buffer. The bottom of the screen shows:

```
─────────────────────────────────────
> your input here
```

Output is color-coded:
- **Bold** — your input
- <span style={{color: '#3b82f6'}}>Blue ◆</span> — tool calls
- <span style={{color: '#22c55e'}}>Green ✓</span> — tool results
- <span style={{color: '#f97316'}}>Orange ⚡</span> — subscription events

You can type while the model is streaming — input is buffered and sent on Enter. Press `Ctrl+C` or `Ctrl+D` to exit.

## RAP Tool Servers

The CLI supports loading RAP tool servers via a local config file. This lets you develop and test tool servers locally before deploying them to Lambda.

### Configuration

Create a `rap-servers.json` file in the repo root (or set `RAP_CONFIG` to point elsewhere):

```json
{
  "tool_sets": [
    {
      "type": "toolset_server",
      "server_url": "http://127.0.0.1:3001"
    }
  ]
}
```

Each entry points to a local HTTP server that serves `/.well-known/rap-toolset` for discovery. The CLI fetches toolset definitions at startup and makes the tools available to the LLM.

### Running a local tool server

The `get-time` tool includes a standalone local server:

```bash
# Terminal 1: start the tool server
node agent/lib/toolsets/get-time/get-time-tool/local.mjs --port 3001

# Terminal 2: start the CLI (with rap-servers.json pointing to localhost:3001)
cargo run -p infinity-agent-cli
```

The tool server handles both discovery (`GET /.well-known/rap-toolset`) and invocations (`POST /`). When invoked, it acknowledges immediately and POSTs the result back to the CLI's callback URL using plain HTTP — no SigV4 signing needed for local development.

### Writing your own local tool server

Any HTTP server that implements the RAP protocol works. Your server needs to:

1. Serve a toolset manifest at `GET /.well-known/rap-toolset`
2. Accept tool invocations via `POST`, acknowledge with HTTP 200 immediately
3. POST a `tool_result` to the `callback_url` from the invocation payload

No authentication is needed for local development — the CLI's callback server accepts plain HTTP.

## Built-in Tools

The CLI ships with the same core tools as the Lambda runtime:

| Tool | Description |
|------|-------------|
| `sleep` | Hibernate for a fixed number of seconds (in-memory `tokio::time::sleep` timer) |
| `sleep_until` | Hibernate until a specific date/time/timezone (in-memory timer) |
| `sleep_until_event_or_input` | Hibernate indefinitely — pauses the agent loop until a new message arrives |
| `spawn_thread` | Creates a child thread with its own conversation history |
| `report_to_parent` | Sends a result back to the parent thread |
| `close_thread` | Marks a thread as complete |

## Limitations

The CLI is designed for local development and testing. There are a few things it can't do that the cloud runtime handles:

- **No persistent state.** Conversation history and thread state live in memory. If the process exits, everything is lost.
- **Process must stay running during hibernation.** All sleep tools (`sleep`, `sleep_until`, `sleep_until_event_or_input`) use in-memory timers and channels. The Lambda runtime exits and is restarted by SQS/EventBridge — the CLI process must stay alive for the duration.
- **RAP HTTP tools work, remote tools behind a firewall do not.** The CLI can invoke any RAP tool server reachable over HTTP from your machine. Local tool servers work out of the box. Remote tool servers that require SigV4 auth (Lambda Function URLs) or are behind a VPC/firewall won't be reachable — use the cloud runtime for those.
- **No OAuth flows.** OAuth challenges require a publicly reachable callback endpoint that the CLI doesn't expose.
- **Single model.** The CLI is hardcoded to `claude-haiku-4-5` via Bedrock. Change the model ID in `main.rs` if you need a different one.

## When to Use It

The CLI is useful for:

- Iterating on agent behavior without deploying infrastructure
- Testing conversation flows and threading logic
- Debugging tool call sequences
- Developing and testing RAP tool servers locally
- Developing new built-in tools before wiring them into the CDK stack
