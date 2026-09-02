//! The daemon's [`ThreadObserver`]: fans agent events out to attached
//! clients as [`DaemonMessage`]s and persists derived presentation state.

use infinity_agent_core::ThreadId;
use std::collections::HashMap;
use std::sync::Arc;

use infinity_agent_core::system::{AgentEvent, ReplaySnapshot, ThreadObserver};
use infinity_protocol::DaemonMessage;
use tokio::sync::mpsc;

use super::display::{agent_event_to_daemon, history_message_to_daemon};
use crate::memory_store::PersistentConversationStore;

/// A subscriber attached to a thread's display events.
#[derive(Clone)]
pub struct Subscriber {
    pub tx: mpsc::UnboundedSender<DaemonMessage>,
    /// When `false`, this subscriber does not prevent the session from
    /// idling out (e.g. the Slack bot's persistent connections).
    pub keeps_session_alive: bool,
}

/// A request to subscribe to a thread's display events.
pub struct SubscribeRequest {
    pub tx: mpsc::UnboundedSender<DaemonMessage>,
    pub wants_replay: bool,
    pub keeps_session_alive: bool,
}

/// Shared subscriber list for one thread.
pub type ThreadSubscribers = Arc<std::sync::Mutex<Vec<Subscriber>>>;

/// Maps thread_id → subscriber list (for inheriting to children and idle
/// detection).
pub type SubscriberMap = Arc<std::sync::Mutex<HashMap<ThreadId, ThreadSubscribers>>>;

/// Deliver `msg` to every subscriber in the list, pruning subscribers whose
/// client has disconnected (their channel is closed). This is the single
/// fan-out primitive; every broadcast in the daemon goes through it so
/// delivery and pruning semantics cannot drift between call sites.
///
/// Returns `true` when a delivery went out on the same channel as
/// `requester` (`None` always returns `false`), so a caller responding to a
/// client request can avoid double-sending to a requester that is also
/// subscribed.
pub fn broadcast_pruning(
    subs: &ThreadSubscribers,
    msg: &DaemonMessage,
    requester: Option<&mpsc::UnboundedSender<DaemonMessage>>,
) -> bool {
    let mut requester_reached = false;
    let mut subs = subs.lock().expect("bug: mutex poisoned");
    subs.retain(|sub| {
        let delivered = sub.tx.send(msg.clone()).is_ok();
        if delivered && requester.is_some_and(|r| r.same_channel(&sub.tx)) {
            requester_reached = true;
        }
        delivered
    });
    requester_reached
}

/// Look up `thread_id`'s subscriber list and [`broadcast_pruning`] to it.
/// The map guard is released before the list is locked, so this never holds
/// both locks at once.
pub fn broadcast_to_thread(
    map: &SubscriberMap,
    thread_id: &ThreadId<str>,
    msg: &DaemonMessage,
    requester: Option<&mpsc::UnboundedSender<DaemonMessage>>,
) -> bool {
    let subs = map
        .lock()
        .expect("bug: mutex poisoned")
        .get(thread_id)
        .cloned();
    match subs {
        Some(subs) => broadcast_pruning(&subs, msg, requester),
        None => false,
    }
}

/// Per-thread observer wired up by the session's observer factory.
///
/// Event fan-out and state updates happen synchronously in
/// [`on_event`](ThreadObserver::on_event) — the runtime invokes it at the
/// exact emission point and handles subscriber attachment on the same task,
/// so each event is either reflected in the replay snapshot a new subscriber
/// receives or broadcast to it afterwards, never both.
pub struct DaemonObserver {
    pub(crate) subscribers: ThreadSubscribers,
    pub(crate) conversation_store: PersistentConversationStore,
}

impl DaemonObserver {
    fn broadcast(&self, dm: DaemonMessage) {
        broadcast_pruning(&self.subscribers, &dm, None);
    }
}

#[async_trait::async_trait(?Send)]
impl ThreadObserver for DaemonObserver {
    type SubscribeRequest = SubscribeRequest;

    fn on_event(&self, thread_id: &ThreadId<str>, event: &AgentEvent) {
        // Persist state derived from the event before broadcasting it, so a
        // client can never observe a message whose session state is missing.
        match event {
            AgentEvent::CompletionFinished { usage } => {
                // Only persist usage the provider actually reported; a
                // response without usage metadata must not reset the stored
                // total to zero.
                if let Some(usage) = usage {
                    self.conversation_store
                        .set_total_tokens_used(thread_id, usage.total_tokens as usize);
                }
                self.conversation_store
                    .set_last_updated(thread_id, &chrono::Utc::now().to_rfc3339());
            }
            // Reset the persisted context usage once compaction is applied:
            // the stored pre-compaction total is stale and would otherwise be
            // shown (and replayed on reconnect) until the next response
            // reports fresh usage.
            AgentEvent::CompactionApplied => {
                self.conversation_store.set_total_tokens_used(thread_id, 0);
            }
            _ => {}
        }

        self.broadcast(agent_event_to_daemon(thread_id, event));
    }

    fn on_subscribe(
        &self,
        thread_id: &ThreadId<str>,
        request: SubscribeRequest,
        snapshot: ReplaySnapshot,
    ) {
        if request.wants_replay {
            let mut history: Vec<DaemonMessage> = snapshot
                .history
                .iter()
                .filter_map(|m| history_message_to_daemon(m, thread_id, &snapshot.history))
                .collect();
            // Include the in-progress thinking (streamed reasoning is only
            // committed to history once it completes) so a client attaching
            // mid-thinking recomputes a live "thinking" state from the end of
            // the replay instead of appearing idle.
            if let Some(thinking) = snapshot.current_thinking {
                history.push(DaemonMessage::ThinkingStart {
                    thread_id: Some(thread_id.to_string()),
                });
                history.push(DaemonMessage::ThinkingChunk {
                    thread_id: Some(thread_id.to_string()),
                    chunk: thinking,
                });
            }
            let choices = snapshot
                .pending_choices
                .iter()
                .map(|choice| DaemonMessage::UserChoiceRequired {
                    thread_id: Some(thread_id.to_string()),
                    id: choice.id.clone(),
                    prompt: choice.prompt.clone(),
                    choices: choice.choices.clone(),
                    default: choice.default,
                })
                .collect::<Vec<_>>();
            let views = self.conversation_store.get_views(thread_id);
            if !history.is_empty() || !choices.is_empty() || !views.is_empty() {
                let _ = request.tx.send(DaemonMessage::Replay {
                    history,
                    pending_choices: choices,
                    views,
                    // Only an actual in-flight completion counts: while
                    // waiting on a tool result the clients derive their
                    // "waiting for tool call" spinner from the trailing
                    // ToolCall in the history instead.
                    in_progress: snapshot.in_progress,
                });
            }
        }
        let mut subs = self.subscribers.lock().expect("bug: mutex poisoned");
        // A client can legitimately re-send Connect on the same connection
        // (e.g. its connect-retry timer fires while a slow resume is still in
        // flight). Replace any existing subscription on the same channel
        // instead of stacking a duplicate, which would deliver every display
        // event to that client N times from then on.
        subs.retain(|sub| !sub.tx.same_channel(&request.tx));
        subs.push(Subscriber {
            tx: request.tx,
            keeps_session_alive: request.keeps_session_alive,
        });
    }
}
