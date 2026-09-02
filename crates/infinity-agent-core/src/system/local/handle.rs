//! Thread handles: the channel-based interface returned by [`ThreadBuilder`].
//!
//! A [`ThreadHandle`] sends inputs to one launched thread and receives its
//! [`AgentEvent`]s after an initial [`ReplaySnapshot`]. The observer and
//! subscription machinery in this module are internal to [`LaunchingSystem`].
//!
//! [`ThreadBuilder`]: super::ThreadBuilder
//! [`LaunchingSystem`]: super::LaunchingSystem

use rap_protocol::ThreadId;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::message::InputMessage;
use crate::traits::InputSender;

use super::router::RunningSystem;
use super::sender::{ChannelSendError, ChannelSender};
use crate::system::events::{AgentEvent, ReplaySnapshot};
use crate::system::observer::ThreadObserver;

/// The subscribe request used by [`HandleObserver`]: where to send the
/// thread's live events, and a one-shot for the initial replay.
pub(crate) struct HandleSubscribeRequest {
    events_tx: mpsc::UnboundedSender<AgentEvent>,
    replay_tx: oneshot::Sender<ReplaySnapshot>,
}

/// The built-in observer behind [`ThreadHandle`]s. The subscriber registry is
/// shared across driver instances so handles survive driver idle and respawn.
#[derive(Clone)]
pub(crate) struct HandleObserver {
    subscribers: Rc<RefCell<HashMap<ThreadId, Vec<mpsc::UnboundedSender<AgentEvent>>>>>,
}

#[async_trait(?Send)]
impl ThreadObserver for HandleObserver {
    type SubscribeRequest = HandleSubscribeRequest;

    fn on_event(&self, thread_id: &ThreadId<str>, event: &AgentEvent) {
        let mut subs = self.subscribers.borrow_mut();
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
        thread_id: &ThreadId<str>,
        request: HandleSubscribeRequest,
        snapshot: ReplaySnapshot,
    ) {
        // A dropped receiver just means the handle no longer wants the replay.
        let _ = request.replay_tx.send(snapshot);
        self.subscribers
            .borrow_mut()
            .entry(thread_id.to_owned())
            .or_default()
            .push(request.events_tx);
    }
}

/// A live connection to one thread of a running local system: send it
/// inputs and receive its events. Created by [`super::ThreadBuilder::launch`]
/// or by reattaching through [`super::LaunchingSystem::thread_handle`].
///
/// Dropping the handle detaches it; the thread itself is unaffected.
pub struct ThreadHandle {
    thread_id: ThreadId,
    sender: ChannelSender,
    /// The thread's events, in emission order, starting from the moment the
    /// handle attached. Every event is either reflected in
    /// [`replay`](Self::replay) or delivered here, exactly once.
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    replay: ReplaySnapshot,
}

impl ThreadHandle {
    /// The thread this handle is attached to.
    pub fn thread_id(&self) -> &ThreadId<str> {
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

/// Subscribe to `thread_id` and package the subscription into a
/// [`ThreadHandle`]. Callers must first establish that the thread was launched
/// or already exists in the conversation store. Infallible: shutting a system
/// down requires owning it, so a borrowed system's router is alive.
pub(crate) async fn attach(
    running: &RunningSystem<HandleSubscribeRequest>,
    thread_id: &ThreadId<str>,
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
pub(crate) fn handle_observer_factory() -> impl Fn(&ThreadId<str>) -> HandleObserver + 'static {
    let subscribers: Rc<RefCell<HashMap<ThreadId, Vec<mpsc::UnboundedSender<AgentEvent>>>>> =
        Default::default();
    move |_thread_id| HandleObserver {
        subscribers: subscribers.clone(),
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
                let (mut system, mut ctrl) = start_launcher_system(vec![], "");

                let mut handle = system.thread_builder().launch().await;
                let thread_id = handle.thread_id().to_owned();
                assert!(handle.replay().history.is_empty(), "fresh thread");
                assert!(!handle.replay().in_progress);

                handle.send_user_text("hello").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("hi there");
                ctrl.finish();
                assert_eq!(handle_texts_until_finished(&mut handle).await, ["hi there"]);

                // A handle attached after the exchange replays the history.
                wait_idle(&mut system.running).await;
                let late = system
                    .thread_handle(&thread_id)
                    .await
                    .expect("load existing thread")
                    .expect("launched thread exists");
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
                let (mut system, mut ctrl) = start_launcher_system(vec![], "");

                let mut handle = system.thread_builder().launch().await;
                assert!(handle.replay().history.is_empty());
                wait_idle(&mut system.running).await;
                assert!(system.is_idle(), "launching alone does not keep a driver");

                // The first message respawns the driver; the handle still
                // receives every event.
                handle.send_user_text("hello").await.expect("send input");
                let _req = ctrl.next_request().await;
                ctrl.send_text("first");
                ctrl.finish();
                assert_eq!(handle_texts_until_finished(&mut handle).await, ["first"]);

                // And again across another idle/respawn cycle.
                wait_idle(&mut system.running).await;
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
                let (system, mut ctrl) = start_launcher_system(vec![], "");

                let dropped = system.thread_builder().launch().await;
                let mut kept = system
                    .thread_handle(dropped.thread_id())
                    .await
                    .expect("load launched thread")
                    .expect("launched thread exists");
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
                let (mut system, mut ctrl) = start_launcher_system(vec![Box::new(AsyncTool)], "");
                let mut handle = system.thread_builder().launch().await;
                let thread_id = handle.thread_id().to_owned();

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
                    !system.is_idle(),
                    "the thread is waiting on the tool result"
                );

                // The stream did not stop: the tool result arrives later and
                // starts round 2.
                handle
                    .send(
                        tool_result_input(thread_id.as_str(), "tc-1", "tool done").0,
                        "res-1",
                    )
                    .await
                    .expect("send tool result");
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
                wait_idle(&mut system.running).await;
            })
            .await;
    }
}
