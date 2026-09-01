//! The router: dispatches messages to per-thread drivers, spawning them on
//! demand. This plus the [drivers](super::driver) is the "actor system" of a
//! local agent system. The router owns the driver futures directly (one
//! `FuturesUnordered` pool rather than one spawned task per thread), so a
//! driver's memory is fully released the moment it goes idle.

use rap_protocol::ThreadId;
use std::collections::HashMap;
use std::rc::Rc;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::message::InputMessage;
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;

use super::driver::{ActiveThreads, ThreadLifecycleEvent, drive_thread};
use super::sender::ChannelSender;
use crate::system::builder::{LocalAgentSystem, SystemInner};
use crate::system::observer::ThreadObserver;
use crate::system::thread::is_user_text_input;

/// A subscribe request routed through the local system: the target thread,
/// the observer-specific request, and an ack fired once the subscriber has
/// been installed (its replay sent and its registration completed).
pub(crate) type SubscribeMessage<Sub> = (ThreadId, Sub, oneshot::Sender<()>);

/// A clonable handle for attaching subscribers to a running system's threads.
pub struct SubscribeHandle<Sub: Send + 'static> {
    tx: mpsc::UnboundedSender<SubscribeMessage<Sub>>,
}

impl<Sub: Send + 'static> Clone for SubscribeHandle<Sub> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<Sub: Send + 'static> SubscribeHandle<Sub> {
    /// Attach a subscriber to a thread. The request is handed to that
    /// thread's [`ThreadObserver::on_subscribe`] together with a live replay
    /// snapshot (spawning a driver for the thread if none is running).
    ///
    /// Resolves once the subscriber is **installed** — its replay has been
    /// sent and it is registered for live events — so a caller that
    /// subscribes and then sends a message is guaranteed to observe that
    /// message's events. A driver that exits while requests race in hands
    /// them back to the router, so installation is reliable; `false` is
    /// returned only if the whole system was shut down.
    pub async fn subscribe(&self, thread_id: &ThreadId, request: Sub) -> bool {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send((thread_id.clone(), request, ack_tx)).is_err() {
            return false;
        }
        // An error means the ack sender was dropped mid-shutdown — the
        // subscriber was not installed.
        ack_rx.await.is_ok()
    }
}

/// A running local agent system: the router task plus handles for feeding it
/// input, attaching subscribers, and observing thread lifecycle.
///
/// The system runs until [`shutdown`](Self::shutdown) consumes it: individual
/// threads idle out (and respawn on demand) while the router keeps running, so
/// sending to a thread never races a teardown.
///
/// `Sub` is the observer's
/// [`SubscribeRequest`](ThreadObserver::SubscribeRequest) type.
pub struct RunningSystem<Sub: Send + 'static> {
    sender: ChannelSender,
    subscribe_tx: mpsc::UnboundedSender<SubscribeMessage<Sub>>,
    active_threads: ActiveThreads,
    lifecycle_rx: mpsc::UnboundedReceiver<ThreadLifecycleEvent>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl<Sub: Send + 'static> RunningSystem<Sub> {
    /// The system's [`InputSender`](crate::traits::InputSender), for callback
    /// servers and any other external message injectors.
    pub fn sender(&self) -> ChannelSender {
        self.sender.clone()
    }

    /// Deliver an input message to its thread (`message.group_id`).
    /// `dedup_id` should be stable across redeliveries of the same message.
    pub async fn send(&self, message: InputMessage, dedup_id: &str) {
        self.sender
            .send_to_input_queue(message, dedup_id)
            .await
            .expect("bug: router exited while the system was alive");
    }

    /// Convenience: send plain user text to a thread.
    pub async fn send_user_text(&self, thread_id: &ThreadId, text: impl Into<String>) {
        let msg = InputMessage::user_text(thread_id.clone(), text);
        self.send(msg, &uuid::Uuid::new_v4().to_string()).await
    }

    /// Attach a subscriber to a thread; resolves once the subscriber is
    /// installed. See [`SubscribeHandle::subscribe`].
    pub async fn subscribe(&self, thread_id: &ThreadId, request: Sub) {
        assert!(
            self.subscribe_handle().subscribe(thread_id, request).await,
            "bug: router exited while the system was alive"
        );
    }

    /// A clonable handle for attaching subscribers (see
    /// [`SubscribeHandle::subscribe`]).
    pub fn subscribe_handle(&self) -> SubscribeHandle<Sub> {
        SubscribeHandle {
            tx: self.subscribe_tx.clone(),
        }
    }

    /// Thread IDs with a live driver.
    #[cfg(test)]
    pub(crate) fn active_threads(&self) -> ActiveThreads {
        self.active_threads.clone()
    }

    /// Whether no thread driver is currently live. Threads with active
    /// subscriptions but no pending work do not count as live; their events
    /// respawn a driver when they arrive.
    pub fn is_idle(&self) -> bool {
        self.active_threads
            .lock()
            .expect("bug: mutex poisoned")
            .is_empty()
    }

    /// Wait for the next thread-driver lifecycle transition. Returns `None`
    /// after the router shuts down and all lifecycle senders are dropped.
    pub async fn next_lifecycle_event(&mut self) -> Option<ThreadLifecycleEvent> {
        self.lifecycle_rx.recv().await
    }

    /// Return the next queued lifecycle transition without waiting.
    pub fn try_next_lifecycle_event(
        &mut self,
    ) -> Result<ThreadLifecycleEvent, mpsc::error::TryRecvError> {
        self.lifecycle_rx.try_recv()
    }

    /// Wind the whole system down (process exit): every driver interrupts
    /// its in-flight completion (flushing pending history to the store) and
    /// exits; resolves when the wind-down is complete.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        if let Err(e) = self.task.await {
            tracing::error!("router task failed during shutdown: {e}");
        }
    }
}

