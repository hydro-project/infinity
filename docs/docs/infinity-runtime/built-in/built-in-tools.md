---
sidebar_position: 1
title: Built-in Tools
---

# Built-in Tools

The Infinity Runtime ships with a set of built-in tools that are available to every agent. These are not part of the RAP protocol; instead, they are provided by the runtime itself. Because they are implemented in `infinity_agent_core::tools` and are generic over the [`InputSender` trait](../low-level/overview.md#the-platform-traits), they work identically on Lambda and in embedded runtimes.

## Sleep tools

The runtime yields after every tool call, but sometimes the agent should explicitly wait before continuing. To support this, the sleep tools schedule a future wake-up message. Both deployments support all three tools; the only difference is the underlying timer mechanism.

**`sleep(seconds)`**: Hibernates for a fixed duration. On Lambda, delays of 900 seconds or less will go through SQS `DelaySeconds` via a relay queue, while longer delays use EventBridge Scheduler. In an embedded runtime, the core's tokio-backed version (registered via `AgentSystemBuilder::with_tokio_sleep_tools`) delivers the wake-up through the in-memory channel.

**`sleep_until(date, time, timezone)`**: Hibernates until a specific wall-clock time, which is useful for requests such as "wake me when the market opens at 9:30 AM Eastern." The tool converts the target to a UTC delay and uses the same mechanism as `sleep`; if the target is in the past, it will return immediately.

**`sleep_until_event_or_input()`**: Hibernates indefinitely. The tool is a no-op: the slice ends without scheduling anything, and the agent will wake when the next message (user input or a subscription event) arrives naturally. Agents use this tool after setting up subscriptions, when there is nothing else to do.

All sleep tools are interruptible. If a user message or subscription event arrives while the agent is sleeping, the runtime will process it immediately, because sleep tools are *passive* (via `Tool::is_passive`) and pending sleep calls never hold back other work. The pending sleep result arrives later and is appended to history normally.

## Thread tools

`spawn_thread` and `cancel_subscription` are [synchronous tools](../architecture.md#synchronous-tools-loop-back), which means that they execute inline and loop back into the completion rather than yielding. The other thread tools dispatch like ordinary tools, and their results are delivered back through the input queue.

**`spawn_thread(instructions, child_of)`**: Creates a child thread for parallel work. The child gets its own message group on the input queue, and inherits the parent's conversation history truncated at the spawn point. The required `child_of` argument is the caller's full thread stack (from the root to the current thread); the runtime will reject the call if it does not match, which stops a child thread that inherited the parent's plans from accidentally spawning the parent's threads. See [Threading](./threading.md).

**`report_to_parent(report)`**: Sends intermediate results to the parent thread without closing the current thread. The report appears in the parent's conversation as a synthetic tool result.

**`close_thread(thread_id, report_to_parent?)`**: Shuts down the current thread, optionally sending a final report to the parent. Subscriptions should be cancelled before closing.

**`send_message_to_child(thread_id, message)`**: Injects a message into a running child thread's conversation. The target must be a direct child of the calling thread.

**`cancel_subscription(tool_call_id)`**: Cancels an active RAP subscription, notifying the tool server and removing it from the thread's active subscription tracking.

Applications can register additional tools alongside the built-ins. For example, the Infinity Code daemon adds `set_title`, which lets the agent set a short human-readable title for the current thread. These titles show up in the session picker and the web UI.
