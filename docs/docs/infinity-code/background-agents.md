---
sidebar_position: 5
title: Background Agents
---

# Background Agents

Infinity Code runs a daemon process that manages your agent sessions. When you run `infinity`, it auto-launches the daemon if one isn't already running. This means you can have multiple agents working simultaneously, and keep them running even after you close the terminal.

## Multiple Sessions

Each time you start a conversation with the agent, it creates a session. You can run several sessions at once:

- `/new` (or Ctrl+N): start a fresh session
- `/load` (or Ctrl+L): switch to an existing session

Each session has its own conversation history and thread tree. The session picker shows all your sessions sorted by last activity, so you can quickly jump back to whatever you were working on.

## Backgrounding a Busy Agent

When you try to quit (`/quit`, Ctrl+C, or Ctrl+D) while the agent is busy (running tools, waiting for results, etc.), a picker appears with two options:

```
▸ Continue running agent in background
  Shut down agent
```

- **Continue running agent in background** (default): detaches from the session. The agent keeps running in the daemon, finishing whatever it was doing. Reconnect later with `/load`.
- **Shut down agent**: kills the session immediately

If the agent is idle when you quit, it detaches automatically without showing the picker.

The same picker appears when you switch sessions (`/new` or `/load`) while the current agent is busy. You can background the current session and switch to another without losing any work.

## Headless Tasks

You can hand a task to the daemon without opening the TUI at all:

```bash
infinity -H "update the dependencies and fix any compile errors"
```

This creates a new session in the current directory, sends the message, and exits. The agent works in the background; attach later with `/load` or the desktop UI to review.

## Daemon Management

The daemon starts automatically on your first `infinity` run. To manage it manually:

```bash
infinity daemon stop      # stop the running daemon
infinity daemon restart   # stop it if running, then start a fresh instance
infinity daemon           # run the daemon in the foreground (for debugging)
```

After `stop`, it will auto-start again the next time you run `infinity`. Daemon logs are written to `~/.infinity/daemon.log`.

## Persistence

Conversation history is persisted to disk under `~/.infinity/`. You can shut down the CLI entirely, boot it back up later, and continue right where you left off with all your existing context intact.
