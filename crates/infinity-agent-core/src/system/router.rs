//! The router: dispatches messages to per-thread drivers, spawning them on
//! demand. This plus the [drivers](super::driver) is the "actor system" of a
//! local agent system.

use std::collections::HashMap;
use std::rc::Rc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::message::{InputMessage, InputMessageContent};
use crate::traits::{ConversationStore, InputSender, StateStore};
use rap_client::http::HttpClient;
use rig::message::UserContent;

use super::builder::{LocalAgentSystem, SystemInner};
use super::driver::{ActiveThreads, drive_thread};
use super::observer::ThreadObserver;
use super::sender::ChannelSender;

/// A subscribe request routed through the local system: the target thread,
/// the observer-specific request, and an ack fired once the subscriber has
/// been installed (its replay sent and its registration completed).
pub type SubscribeMessage<Sub> = (String, Sub, oneshot::Sender<()>);

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
    pub async fn subscribe(&self, thread_id: &str, request: Sub) -> bool {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .tx
            .send((thread_id.to_owned(), request, ack_tx))
            .is_err()
        {
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
/// The system is expected to run for the lifetime of the process —
/// individual threads idle out (and respawn on demand) while the router keeps
/// running, so senders never race a teardown.
/// [`begin_shutdown`](Self::begin_shutdown) exists for whole-process exit.
///
/// `Sub` is the observer's
/// [`SubscribeRequest`](ThreadObserver::SubscribeRequest) type.
pub struct RunningSystem<Sub: Send + 'static> {
    sender: ChannelSender,
    subscribe_tx: mpsc::UnboundedSender<SubscribeMessage<Sub>>,
    active_threads: ActiveThreads,
    /// Receives the thread ID each time a thread's driver exits (the thread
    /// went idle: no pending tool call and no active subscription).
    /// Embeddings use this to release per-conversation resources — e.g. the
    /// Infinity Code daemon shuts down a session's RAP servers once none of
    /// the session's threads are live and no keep-alive client is attached.
    pub thread_exits: mpsc::UnboundedReceiver<String>,
    shutdown: CancellationToken,
    /// The router task. Resolves only after
    /// [`begin_shutdown`](Self::begin_shutdown), once all drivers have
    /// flushed their in-flight turns and exited.
    pub task: tokio::task::JoinHandle<()>,
}

impl<Sub: Send + 'static> RunningSystem<Sub> {
    /// The system's [`InputSender`](crate::traits::InputSender), for callback
    /// servers and any other external message injectors.
    pub fn sender(&self) -> ChannelSender {
        self.sender.clone()
    }

    /// Deliver an input message to its thread (`message.group_id`).
    /// `dedup_id` should be stable across redeliveries of the same message.
    pub async fn send(
        &self,
        message: InputMessage,
        dedup_id: &str,
    ) -> Result<(), super::sender::ChannelSendError> {
        let group_id = message.group_id.clone();
        self.sender
            .send_to_input_queue(message, &group_id, dedup_id)
            .await
    }

    /// Convenience: send plain user text to a thread.
    pub async fn send_user_text(
        &self,
        thread_id: &str,
        text: impl Into<String>,
    ) -> Result<(), super::sender::ChannelSendError> {
        let msg = InputMessage {
            content: InputMessageContent::User(UserContent::text(text.into())),
            group_id: thread_id.to_owned(),
            metadata: None,
            synthetic: None,
            display_as: None,
            subscription: false,
        };
        self.send(msg, &uuid::Uuid::new_v4().to_string()).await
    }

    /// Attach a subscriber to a thread. See [`SubscribeHandle::subscribe`].
    pub async fn subscribe(&self, thread_id: &str, request: Sub) -> bool {
        self.subscribe_handle().subscribe(thread_id, request).await
    }

    /// A clonable handle for attaching subscribers (see
    /// [`SubscribeHandle::subscribe`]).
    pub fn subscribe_handle(&self) -> SubscribeHandle<Sub> {
        SubscribeHandle {
            tx: self.subscribe_tx.clone(),
        }
    }

    /// Thread IDs with a live driver.
    pub fn active_threads(&self) -> ActiveThreads {
        self.active_threads.clone()
    }

    /// Whether no thread driver is currently live.
    pub fn is_idle(&self) -> bool {
        self.active_threads
            .lock()
            .expect("bug: mutex poisoned")
            .is_empty()
    }

    /// Begin winding the whole system down (process exit): every driver
    /// interrupts its in-flight completion (flushing pending history to the
    /// store) and exits. Await [`task`](Self::task) to know when the
    /// wind-down is complete.
    pub fn begin_shutdown(&self) {
        self.shutdown.cancel();
    }
}

