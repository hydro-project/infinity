---
sidebar_position: 7
title: RAP Servers
---

# RAP Servers

Infinity Code uses [RAP (Reactive Agent Protocol)](/docs/what-is-rap) servers to provide tools to the agent. These are installed via `infinity rap install` and registered in your `~/.infinity/rap.json`.

## Sandbox

The core sandbox server that powers all the coding tools: `clone_repo`, `execute_command`, `read_file`, `edit_file`, `create_file`, `grep`, `squash_sandbox`, and more. It handles Jujutsu workspaces, git worktrees — each agent thread gets its own sandboxed copy of the repo with restricted filesystem writes.

This is installed during the [quickstart](/docs/infinity-code/quickstart) setup.

```bash
infinity rap install --user --git https://github.com/hydro-project/infinity --crate sandbox-local
```

## Steering

Discovers and loads project steering files so the agent can follow your project's conventions automatically. Scans for:

- `CLAUDE.md`, `AGENTS.md`, `CONVENTIONS.md`
- `.kiro/steering/`, `.cursor/rules/`, `.ai/rules/`
- `.cursorrules`, `.windsurfrules`
- `.github/copilot-instructions.md`

Provides two tools: `list_steering` (find all steering files in the project) and `load_steering` (read a specific file's content).

```bash
infinity rap install --user --git https://github.com/hydro-project/infinity --crate rap-steering-server
```

## GitHub Event Poller

Subscribes to GitHub repository events via polling — no webhooks or public endpoints required. Provides the `subscribe_github_events` tool with filters for event type, action, actor, branch, PR number, issue number, and commit SHA.

Events are delivered as subscription events that wake the agent automatically. Useful for building agents that react to PRs, pushes, and issues in real time.

```bash
infinity rap install --user --git https://github.com/hydro-project/infinity --crate rap-github-event-poller
```