impl<C, S, H> LocalAgentSystem<C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Run the system with a custom [`ThreadObserver`]. Use this lower-level
    /// path when the embedding owns event fan-out and thread identity.
    ///
    /// `make_observer` creates the observer for each thread driver as it
    /// spawns. Drivers are spawned on demand when a thread receives its
    /// first message and respawned when an idle thread receives another; the
    /// same thread never has two drivers.
    pub fn start_with_observer<O, F>(self, make_observer: F) -> RunningSystem<O::SubscribeRequest>
    where
        O: ThreadObserver + 'static,
        F: Fn(&ThreadId) -> O + 'static,
    {
        self.start_inner(make_observer)
    }
}

impl<C, S, H> LocalAgentSystem<C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Spawn the router on the current
    /// [`LocalSet`](tokio::task::LocalSet).
    pub(crate) fn start_inner<O, F>(self, make_observer: F) -> RunningSystem<O::SubscribeRequest>
    where
        O: ThreadObserver + 'static,
        F: Fn(&ThreadId) -> O + 'static,
    {
        let sender = self.system.inner.sender.clone();
        let (subscribe_tx, subscribe_rx) = mpsc::unbounded_channel();
        let (lifecycle_tx, thread_lifecycle) = mpsc::unbounded_channel();
        let active_threads: ActiveThreads = Default::default();
        let shutdown = CancellationToken::new();

        let task = tokio::task::spawn_local(route_loop(
            self.system.inner,
            self.input_rx,
            subscribe_rx,
            make_observer,
            active_threads.clone(),
            lifecycle_tx,
            shutdown.clone(),
        ));

        RunningSystem {
            sender,
            subscribe_tx,
            active_threads,
            lifecycle_rx: thread_lifecycle,
            shutdown,
            task,
        }
    }
}

struct WorkerChannels<Sub> {
    input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
    subscribe_tx: mpsc::UnboundedSender<(Sub, oneshot::Sender<()>)>,
}

enum RoutedMessage<Sub> {
    Input(Box<InputMessage>, String),
    Subscribe(ThreadId, Sub, oneshot::Sender<()>),
}

