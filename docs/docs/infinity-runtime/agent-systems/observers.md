---
sidebar_position: 8
title: Observers
---

# Observers
A `ThreadObserver` receives everything a thread does. [Thread handles](./running-locally.md#sending-input-and-receiving-events) deliver events on a channel, and when that is all you need, you never write an observer. You implement one when your embedding has its own clients to serve: a UI that renders events live, a database that records token usage, a protocol that surfaces pending user choices. The trait provides two instruments: a **synchronous event feed** for display, and **awaited durability hooks** for persisting your own state at defined moments.

For example, a chat server that broadcasts every thread's activity to connected WebSocket clients while keeping a per-thread token counter in a database is one observer:

```rust
struct MyObserver {
    thread_id: String,
    clients: ClientRegistry,          // your fan-out list
    db: Database,                     // your storage
    tokens_this_step: Cell<u64>,
}

#[async_trait(?Send)]
impl ThreadObserver for MyObserver {
    type SubscribeRequest = WebSocketClient;

    fn on_event(&self, thread_id: &str, event: &AgentEvent) {
        // Called synchronously at the emission point: keep it fast.
        if let AgentEvent::CompletionFinished { usage: Some(u) } = event {
            self.token_counter.add(thread_id, u.total_tokens);
        }
        self.clients.broadcast(render(thread_id, event));
    }

    fn on_subscribe(&self, thread_id: &str, client: WebSocketClient, snapshot: ReplaySnapshot) {
        client.send(render_replay(thread_id, &snapshot));
        self.clients.register(client);
    }
}

let running = system.start_with_observer(|thread_id| MyObserver::new(thread_id));
```

`start_with_observer` takes a factory rather than an observer because each thread's driver receives its own instance, created when the driver spawns. State that must outlive a driver, such as the client registry above, lives outside and is cloned into each observer. Two provided implementations cover the trivial cases: `EventCollector` buffers `(thread_id, event)` pairs in memory (suited to a [step-mode](./step-mode.md) handler that inspects them after the slice), and `NullObserver` discards everything.

The observer API has three connected responsibilities. `AgentEvent` is the display-level record of a step, in order: the `UserInput` echo, `CompletionStarted`, streamed `TextChunk`s and `ThinkingStarted`/`ThinkingChunk`/`ThinkingEnded`, `ToolCall` (with a pretty-printed `display_as` when the tool has a display script), `ToolResult` (with RAP display segments), and `CompletionFinished` (with token usage), plus out-of-band `SubscriptionEvent`, `OAuthRequired`, `CompactionApplied`, and `Info` diagnostics. The type is `Clone` and generic-free, so you can fan events out to any number of subscribers or buffer them freely.

`on_event` is synchronous and called inline at the emission point. Keep it fast: push to channels, append to buffers, or update in-memory counters. Work that needs to await belongs on a task you spawn from it.

## Persist State and Attach Live Clients
The async methods are awaited by the runtime at precise points in the step, so state you persist in them is durable before the world can move on:

- **`on_user_choice_required`** fires when a tool server asks the user to choose among options. It is awaited before the step continues, so a crash can never lose a choice the user has already been shown; persist the pending choice, then surface it to clients.
- **`on_user_choice_dismissed`** fires when a pending choice becomes moot because its tool call was interrupted. Remove the pending-choice record here, durably, before the agent acts on the interruption.

The turn itself needs no hook: the runtime syncs history to the `ConversationStore` before dispatching any tool call, so by the time your embedding can observe an external effect, the turn that caused it is already durable. This is the same turn-durability barrier described in [Architecture](../architecture.md).

In [local mode](./running-locally.md), clients can attach to a thread that is already running mid-completion, and `on_subscribe` is where you catch them up. `RunningSystem::subscribe(thread_id, request)` routes your `SubscribeRequest` (any type you choose: a client handle, a channel, a session token) to the thread's driver, which calls `on_subscribe` with a `ReplaySnapshot`: the committed history *plus* in-memory state that exists nowhere else, namely the partially streamed turn and any in-progress reasoning. Render the snapshot into your catch-up message, then register the subscriber in the same list your `on_event` fan-out broadcasts to, as the example above does.

The guarantee that makes this correct is **exactly-once delivery relative to attach**: every event is either already reflected in the snapshot a new subscriber receives, or broadcast to it afterwards, never both and never neither. To keep the guarantee, register the subscriber inside `on_subscribe` and render the snapshot from there. Handing the snapshot to another task to register later reopens the gap the guarantee closes, and events emitted in between are lost.

`subscribe` resolves once the subscriber is installed, so the common attach-then-send sequence is safe: any message sent after it returns is observed by the new subscriber.

For a production-grade example, read the Infinity Code daemon's `DaemonObserver`: `on_event` updates session state derived from events (its stores are in-memory, so this is a cheap synchronous call) and broadcasts protocol messages to attached terminal and web clients, the choice hooks persist pending choices so a reconnecting client sees them, and `on_subscribe` replays history, mid-stream text, and in-progress thinking to the attaching client.
