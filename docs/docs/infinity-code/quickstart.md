---
sidebar_position: 1
title: Quickstart
---

# Quickstart

Get Infinity Code running in your terminal in a few minutes.

## Prerequisites

- [Rust](https://rustup.rs) (for building from source)
- [Ripgrep](https://github.com/BurntSushi/ripgrep) — `brew install ripgrep`
- [Rust](https://rustup.rs) toolchain
- [Jujutsu](https://docs.jj-vcs.dev/latest) (recommended) — `brew install jj`

## Install

```bash
# Install the CLI and the sandbox RAP server
cargo install infinity-agent-cli --git https://github.com/hydro-project/infinity

infinity rap install --user --git https://github.com/hydro-project/infinity --crate sandbox-local
```

To update later:

```bash
infinity update
```

This updates both the CLI binary and any installed RAP servers.

## First run

`cd` into any repository and start the agent:

```bash
cd ~/my-project
infinity
```

The sandbox auto-detects your repo type:
- **Jujutsu** (`.jj` directory present) — creates isolated jj workspaces
- **Git** (plain git repo) — creates git worktrees

Type a message and press Enter. The agent will read your code, make changes in a sandboxed workspace, and report back. Your working directory is never modified — changes appear on branches or bookmarks you can inspect and merge.

## Slash commands

| Command | Shortcut | Description |
|---------|----------|-------------|
| `/help` | `Ctrl+H` | Show help |
| `/quit` | `Ctrl+C` | Exit |
| `/new` | `Ctrl+N` | Start a new session |
| `/load` | `Ctrl+L` | Load an existing session |
| `/model` | `Ctrl+M` | Switch model |
| `/compact` | `Ctrl+K` | Trigger context compaction |

## Recommended terminal

[Ghostty](https://ghostty.org) provides the best experience with Infinity Code's TUI.

## Next steps

- [Coding with Jujutsu](./coding-with-jj.md) — the recommended workflow
- [Coding with Git](./coding-with-git.md) — for plain git repos
- [Background Agents](./background-agents.md) — run multiple agents concurrently
- [Configuring MCP](./configuring-mcp.md) — add MCP servers as tool sets
