---
sidebar_position: 2
title: Getting Started
---

# Getting Started

This guide walks you through running a RAP agent locally using the Infinity Runtime CLI. By the end you'll have a working agent in your terminal, connected to a local RAP tool server.

## Prerequisites

### Rust

The Infinity Runtime is written in Rust. If you don't have Rust installed, the easiest way is via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

After installation, make sure `cargo` is on your path:

```bash
cargo --version
```

:::tip
If you already have Rust installed, make sure you're on a recent stable release. Run `rustup update stable` to get the latest.
:::

### Node.js

The example tool server (`get-time`) is a Node.js script. You'll need Node.js 18+ installed. Check with:

```bash
node --version
```

### Amazon Bedrock access

The CLI uses Amazon Bedrock for model inference (Claude Haiku 4.5 by default). You need AWS credentials in your environment with Bedrock model access enabled.

The CLI picks up credentials through the standard AWS SDK credential chain — environment variables, SSO, instance profiles, etc. The most common setups:

**AWS SSO (recommended for development):**

```bash
aws sso login --profile your-profile
export AWS_PROFILE=your-profile
```

**Environment variables:**

```bash
export AWS_ACCESS_KEY_ID=your-key
export AWS_SECRET_ACCESS_KEY=your-secret
export AWS_REGION=us-east-1
```

Make sure the credentials you're using have access to invoke Bedrock models. If you haven't enabled model access in your account, go to the [Bedrock console](https://console.aws.amazon.com/bedrock/) → Model access → Enable the Claude models.

## Clone and build the CLI

```bash
git clone https://github.com/hydro-project/infinity
cd InfinityAgents
```

Build and run the CLI:

```bash
cargo run -p infinity-agent-cli
```

The first build will take a few minutes while Cargo downloads and compiles dependencies. Subsequent runs are fast.

Once it starts, you'll see a streaming chat interface in your terminal. The bottom of the screen shows an input prompt:

```
─────────────────────────────────────
> your input here
```

Type a message and press Enter. The agent will respond using Bedrock. You can type while the model is streaming — input is buffered. Press `Ctrl+C` or `Ctrl+D` to exit.

At this point you have a working agent, but it has no tools beyond the built-in sleep and threading tools. Let's fix that.

## Run a local RAP tool server

The repo includes a `get-time` tool — a simple RAP tool server that returns the current time in any timezone. It's a good first tool to verify everything is wired up.

### Start the tool server

In a separate terminal, from the repo root:

```bash
node agent/lib/toolsets/get-time/get-time-tool/local.mjs --port 3001
```

You should see:

```
get-time RAP tool server listening on http://127.0.0.1:3001
Discovery: http://127.0.0.1:3001/.well-known/rap-toolset
```

This server does two things:
1. Serves a toolset manifest at `GET /.well-known/rap-toolset` so the CLI can discover available tools
2. Accepts tool invocations via `POST`, acknowledges immediately, then POSTs the result back to the agent's callback URL

That's the RAP pattern in action — fire-and-forget invocation with async result delivery.

### Connect the tool server to the CLI

Create a `rap-servers.json` file in the repo root:

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

Now start (or restart) the CLI:

```bash
cargo run -p infinity-agent-cli
```

The CLI reads `rap-servers.json` at startup and fetches the toolset manifest from each configured server. The `get_time` tool is now available to the agent.

### Try it out

In the CLI, ask the agent something like:

```
> What time is it in Tokyo?
```

You'll see the tool call and result in the output:

- <span style={{color: '#3b82f6'}}>Blue ◆</span> — the agent calls `get_time` with `{"timezone": "Asia/Tokyo"}`
- <span style={{color: '#22c55e'}}>Green ✓</span> — the tool result comes back with the current time
- The agent incorporates the result into its response

In the tool server terminal, you'll see the invocation logged:

```
Processing get_time: { args: { timezone: 'Asia/Tokyo' }, id: '...', call_id: '...' }
Sent tool result to http://127.0.0.1:...
```

That's a complete RAP round trip: the agent invoked the tool, the tool acknowledged immediately, processed the request, and POSTed the result back to the agent's callback URL.

## What's next

Now that you have a local agent running with a tool, here are good next steps:

- **[Build a RAP Tool](/docs/rap/using-rap/building-a-rap-tool)** — create your own tool server that speaks RAP
- **[Architecture](/docs/rap/about/architecture)** — understand the full message flow, callback lifecycle, and hibernation model
- **[Built-in Tools](/docs/infinity-runtime/built-in-tools)** — sleep, threading, and utility tools available to every agent
- **[RAP Specification](/docs/rap/spec/overview)** — the full protocol reference
- **[Cloud Deployment](/docs/infinity-runtime/cloud-deployment)** — deploy your agent to AWS Lambda for persistent state and real hibernation
