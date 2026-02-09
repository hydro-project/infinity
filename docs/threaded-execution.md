# Threaded Execution

Infinity Agents support spawning child threads to decompose complex tasks into parallel sub-tasks. Threads can be arbitrarily nested, and each child thread inherits the conversation context of its ancestors up to the point it was spawned.

## How It Works

A running agent can call `spawn_thread(instructions)` to create a child thread. This produces two tool results:

1. The **parent** thread receives: "Child thread is successfully spawned and has ID: {id}"
2. The **child** thread receives: "You are now in the spawned thread"

The child thread sees the full conversation history of all its ancestors, truncated at each spawn point. This means the child sees exactly the context that existed when it was created — not any work the parent does after spawning.

When a child thread finishes its work, it calls `close_thread(thread_id)` to mark itself complete. If the thread has something worth reporting — results, decisions, or information that should be remembered for future events handled by the parent or its future children — it can include a `report_to_parent` string. This sends a synthetic subscription event back to the parent thread (using the same mechanism as GitHub webhook notifications), waking the parent to process the report.

## History Construction

When a thread loads its conversation history, it walks the ancestor chain from root to leaf:

```
root (messages 1..N₁) → child_1 (messages 1..N₂) → child_2 (messages 1..N₃) → leaf (all messages)
```

Each ancestor's history is truncated at the `spawn_message_order` recorded when its child was spawned. The leaf thread's history is included in full. These segments are concatenated to form the complete context window.

For example, if the root thread has 50 messages and spawns a child at message 30, the child sees:
- Root messages 1–30 (truncated at spawn point)
- Its own full message history

No synthetic messages are injected between segments. The `spawn_thread` tool call naturally appears at the end of each truncated parent segment, and the `instructions` parameter in the tool call arguments tells the child what to do.

## Thread Hierarchy Table

Thread relationships are stored in DSQL:

```sql
CREATE TABLE thread_hierarchy (
    thread_id VARCHAR(255) PRIMARY KEY,
    parent_thread_id VARCHAR(255),
    root_thread_id VARCHAR(255) NOT NULL,
    spawn_message_order BIGINT,
    closed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
);
```

- `parent_thread_id` is NULL for root threads
- `spawn_message_order` records where to truncate the parent's history
- `root_thread_id` is the top-level conversation ID (e.g. the Slack thread)

## Output Labeling

Messages from non-root threads are prefixed with a nesting label so users can see which thread produced them:

```
[abcd1234:efgh5678] I've finished reviewing the PR.
```

Each segment is the first 8 characters of the thread ID, from root to leaf.

## Metadata

Conversation metadata (stored in DynamoDB) is always keyed by the root thread ID. This ensures all threads in a conversation share the same metadata — things like Slack channel info and user identity are consistent regardless of nesting depth.

## Use Cases

- **Parallel code review**: Spawn one thread per file or module to review concurrently
- **Research and execute**: One thread researches while another implements
- **Long-running monitoring**: Spawn a watcher thread that subscribes to events while the parent continues other work
- **Divide and conquer**: Break a large task into independent sub-tasks, each in its own thread
