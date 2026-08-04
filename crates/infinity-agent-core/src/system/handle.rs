//! Thread handles: a channel-based convenience layer over a local system.
//!
//! [`LocalAgentSystem::start_with_handles`] runs the system with a built-in
//! observer that fans events out over in-memory channels, and
//! [`RunningSystem::thread_handle`] attaches to one thread, returning a
//! [`ThreadHandle`] for sending inputs and receiving the thread's
//! [`AgentEvent`]s along with the initial [`ReplaySnapshot`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::message::InputMessage;
use crate::traits::{ConversationStore, StateStore};
use rap_client::http::HttpClient;

use super::builder::LocalAgentSystem;
use super::events::{AgentEvent, ReplaySnapshot};
use super::observer::ThreadObserver;
use super::router::RunningSystem;
use super::sender::{ChannelSendError, ChannelSender};

/// The handle subscribers attached to each thread. One registry is shared by
/// every [`HandleObserver`] the system creates, so a subscription survives
/// its thread's driver idling out and respawning.
#[derive(Clone, Default)]
struct HandleRegistry {
    subscribers: Rc<RefCell<HashMap<String, Vec<mpsc::UnboundedSender<AgentEvent>>>>>,
}

/// The subscribe request used by [`HandleObserver`]: where to send the
/// thread's live events, and a one-shot for the initial replay.
pub struct HandleSubscribeRequest {
    events_tx: mpsc::UnboundedSender<AgentEvent>,
    replay_tx: oneshot::Sender<ReplaySnapshot>,
}

/// The built-in observer behind [`ThreadHandle`]s: broadcasts every event to
/// the handles attached to the thread and answers subscribe requests with a
/// replay snapshot. Created per thread driver by
/// [`LocalAgentSystem::start_with_handles`]; the underlying subscriber
/// registry is shared across drivers.
pub struct HandleObserver {
    registry: HandleRegistry,
}

#[async_trait(?Send)]
impl ThreadObserver for HandleObserver {
    type SubscribeRequest = HandleSubscribeRequest;

    fn on_event(&self, thread_id: &str, event: &AgentEvent) {
        let mut subs = self.registry.subscribers.borrow_mut();
        if let Some(list) = subs.get_mut(thread_id) {
            // Prune handles that have been dropped.
            list.retain(|tx| tx.send(event.clone()).is_ok());
            if list.is_empty() {
                subs.remove(thread_id);
            }
        }
    }

    fn on_subscribe(
        &self,
        thread_id: &str,
        request: HandleSubscribeRequest,
        snapshot: ReplaySnapshot,
    ) {
        // A dropped receiver just means the handle no longer wants the replay.
        let _ = request.replay_tx.send(snapshot);
        self.registry
            .subscribers
            .borrow_mut()
            .entry(thread_id.to_owned())
            .or_default()
            .push(request.events_tx);
    }
}

/// A live connection to one thread of a running local system: send it
/// inputs, receive its events. Obtained from
/// [`RunningSystem::thread_handle`].
///
/// Dropping the handle detaches it; the thread itself is unaffected.
pub struct ThreadHandle {
    thread_id: String,
    sender: ChannelSender,
    /// The thread's events, in emission order, starting from the moment the
    /// handle attached. Every event is either reflected in
    /// [`replay`](Self::replay) or delivered here, exactly once.
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    replay: ReplaySnapshot,
}

impl ThreadHandle {
    /// The thread this handle is attached to.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The state of the thread at the moment the handle attached: committed
    /// history plus any in-flight turn and in-progress reasoning.
    pub fn replay(&self) -> &ReplaySnapshot {
        &self.replay
    }

    /// Receive the thread's next event. Returns `None` only after the system
    /// has shut down.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    /// Send plain user text to the thread.
    pub async fn send_user_text(&self, text: impl Into<String>) -> Result<(), ChannelSendError> {
        let msg = InputMessage {
            content: crate::message::InputMessageContent::User(rig::message::UserContent::text(
                text.into(),
            )),
            group_id: self.thread_id.clone(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };
        self.send(msg, &uuid::Uuid::new_v4().to_string()).await
    }

    /// Send an input message to the thread. The message's `group_id` is
    /// overwritten with this handle's thread ID; `dedup_id` should be stable
    /// across redeliveries of the same message.
    pub async fn send(
        &self,
        mut message: InputMessage,
        dedup_id: &str,
    ) -> Result<(), ChannelSendError> {
        message.group_id = self.thread_id.clone();
        use crate::traits::InputSender;
        self.sender
            .send_to_input_queue(message, &self.thread_id, dedup_id)
            .await
    }
}

impl RunningSystem<HandleSubscribeRequest> {
    /// Attach to `thread_id` and return a [`ThreadHandle`] for it. The thread
    /// does not need to exist yet: attaching to a fresh ID installs the
    /// subscription (with an empty replay) so events flow from the very first
    /// message later sent to it.
    ///
    /// Multiple handles can attach to the same thread; each receives every
    /// event. Returns `None` if the system is shutting down.
    pub async fn thread_handle(&self, thread_id: &str) -> Option<ThreadHandle> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (replay_tx, replay_rx) = oneshot::channel();
        let request = HandleSubscribeRequest {
            events_tx,
            replay_tx,
        };
        if !self.subscribe(thread_id, request).await {
            return None;
        }
        // The subscribe ack is sent after `on_subscribe` runs, so the replay
        // is already available.
        let replay = replay_rx.await.ok()?;
        Some(ThreadHandle {
            thread_id: thread_id.to_owned(),
            sender: self.sender(),
            events,
            replay,
        })
    }
}

impl<C, S, H> LocalAgentSystem<C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Run the system with the built-in [`HandleObserver`], enabling
    /// [`RunningSystem::thread_handle`]. Use [`start`](Self::start) with a
    /// custom [`ThreadObserver`] instead when you need durability hooks or
    /// your own fan-out.
    pub fn start_with_handles(self) -> RunningSystem<HandleSubscribeRequest> {
        let registry = HandleRegistry::default();
        self.start(move |_thread_id| HandleObserver {
            registry: registry.clone(),
        })
    }
}