impl<C, S, H> LocalAgentSystem<C, S, H>
where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
{
    /// Run the system: spawns the router on the current
    /// [`LocalSet`](tokio::task::LocalSet).
    ///
    /// `make_observer` creates the [`ThreadObserver`] for each thread driver
    /// as it spawns. Drivers are spawned on demand when a thread receives its
    /// first message and respawned when an idle thread receives another; the
    /// same thread never has two drivers.
    pub fn start<O, F>(self, make_observer: F) -> RunningSystem<O::SubscribeRequest>
    where
        O: ThreadObserver + 'static,
        F: Fn(&str) -> O + 'static,
    {
        let sender = self.system.inner.sender.clone();
        let (subscribe_tx, subscribe_rx) = mpsc::unbounded_channel();
        let (exit_tx, thread_exits) = mpsc::unbounded_channel();
        let active_threads: ActiveThreads = Default::default();
        let shutdown = CancellationToken::new();

        let task = tokio::task::spawn_local(route_loop(
            self.system.inner,
            self.input_rx,
            subscribe_rx,
            subscribe_tx.clone(),
            make_observer,
            active_threads.clone(),
            exit_tx,
            shutdown.clone(),
        ));

        RunningSystem {
            sender,
            subscribe_tx,
            active_threads,
            thread_exits,
            shutdown,
            task,
        }
    }
}

struct WorkerChannels<Sub> {
    input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
    subscribe_tx: mpsc::UnboundedSender<(Sub, oneshot::Sender<()>)>,
    handle: tokio::task::JoinHandle<()>,
}

enum RoutedMessage<Sub> {
    Input(Box<InputMessage>, String),
    Subscribe(String, Sub, oneshot::Sender<()>),
}

#[expect(
    clippy::too_many_arguments,
    reason = "router wiring requires many channels"
)]
async fn route_loop<C, S, H, O, F>(
    inner: Rc<SystemInner<C, S, ChannelSender, H>>,
    mut input_rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    mut subscribe_rx: mpsc::UnboundedReceiver<SubscribeMessage<O::SubscribeRequest>>,
    // A sender for the router's own subscribe queue, handed to drivers so an
    // exiting driver can requeue attach requests that raced in.
    router_subscribe_tx: mpsc::UnboundedSender<SubscribeMessage<O::SubscribeRequest>>,
    make_observer: F,
    active_threads: ActiveThreads,
    exit_tx: mpsc::UnboundedSender<String>,
    shutdown: CancellationToken,
) where
    C: ConversationStore + 'static,
    S: StateStore + 'static,
    H: HttpClient + 'static,
    O: ThreadObserver + 'static,
    F: Fn(&str) -> O + 'static,
{
    let mut workers: HashMap<String, WorkerChannels<O::SubscribeRequest>> = HashMap::new();
    let mut subscribe_closed = false;

    loop {
        let msg: Option<RoutedMessage<O::SubscribeRequest>> = tokio::select! {
            biased;
            _ = shutdown.cancelled() => None,
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

        // Spawn a new driver.
        let (input_tx, input_rx_worker) = mpsc::unbounded_channel();
        let (worker_subscribe_tx, worker_subscribe_rx) = mpsc::unbounded_channel();
        let observer = make_observer(&thread_id);

        let handle = tokio::task::spawn_local(rap_protocol::log_panic(
            "thread_driver",
            drive_thread(
                inner.clone(),
                thread_id.clone(),
                input_rx_worker,
                worker_subscribe_rx,
                router_subscribe_tx.clone(),
                observer,
                active_threads.clone(),
                exit_tx.clone(),
            ),
        ));

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
                handle,
            },
        );
    }

    // Wind down: dropping each driver's channels signals it to interrupt any
    // in-flight completion (which flushes pending history items to the store)
    // and exit. Wait for every driver to finish so the embedding is not torn
    // down underneath them.
    let handles: Vec<(String, tokio::task::JoinHandle<()>)> = workers
        .drain()
        .map(|(thread_id, w)| (thread_id, w.handle))
        .collect();
    for (thread_id, handle) in handles {
        if let Err(e) = handle.await {
            if e.is_panic() {
                tracing::error!("thread driver {thread_id} panicked during shutdown: {e}");
            } else {
                tracing::warn!("thread driver {thread_id} cancelled during shutdown: {e}");
            }
        }
    }
}
