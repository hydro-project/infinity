# Infinity Agents

This repo contains three components:

1. **Infinity Code** — A coding agent CLI with threads, sandboxes, and persistence
2. **Infinity Agents** — The core Rust runtime for building RAP agents (local CLI + AWS Lambda)
3. [**Reactive Agent Protocol (RAP)**](docs/) — An async, event-driven protocol for agent-tool communication

---

## Infinity Code

A coding agent built with RAP that you can run in your terminal today.

### Features

- **Agent threads** — The agent can spin up subthreads that work on tasks concurrently while the parent continues to run. Subthreads send updates to the parent in real-time.
- **Sandboxes** — Uses macOS sandboxing APIs to restrict filesystem writes and guard against rogue commands. Each sandbox is an isolated workspace (Jujutsu workspace or git worktree) so agent work never touches your repo folder — it shows up as a branch you can inspect and merge. Supports macOS sandbox APIs and Linux via [bubblewrap](https://github.com/containers/bubblewrap).
- **Persistence** — Full conversation context is persisted to local disk. You can shut down the CLI, boot it back up, and continue with all your existing context.
- **MCP support** — Load any MCP server as a tool set via config.

### Quick Start

You'll need:
- [Rust](https://rustup.rs)
- [Ripgrep](https://github.com/BurntSushi/ripgrep) (`brew install ripgrep`)

```bash
# Install the CLI and the sandbox RAP server
cargo install infinity-agent-cli --git https://github.com/hydro-project/infinity
infinity rap install --user --git https://github.com/hydro-project/infinity --crate sandbox-local

# later, to update:
infinity update
```

### Sandbox Modes

The sandbox automatically detects your repository type and uses the appropriate isolation strategy. In all modes, the agent never writes to your working directory — changes appear on a branch or bookmark you can inspect and merge.

#### Jujutsu (default for repos with `.jj`)

If your repo has a `.jj` directory, the sandbox creates isolated [Jujutsu workspaces](https://jj-vcs.dev) via `jj workspace add`. Each thread gets its own workspace with a bookmark named `sandbox-{thread_id}`.

Requires [Jujutsu](https://docs.jj-vcs.dev/latest) installed (`brew install jj`). Your repo can be a regular Git repo — just run `jj git init --colocate` to add jj on top.

```bash
infinity

# Inspect what the agent did:
jj log -r 'bookmarks("sandbox-")'
jj show sandbox-...

# Incorporate changes into your working copy:
jj squash --from sandbox-...
```

#### Git (plain git repos without jj)

For git repos without a `.jj` directory, the sandbox uses [git worktrees](https://git-scm.com/docs/git-worktree). Each thread gets a worktree on a branch named `sandbox-{thread_id}`.

No extra dependencies beyond git.

```bash
infinity

# Inspect what the agent did:
git log sandbox-...
git diff HEAD...sandbox-...

# Incorporate changes into your working copy:
git merge sandbox-...
```

### MCP Servers

Infinity Code supports MCP servers. For example, you can add an MCP server. Edit `~/.infinity/rap.json` (or `.infinity/rap.json` for local config):
```
"tool_sets": [
  ...,
  {
    "type": "mcp_server",
    "name": "my-mcp-server",
    "command": [ "/path/to/mcp-server" ]
  }
]
```

I recommend [Ghostty](https://ghostty.org) as your terminal for the best experience.

---

## Infinity Agents (Runtime)

The core Rust runtime that powers Infinity Code and can be used to build your own RAP agents. It comes in two flavors that share the same engine (`infinity-agent-core`):

| | Cloud (Lambda) | Local (CLI) |
|---|---|---|
| State | Aurora DSQL + DynamoDB | In-memory + file persistence |
| Messaging | SQS FIFO | `mpsc` channels |
| Hibernation | Lambda exits, SQS/EventBridge restarts | Process stays alive, idle on channel |
| Tool auth | SigV4-signed HTTP | Plain HTTP |

Both run the same three-phase agent loop: **Prepare** (load history, append input) → **Completion** (stream LLM response, collect tool calls) → **Execute** (dispatch tools via HTTP, persist state, exit).

See the [runtime docs](docs/docs/infinity-runtime/overview) for details.

---

## Reactive Agent Protocol (RAP)

RAP replaces MCP's synchronous request/response model with async, event-driven communication. Tool calls are fire-and-forget: the agent invokes a tool via HTTP, the tool acknowledges immediately, and the agent shuts down. When the tool finishes — 100ms or 3 days later — it POSTs the result to a callback URL and the agent wakes up.

This enables:
- **Subscriptions** — Tools register ongoing event streams (GitHub webhooks, Slack messages, etc.) that wake the agent on each event
- **Long-running tool calls** — CI pipelines, deployments, and approval workflows don't block anything
- **Agent hibernation** — Zero compute cost between messages; agents can run for weeks

RAP is fully compatible with MCP — any MCP server works through a proxy layer.

See the [RAP spec](docs/spec/overview) and [docs](docs/docs/what-is-rap) for the full protocol.

---

## Project Structure

```
crates/
  infinity-agent-cli/    # Local CLI binary
  infinity-agent-core/   # Shared agent loop, tools, traits
  infinity-agent-lambda/  # AWS Lambda runtime
  sandbox-core/          # Sandbox RAP server (shared logic)
  sandbox-local/         # Local sandbox backend (macOS sandboxing + jj)
  sandbox-remote/        # Remote sandbox backend
agent/                   # CDK constructs for cloud deployment
docs/                  # RAP specification and documentation site
```
