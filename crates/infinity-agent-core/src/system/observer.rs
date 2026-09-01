//! The [`ThreadObserver`] trait: how embeddings observe thread execution.

use async_trait::async_trait;
use rap_protocol::ThreadId;
use std::cell::RefCell;

use super::events::{AgentEvent, ReplaySnapshot};

/// Observes a thread's execution and commits any platform state derived from
/// it.
///
/// The step pipeline calls these methods at exact points in its execution —
/// there is no intermediate channel or forwarding task — which gives two
/// guarantees:
///
/// - **Ordering**: [`on_event`](Self::on_event) is invoked synchronously at
///   the moment the event is emitted, in the same executor poll that produced
///   it. An embedding that broadcasts events to subscribers and answers
///   attach requests from [`on_subscribe`](Self::on_subscribe) therefore gets
///   exactly-once delivery for free: an event is either already reflected in
///   the [`ReplaySnapshot`] a new subscriber receives, or it is broadcast to
///   that subscriber afterwards — never both, never neither.
/// - **Durability**: stateful events are emitted after the runtime has awaited
///   their corresponding [`StateStore`](crate::traits::StateStore) transition.
///
/// Implementations that need mutable state should use interior mutability
/// (`RefCell`); the methods take `&self` so the runtime can hold shared
/// references across a step.
#[async_trait(?Send)]
pub trait ThreadObserver {
    /// The attach-request type routed to running threads (see
    /// [`RunningSystem::subscribe`](super::local::RunningSystem::subscribe)).
    /// Embeddings that do not use the local driver can use `()`.
    type SubscribeRequest: Send + 'static;

    /// A display event was emitted. Called synchronously at the emission
    /// point; keep this fast (fan-out, buffering).
    fn on_event(&self, thread_id: &ThreadId, event: &AgentEvent);

    /// A subscriber attached to a running thread (local driver mode only;
    /// step-mode embeddings never receive this call).
    ///
    /// This is the **live-attach hook**: when a client attaches to a thread
    /// that is already running (via
    /// [`RunningSystem::subscribe`](super::local::RunningSystem::subscribe)), the
    /// driver produces a [`ReplaySnapshot`] — including in-memory state that
    /// exists nowhere else, like the partially streamed turn and in-progress
    /// reasoning — and hands it to the observer together with the
    /// embedding-specific `request`. The implementation should render the
    /// snapshot into its client-facing catch-up message and then register the
    /// subscriber in the same list its [`on_event`](Self::on_event) fan-out
    /// broadcasts to.
    ///
    /// The registration lives on the observer (rather than on
    /// [`RunningSystem`](super::local::RunningSystem)) for two reasons: only the
    /// embedding knows its client message type and subscriber registry, and
    /// the snapshot + registration must happen **atomically relative to the
    /// live event stream**. The driver invokes this method on the same task
    /// that emits events, at a safe point between step polls, so every event
    /// is either reflected in the snapshot or broadcast to the newly
    /// registered subscriber afterwards — never both, never neither. Handing
    /// the snapshot to another task to do the registration would reopen that
    /// race.
    fn on_subscribe(
        &self,
        _thread_id: &ThreadId,
        _request: Self::SubscribeRequest,
        _snapshot: ReplaySnapshot,
    ) {
    }
}

/// An observer that buffers all events in memory, tagged with the thread that
/// emitted them. Useful for step-mode embeddings (e.g. a Lambda handler) that
/// inspect the events after the slice completes — a slice's batch may span
/// multiple threads.
#[derive(Default)]
pub struct EventCollector {
    events: RefCell<Vec<(ThreadId, AgentEvent)>>,
}

impl EventCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the buffered `(thread_id, event)` pairs, leaving the collector
    /// empty.
    pub fn take(&self) -> Vec<(ThreadId, AgentEvent)> {
        std::mem::take(&mut *self.events.borrow_mut())
    }
}

#[async_trait(?Send)]
impl ThreadObserver for EventCollector {
    type SubscribeRequest = ();

    fn on_event(&self, thread_id: &ThreadId, event: &AgentEvent) {
        self.events
            .borrow_mut()
            .push((thread_id.clone(), event.clone()));
    }
}