async fn route_loop<C, S, H, O, F>(
    inner: Rc<SystemInner<C, S, ChannelSender, H>>,
    mut input_rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    mut subscribe_rx: mpsc::UnboundedReceiver<SubscribeMessage<O::SubscribeRequest>>,
    make_observer: F,
    active_threads: ActiveThreads,
    lifecycle_tx: mpsc::UnboundedSender<ThreadLifecycleEvent>,
    shutdown: CancellationToken,
) where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
    O: ThreadObserver + 'static,
    F: Fn(&ThreadId) -> O + 'static,
{
    let mut workers: HashMap<ThreadId, WorkerChannels<O::SubscribeRequest>> = HashMap::new();
    let mut subscribe_closed = false;
    // The router owns the driver futures directly (instead of spawning each
    // as its own task): a completed driver yields its thread ID and its
    // memory — the future itself and its worker entry — is released
    // immediately. With per-thread tasks, both would be retained until the
    // thread's next message, which adds up across many idle threads.
    let mut drivers = FuturesUnordered::new();

    loop {
        let msg: Option<RoutedMessage<O::SubscribeRequest>> = tokio::select! {
            biased;
            _ = shutdown.cancelled() => None,
            exited = drivers.next(), if !drivers.is_empty() => {
                let exited: ThreadId = exited.expect("bug: drivers is empty");
                // Only remove the exited driver's own entry: if the thread
                // already respawned, the new entry's channel is still open.
                if workers.get(&exited).is_some_and(|w| w.input_tx.is_closed()) {
                    workers.remove(&exited);
                }
                continue;
            }
            msg = input_rx.recv() => msg.map(|(m, id)| RoutedMessage::Input(Box::new(m), id)),
            req = subscribe_rx.recv(), if !subscribe_closed => {
                match req {
                    Some((thread_id, req, ack)) => Some(RoutedMessage::Subscribe(thread_id, req, ack)),
                    None => {
                        subscribe_closed = true;
                        continue;
                    }
                }
            }
        };
        let Some(msg) = msg else { break };

        let thread_id = match &msg {
            RoutedMessage::Input(input, _) => input.group_id.clone(),
            RoutedMessage::Subscribe(thread_id, _, _) => thread_id.clone(),
        };

        // Reuse a live driver if one exists.
        if let Some(w) = workers.get(&thread_id) {
            if !w.input_tx.is_closed() {
                match msg {
                    RoutedMessage::Input(input, id) => {
                        let _ = w.input_tx.send((*input, id));
                    }
                    RoutedMessage::Subscribe(_, req, ack) => {
                        let _ = w.subscribe_tx.send((req, ack));
                    }
                }
                continue;
            }
            workers.remove(&thread_id);
        }

        // Admission: event-style input cannot create or resume a thread. User
        // text bypasses both checks because it is how threads are created and
        // stopped threads are resumed. Subscribe requests are also ungated so
        // a client can attach before a thread's first activity.
        if let RoutedMessage::Input(input, dedup_id) = &msg
            && !is_user_text_input(input)
        {
            match inner.conversation_store.thread_exists(&thread_id).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(
                        %thread_id,
                        %dedup_id,
                        "dropping event for unknown thread",
                    );
                    continue;
                }
                Err(e) => {
                    // Fail open: a flaky store must not drop a real tool
                    // result; the driver's own preparation still absorbs
                    // anything stale.
                    tracing::warn!(
                        %thread_id,
                        "thread existence check failed, processing event anyway: {e}",
                    );
                }
            }

            match inner.state_store.is_thread_stopped(&thread_id).await {
                Ok(false) => {}
                Ok(true) => {
                    tracing::info!(
                        %thread_id,
                        %dedup_id,
                        "dropping event for stopped thread",
                    );
                    continue;
                }
                Err(e) => {
                    // Fail open for the same reason as the existence check.
                    tracing::warn!(
                        %thread_id,
                        "stopped-thread check failed, processing event anyway: {e}",
                    );
                }
            }
        }

        // Spawn a new driver.
        let (input_tx, input_rx_worker) = mpsc::unbounded_channel();
        let (worker_subscribe_tx, worker_subscribe_rx) = mpsc::unbounded_channel();
        let observer = make_observer(&thread_id);

        drivers.push({
            let thread_id = thread_id.clone();
            let driver = drive_thread(
                inner.clone(),
                thread_id.clone(),
                input_rx_worker,
                worker_subscribe_rx,
                observer,
                active_threads.clone(),
                lifecycle_tx.clone(),
            );
            async move {
                // Contain a panicking driver to its own thread (the panic is
                // logged); the router and the other drivers keep running.
                rap_protocol::log_panic("thread_driver", driver).await;
                thread_id
            }
        });

        match msg {
            RoutedMessage::Input(input, id) => {
                let _ = input_tx.send((*input, id));
            }
            RoutedMessage::Subscribe(_, req, ack) => {
                let _ = worker_subscribe_tx.send((req, ack));
            }
        }
        workers.insert(
            thread_id,
            WorkerChannels {
                input_tx,
                subscribe_tx: worker_subscribe_tx,
            },
        );
    }

    // Wind down: dropping each driver's channels signals it to interrupt any
    // in-flight completion (which flushes pending history items to the store)
    // and exit. Drive every remaining driver to completion so the embedding
    // is not torn down underneath them.
    drop(workers);
    while drivers.next().await.is_some() {}
}
#[cfg(test)]
mod tests {
    use super::ThreadLifecycleEvent;
    use crate::system::events::AgentEvent;
    use crate::system::local::ThreadLifecycleState;
    use crate::system::test_support::*;

