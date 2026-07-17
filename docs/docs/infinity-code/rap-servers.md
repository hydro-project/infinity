---
sidebar_position: 7
title: RAP Servers
---

# RAP Servers

Infinity Code uses [RAP (Reactive Agent Protocol)](/docs/rap/what-is-rap) servers to provide tools to the agent. These are installed via `infinity rap install` and registered in your `~/.infinity/rap.json`.

## Sandbox

The core sandbox server that powers all the coding tools: `clone_repo`, `execute_command`, `read_file`, `edit_file`, `create_file`, `grep`, `squash_sandbox`, `describe_overall_changes`, and `open_sandbox_direct`. It handles Jujutsu workspaces and git worktrees, so each agent thread gets its own sandboxed copy of the repo with restricted filesystem writes.

This is installed during the [quickstart](/docs/infinity-code/overview#quickstart) setup.

```bash
infinity rap install --user --git https://github.com/hydro-project/infinity --crate sandbox-local
```

A few behaviors worth knowing:

- **Streaming output**: `execute_command` streams the output of long-running commands back to the agent as subscription events, so the agent can react to logs while the command is still running, and commands can be cancelled mid-flight.
- **Permission approvals**: the sandbox restricts writes to the sandbox workspace. When the agent requests extra permissions (`write-orig` to write to your original repo directory, e.g. for `git push`, or `write:/some/path` for a specific path), you get an approve/deny prompt in the TUI or web UI before anything runs.
- **Direct mode**: `open_sandbox_direct` operates on the original directory without a worktree, for repos where sandboxing is not possible (e.g. no commits yet). In this mode every file edit requires your approval.

## Steering

Discovers and loads project steering files so the agent can follow your project's conventions automatically. Scans for:

- `INFINITY.md`, `CLAUDE.md`, `AGENTS.md`, `CONVENTIONS.md`
- `.kiro/steering/`, `.cursor/rules/`, `.ai/rules/`, `.claude/skills/`
- `.cursorrules`, `.windsurfrules`, `.claude/settings.json`
- `.github/copilot-instructions.md`

Provides two tools: `list_steering` (find all steering files in the project) and `load_steering` (read a specific file's content).

```bash
infinity rap install --user --git https://github.com/hydro-project/infinity --crate rap-steering-server
```

## GitHub Event Poller

Subscribes to GitHub repository events via polling, so no webhooks or public endpoints are required. Provides the `subscribe_github_events` tool with filters for event type, action, actor, branch, PR number, issue number, and commit SHA.

Events are delivered as subscription events that wake the agent automatically. Useful for building agents that react to PRs, pushes, and issues in real time.

Set `GITHUB_TOKEN` in the daemon's environment; without it, GitHub's unauthenticated rate limit (60 requests/hour) makes polling impractical.

```bash
infinity rap install --user --git https://github.com/hydro-project/infinity --crate rap-github-event-poller
```

## Connecting a Running RAP Server

`infinity rap install` registers a command that the daemon spawns and manages. If you already have a RAP toolset server running somewhere (it serves `/.well-known/rap-toolset`), you can point the daemon at it directly by adding a `toolset_server` entry to your user-level (`~/.infinity/rap.json`) or project-level (`.infinity/rap.json`) config:

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

Installed servers appear in the same file as `toolset_command` entries, which spawn the server binary on demand and read its port from stdout. [MCP servers](./configuring-mcp.md) use the `mcp_server` and `http_mcp_server` entry types in the same `tool_sets` array.
