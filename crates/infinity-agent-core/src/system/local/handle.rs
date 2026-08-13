//! Thread handles: a channel-based convenience layer over a local system.
//!
//! [`LocalAgentSystem::start`] runs the system with a built-in
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
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;

use super::router::RunningSystem;
use super::sender::{ChannelSendError, ChannelSender};
use crate::system::builder::LocalAgentSystem;
use crate::system::events::{AgentEvent, ReplaySnapshot};
use crate::system::observer::ThreadObserver;

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
/// [`LocalAgentSystem::start`]; the underlying subscriber
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
        let msg = InputMessage::user_text(self.thread_id.clone(), text);
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
        self.sender.send_to_input_queue(message, dedup_id).await
    }
}

impl RunningSystem<HandleSubscribeRequest> {
    /// Attach to `thread_id` and return a [`ThreadHandle`] for it. The thread
    /// does not need to exist yet: attaching to a fresh ID installs the
    /// subscription (with an empty replay) so events flow from the very first
    /// message later sent to it.
    ///
    /// Multiple handles can attach to the same thread; each receives every
    /// event.
    pub async fn thread_handle(&self, thread_id: &str) -> ThreadHandle {
        attach(self, thread_id).await
    }
}

/// Subscribe to `thread_id` on `running` and package the subscription into a
/// [`ThreadHandle`]. Shared by [`RunningSystem::thread_handle`] and the
/// [launcher](super::LaunchingSystem). Infallible: shutting a system down
/// requires owning it, so a borrowed system's router is alive.
pub(crate) async fn attach(
    running: &RunningSystem<HandleSubscribeRequest>,
    thread_id: &str,
) -> ThreadHandle {
    let (events_tx, events) = mpsc::unbounded_channel();
    let (replay_tx, replay_rx) = oneshot::channel();
    let request = HandleSubscribeRequest {
        events_tx,
        replay_tx,
    };
    running.subscribe(thread_id, request).await;
    // The subscribe ack is sent after `on_subscribe` runs, so the replay
    // is already available.
    let replay = replay_rx
        .await
        .expect("bug: subscribe acked without a replay");
    ThreadHandle {
        thread_id: thread_id.to_owned(),
        sender: running.sender(),
        events,
        replay,
    }
}

/// A per-system factory for [`HandleObserver`]s sharing one subscriber
/// registry (so subscriptions survive driver respawns).
pub(crate) fn handle_observer_factory() -> impl Fn(&str) -> HandleObserver + 'static {
    let registry = HandleRegistry::default();
    move |_thread_id| HandleObserver {
        registry: registry.clone(),
    }
}

impl<C, S, H> LocalAgentSystem<C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Run the system with the built-in [`HandleObserver`]: attach to
    /// threads with [`RunningSystem::thread_handle`]. Use
    /// [`start_with_observer`](Self::start_with_observer) instead when you
    /// need durability hooks or your own event fan-out, or switch to
    /// [`with_thread_launcher`](Self::with_thread_launcher) to launch
    /// threads with their own tools.
    pub fn start(self) -> RunningSystem<HandleSubscribeRequest> {
        self.start_inner(handle_observer_factory())
    }
}
#[cfg(test)]
mod tests {
    use crate::system::events::AgentEvent;
    use crate::system::test_support::*;