    /// A stale event addressed to a thread that was never created must not wake
    /// a phantom driver: no store records appear and no thread exit is reported.
    #[tokio::test(flavor = "current_thread")]
    async fn stale_event_to_unknown_thread_wakes_nothing() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut running, mut rx, mut ctrl, conv) = start_system(vec![], None);

                // Create a real thread so the system is not trivially empty.
                running.send_user_text(&"t1".into(), "hello").await;
                let _req = ctrl.next_request().await;
                ctrl.send_text("hi");
                ctrl.finish();
                collect_until_finished(&mut rx).await;
                wait_idle(&mut running).await;

                // A stale subscription event and a stale tool result for a
                // thread that does not exist.
                running
                    .send(
                        subscription_event_input("ghost", "tc-ghost", "stale event").0,
                        "ghost-sub",
                    )
                    .await;
                running
                    .send(
                        tool_result_input("ghost", "tc-ghost", "stale result").0,
                        "ghost-res",
                    )
                    .await;
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }

                assert!(
                    running.is_idle(),
                    "no driver may wake for an unknown thread"
                );
                // Lifecycle notifications for t1 may still be queued; what must
                // never appear is any transition for the ghost thread.
                while let Ok(event) = running.try_next_lifecycle_event() {
                    assert_ne!(
                        event.thread_id.as_str(),
                        "ghost",
                        "no lifecycle transition may be reported for a dropped event"
                    );
                }
                assert!(
                    conv.thread_info(&"ghost".into()).is_none(),
                    "a dropped event must not create thread records"
                );
            })
            .await;
    }

    /// User text to a brand-new thread ID must still create the thread: the wake
    /// policy gates only events.
    #[tokio::test(flavor = "current_thread")]
    async fn user_text_still_creates_new_threads() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (running, mut rx, mut ctrl, conv) = start_system(vec![], None);
                running.send_user_text(&"brand-new".into(), "hello").await;
                let _req = ctrl.next_request().await;
                ctrl.send_text("created");
                ctrl.finish();
                collect_until_finished(&mut rx).await;
                assert!(conv.thread_info(&"brand-new".into()).is_some());
            })
            .await;
    }

    /// An event for a real, idled-out thread must still respawn its driver: the
    /// existence check refuses unknown threads, not idle ones. The stale result is
    /// absorbed during preparation, but the driver wakes to do it, observable as
    /// a `Live` transition followed by another `Idle`.
    #[tokio::test(flavor = "current_thread")]
    async fn event_respawns_idle_existing_thread() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut running, mut rx, mut ctrl, _conv) = start_system(vec![], None);

                running.send_user_text(&"t1".into(), "hello").await;
                let _req = ctrl.next_request().await;
                ctrl.send_text("hi");
                ctrl.finish();
                collect_until_finished(&mut rx).await;
                wait_idle(&mut running).await;
                // Drain transitions from the first driver so the assertions below
                // observe only the respawned one.
                while running.try_next_lifecycle_event().is_ok() {}

                running
                    .send(
                        tool_result_input("t1", "tc-old", "late result").0,
                        "res-late",
                    )
                    .await;
                let live = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    running.next_lifecycle_event(),
                )
                .await
                .expect("timed out waiting for the driver to respawn")
                .expect("thread lifecycle channel closed");
                assert_eq!(
                    live,
                    ThreadLifecycleEvent {
                        thread_id: "t1".into(),
                        state: ThreadLifecycleState::Live,
                    },
                    "the existing thread's driver must wake"
                );
                let idle = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    running.next_lifecycle_event(),
                )
                .await
                .expect("timed out waiting for the respawned driver to exit")
                .expect("thread lifecycle channel closed");
                assert_eq!(
                    idle,
                    ThreadLifecycleEvent {
                        thread_id: "t1".into(),
                        state: ThreadLifecycleState::Idle,
                    },
                    "the respawned driver must idle back out"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_flushes_in_flight_turn() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (running, mut rx, mut ctrl, conv) = start_system(vec![], None);
                running.send_user_text(&"t1".into(), "hello").await;
                let _req = ctrl.next_request().await;
                ctrl.send_text("partial answer");
                loop {
                    if let Evt::E(AgentEvent::TextChunk { .. }) = next_evt(&mut rx).await {
                        break;
                    }
                }

                // Shut down mid-completion: the driver cancels, flushes, syncs.
                running.shutdown().await;

                use crate::traits::ConversationStore;
                let history = conv
                    .load_history_up_to(&"t1".into(), None, None)
                    .await
                    .expect("load history");
                assert!(
                    history
                        .iter()
                        .any(|m| matches!(m, crate::message::InfinityMessage::Assistant { .. })),
                    "partial assistant text must be persisted on shutdown, got {history:?}"
                );
            })
            .await;
    }
}
