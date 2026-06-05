---
sidebar_position: 1
title: Overview
---

# Infinity Runtime

The Infinity Runtime is the reference RAP agent runtime. It implements a time-sliced execution model: agents process work in short bursts (load state, run the LLM, dispatch tool calls) then release resources. In the cloud the process exits entirely; locally the daemon idles at zero CPU. Cost is proportional to work done, not time elapsed.

## Time-sliced execution

Every agent invocation follows the same three-phase cycle:

1. **Load** — Restore conversation history and deduplication state from durable storage. Append the new input message (user text, tool result, or subscription event).
2. **Complete** — Send the conversation to the LLM and stream back the response. If the model produces a tool call, collect it.
3. **Dispatch & Yield** — POST the tool call to the RAP server (fire-and-forget), persist updated state, and yield. In the cloud the Lambda exits; locally the daemon returns to an idle wait on the message channel.

The cycle repeats when the next message arrives. The runtime doesn't distinguish between tool results, user messages, and subscription events at the execution level — it loads state and processes whatever's in the queue.

```
┌─────────┐     ┌─────────┐     ┌───────────────┐
│  Load   │ ──▶ │Complete │ ──▶ │Dispatch/Yield │
└─────────┘     └─────────┘     └───────────────┘
     ▲                                    │
     │    ╌╌╌╌ idle / exited ╌╌╌╌╌╌       │
     └───────── next message ◀────────────┘
```

This architecture makes hibernation free. An agent waiting for a 3-day CI pipeline, a human approval, or a GitHub webhook costs exactly zero compute. It wakes instantly when the next message arrives.

## Resource efficiency

Because agents are stateless between slices, multiple agents share the same compute. In the cloud (Lambda), each slice is a separate invocation — hundreds of agents share the same function, and idle agents consume nothing. Locally, threads idle on `mpsc` channels at zero CPU. The runtime serializes work within each conversation thread via FIFO message ordering, while different threads execute concurrently.

This is fundamentally different from long-lived agent processes that hold connections open and spin idle. A RAP agent monitoring GitHub PRs, reacting to Slack messages, and tracking stock prices can stay alive for months, waking only when something happens.

## Deployment modes

The runtime ships in two deployment modes that share a single core engine (`infinity-agent-core`). The time-sliced execution model is identical in both — the only differences are where state lives and how messages are delivered.

| | Cloud (Lambda) | Local (Daemon) |
|---|---|---|
| State | Aurora DSQL + DynamoDB | In-memory + file persistence |
| Messaging | SQS FIFO | `mpsc` channels |
| Hibernation | Process exits, SQS/EventBridge restarts | Process idles on channel |
| Tool auth | SigV4-signed HTTP | Plain HTTP |

See [Deployment Modes](./deployment-modes.md) for the full comparison.

## When to use which

Use the **cloud deployment** for production agents that need to run indefinitely, survive restarts, handle concurrent threads across invocations, and scale to zero between activity.

Use the **local daemon** for developing and testing agent behavior, iterating on conversation flows, debugging tool call sequences, and building RAP tool servers.
