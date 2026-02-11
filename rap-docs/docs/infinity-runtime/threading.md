---
sidebar_position: 3
title: Threading
---

# Threading

The Infinity Runtime supports spawning child threads for parallel work. Each thread has its own message group on the input FIFO queue, so threads process concurrently without blocking each other.

## Spawning

The `spawn_thread(instructions)` tool creates a child thread. The runtime records the parent-child relationship, then sends two messages: a result to the parent ("Child thread spawned with ID: \{id\}") and a result to the child ("You are inside the spawned thread"). Both are enqueued with their respective `group_id`s and process independently.

## Context inheritance

When loading history for a child thread, the runtime walks the ancestor chain from root to leaf. Each ancestor's history is truncated at the `spawn_message_order` recorded when its child was created. The leaf thread's history is included in full. These segments are concatenated to form the complete context window.

The `spawn_thread` tool call naturally appears at the end of each truncated segment, and its `instructions` parameter tells the child what to do.

## Reporting

`report_to_parent(report)` sends a message to the parent thread without closing the current thread. The report is delivered as a synthetic thread report — injected into the parent's conversation as a tool result tied to the original `spawn_thread` call.

`close_thread(thread_id, report_to_parent?)` marks the thread as closed and optionally sends a final report. For subscription event threads, the report is annotated to indicate it came from an event handler.

## Subscription event threads

When a subscription event arrives, the runtime automatically spawns a temporary child thread to process it. The child is seeded with the event content and instructions to process it, then close with a report. The parent's subscription remains active — future events spawn new child threads. Multiple events can be processed concurrently.

## Thread hierarchy

Thread relationships are stored in DSQL:

```sql
CREATE TABLE thread_hierarchy (
    thread_id            VARCHAR(255) PRIMARY KEY,
    parent_thread_id     VARCHAR(255),
    root_thread_id       VARCHAR(255) NOT NULL,
    spawn_message_order  BIGINT,
    spawn_tool_call_id   VARCHAR(255),
    closed               BOOLEAN NOT NULL DEFAULT FALSE,
    is_subscription_event BOOLEAN NOT NULL DEFAULT FALSE
);
```

Output from non-root threads is prefixed with a nesting label (`[abcd1234:efgh5678]`) so users can identify the source. Conversation metadata is always keyed by the root thread ID — all threads in a conversation share the same metadata.
