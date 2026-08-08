//! The [`ThreadObserver`] trait: how embeddings observe thread execution.

use async_trait::async_trait;
use std::cell::RefCell;

use super::events::{AgentEvent, ReplaySnapshot, UserChoice};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
/// - **Durability**: the async methods are awaited inline at their exact
///   transition points, and [`on_commit`](Self::on_commit) is awaited after
///   the step's history has been synced to the [`ConversationStore`] and
///   *before* any tool call is dispatched or the step returns. State an
///   implementation persists there is durable before the outside world can
///   react to the step.
///
/// Implementations that need mutable state should use interior mutability
/// (`RefCell`); the methods take `&self` so the runtime can hold shared
/// references across a step.
///
/// [`ConversationStore`]: crate::traits::ConversationStore
#[async_trait(?Send)]
pub trait ThreadObserver {
    /// The attach-request type routed to running threads (see
    /// [`RunningSystem::subscribe`](super::RunningSystem::subscribe)).
    /// Embeddings that do not use the local driver can use `()`.
    type SubscribeRequest: Send + 'static;

    /// A display event was emitted. Called synchronously at the emission
    /// point; keep this fast (fan-out, buffering). Persist derived state in
    /// [`on_commit`](Self::on_commit).
    fn on_event(&self, thread_id: &str, event: &AgentEvent);

    /// A tool server requested a user choice. Awaited before the step
    /// continues: persist the pending choice here (so a crash cannot lose a
    /// choice the user has already been shown), then surface it to clients.
    /// An error fails the step.
    async fn on_user_choice_required(
        &self,
        _thread_id: &str,
        _choice: &UserChoice,
    ) -> Result<(), BoxError> {
        Ok(())
    }

    /// A pending user choice became moot (its tool call was interrupted).
    /// Awaited before the step continues processing — the pending-choice
    /// state is removed durably before the agent can act on the
    /// interruption. An error fails the step.
    async fn on_user_choice_dismissed(
        &self,
        _thread_id: &str,
        _choice_id: &str,
    ) -> Result<(), BoxError> {
        Ok(())
    }

    /// Durability barrier: called once per step, after the thread's history
    /// has been synced to the conversation store and before any tool call is
    /// dispatched. Persist state derived from the observed events here. An
    /// error fails the step before the tool call is dispatched.
    async fn on_commit(&self, _thread_id: &str) -> Result<(), BoxError> {
        Ok(())
    }

    /// A subscriber attached to a running thread (local driver mode only;
    /// step-mode embeddings never receive this call).
    ///
    /// This is the **live-attach hook**: when a client attaches to a thread
    /// that is already running (via
    /// [`RunningSystem::subscribe`](super::RunningSystem::subscribe)), the
    /// driver produces a [`ReplaySnapshot`] — including in-memory state that
    /// exists nowhere else, like the partially streamed turn and in-progress
    /// reasoning — and hands it to the observer together with the
    /// embedding-specific `request`. The implementation should render the
    /// snapshot into its client-facing catch-up message and then register the
    /// subscriber in the same list its [`on_event`](Self::on_event) fan-out
    /// broadcasts to.
    ///
    /// The registration lives on the observer (rather than on
    /// [`RunningSystem`](super::RunningSystem)) for two reasons: only the
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
        _thread_id: &str,
        _request: Self::SubscribeRequest,
        _snapshot: ReplaySnapshot,
    ) {
    }
}

/// An observer that buffers all events in memory. Useful for step-mode
/// embeddings (e.g. a Lambda handler) that inspect the events after the slice
/// completes.
#[derive(Default)]
pub struct EventCollector {
    events: RefCell<Vec<AgentEvent>>,
}

impl EventCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the buffered events, leaving the collector empty.
    pub fn take(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
    }
}

#[async_trait(?Send)]
impl ThreadObserver for EventCollector {
    type SubscribeRequest = ();

    fn on_event(&self, _thread_id: &str, event: &AgentEvent) {
        self.events.borrow_mut().push(event.clone());
    }
}

/// An observer that ignores all events.
pub struct NullObserver;

#[async_trait(?Send)]
impl ThreadObserver for NullObserver {
    type SubscribeRequest = ();

    fn on_event(&self, _thread_id: &str, _event: &AgentEvent) {}
}
