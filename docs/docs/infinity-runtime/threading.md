---
sidebar_position: 6
title: Threading
---

# Threading

The Infinity Runtime lets agents spawn child threads for parallel work. Each thread runs independently with its own context, and can report results back to the parent.

Threads are useful for concurrent processing and context management:
- **Parallel code review** — spawn one thread per file, each reviews independently, reports back
- **Research and execute** — one thread researches the approach while another starts implementing
- **Event processing** — subscription events are handled in isolated threads without polluting the parent context
- **Divide and conquer** — break a large task into sub-tasks, each in its own thread with focused context


## Spawning a thread

When the agent needs to do something in parallel — review multiple files, research while implementing, process a subscription event — it calls `spawn_thread`:

```
🤖 Agent:  I'll review these three files in parallel.

🔧 Tool call:  spawn_thread({ instructions: "Review src/auth.ts for security issues" })
📥 Result:     "Child thread spawned with ID: thread_a1b2"

🔧 Tool call:  spawn_thread({ instructions: "Review src/api.ts for error handling" })
📥 Result:     "Child thread spawned with ID: thread_c3d4"
```

Each child thread starts with the parent's conversation history up to the point it was spawned, plus the instructions. The parent continues immediately — it doesn't wait for children to finish.

The child thread inherits context from its ancestors. If the parent had a 30-message conversation before spawning, the child sees those 30 messages truncated at the spawn point, followed by its own spawn instruction and result:

```
── inherited from parent (messages 1–30) ──

👤 User:       Please review src/auth.ts and src/api.ts for issues.
🤖 Agent:      I'll review these in parallel.
🔧 Tool call:  spawn_thread({ instructions: "Review src/auth.ts for security issues" })

── child thread starts here ──

📥 Result:     "You are now inside the spawned thread. Follow the
                instructions in the tool call. Your thread ID is thread_a1b2."
```

The child has enough context to understand the task without the parent having to repeat anything. It doesn't see any messages the parent produces after the spawn point.

## Reporting back

Children can send results to the parent at any time using `report_to_parent`:

```
[Child thread_a1b2]

🤖 Agent:      Found a SQL injection vulnerability in the auth handler.

🔧 Tool call:  report_to_parent({ report: "Critical: SQL injection in auth.ts
               line 42. The user input is interpolated directly into the query." })
📥 Result:     "Report sent to parent thread."
```

The parent sees this as a [synthetic tool call](/docs/about/subscription-events#synthetic-tool-calls) — the same mechanism used for subscription events. The runtime injects a synthetic `spawn_thread` call and result into the parent's history:

```
[Parent thread]

🔧 Synthetic:  spawn_thread({
                 instructions: "Review src/auth.ts for security issues",
                 kind: "thread_report:call_spawn_a1b2"
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

The parent sees the report via the same synthetic tool call mechanism:

```
[Parent thread]

🔧 Synthetic:  spawn_thread({
                 instructions: "Review src/auth.ts for security issues",
                 kind: "thread_report:call_spawn_a1b2"
               })
📥 Result:     "Child thread thread_a1b2 has shut down. Report:
                Review complete. 1 critical issue, 2 warnings."
```

## Subscription event threads

When a [subscription event](/docs/about/subscription-events) arrives, the Infinity Runtime automatically spawns a temporary child thread to process it. This keeps the parent's context clean — each event gets its own fresh context window.

The child is seeded with the event data and instructions to process it:

```
[Auto-spawned child for subscription event]

🔧 Synthetic:  subscribe_github_events({
                 owner: "acme", repo: "api",
                 kind: "interrupt:call_abc123 (subscription remains active)"
               })
📥 Result:     {"event_type": "pull_request", "action": "opened", "number": 42}

🔧 Synthetic:  spawn_thread({
                 instructions: "Process the subscription event above, then
                 close with a report."
               })
📥 Result:     "You are in a new thread created for processing a
                subscription event."
```

The child processes the event — reads the PR diff, runs checks, posts a review — and then closes with a report. The parent sees the report without its context being cluttered by the raw event data. If an event is irrelevant to the parent, the child can also shut down without providing a report, in which case the parent thread continues running as if the event never happened.

If multiple events arrive close together, each gets its own child thread and they process concurrently.
