---
sidebar_position: 8
title: Configuring Remotes
---

# Configuring Remotes

Infinity Code can connect to Infinity daemons running on other machines over SSH. This lets you run agents on a powerful remote dev server while interacting with them from your laptop. Remote sessions appear alongside local ones in the session picker.

## Adding Remotes via CLI

Use the `infinity remote add` command:

```bash
infinity remote add <name> -- ssh <ssh_args...>
```

Everything after `-- ssh` is captured as the SSH arguments. Examples:

```bash
infinity remote add devbox -- ssh my-devbox.example.com
infinity remote add gpu-server -- ssh -J bastion.example.com gpu.internal
```

This appends a new entry to `~/.infinity/remotes.json`, creating the file if it doesn't exist. If a remote with the same name already exists, the command will error.

Restart the daemon (`infinity daemon restart`) for it to pick up the new remote.

## Configuration File

You can also edit `~/.infinity/remotes.json` directly. It contains an array of remote entries:

```json
[
  {
    "name": "devbox",
    "ssh_args": ["my-devbox.example.com"]
  }
]
```

Each entry has two fields:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | A short label for this remote. Shows as a prefix in the session picker. |
| `ssh_args` | string[] | Arguments passed directly to `ssh`. Can be a hostname, user@host, or any combination of SSH flags. |

The daemon reads this file on startup and establishes an SSH tunnel to each remote's `~/.infinity/daemon.sock`.

## SSH Setup

The `ssh_args` array is passed directly to the `ssh` command, so you can use anything your SSH client supports: host aliases from `~/.ssh/config`, custom ports, jump hosts, etc.

```json
[
  {
    "name": "devbox",
    "ssh_args": ["devbox"]
  },
  {
    "name": "gpu-server",
    "ssh_args": ["-J", "bastion.example.com", "gpu.internal"]
  }
]
```

Make sure you can `ssh <your-args>` without a password prompt (use SSH keys or an agent).

## Using Remote Sessions

Once configured, remote sessions appear in the session picker (`/load` or Ctrl+L) prefixed with the remote name:

```
  Local sessions
    abc123  my-project  2 min ago
  devbox (connected)
    devbox/def456  api-service  5 min ago
    devbox/ghi789  frontend  1 hour ago
```

Select a remote session and interact with it exactly like a local one; the daemon proxies all communication through the SSH tunnel transparently.

You can also start new sessions on a remote by SSHing in and running `infinity` there. The session will show up in your local picker automatically.

## Connection Status

The daemon auto-reconnects if an SSH tunnel drops, retrying every 5 seconds. Connection status for each remote is visible in the UI:

- **connecting**: establishing the SSH tunnel
- **connected**: tunnel is active, remote sessions are available
- **disconnected**: tunnel failed (hover or check logs for the reason)

## Migrating Sessions Between Machines

A session doesn't have to stay where it started. From the desktop web UI you can migrate a session between your local daemon and any configured remote (in either direction). The daemon orchestrates the move: it shuts down the source session, transfers the conversation history and thread tree, and asks the session's RAP servers (like the sandbox) to migrate their state to servers on the destination. Pick the session, choose a destination and working directory, and continue the conversation on the other machine.

## Requirements

- Infinity Code must be installed on the remote machine
- The remote daemon must be running (it starts automatically on the first `infinity` run)
- SSH access to the remote with key-based authentication
