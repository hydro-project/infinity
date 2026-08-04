//! The [`ThreadObserver`] trait: how embeddings observe thread execution.

use async_trait::async_trait;
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
    fn on_event(&self, thread_id: &str, event: &AgentEvent);

    /// A subscriber attached to a running thread (local driver mode only;
    /// step-mode embeddings never receive this call).
    fn on_subscribe(
        &self,
        _thread_id: &str,
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
    events: RefCell<Vec<(String, AgentEvent)>>,
}

impl EventCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the buffered `(thread_id, event)` pairs, leaving the collector
    /// empty.
    pub fn take(&self) -> Vec<(String, AgentEvent)> {
        std::mem::take(&mut *self.events.borrow_mut())
    }
}

#[async_trait(?Send)]
impl ThreadObserver for EventCollector {
    type SubscribeRequest = ();

    fn on_event(&self, thread_id: &str, event: &AgentEvent) {
        self.events
            .borrow_mut()
            .push((thread_id.to_owned(), event.clone()));
    }
}
