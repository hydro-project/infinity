---
sidebar_position: 1
title: What is RAP?
---

# What is RAP?

RAP (Reactive Agent Protocol) is a protocol for AI agent tool execution. It replaces the synchronous request/response model of protocols like MCP with an asynchronous, event-driven contract between agents and tools.

In MCP, the agent calls a tool and blocks until it returns. This works for fast lookups. It falls apart when a CI pipeline takes 20 minutes, a deployment needs human approval, or you want to be notified when a PR gets reviewed. The agent burns compute waiting, or resorts to polling.

RAP inverts this. Tool calls are fire-and-forget. The agent invokes a tool via HTTP, the tool acknowledges immediately, and the agent shuts down. When the tool finishes — 100ms or 3 days later — it POSTs the result back to the agent's callback URL, and the agent wakes up to continue.

This enables three capabilities:
- **Subscriptions.** A tool registers an ongoing subscription (e.g. "notify me on new GitHub PRs"). Each matching event wakes the agent, which processes it and goes back to sleep. The subscription persists across hibernations.
- **Long-running tool calls.** A tool that takes minutes or hours doesn't block anything. The agent hibernates at zero cost and resumes when the result arrives.
- **Agent hibernation.** When there's nothing to do, the agent process shuts down entirely. No idle compute. It restarts instantly when a message arrives — user input, tool result, or external event.

RAP is also fully compatible with MCP. Any MCP server works as a RAP tool through a backwards compatibility layer — you keep the entire MCP ecosystem and gain async execution for the tools that need it.

## Who should use RAP?

RAP is for anyone building AI agents that need to wait for things — webhooks, CI/CD pipelines, approval workflows, incoming emails, price feeds, monitoring alerts. If your agent needs to subscribe to ongoing data streams, execute operations that take minutes or hours, or run indefinitely while scaling to zero when idle, RAP is the protocol you want.

If your agents only need simple request/response tool calls, MCP works fine. RAP is for when your agent needs to live in the real world, where things happen on their own schedule.

## Why does it matter?

Today's agent protocols assume tools are fast. Call a tool, get a result, move on. But real-world agent tasks involve waiting — for builds to finish, for humans to approve, for events to happen. Without a protocol-level answer, every team reinvents the same patterns: polling loops, webhook plumbing, state persistence, wake-up scheduling.

RAP makes these patterns first-class. Subscriptions, long-running calls, and hibernation are part of the protocol, not bolted on. A RAP tool that subscribes to Stripe webhooks works the same way as one that monitors GitHub PRs — the agent doesn't need to know the difference. It calls the tool, goes to sleep, and wakes up when something happens.

The result is agents that can run for days or weeks, react to events in real time, and cost nothing when idle.

## Next Steps

Ready to dig in? Here's where to go next:

- **[Architecture](/docs/rap/about/architecture)** — Understand how RAP's message-passing model works, including the callback lifecycle and agent hibernation.
- **[Your First RAP Agent](/docs/infinity-runtime/getting-started)** — A hands-on walkthrough to get a RAP agent running locally or in the cloud.
- **[RAP Specification](/docs/rap/spec/overview)** — The authoritative protocol spec. Read this if you want to build your own runtime or tool server, or just want the full technical details.
- **[Infinity Runtime](/docs/infinity-runtime/overview)** — The reference RAP runtime, written in Rust. This is where you actually run agents — locally with the CLI for development, or deployed to AWS Lambda for production.
