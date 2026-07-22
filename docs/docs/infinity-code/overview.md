---
sidebar_position: 0
title: Overview
---

# Infinity Code

Infinity Code is an AI coding agent that runs locally on your machine. It reads your codebase, makes changes in isolated sandboxes, spawns parallel threads for complex tasks, and can run in the background while you do other work. Your working directory is never touched; you review and merge changes when you're ready.

1. **You ask**: describe what you want in natural language.
2. **The agent works**: it reads files, runs commands, and edits code in a sandboxed copy of your repo (a jj workspace, git worktree).
3. **You review**: inspect the diff on the sandbox branch/bookmark and squash it in when you're happy.

The agent never modifies your working copy directly. Every change lives on a `sandbox-{thread_id}` branch that you control.

## Quickstart

First install the prerequisites:

- [Rust](https://rustup.rs) (for building from source)
- [Ripgrep](https://github.com/BurntSushi/ripgrep): `brew install ripgrep`
- [Jujutsu](https://docs.jj-vcs.dev/latest) (optional, recommended): `brew install jj`

Then install:

```bash

# Install the CLI (includes the desktop web UI; remove --features bundled-web if you don't have npm)
cargo install infinity-agent-cli --git https://github.com/hydro-project/infinity --features bundled-web

# Install a model provider; Bedrock invokes models with your AWS credentials
infinity provider install bedrock --git https://github.com/hydro-project/infinity --crate infinity-provider-bedrock

infinity rap install --user --git https://github.com/hydro-project/infinity --crate sandbox-local
```

[Model providers](./model-providers.md) run as separate processes managed by the daemon and are registered in `~/.infinity/providers.json`; at least one must be installed. The Bedrock provider uses your ambient AWS configuration (e.g. `AWS_PROFILE` or environment credentials).

To update later:

```bash
infinity update
```

This updates the CLI binary, installed model providers, and any installed RAP servers.

### First run

`cd` into any repository and start the agent:

```bash
cd ~/my-project
infinity
```

The sandbox auto-detects your repo type:
- **Jujutsu** (`.jj` directory present): creates isolated jj workspaces
- **Git** (plain git repo): creates git worktrees

Type a message and press Enter. The agent will read your code, make changes in a sandboxed workspace, and report back. Your working directory is never modified; changes appear on branches or bookmarks you can inspect and merge.

### Desktop UI

There are two ways to run the desktop interface:

**Bundled**: if you installed with `--features bundled-web`, the daemon already serves the UI. Open `http://localhost:8080` in a browser. The port and bind address are configurable with the `INFINITY_WS_PORT` and `INFINITY_WS_BIND` environment variables (defaults: `8080`, `127.0.0.1`).

**Standalone**: run the Vite dev server separately:

```bash
cd infinity-web
npm ci
npm run dev
```

Then open the URL printed in your terminal (typically `http://localhost:5173`).

Both modes connect to the same daemon as the CLI, so you can use all three interchangeably.

## Key Features

- **Sandboxed editing**: changes happen in isolated workspaces. Supports [Jujutsu](./coding-with-jj.md) (recommended) and [Git](./coding-with-git.md).
- **Pluggable models**: models come from [model provider](./model-providers.md) processes; install, mix, and switch between providers.
- **Parallel threads**: the agent spawns child threads for independent sub-tasks. Each thread gets its own sandbox and reports back when done.
- **Background sessions**: detach from a busy agent and reconnect later. Multiple sessions can run concurrently via the [daemon](./background-agents.md).
- **Remote sessions**: connect to agents running on other machines over SSH. See [Configuring Remotes](./configuring-remotes.md).
- **Extensible tools**: add [MCP servers](./configuring-mcp.md) and [RAP servers](./rap-servers.md) to give the agent new capabilities.
- **Session persistence**: conversation history is saved to disk. Quit, reboot, come back, and your context is intact.

## Terminal UI

The TUI runs anywhere you have a terminal. Start it with `infinity` in any repo.

| Command | Shortcut | Description |
|---------|----------|-------------|
| `/help` | `Ctrl+H` | Show help |
| `/quit` | `Ctrl+C` / `Ctrl+D` | Exit (or detach; see [Background Agents](./background-agents.md)) |
| `/new` | `Ctrl+N` | Start a new session |
| `/load` | `Ctrl+L` | Load an existing session |
| `/model` | `Ctrl+M` | Switch model |
| `/stop` | `Ctrl+S` | Stop the agent; send a message to resume |
| `/compact` | | Trigger context compaction |
| `/archive` | | Archive the current session |

Most commands have single-letter aliases (`/q`, `/n`, `/l`, `/m`, `/s`, `/a`).

[Ghostty](https://ghostty.org) provides the best experience with the TUI.

### Useful flags

```bash
infinity -i "fix the failing test"   # start with an initial message
infinity -H "fix the failing test"   # headless: hand the task to the daemon and exit
infinity -s <session-id>             # connect directly to an existing session
infinity --local                     # skip the daemon, run everything in-process
```

Headless mode is handy for scripting: the agent keeps working inside the daemon, and you can attach later with `/load` (or from the desktop UI) to review the result.

### Editor integration (ACP)

The CLI can run as an [Agent Client Protocol](https://agentclientprotocol.com) server over stdio:

```bash
infinity acp
```

This exposes the daemon's sessions to any ACP-capable editor (such as Zed): you can list and load existing sessions, start new ones, send prompts, switch models, and watch tool calls and diffs stream in, all from the editor's agent panel. Point your editor's agent server configuration at the `infinity acp` command.

## Next steps

- [Coding with Jujutsu](./coding-with-jj.md): the recommended workflow
- [Coding with Git](./coding-with-git.md): for plain git repos
- [Model Providers](./model-providers.md): install, configure, and update model backends
- [Background Agents](./background-agents.md): run multiple agents concurrently
- [Configuring MCP](./configuring-mcp.md): add MCP servers as tool sets
