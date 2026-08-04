//! The daemon's [`ThreadObserver`]: fans agent events out to attached
//! clients as [`DaemonMessage`]s and persists derived session state (token
//! usage, pending choices) at the exact emission point.

use std::collections::HashMap;
use std::sync::Arc;

use infinity_agent_core::system::{AgentEvent, ReplaySnapshot, ThreadObserver, UserChoice};
use infinity_protocol::DaemonMessage;
use tokio::sync::mpsc;

use super::display::{agent_event_to_daemon, history_message_to_daemon};
use crate::memory_store::InMemoryConversationStore;
use crate::session_store;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
pub type SubscriberMap = Arc<std::sync::Mutex<HashMap<String, ThreadSubscribers>>>;

/// Per-thread observer wired up by the session's observer factory.
///
/// Event fan-out and state updates happen synchronously in
/// [`on_event`](ThreadObserver::on_event) — the runtime invokes it at the
/// exact emission point and handles subscriber attachment on the same task,
/// so each event is either reflected in the replay snapshot a new subscriber
/// receives or broadcast to it afterwards, never both.
pub struct DaemonObserver {
    pub(crate) root_session_id: String,
    pub(crate) subscribers: ThreadSubscribers,
    pub(crate) conversation_store: InMemoryConversationStore,
}

impl DaemonObserver {
    fn broadcast(&self, dm: DaemonMessage) {
        let mut subs = self.subscribers.lock().expect("bug: mutex poisoned");
        subs.retain(|sub| sub.tx.send(dm.clone()).is_ok());
    }
}

#[async_trait::async_trait(?Send)]
impl ThreadObserver for DaemonObserver {
    type SubscribeRequest = SubscribeRequest;

    fn on_event(&self, thread_id: &str, event: &AgentEvent) {
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

        if let Some(dm) = agent_event_to_daemon(thread_id, event) {
            self.broadcast(dm);
        }
    }

    /// Durable state transition: the pending choice is recorded in the
    /// conversation store (and thus replayed to late subscribers) before the
    /// runtime continues the step, and only then surfaced to live clients.
    async fn on_user_choice_required(
        &self,
        thread_id: &str,
        choice: &UserChoice,
    ) -> Result<(), BoxError> {
        let dm = DaemonMessage::UserChoiceRequired {
            thread_id: Some(thread_id.to_owned()),
            id: choice.id.clone(),
            prompt: choice.prompt.clone(),
            choices: choice.choices.clone(),
            default: choice.default,
        };
        self.conversation_store.add_pending_choice(
            &self.root_session_id,
            session_store::PendingChoice {
                id: choice.id.clone(),
                message: dm.clone(),
                response_url: choice.response_url.clone(),
            },
        );
        self.broadcast(dm);
        Ok(())
    }

    /// Durable state transition: the pending choice is removed before the
    /// runtime continues past the interruption, so the agent cannot proceed
    /// while a stale choice is still recorded.
    async fn on_user_choice_dismissed(
        &self,
        _thread_id: &str,
        choice_id: &str,
    ) -> Result<(), BoxError> {
        self.conversation_store
            .remove_pending_choice(&self.root_session_id, choice_id);
        self.broadcast(DaemonMessage::UserChoiceComplete {
            choice_id: choice_id.to_owned(),
        });
        Ok(())
    }

    fn on_subscribe(&self, thread_id: &str, request: SubscribeRequest, snapshot: ReplaySnapshot) {
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
                    thread_id: Some(thread_id.to_owned()),
                });
                history.push(DaemonMessage::ThinkingChunk {
                    thread_id: Some(thread_id.to_owned()),
                    chunk: thinking,
                });
            }
            let choices = self
                .conversation_store
                .get_pending_choice_messages(&self.root_session_id);
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
        self.subscribers
            .lock()
            .expect("bug: mutex poisoned")
            .push(Subscriber {
                tx: request.tx,
                keeps_session_alive: request.keeps_session_alive,
            });
    }
}
