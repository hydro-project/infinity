---
sidebar_position: 3
title: Deployment Modes
---

# Deployment Modes

The Infinity Runtime ships in two deployment modes that share a single core engine (`infinity-agent-core`). The time-sliced execution model is identical — the differences are infrastructure-level: where state lives, how messages are delivered, and how hibernation is implemented.

## Shared core

Both modes are built on `infinity-agent-core`, a Rust crate containing the agent loop, conversation history management, tool dispatch, and all platform-independent logic. The core defines trait abstractions that each mode implements:

| Trait | Cloud (Lambda) | Local (Daemon) |
|---|---|---|
| `ConversationStore` | Aurora DSQL | In-memory `Vec` + file persistence |
| `StateStore` | DynamoDB | In-memory `HashMap` + file persistence |
| `InputSender` | SQS FIFO queue | `mpsc` channel |
| `HttpClient` | SigV4-signed requests | Plain HTTP |

The core also owns the built-in tools that work identically in both environments: `sleep_until_event_or_input`, `spawn_thread`, `report_to_parent`, and `close_thread`. These tools are generic over the `InputSender` trait, so they route messages through SQS in the cloud and through an in-memory channel locally.

## State persistence

The cloud runtime persists conversation history to Aurora DSQL and deduplication state to DynamoDB, so it survives across Lambda invocations. The daemon persists sessions to disk (`~/.infinity/sessions.json`) and can restore them across restarts.

## Hibernation

In the cloud, hibernation is real: the Lambda function exits, zero compute runs, and a future message on SQS restarts it. The daemon simulates this by pausing the agent loop on an `mpsc` channel, consuming zero CPU while idle.

## Timed sleep

Both modes support `sleep`, `sleep_until`, and `sleep_until_event_or_input`, but the underlying mechanism differs:

- **Cloud:** Delays ≤ 900 seconds use SQS `DelaySeconds` via a relay queue. Longer delays use EventBridge Scheduler. The Lambda exits and restarts when the timer fires.
- **Local:** `tokio::time::sleep` timers fire in spawned tasks and deliver the result through the in-memory channel. The process stays alive.

## Tool authentication

Cloud tool invocations use SigV4-signed HTTP requests to Lambda Function URLs with IAM auth. The daemon uses plain HTTP to local tool servers.

## Deployment

The cloud runtime is provisioned via CDK (`InfinityAgent` construct) with Lambda, SQS, DSQL, EventBridge, and IAM wiring. The daemon is installed with `cargo install` and runs locally.
