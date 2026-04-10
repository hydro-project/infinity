# Infinity Code

A coding agent you run in your terminal. It spins up sandboxed workspaces, spawns parallel threads, and persists conversations to disk — so you can background an agent, switch to another, and come back later.

## What it does

- **Sandboxed editing** — The agent never writes to your working directory. Changes land on isolated branches (Jujutsu bookmarks or git worktree branches) that you inspect and merge when ready.
- **Parallel threads** — The agent can spawn child threads that work on sub-tasks concurrently. Each thread gets its own sandbox. Threads report results back to the parent in real-time.
- **Background agents** — Run multiple agent sessions at once. Background a busy agent, start a new one, and switch between them with `/load`. Conversations persist across CLI restarts.
- **MCP support** — Load any MCP server as a tool source.
- **Steering files** — Automatically discovers CLAUDE.md, AGENTS.md, .kiro/steering/, and other project convention files.

## Quick start

First install the prerequisites:

- [Rust](https://rustup.rs) (for building from source)
- [Ripgrep](https://github.com/BurntSushi/ripgrep) — `brew install ripgrep`
- [Jujutsu](https://docs.jj-vcs.dev/latest) (optional, recommended) — `brew install jj`

Then install:

```bash
# Install the CLI (includes the desktop web UI; remove --features bundled-web if you don't have npm)
cargo install infinity-agent-cli --git https://github.com/hydro-project/infinity --features bundled-web
infinity rap install --user --git https://github.com/hydro-project/infinity --crate sandbox-local

# Run it
cd your-repo
infinity
```

To update: `infinity update`

### Desktop UI

Infinity Code comes with a desktop UI that provides a native interface for concurrent threads. There are two ways to run it:

**Bundled (recommended if you have npm)** — install with the `bundled-web` feature and the daemon serves the UI automatically:

```bash
cargo install infinity-agent-cli --git https://github.com/hydro-project/infinity --features bundled-web
```

Then open `http://localhost:8080` in a browser. The `bundled-web` feature is preserved across `infinity update`.

**Standalone** — run the Vite dev server separately (requires npm):

```bash
cd infinity-web
npm ci
npm run dev
```

Then open the URL printed in your terminal (typically http://localhost:5173).

Both modes connect to the same daemon as the CLI, so you can use all three interchangeably.

## Documentation

Full docs are in the [Infinity Code section](docs/docs/infinity-code/overview.md) of the docs site:

- [Overview](docs/docs/infinity-code/overview.md) — Installation, first run, slash commands
- [Coding with Jujutsu](docs/docs/infinity-code/coding-with-jj.md) — Jujutsu workspace sandboxes
- [Coding with Git](docs/docs/infinity-code/coding-with-git.md) — Git worktree sandboxes
- [Background Agents](docs/docs/infinity-code/background-agents.md) — Multiple sessions, backgrounding, persistence
- [Configuring MCP](docs/docs/infinity-code/configuring-mcp.md) — Adding MCP servers
- [RAP Servers](docs/docs/infinity-code/rap-servers.md) — Sandbox, steering, GitHub event poller, and more

---

## Infinity Agents (Runtime)

The Rust runtime that powers Infinity Code. It comes in two flavors that share the same engine (`infinity-agent-core`):

| | Cloud (Lambda) | Local (CLI) |
|---|---|---|
| State | Aurora DSQL + DynamoDB | In-memory + file persistence |
| Messaging | SQS FIFO | `mpsc` channels |
| Hibernation | Lambda exits, SQS/EventBridge restarts | Process stays alive, idle on channel |
| Tool auth | SigV4-signed HTTP | Plain HTTP |

See the [runtime docs](docs/docs/infinity-runtime/overview.md) for details.

## Reactive Agent Protocol (RAP)

RAP replaces MCP's synchronous request/response model with async, event-driven communication. Tool calls are fire-and-forget: the agent invokes a tool via HTTP, the tool acknowledges immediately, and the agent shuts down. When the tool finishes — 100ms or 3 days later — it POSTs the result to a callback URL and the agent wakes up.

This enables subscriptions, long-running tool calls, and agent hibernation at zero compute cost.

See the [RAP spec](docs/spec/overview.md) and [docs](docs/docs/what-is-rap.md).

## Project Structure

```
crates/
  infinity-agent-cli/       # Local CLI binary
  infinity-agent-core/       # Shared agent loop, tools, traits
  infinity-agent-lambda/     # AWS Lambda runtime
  sandbox-core/              # Sandbox RAP server (shared logic)
  sandbox-local/             # Local sandbox backend (macOS/Linux sandboxing + jj/git)
  sandbox-remote/            # Remote sandbox backend
  rap-steering-server/       # Steering file discovery RAP server
  rap-github-event-poller/   # GitHub event subscription RAP server
agent/                       # CDK constructs for cloud deployment
docs/                        # RAP specification and documentation site
```
