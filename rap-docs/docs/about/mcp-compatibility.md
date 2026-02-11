---
sidebar_position: 4
title: MCP Compatibility
---

# MCP Compatibility

RAP is designed to coexist with MCP, not replace it. Any MCP server can be used as a RAP tool through a proxy layer that translates between MCP's synchronous JSON-RPC protocol and RAP's async HTTP contract. From the LLM's perspective, MCP tools and native RAP tools are indistinguishable.

## Stateless MCP servers

Most MCP servers are stateless — they don't maintain in-memory state between tool calls. A GitHub MCP server, for example, makes API calls on each invocation and doesn't need to remember anything between them.

For these servers, the RAP proxy layer is straightforward. The proxy spawns the MCP server process, sends the JSON-RPC request, collects the response, and POSTs the result to the callback URL. The MCP process can be torn down after each invocation. This turns a synchronous MCP tool call into an asynchronous RAP tool call — the agent runtime fires the request, the proxy acknowledges immediately, and the result arrives later through the callback.

This is where RAP adds the most value over raw MCP. A synchronous MCP call that takes 30 seconds blocks the agent for 30 seconds. The same call through the RAP proxy lets the agent hibernate during those 30 seconds, freeing compute for other work.

## Session continuity with Mcp-Session-Id

The MCP specification includes a session mechanism (`Mcp-Session-Id` header) that allows clients to resume sessions with a server. RAP's proxy layer uses this to bridge the gap between MCP's session model and RAP's ephemeral execution: when an MCP server returns a session ID, the proxy persists it. On subsequent invocations, the proxy includes the stored session ID in the request, allowing the server to restore context.

For MCP servers that support this pattern — where the session ID is enough to reconstruct state (e.g. by re-authenticating, reconnecting to a database, or loading cached data from a shared store) — the proxy can spawn a fresh process on each invocation and still maintain session continuity. The MCP server is effectively stateless from an infrastructure perspective, even if it has logical state.

## Stateful MCP servers and our vision

Some MCP servers maintain in-memory state across calls — database connections, authentication sessions, cached resources. These servers expect to stay alive for the duration of a conversation. You can handle this in RAP by keeping the MCP server process running continuously (e.g. in a long-lived container), with the proxy routing requests to the persistent process and returning results through the callback.

This works, but it undermines RAP's core value proposition. You're back to paying for idle compute, and you lose durability if the process crashes. It's a fundamental tension: MCP's design assumes a long-lived client-server relationship, while RAP assumes everything is ephemeral.

The model RAP is pushing toward is one where MCP servers externalize their state — to a database, a cache, or a session store — and can be cold-started with a session ID to resume where they left off. This aligns with how modern web services work: stateless processes, externalized state, horizontal scaling. An MCP server that can be started, handed a session ID, and reconstruct its context gives you the durability and scalability of ephemeral compute while preserving MCP's session semantics.

RAP's proxy layer supports both models today: ephemeral proxies for stateless servers (the common case) and session-aware proxies for servers that support `Mcp-Session-Id`. Over time, as more MCP servers adopt externalized state, the gap between MCP and RAP narrows — and agents get the best of both ecosystems.
