//! The [`DeferQueue`] trait: holding back synthetic events while a tool call
//! is in flight.
//!
//! When a thread has dispatched a tool call and is waiting for its result,
//! processing an unrelated synthetic event (a subscription event, a child
//! thread's report, a message from the parent) would *interrupt* the pending
//! call: the runtime injects a synthetic "interrupted" result so the history
//! stays well-formed, and the real result is later dropped as stale. To avoid
//! cancelling work that is still running, such events should be deferred
//! until the pending call is answered.
//!
//! Whether deferral is possible depends on the platform. A resident runtime
//! (the local driver, the Infinity Code daemon) can hold events in memory. A
//! serverless embedding has no process to hold them in — it can either
//! implement this trait against its transport (e.g. re-enqueueing the message
//! with a delay on SQS) or use [`NoDeferral`] to process events immediately,
//! accepting the interruption semantics.

use async_trait::async_trait;

use crate::message::InputMessage;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Where deferrable inputs wait while the thread cannot process them.
#[async_trait(?Send)]
pub trait DeferQueue {
    /// Hold back an input for later redelivery. Returns `Ok(false)` if this
    /// queue cannot defer, in which case the step processes the input
    /// immediately (interrupting any pending tool call).
    async fn push(&mut self, input: InputMessage, message_id: String) -> Result<bool, BoxError>;

    /// Take back all deferred inputs. Called when the thread can process them
    /// (the pending tool call was answered or deliberately interrupted).
    async fn drain(&mut self) -> Result<Vec<(InputMessage, String)>, BoxError>;
}

/// In-memory deferral for resident runtimes. Deferred events are lost if the
/// process dies; on a durable transport prefer a transport-level
/// implementation.
#[derive(Default)]
pub struct InMemoryDeferQueue {
    items: Vec<(InputMessage, String)>,
}

impl InMemoryDeferQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[async_trait(?Send)]
impl DeferQueue for InMemoryDeferQueue {
    async fn push(&mut self, input: InputMessage, message_id: String) -> Result<bool, BoxError> {
        self.items.push((input, message_id));
        Ok(true)
    }

    async fn drain(&mut self) -> Result<Vec<(InputMessage, String)>, BoxError> {
        Ok(std::mem::take(&mut self.items))
    }
}

/// No deferral: every input is processed in the slice it arrives in, even if
/// that interrupts a pending tool call. This matches platforms without a
/// place to hold messages (e.g. a plain SQS-driven Lambda).
pub struct NoDeferral;

#[async_trait(?Send)]
impl DeferQueue for NoDeferral {
    async fn push(&mut self, _input: InputMessage, _message_id: String) -> Result<bool, BoxError> {
        Ok(false)
    }

    async fn drain(&mut self) -> Result<Vec<(InputMessage, String)>, BoxError> {
        Ok(Vec::new())
    }
}
