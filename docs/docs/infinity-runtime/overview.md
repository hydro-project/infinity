---
sidebar_position: 1
title: Overview
---

# Infinity Runtime

The Infinity Runtime is the reference RAP agent runtime. It's written in Rust and comes in two flavors: a cloud deployment on AWS Lambda and an in-memory local CLI for development. Both share the same core engine — the difference is where state lives and how messages are delivered.

## Shared core

Both runtimes are built on `infinity-agent-core`, a Rust crate that contains the agent loop, conversation history management, tool dispatch, and all the logic that doesn't depend on a specific platform. The core defines trait abstractions that each runtime implements:

| Trait | Cloud (Lambda) | Local (CLI) |
|---|---|---|
| `ConversationStore` | Aurora DSQL | In-memory `Vec` |
| `StateStore` | DynamoDB | In-memory `HashMap` |
| `InputSender` | SQS FIFO queue | `mpsc` channel |
| `HttpClient` | SigV4-signed requests | Plain HTTP |

The core also owns the built-in tools that work identically in both environments: `sleep_until_event_or_input`, `spawn_thread`, `report_to_parent`, and `close_thread`. These tools are generic over the `InputSender` trait, so they route messages through SQS in the cloud and through an in-memory channel locally.

## The agent loop

Both runtimes run the same three-phase cycle:

1. **Prepare** — load conversation history, append the new input message, resolve any synthetic events (thread reports, subscription events).
2. **Completion** — send the conversation to the LLM and stream back the response. Tool calls are collected as they arrive.
3. **Execute** — dispatch each tool call via HTTP (fire-and-forget), persist updated state, and exit.

The loop repeats when the next message arrives — a tool result, user input, or subscription event. The runtime doesn't distinguish between these; it loads state and processes whatever's there.

## What differs

The core loop is the same, but the two runtimes diverge in how they handle infrastructure concerns:

**State persistence.** The cloud runtime persists conversation history to DSQL and deduplication state to DynamoDB, so it survives across Lambda invocations. The CLI keeps everything in memory — if the process exits, state is gone.

**Hibernation.** In the cloud, hibernation is real: the Lambda function exits, zero compute runs, and a future message on SQS restarts it. The CLI simulates this by pausing the agent loop on the channel — the process stays alive but idle.

**Timed sleep.** Both runtimes have `sleep` and `sleep_until` tools, but the implementation differs. The cloud runtime schedules future wake-ups via SQS delay queues (≤ 900s) or EventBridge Scheduler (longer) — the Lambda exits and restarts when the timer fires. The CLI uses in-memory `tokio::time::sleep` timers in spawned tasks — the process stays alive and delivers the result through the channel when the timer completes.

**Tool authentication.** Cloud tool invocations use SigV4-signed HTTP requests to Lambda Function URLs with IAM auth. The CLI uses plain HTTP to local tool servers — no signing needed.

**Deployment.** The cloud runtime is provisioned via CDK (`InfinityAgent` construct) with Lambda, SQS, DSQL, EventBridge, and IAM wiring. The CLI is `cargo run`.

## When to use which

Use the **cloud deployment** for production agents that need to run indefinitely, survive restarts, handle concurrent threads across invocations, and scale to zero between activity. This is the full-featured runtime.

Use the **local CLI** for developing and testing agent behavior, iterating on conversation flows, debugging tool call sequences, and building RAP tool servers. It's faster to iterate with and doesn't require any AWS infrastructure.
