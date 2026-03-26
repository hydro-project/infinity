---
sidebar_position: 5
title: Built-in Tools
---

# Built-in Tools

The Infinity Runtime ships with a set of built-in tools that are available to every agent. These aren't part of the RAP protocol — they're conveniences provided by the runtime.

## Sleep tools

While RAP's hibernation means the runtime exits after every tool call, sometimes you want the agent to explicitly wait before continuing. The sleep tools schedule a future wake-up message. Both the cloud runtime and the local CLI support all three sleep tools — the difference is the underlying mechanism.

**`sleep(seconds)`** — Hibernate for a fixed duration. In the cloud, delays ≤ 900 seconds use SQS `DelaySeconds` via a relay queue; longer delays use EventBridge Scheduler. In the CLI, a `tokio::time::sleep` timer fires in a spawned task and delivers the result through the in-memory channel.

**`sleep_until(date, time, timezone)`** — Hibernate until a specific wall-clock time. Useful for "wake me when the market opens at 9:30 AM Eastern." Converts the target to a UTC delay and uses the same mechanism as `sleep`. Returns immediately if the target is in the past.

**`sleep_until_event_or_input()`** — Hibernate indefinitely. The runtime stops without scheduling anything. The agent wakes when the next message arrives naturally: user input or subscription event. This is the tool agents use after setting up subscriptions when there's nothing else to do.

All sleep tools are interruptible. If a user message or subscription event arrives while the agent is sleeping, the runtime processes it immediately. The pending sleep result arrives later and is appended to history normally.

## Thread tools

**`spawn_thread(instructions)`** — Create a child thread for parallel work. The child gets its own message group on the input queue and inherits the parent's conversation history truncated at the spawn point. See [Threading](/docs/infinity-runtime/threading).

**`report_to_parent(report)`** — Send intermediate results to the parent thread without closing the current thread. The report appears in the parent's conversation as a synthetic tool result.

**`close_thread(thread_id, report_to_parent?)`** — Shut down the current thread. Optionally sends a final report to the parent. Subscriptions should be cancelled before closing.

## Utility tools

**`get_time()`** — Returns the current time in a specified timezone. This is a RAP tool server (not a built-in), available as a CDK construct (`GetTimeToolSet`) or as a local standalone server for development.
