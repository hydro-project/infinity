---
sidebar_position: 6
title: Built-in Tools
---

# Built-in Tools

The Infinity Runtime ships with a set of built-in tools that are available to every agent. These aren't part of the RAP protocol; they're conveniences provided by the runtime itself, implemented in `infinity_agent_core::tools` and generic over the [`InputSender` trait](./rust-api.md#the-platform-traits) so they work identically on Lambda and in embedded runtimes.

## Sleep tools

The runtime yields after every tool call, but sometimes the agent should explicitly wait before continuing. The sleep tools schedule a future wake-up message. Both deployments support all three; the difference is the underlying timer mechanism.

**`sleep(seconds)`**: Hibernate for a fixed duration. On Lambda, delays of 900 seconds or less use SQS `DelaySeconds` via a relay queue, and longer delays use EventBridge Scheduler. In an embedded runtime, a `tokio::time::sleep` timer fires in a spawned task and delivers the result through the in-memory channel.

**`sleep_until(date, time, timezone)`**: Hibernate until a specific wall-clock time. Useful for "wake me when the market opens at 9:30 AM Eastern." Converts the target to a UTC delay and uses the same mechanism as `sleep`. Returns immediately if the target is in the past.

**`sleep_until_event_or_input()`**: Hibernate indefinitely. The tool is a no-op; the slice simply ends without scheduling anything, and the agent wakes when the next message arrives naturally (user input or subscription event). This is the tool agents use after setting up subscriptions when there's nothing else to do.

All sleep tools are interruptible. If a user message or subscription event arrives while the agent is sleeping, the runtime processes it immediately. The pending sleep result arrives later and is appended to history normally.

## Thread tools

`spawn_thread` and `cancel_subscription` are [synchronous tools](./architecture.md#synchronous-tools-loop-back): they execute inline and loop back into the completion rather than yielding. The other thread tools dispatch like ordinary tools, with their results delivered back through the input queue.

**`spawn_thread(instructions, child_of)`**: Create a child thread for parallel work. The child gets its own message group on the input queue and inherits the parent's conversation history truncated at the spawn point. The required `child_of` argument is the caller's full thread stack (root to current thread); the runtime rejects the call if it doesn't match, which stops a child thread that inherited the parent's plans from accidentally spawning the parent's threads. See [Threading](./threading.md).

**`report_to_parent(report)`**: Send intermediate results to the parent thread without closing the current thread. The report appears in the parent's conversation as a synthetic tool result.

**`close_thread(thread_id, report_to_parent?)`**: Shut down the current thread, optionally sending a final report to the parent. Subscriptions should be cancelled before closing.

**`send_message_to_child(thread_id, message)`**: Inject a message into a running child thread's conversation. The target must be a direct child of the calling thread.

**`cancel_subscription(tool_call_id)`**: Cancel an active RAP subscription, notifying the tool server and removing it from the thread's active subscription tracking.

## Embedding-specific tools

Embeddings can register additional tools alongside the built-ins. The Infinity Code daemon adds `set_title`, which lets the agent set a short human-readable title for the current thread; the titles show up in the session picker and web UI.