    #[tokio::test(flavor = "current_thread")]
    async fn thread_handle_sends_inputs_and_receives_events() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut running, mut ctrl) = start_handle_system(vec![]);

                let mut handle = running.thread_handle("t1").await;
                assert_eq!(handle.thread_id(), "t1");
                assert!(handle.replay().history.is_empty(), "fresh thread");
                assert!(!handle.replay().in_progress);

                handle.send_user_text("hello").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("hi there");
                ctrl.finish();
                assert_eq!(handle_texts_until_finished(&mut handle).await, ["hi there"]);

                // A handle attached after the exchange replays the history.
                wait_idle(&mut running).await;
                let late = running.thread_handle("t1").await;
                assert!(
                    !late.replay().history.is_empty(),
                    "late handle sees committed history"
                );
                assert!(!late.replay().in_progress);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thread_handle_survives_driver_respawn() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut running, mut ctrl) = start_handle_system(vec![]);

                // Attaching to a thread that does not exist yet sets up the
                // subscription; the driver spawned for the attach idles right
                // back out.
                let mut handle = running.thread_handle("fresh").await;
                assert!(handle.replay().history.is_empty());
                wait_idle(&mut running).await;
                assert!(running.is_idle(), "attach alone does not keep a driver");

                // The first message respawns the driver; the handle still
                // receives every event.
                handle.send_user_text("hello").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("first");
                ctrl.finish();
                assert_eq!(handle_texts_until_finished(&mut handle).await, ["first"]);

                // And again across another idle/respawn cycle.
                wait_idle(&mut running).await;
                handle.send_user_text("more").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("second");
                ctrl.finish();
                assert_eq!(handle_texts_until_finished(&mut handle).await, ["second"]);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_thread_handle_is_pruned_and_others_keep_receiving() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (running, mut ctrl) = start_handle_system(vec![]);

                let dropped = running.thread_handle("t1").await;
                let mut kept = running.thread_handle("t1").await;
                drop(dropped);

                kept.send_user_text("hello").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("still here");
                ctrl.finish();
                assert_eq!(handle_texts_until_finished(&mut kept).await, ["still here"]);
            })
            .await;
    }

    /// The consumption contract of a [`ThreadHandle`] event stream:
    /// `CompletionFinished` ends one *round*, not the conversation. A round that
    /// finished with a tool call means more events are coming (the tool result
    /// and the follow-up round), so a consumer keeps pulling until a round
    /// finishes without a pending tool call.
    #[tokio::test(flavor = "current_thread")]
    async fn thread_handle_streams_across_tool_call_rounds() {
        /// The significant events of this scenario, in stream order.
        #[derive(Debug, PartialEq)]
        enum Tag {
            UserInput(String),
            ToolCall(String),
            ToolResult(String),
            TextChunk(String),
            CompletionFinished,
        }
        fn tag(event: AgentEvent) -> Option<Tag> {
            match event {
                AgentEvent::UserInput { text } => Some(Tag::UserInput(text)),
                AgentEvent::ToolCall { name, .. } => Some(Tag::ToolCall(name)),
                AgentEvent::ToolResult { segments } => segments.iter().find_map(|s| {
                    if let rap_protocol::DisplaySegment::Text(t) = s {
                        Some(Tag::ToolResult(t.clone()))
                    } else {
                        None
                    }
                }),
                AgentEvent::TextChunk { text } => Some(Tag::TextChunk(text)),
                AgentEvent::CompletionFinished { .. } => Some(Tag::CompletionFinished),
                _ => None,
            }
        }
        /// Pull tagged events until (and including) the next `CompletionFinished`.
        async fn collect_round(handle: &mut super::ThreadHandle) -> Vec<Tag> {
            let mut tags = Vec::new();
            loop {
                let event = tokio::time::timeout(std::time::Duration::from_secs(5), handle.recv())
                    .await
                    .expect("timed out waiting for a handle event")
                    .expect("handle event channel closed");
                if let Some(t) = tag(event) {
                    let done = t == Tag::CompletionFinished;
                    tags.push(t);
                    if done {
                        return tags;
                    }
                }
            }
        }

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut running, mut ctrl) = start_handle_system(vec![Box::new(AsyncTool)]);
                let mut handle = running.thread_handle("t1").await;

                // Round 1: the model answers the user with a tool call.
                handle.send_user_text("use tool").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_tool_call("tc-1", "async_tool", serde_json::json!({}));
                ctrl.finish();

                let round1 = collect_round(&mut handle).await;
                assert_eq!(
                    round1,
                    vec![
                        Tag::UserInput("use tool".into()),
                        Tag::ToolCall("async_tool".into()),
                        Tag::CompletionFinished,
                    ],
                    "round 1 ends at a CompletionFinished whose round issued a tool call"
                );
                assert!(
                    !running.is_idle(),
                    "the thread is waiting on the tool result"
                );

                // The stream did not stop: the tool result arrives later and
                // starts round 2.
                running
                    .send(tool_result_input("t1", "tc-1", "tool done").0, "res-1")
                    .await;
                let _req = ctrl.next_request().await;
                ctrl.send_text("all done");
                ctrl.finish();

                let round2 = collect_round(&mut handle).await;
                assert_eq!(
                    round2,
                    vec![
                        Tag::ToolResult("tool done".into()),
                        Tag::TextChunk("all done".into()),
                        Tag::CompletionFinished,
                    ],
                    "round 2 delivers the tool result echo and the follow-up text"
                );

                // A round that finished without a pending tool call: the thread
                // idles out and the consumer is done.
                wait_idle(&mut running).await;
            })
            .await;
    }
}
