# Infinity

A tool protocol, agent runtime, and coding harness for principled agent concurrency.

Infinity is an ecosystem for building AI agents that can wait for things, work in parallel, and cost nothing when idle. It consists of three layers:

- **[Reactive Agent Protocol (RAP)](#reactive-agent-protocol-rap)**: An async tool protocol with native support for subscriptions, long-running operations, and agent hibernation.
- **[Infinity Runtime](#infinity-runtime)**: A time-sliced agent runtime that processes work in short bursts and releases resources between them, whether deployed as a serverless function or running as a local daemon.
- **[Infinity Code](#infinity-code)**: A coding agent built on the runtime, with sandboxed editing, durable concurrent threads, and background sessions.

## Core Capabilities

### Native Tool Call Asynchrony & Subscriptions (RAP)

RAP replaces MCP's synchronous request/response model with fire-and-forget tool calls. The agent invokes a tool via HTTP, the tool acknowledges immediately, and the agent shuts down. When the tool finishes (100ms or 3 days later) it POSTs the result to a callback URL and the agent wakes up.

This enables:
- **Subscriptions**: Tools register ongoing event streams (GitHub webhooks, price alerts, Slack messages). Each event wakes the agent, which processes it and goes back to sleep.
- **Long-running operations**: A CI pipeline or deployment takes 20 minutes? The agent hibernates at zero cost and resumes when it completes.
- **MCP compatibility**: Any MCP server works through a proxy layer. You keep the full MCP ecosystem and gain async execution for tools that need it.

### Time-Sliced Agent Runtime

The Infinity Runtime is stateless and ephemeral. Each execution slice follows a three-phase cycle:

1. **Load**: Restore conversation history and state from durable storage.
2. **Complete**: Run the LLM, stream the response, collect tool calls.
3. **Dispatch & Exit**: Fire off tool calls via HTTP, persist state, shut down.

Nothing runs between slices. An agent waiting for a 3-day CI pipeline costs exactly the same as one that was never created. In the cloud, the process literally exits. Locally, it idles on a channel at zero CPU. Multiple agents share compute because they're never all active simultaneously; work is serialized through FIFO queues.

### Durable Concurrent Threads

Agents can spawn child threads for parallel work. Each thread has its own conversation context, message stream, and lifecycle:

- Threads inherit the parent's history up to the spawn point, then diverge.
- Children report results back via message passing. The parent sees reports as synthetic events without its context being polluted.
- Subscription events are automatically routed to isolated child threads for processing.
- Threads are durable: they survive restarts, process interruptions, and cold starts.

This enables patterns like parallel code review (one thread per file), research-while-implementing, and long-running event processing, all with proper context isolation.

## Quick Start

### Install Infinity Code

Prerequisites: [Rust](https://rustup.rs), [Ripgrep](https://github.com/BurntSushi/ripgrep) (`brew install ripgrep`), [Jujutsu](https://docs.jj-vcs.dev/latest) (optional, `brew install jj`).

```bash
# Install the CLI
cargo install infinity-agent-cli --git https://github.com/hydro-project/infinity --features bundled-web

# Install the local sandbox RAP server
infinity rap install --user --git https://github.com/hydro-project/infinity --crate sandbox-local

# Run it in any repo
cd your-repo
infinity
```

To update: `infinity update`

### Build a RAP Agent

See the [Getting Started guide](docs/docs/infinity-runtime/getting-started.md) for a walkthrough of running a RAP agent locally with a custom tool server.

### Deploy to Production

The cloud runtime deploys to AWS Lambda via CDK. Agents persist state to Aurora DSQL, route messages through SQS FIFO, and hibernate at true zero compute. See [Cloud Deployment](docs/docs/infinity-runtime/cloud-deployment.mdx).

## Documentation

### RAP (Reactive Agent Protocol)

- [What is RAP?](docs/docs/rap/what-is-rap.md): Motivation, capabilities, and when to use it
- [Architecture](docs/docs/rap/about/architecture.md): Message flow, callbacks, hibernation, and threading
- [RAP Specification](docs/docs/rap/spec/overview.md): The authoritative protocol reference
- [Building a RAP Tool](docs/docs/rap/using-rap/building-a-rap-tool.md): Create your own async tool server
- [Building a Runtime](docs/docs/rap/using-rap/building-a-runtime.md): Implement the runtime contract

### Infinity Runtime

- [Overview](docs/docs/infinity-runtime/overview.md): Time-sliced architecture and deployment options
- [Getting Started](docs/docs/infinity-runtime/getting-started.md): Run a local RAP agent
- [Threading](docs/docs/infinity-runtime/threading.md): Spawn threads, report results, process events
- [Built-in Tools](docs/docs/infinity-runtime/built-in-tools.md): Sleep, threading, and subscriptions

### Infinity Code

- [Overview](docs/docs/infinity-code/overview.md): Installation, first run, slash commands
- [Coding with Jujutsu](docs/docs/infinity-code/coding-with-jj.md): Jujutsu workspace sandboxes
- [Coding with Git](docs/docs/infinity-code/coding-with-git.md): Git worktree sandboxes
- [Background Agents](docs/docs/infinity-code/background-agents.md): Multiple sessions, backgrounding, persistence
- [Configuring MCP](docs/docs/infinity-code/configuring-mcp.md): Adding MCP servers
- [RAP Servers](docs/docs/infinity-code/rap-servers.md): Sandbox, steering, GitHub event poller

## Project Structure

```
crates/
  infinity-agent-core/       # Shared agent loop, tools, traits
  infinity-agent-cli/        # Local CLI binary (Infinity Code)
  infinity-agent-lambda/     # AWS Lambda runtime
  infinity-daemon/           # Local Infinity Runtime
  sandbox-core/              # Sandbox RAP server (shared logic)
  sandbox-local/             # Local sandbox backend (macOS/Linux sandboxing + jj/git)
  sandbox-remote/            # Remote sandbox backend
  rap-steering-server/       # Steering file discovery RAP server
  rap-github-event-poller/   # GitHub event subscription RAP server
agent/                       # CDK constructs for cloud deployment
docs/                        # RAP specification and documentation site
infinity-web/                # Desktop web UI
```
