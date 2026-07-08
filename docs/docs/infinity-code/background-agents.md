---
sidebar_position: 5
title: Background Agents
---

# Background Agents

Infinity Code runs a daemon process that manages your agent sessions. When you run `infinity`, it auto-launches the daemon if one isn't already running. This means you can have multiple agents working simultaneously — and keep them running even after you close the terminal.

## Multiple Sessions

Each time you start a conversation with the agent, it creates a session. You can run several sessions at once:

- `/new` (or Ctrl+N) — start a fresh session
- `/load` (or Ctrl+L) — switch to an existing session

Each session has its own conversation history and thread tree. The session picker shows all your sessions sorted by last activity, so you can quickly jump back to whatever you were working on.

## Backgrounding a Busy Agent

When you try to quit (`/quit` or Ctrl+C) while the agent is busy — running tools, waiting for results, etc. — a picker appears with two options:

```
  Shut down agent
▸ Continue running agent in background
```

- **Shut down agent** — kills the session immediately
- **Continue running agent in background** — detaches from the session. The agent keeps running in the daemon, finishing whatever it was doing. Reconnect later with `/load`.

If the agent is idle when you quit, it detaches automatically — no picker needed.

The same picker appears when you switch sessions (`/new` or `/load`) while the current agent is busy. You can background the current session and switch to another without losing any work.

## Daemon Management

The daemon starts automatically on your first `infinity` run. To stop it manually:

```bash
infinity daemon stop
```

It will auto-start again the next time you run `infinity`.

## Persistence

Conversation history is persisted to disk under `~/.infinity/`. You can shut down the CLI entirely, boot it back up later, and continue right where you left off with all your existing context intact.

### Recovering pending tool calls

If the daemon shuts down (or crashes) while a tool call is in flight or a subscription is active, the RAP server handling it may give up in the meantime — for example, an embedded server is restarted along with the session and loses its in-memory state. Without intervention, the conversation would wait forever for a result that will never arrive.

To prevent this, whenever the daemon boots an agent session it reconciles the session's threads against the RAP servers: for every pending tool call and every active subscription, it asks the originating server whether the call is still alive using the RAP [tool call status check](/docs/rap/spec/basic/tool-call-status). Calls the server has given up on are pruned:

- A **pending tool call** gets a synthetic error result injected into the conversation, so the agent sees the failure and can retry the call.
- A **lost subscription** gets a synthetic final subscription event and is removed from the thread's active subscriptions, so the agent can re-subscribe.

Servers that answer `alive: false` — or that respond without supporting the status endpoint at all — are treated as having given up, and their calls are pruned. Only servers that can't be reached (or return a transient server error) are left alone: those calls simply stay pending, and the results are delivered normally if they eventually arrive.
