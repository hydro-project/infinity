---
sidebar_position: 2
title: Threading
---

# Threading

The Infinity Runtime lets agents spawn child threads for parallel work. Each thread runs independently with its own context, and can report results back to the parent.

Threads are useful for concurrent processing and for context management:
- **Parallel code review**: the agent spawns one thread per file, and each thread reviews its file independently and reports back.
- **Research and execute**: one thread researches the approach while another starts implementing.
- **Event processing**: subscription events are handled in isolated threads, so they do not pollute the parent context.
- **Divide and conquer**: a large task can be broken into sub-tasks, each in its own thread with a focused context.

## Spawning a thread

When the agent needs to do something in parallel, such as reviewing multiple files or researching while implementing, it calls `spawn_thread`:

```
🤖 Agent:  I'll review these three files in parallel.

🔧 Tool call:  spawn_thread({ instructions: "Review src/auth.ts for security issues",
                              child_of: ["thread_root"] })
📥 Result:     "Child thread spawned with ID: thread_a1b2"

🔧 Tool call:  spawn_thread({ instructions: "Review src/api.ts for error handling",
                              child_of: ["thread_root"] })
📥 Result:     "Child thread spawned with ID: thread_c3d4"
```

The required `child_of` argument is the caller's full thread stack, from the root thread to itself. Because children inherit the parent's context (including any plans to spawn threads), a child can get confused and try to execute the parent's spawns. The stack check will reject those calls with an error telling the child to focus on its own task.

Each child thread starts with the parent's conversation history up to the point it was spawned, plus the instructions. The parent continues immediately, without waiting for its children to finish.

The child thread inherits context from its ancestors. For example, if the parent had a 30-message conversation before spawning, the child will see those 30 messages truncated at the spawn point, followed by its own spawn instruction and result:

```
── inherited from parent (messages 1–30) ──

👤 User:       Please review src/auth.ts and src/api.ts for issues.
🤖 Agent:      I'll review these in parallel.
🔧 Tool call:  spawn_thread({ instructions: "Review src/auth.ts for security issues" })

── child thread starts here ──

📥 Result:     "You are now inside the spawned thread. Follow the
                instructions in the tool call. Your thread ID is thread_a1b2."
```

This gives the child enough context to understand the task without the parent having to repeat anything. However, the child will not see any messages that the parent produces after the spawn point.

## Reporting back

Children can send results to the parent at any time using `report_to_parent`:

```
[Child thread_a1b2]

🤖 Agent:      Found a SQL injection vulnerability in the auth handler.

🔧 Tool call:  report_to_parent({ report: "Critical: SQL injection in auth.ts
               line 42. The user input is interpolated directly into the query." })
📥 Result:     "Report sent to parent thread."
```

The parent sees this as a [synthetic tool call](/docs/rap/about/subscription-events#synthetic-tool-calls), which is the same mechanism used for subscription events. The runtime will inject a synthetic `receive_event__injected` call and result into the parent's history:

```
[Parent thread]

🔧 Synthetic:  receive_event__injected({
                 original_tool_name: "spawn_thread",
                 original_tool_call_id: "call_spawn_a1b2",
                 original_args: {
                   instructions: "Review src/auth.ts for security issues"
                 }
               })
📥 Result:     "Report from child thread: Critical: SQL injection in
                auth.ts line 42. The user input is interpolated directly
                into the query."
```

The report is tied to the original `spawn_thread` call, so the LLM knows which child it came from. The child can send multiple reports before closing.

## Closing a thread

When a child is done, it calls `close_thread` with an optional final report:

```
[Child thread_a1b2]

🔧 Tool call:  close_thread({
                 thread_id: "thread_a1b2",
                 report_to_parent: "Review complete. 1 critical issue, 2 warnings."
               })
```

The parent will see the report via the same synthetic tool call mechanism:

```
[Parent thread]

🔧 Synthetic:  receive_event__injected({
                 original_tool_name: "spawn_thread",
                 original_tool_call_id: "call_spawn_a1b2",
                 original_args: {
                   instructions: "Review src/auth.ts for security issues"
                 }
               })
📥 Result:     "Child thread thread_a1b2 has shut down. Report:
                Review complete. 1 critical issue, 2 warnings."
```

## Subscription event threads

When a [subscription event](/docs/rap/about/subscription-events) arrives, the Infinity Runtime automatically spawns a temporary child thread to process it. This keeps the parent's context clean, since each event gets its own fresh context window.

The child is seeded with the event data and instructions to process it:

```
[Auto-spawned child for subscription event]

🔧 Synthetic:  receive_event__injected({
                 original_tool_name: "subscribe_github_events",
                 original_tool_call_id: "call_abc123",
                 original_args: { owner: "acme", repo: "api" }
               })
📥 Result:     {"event_type": "pull_request", "action": "opened", "number": 42}

🔧 Synthetic:  spawn_thread({
                 instructions: "Process the subscription event above, then
                 close with a report."
               })
📥 Result:     "You are in a new thread created for processing a
                subscription event."
```

The child processes the event (e.g. it reads the PR diff, runs checks, and posts a review) and then closes with a report. The parent sees the report without its context being cluttered by the raw event data. If an event is irrelevant to the parent, the child can also shut down without providing a report; in that case, the parent thread will continue running as if the event never happened.

If multiple events arrive close together, each one will get its own child thread, and the threads will process the events concurrently.
