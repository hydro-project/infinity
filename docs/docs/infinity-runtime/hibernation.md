---
sidebar_position: 3
title: Hibernation
---

# How Infinity Achieves Hibernation

RAP defines hibernation as a protocol-level concept — the runtime exits after dispatching tool calls and restarts when results arrive. The Infinity Runtime implements this on AWS using Lambda, SQS, and EventBridge Scheduler.

## The execution model

The Infinity Runtime is a Rust Lambda function triggered by the SQS FIFO input queue. Each invocation processes one batch of messages, runs the LLM completion loop, dispatches any tool calls via HTTP, persists state to DSQL, and returns. Lambda handles the rest — the function scales to zero between invocations.

Tool calls are POSTed to Lambda Function URLs with IAM auth (SigV4). The tool Lambda uses response streaming to acknowledge immediately (`responseStream.write('OK'); responseStream.end()`), then continues processing. The Infinity Runtime doesn't read the response body — it fires and moves on.

When the tool finishes, it POSTs the result to the callback endpoint (also a Lambda Function URL). The endpoint enqueues the result on the input FIFO queue, which triggers the runtime Lambda again.

## Scheduled wake-ups

For timed waits (the `sleep` and `sleep_until` built-in tools), the runtime needs to enqueue a message at a future time. Two mechanisms handle this:

**SQS delay relay (≤ 900 seconds).** SQS FIFO queues don't support per-message delays, so the runtime sends the message to a standard SQS queue with `DelaySeconds` set. A relay Lambda picks it up after the delay and forwards it to the FIFO input queue with the correct `MessageGroupId`.

**EventBridge Scheduler (> 900 seconds).** For longer waits, the runtime creates a one-time EventBridge schedule that sends the wake-up message directly to the input queue at the target time. The schedule is named with a timestamp to avoid collisions.

In both cases, the runtime exits immediately after scheduling. The Lambda function is not running during the wait.

## Interruption

The input queue doesn't care what kind of message arrives. If a user sends a Slack message while the agent is waiting for a tool result or a scheduled wake-up, that message lands on the queue and triggers the runtime. The runtime loads history, processes the user message, and continues normally. The pending tool result or sleep wake-up arrives later as a separate message and is processed in turn.
