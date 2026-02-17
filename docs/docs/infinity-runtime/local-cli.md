---
sidebar_position: 5
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

## Built-in Tools

The CLI ships with the same core tools as the Lambda runtime:

| Tool | Description |
|------|-------------|
| `sleep_until_event_or_input` | Hibernation — pauses the agent loop until a new message arrives |
| `spawn_thread` | Creates a child thread with its own conversation history |
| `report_to_parent` | Sends a result back to the parent thread |
| `close_thread` | Marks a thread as complete |

You can add more tools by modifying the `tool_impls` vector in `main.rs`.

## Limitations

The CLI is designed for local development and testing. There are a few things it can't do that the cloud runtime handles:

- **No persistent state.** Conversation history and thread state live in memory. If the process exits, everything is lost.
- **Process must stay running during hibernation.** When the agent calls `sleep_until_event_or_input`, the CLI just waits on the channel. The Lambda runtime uses EventBridge Scheduler to wake up later — the CLI can't do that.
- **No remote RAP tool servers.** The cloud runtime invokes tool servers via HTTP (Lambda Function URLs). The CLI doesn't have a callback URL, so tools that need to POST results back to the agent won't work. Local-only tool implementations are fine.
- **No OAuth flows.** OAuth challenges require a callback endpoint that the CLI doesn't expose.
- **Single model.** The CLI is hardcoded to `claude-haiku-4-5` via Bedrock. Change the model ID in `main.rs` if you need a different one.

## When to Use It

The CLI is useful for:

- Iterating on agent behavior without deploying infrastructure
- Testing conversation flows and threading logic
- Debugging tool call sequences
- Developing new built-in tools before wiring them into the CDK stack
