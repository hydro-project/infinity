---
sidebar_position: 6
title: Configuring MCP Servers
---

# Configuring MCP Servers

Infinity Code supports [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) servers as tool sources. Any MCP server works through a built-in proxy layer, with no changes to the server needed.

## Configuration Files

There are two levels of configuration:

- **User-level**: `~/.infinity/rap.json`, applies to all repos
- **Project-level**: `.infinity/rap.json` in a repo, applies only to that repo

Both are merged at startup. Project-level entries are combined with user-level entries.

## Adding a Local MCP Server

Add an entry to the `tool_sets` array in your `rap.json`:

```json
{
  "tool_sets": [
    {
      "type": "mcp_server",
      "name": "my-server",
      "command": ["path/to/server", "arg1", "arg2"],
      "env": { "KEY": "value" }
    }
  ]
}
```

The `command` array is the command and arguments to spawn the MCP server as a stdio subprocess. The optional `env` object sets environment variables for the process.

## Adding a Remote HTTP MCP Server

For MCP servers accessible over HTTP:

```json
{
  "tool_sets": [
    {
      "type": "http_mcp_server",
      "name": "my-remote-server",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  ]
}
```


