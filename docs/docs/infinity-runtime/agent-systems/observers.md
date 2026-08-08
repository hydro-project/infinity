---
sidebar_position: 5
title: Observers
---

# Observers

Everything a thread does streams through a `ThreadObserver`. It is one trait with two kinds of methods, reflecting the two things an embedding needs from the runtime: a **synchronous event feed** for showing users what's happening, and **awaited durability hooks** for persisting derived state at exactly the right moments.

```rust
#[async_trait(?Send)]
pub trait ThreadObserver {
    type SubscribeRequest: Send + 'static;

    // The event feed: called synchronously at the emission point.
    fn on_event(&self, thread_id: &str, event: &AgentEvent);

    // Durability hooks: awaited inline at their transition points.
    async fn on_user_choice_required(&self, thread_id: &str, choice: &UserChoice) -> Result<(), BoxError>;
    async fn on_user_choice_dismissed(&self, thread_id: &str, choice_id: &str) -> Result<(), BoxError>;
    async fn on_commit(&self, thread_id: &str) -> Result<(), BoxError>;

    // Live attach (local driver mode only).
    fn on_subscribe(&self, thread_id: &str, request: Self::SubscribeRequest, snapshot: ReplaySnapshot);
}
```

Two ready-made implementations cover the trivial cases: `EventCollector` buffers all events in memory (the natural observer for a [step-mode](./step-mode.md) handler that inspects them after the slice), and `NullObserver` discards everything.

## The event feed

`AgentEvent` is the display-level story of a step: `UserInput` echoes, `CompletionStarted`, `TextChunk`, `ThinkingStarted`/`ThinkingChunk`/`ThinkingEnded`, `ToolCall` (with a pretty-printed `display_as` when the tool has a display script), `ToolResult` (with RAP display segments), `SubscriptionEvent`, `OAuthRequired`, `CompactionApplied`, `CompletionFinished` (with token usage), and `Info` diagnostics. Unlike the low-level `DisplayEvent` stream it is `Clone` and generic-free, so an observer can fan events out to any number of subscribers or buffer them freely.

`on_event` is deliberately synchronous and called inline at the emission point. Keep it fast (push to channels, append to buffers) and put anything durable in `on_commit`. The synchronous call is not an implementation shortcut; it is what makes replay correct, as the next sections explain.

## Durability hooks

The async methods are awaited by the runtime at precise points in the step, so state your implementation persists in them is durable **before the world can move on**:

- **`on_commit`** is the workhorse: awaited once per step, after the turn's history has been synced to the `ConversationStore` and *before* any tool call is dispatched. Anything you derive from the event feed (token counters, timestamps, rendered transcripts) should be persisted here: if it succeeds, your state and the history agree, and only then does the tool call go out. An error fails the step before the dispatch.
- **`on_user_choice_required`** fires when a tool server asks the user to choose among options. It is awaited before the step continues, so a crash can never lose a choice the user has already been shown; persist the pending choice, then surface it to clients.
- **`on_user_choice_dismissed`** fires when a pending choice becomes moot (its tool call was interrupted); the pending-choice record is removed durably before the agent acts on the interruption.

This ordering (events, sync, commit, *then* dispatch) is the same turn-durability barrier described in [Architecture](../architecture.md), extended to embedding state.

## Live attach and replay

In [local mode](./running-locally.md), clients can attach to a thread that is already running mid-completion. `RunningSystem::subscribe(thread_id, request)` routes the embedding-defined `SubscribeRequest` to the thread's driver, which calls `on_subscribe` with a `ReplaySnapshot`: the committed history *plus* in-memory state that exists nowhere else, namely the partially streamed turn and any in-progress reasoning. The implementation renders the snapshot into its catch-up message and registers the subscriber in the same list its `on_event` fan-out broadcasts to.

The guarantee that makes this correct is **exactly-once delivery relative to attach**: the driver invokes `on_subscribe` on the same task that emits events, at a safe point between step polls. Every event is therefore either already reflected in the snapshot a new subscriber receives, or broadcast to it afterwards: never both, never neither. This is why `on_event` is synchronous, and why the subscriber registry lives on the observer rather than in the runtime: handing the snapshot to another task to do the registration would reopen the race.

`subscribe` resolves once the subscriber is installed (whether the thread's driver was already running or had to be consulted through the router), so the common attach-then-send sequence is safe: after `subscribe(...).await` returns `true`, a message sent to the thread is guaranteed to be observed by the new subscriber.

The Infinity Code daemon's `DaemonObserver` exercises the full trait: `on_event` updates session state derived from events (its stores are in-memory, so this is a cheap synchronous call) and broadcasts protocol messages to attached terminal/web clients, the choice hooks persist pending choices so a reconnecting client sees them, and `on_subscribe` replays history, mid-stream text, and in-progress thinking to the attaching client.
