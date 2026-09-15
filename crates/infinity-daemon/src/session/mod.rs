use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use infinity_agent_core::ThreadId;
use infinity_agent_core::message::InputMessage;
use infinity_agent_core::system::AgentSystemBuilder;
use infinity_agent_core::system::local::{
    ChannelSender, SubscribeHandle, ThreadLifecycleEvent, ThreadLifecycleState,
};
use infinity_agent_core::traits::{ConversationStore, InputSender, StateStore};
use infinity_protocol::{DaemonMessage, ModelRef, SessionInfo};
use tokio::sync::mpsc;

use crate::config;
use crate::ids::IdSource;
use crate::memory_store::{PersistentConversationStore, PersistentStateStore};
use crate::models::{self, CatalogModelSource, ModelCatalog};
use crate::rap_servers::SessionRapManager;
use crate::session_store;

pub mod display;
pub mod observer;
#[cfg(test)]
mod tests;

pub use observer::{
    DaemonObserver, SubscribeRequest, Subscriber, SubscriberMap, ThreadSubscribers,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The daemon's view of active threads: a thread is active while its driver
/// is live, and stays active after the driver idles for as long as it holds
/// active subscriptions (its RAP servers must stay up to deliver the
/// events). Maintained by the session activity watcher.
pub type ActiveThreadSet = Arc<std::sync::Mutex<std::collections::HashSet<ThreadId>>>;

pub type SessionStoreHandle = Arc<tokio::sync::Mutex<session_store::SessionStore>>;

/// Shared handle to the [`SessionManager`]. `Rc` rather than `Arc`: the
/// manager (and the whole agent system) lives on one tokio [`LocalSet`]
/// thread, and it holds `!Send` per-session toolsets.
///
/// [`LocalSet`]: tokio::task::LocalSet
pub type SharedSessionManager = std::rc::Rc<tokio::sync::Mutex<SessionManager>>;

/// Configuration for building a [`SessionManager`]. The non-generic
/// constructor ([`SessionManager::new`]) fills in the home-directory
/// defaults; tests can point everything at temp dirs for hermetic runs.
pub struct SessionManagerConfig {
    /// Directory for persisted daemon state (`sessions.json`, thread
    /// history, tool state).
    pub state_dir: PathBuf,
    /// Base URL for the RAP callback server.
    pub callback_url: String,
    /// Path to the user-level RAP config merged into every session
    /// (`~/.infinity/rap.json` in production). `None` disables user-level
    /// RAP servers, so sessions only boot servers from their cwd config.
    pub user_rap_config: Option<PathBuf>,
    /// Source of session/thread ids. Production uses random UUIDs
    /// ([`crate::ids::UuidIdSource`]); tests use
    /// [`crate::ids::SequentialIdSource`] so ids rendered in UIs are
    /// deterministic. Shared (`Arc`) between the manager and the
    /// conversation store so all ids come from one sequence.
    pub id_source: Arc<dyn IdSource>,
}

/// Manages all sessions on top of a single, daemon-lifetime agent system.
///
/// The agent system itself never shuts down: threads idle out individually
/// and respawn on demand, so message delivery never races a teardown. What
/// starts and stops with session activity are the session's RAP tool servers,
/// managed lazily by [`SessionRapManager`] — "shutting down" a session just
/// means stopping its servers (they reboot transparently on the next tool
/// invocation) and flagging it in the session store.
pub struct SessionManager {
    pub session_store: SessionStoreHandle,
    conversation_store: PersistentConversationStore,
    state_store: PersistentStateStore,
    /// Registered model providers and their available models.
    pub catalog: Arc<ModelCatalog>,
    /// Source of new session ids (thread ids come from the conversation
    /// store, which shares the same source).
    id_source: Arc<dyn IdSource>,
    /// Spawned model provider processes. Held so the providers stay alive
    /// for the daemon's lifetime (the processes are killed on drop).
    _provider_processes: Vec<tokio::process::Child>,
    /// Connected clients that receive broadcast updates.
    broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>>,
    /// Remote daemon connections.
    pub remote_daemons: Option<crate::remote::RemoteDaemons>,

    // ── Handles into the always-running agent system ──
    /// Delivers input messages to their threads.
    sender: ChannelSender,
    /// Attaches subscribers to threads (resolves once installed).
    subscribe: SubscribeHandle<SubscribeRequest>,
    /// Threads that are live or waiting on subscription events (see
    /// [`ActiveThreadSet`]).
    active_threads: ActiveThreadSet,
    /// Per-thread subscriber lists for broadcasting display events.
    pub subscriber_map: SubscriberMap,
    /// Per-session RAP servers and toolsets (lazily booted).
    pub rap_manager: SessionRapManager,
    /// Requests an idle re-evaluation for a session (e.g. after a client
    /// disconnect). Consumed by the session activity watcher alongside the
    /// core thread lifecycle events.
    idle_eval_tx: mpsc::UnboundedSender<ThreadId>,
}

impl SessionManager {
    /// Build a manager with the production defaults: providers are spawned
    /// from `~/.infinity/providers.json` and the user-level RAP config is
    /// `~/.infinity/rap.json`.
    pub async fn new(state_dir: PathBuf, callback_url: String) -> Result<Self, BoxError> {
        // Spawn the model providers configured in `~/.infinity/providers.json`
        // (each runs as a separate process serving a Unix socket) and register
        // them. Provider ids are the config keys; the first model of the
        // first provider is the global default.
        let providers_config = models::load_providers_config(&config::providers_config_path()?)?;
        let (providers, provider_processes) =
            models::spawn_configured_providers(&providers_config).await?;
        Self::with_providers(
            SessionManagerConfig {
                state_dir,
                callback_url,
                user_rap_config: Some(config::user_config_path()?),
                id_source: Arc::new(crate::ids::UuidIdSource),
            },
            providers,
            provider_processes,
        )
        .await
    }

    /// Build a manager from explicit `(provider_id, provider)` pairs instead
    /// of spawning provider processes from the on-disk config. This is the
    /// generic constructor used by tests (e.g. with an in-process mock
    /// provider); `provider_processes` are held for the manager's lifetime
    /// and killed on drop.
    pub async fn with_providers(
        config: SessionManagerConfig,
        providers: Vec<(String, Arc<dyn infinity_provider_protocol::ModelProvider>)>,
        provider_processes: Vec<tokio::process::Child>,
    ) -> Result<Self, BoxError> {
        let SessionManagerConfig {
            state_dir,
            callback_url,
            user_rap_config,
            id_source,
        } = config;
        let catalog = Arc::new(ModelCatalog::new(providers).await?);

        std::fs::create_dir_all(&state_dir).ok();
        let sessions_path = state_dir.join("sessions.json");
        let (change_tx, mut change_rx) = mpsc::unbounded_channel::<ThreadId>();
        let change_tx_for_conv = change_tx.clone();
        let session_store = Arc::new(tokio::sync::Mutex::new(session_store::SessionStore::load(
            &sessions_path.to_string_lossy(),
            change_tx,
        )));
        let broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let threads_dir = state_dir.join("threads");
        std::fs::create_dir_all(&threads_dir).ok();
        let mut conversation_store = PersistentConversationStore::new_with_dir(
            &threads_dir,
            catalog.default_ref().clone(),
            id_source.clone(),
        );
        conversation_store.set_change_tx(change_tx_for_conv);
        let state_store = PersistentStateStore::new(
            state_dir.join("state"),
            conversation_store.clone(),
            session_store.clone(),
        );
        let subscription_state = state_store.clone();
        let state_store_for_updates = state_store.clone();

        // Task: listen for session store changes and broadcast to clients
        let bc = broadcast_clients.clone();
        let ss = session_store.clone();
        let cs = conversation_store.clone();
        tokio::task::spawn_local(rap_protocol::log_panic(
            "session_change_broadcaster",
            async move {
                while let Some(session_id) = change_rx.recv().await {
                    let store = ss.lock().await;
                    let info = match store.sessions.get(&session_id) {
                        Some(e) => {
                            let threads = cs.get_open_subthreads(&session_id);
                            SessionInfo {
                                title: cs.get_thread_title(&session_id),
                                last_updated: cs.get_last_updated(&session_id),
                                total_tokens_used: cs.get_total_tokens_used(&session_id),
                                status: e.status(
                                    state_store_for_updates
                                        .has_pending_choices_for_session(&session_id),
                                ),
                                threads,
                                remote: None,
                            }
                        }
                        None => continue,
                    };
                    drop(store);
                    let mut sessions = HashMap::new();
                    sessions.insert(
                        infinity_protocol::ThreadRef::local(session_id.to_owned()),
                        info,
                    );
                    let msg = DaemonMessage::SessionsUpdated { sessions };
                    bc.lock()
                        .expect("bug: mutex poisoned")
                        .retain(|tx| tx.send(msg.clone()).is_ok());
                }
            },
        ));

        // ── The daemon-lifetime agent system ──
        let subscriber_map: SubscriberMap = Default::default();
        let rap_manager = SessionRapManager::new(
            conversation_store.clone(),
            session_store.clone(),
            user_rap_config,
            subscriber_map.clone(),
        );

        let state_store_for_system = state_store.clone();
        let system = AgentSystemBuilder::new_local(
            conversation_store.clone(),
            state_store_for_system,
            CatalogModelSource {
                catalog: catalog.clone(),
                conversation_store: conversation_store.clone(),
            },
        )
        .callback_url(callback_url.clone())
        .thread_config(rap_manager.clone())
        .with_tokio_sleep_tools()
        .build_local();

        // Per-thread observer factory: each new thread driver gets a
        // subscriber list seeded with its parent thread's subscribers (so
        // clients watching a parent automatically see its children).
        let make_observer = {
            let subscriber_map = subscriber_map.clone();
            let conversation_store = conversation_store.clone();
            move |thread_id: &ThreadId<str>| {
                let parent_subs = {
                    let parent_id = conversation_store.get_thread_parent_id(thread_id);
                    let smap = subscriber_map.lock().expect("bug: mutex poisoned");
                    let source = parent_id.as_deref().unwrap_or(thread_id);
                    smap.get(source)
                        .map(|arc| arc.lock().expect("bug: mutex poisoned").clone())
                        .unwrap_or_default()
                };
                let subscribers = subscriber_map
                    .lock()
                    .expect("bug: mutex poisoned")
                    .entry(thread_id.to_owned())
                    .or_insert_with(|| Arc::new(std::sync::Mutex::new(parent_subs)))
                    .clone();
                DaemonObserver {
                    subscribers,
                    conversation_store: conversation_store.clone(),
                }
            }
        };
        let mut running = system.start_with_observer(make_observer);
        let sender = running.sender();
        let subscribe = running.subscribe_handle();

        // ── Session activity watcher ──
        //
        // The single owner of the session store's idle flag and of the
        // daemon's view of thread activity. Every driver spawn (`Live`)
        // marks the thread active and its session non-idle, covering user
        // input and RAP callbacks alike. Every driver exit (`Idle`) asks the
        // state store whether the thread still holds active subscriptions:
        // if it does, the thread stays active (its subscription events will
        // respawn the driver, and its RAP servers must stay up to deliver
        // them); if not, the thread is retired and the session is evaluated
        // for teardown. Stopping servers is always safe: they reboot lazily
        // on the next tool invocation, so there is no teardown/wakeup race
        // to coordinate.
        let active_threads: ActiveThreadSet = Default::default();
        let (idle_eval_tx, mut idle_eval_rx) = mpsc::unbounded_channel::<ThreadId>();
        {
            let conversation_store = conversation_store.clone();
            let session_store = session_store.clone();
            let subscriber_map = subscriber_map.clone();
            let active_threads = active_threads.clone();
            let rap_manager = rap_manager.clone();
            tokio::task::spawn_local(rap_protocol::log_panic(
                "session_activity_watcher",
                async move {
                    loop {
                        enum Activity {
                            Lifecycle(ThreadLifecycleEvent),
                            Reevaluate(ThreadId),
                        }
                        let event = tokio::select! {
                            event = running.next_lifecycle_event() => match event {
                                Some(event) => Activity::Lifecycle(event),
                                None => break,
                            },
                            eval = idle_eval_rx.recv() => match eval {
                                // A client disconnect pings for re-evaluation
                                // without a driver transition.
                                Some(session_id) => Activity::Reevaluate(session_id),
                                None => break,
                            },
                        };
                        match event {
                            Activity::Lifecycle(ThreadLifecycleEvent {
                                thread_id,
                                state: ThreadLifecycleState::Live,
                            }) => {
                                let session_id = conversation_store.get_root_thread_id(&thread_id);
                                active_threads
                                    .lock()
                                    .expect("bug: mutex poisoned")
                                    .insert(thread_id);
                                let mut store = session_store.lock().await;
                                let changed = store.clear_idle(&session_id)
                                    | store.clear_shut_down(&session_id);
                                if changed && let Err(error) = store.save() {
                                    tracing::warn!(
                                        %session_id,
                                        %error,
                                        "failed to persist reactivated session",
                                    );
                                }
                            }
                            Activity::Lifecycle(ThreadLifecycleEvent {
                                thread_id,
                                state: ThreadLifecycleState::Idle,
                            }) => {
                                // A thread waiting on subscription events is
                                // still active even though its driver exited.
                                let subscribed = subscription_state
                                    .get_active_subscriptions(&thread_id)
                                    .await
                                    .map(|subscriptions| !subscriptions.is_empty())
                                    .unwrap_or(false);
                                if !subscribed {
                                    active_threads
                                        .lock()
                                        .expect("bug: mutex poisoned")
                                        .remove(&thread_id);
                                }
                                let session_id = conversation_store.get_root_thread_id(&thread_id);
                                evaluate_session_idle(
                                    &session_id,
                                    &conversation_store,
                                    &session_store,
                                    &subscriber_map,
                                    &active_threads,
                                    &rap_manager,
                                )
                                .await;
                            }
                            Activity::Reevaluate(session_id) => {
                                evaluate_session_idle(
                                    &session_id,
                                    &conversation_store,
                                    &session_store,
                                    &subscriber_map,
                                    &active_threads,
                                    &rap_manager,
                                )
                                .await;
                            }
                        }
                    }
                },
            ));
        }

        Ok(Self {
            session_store,
            conversation_store,
            state_store,
            catalog,
            id_source,
            _provider_processes: provider_processes,
            broadcast_clients,
            remote_daemons: None,
            sender,
            subscribe,
            active_threads,
            subscriber_map,
            rap_manager,
            idle_eval_tx,
        })
    }

    /// Initialize remote daemon connections from config.
    pub fn init_remotes(&mut self, configs: Vec<crate::remote::RemoteConfig>) {
        if configs.is_empty() {
            return;
        }
        self.remote_daemons = Some(crate::remote::RemoteDaemons::new(
            configs,
            self.broadcast_clients.clone(),
        ));
    }

    pub fn conversation_store(&self) -> &PersistentConversationStore {
        &self.conversation_store
    }

    pub fn state_store(&self) -> &PersistentStateStore {
        &self.state_store
    }

    /// Broadcast a message to all connected clients.
    pub fn broadcast(&self, msg: DaemonMessage) {
        self.broadcast_clients
            .lock()
            .expect("bug: mutex poisoned")
            .retain(|tx| tx.send(msg.clone()).is_ok());
    }

    /// Handle a view_update RAP callback: persist the view and broadcast to subscribers.
    pub fn handle_view_update(
        &self,
        group_id: &ThreadId<str>,
        view_type: &str,
        content: serde_json::Value,
    ) {
        self.conversation_store
            .set_view(group_id, view_type, content.clone());

        let msg = DaemonMessage::ViewUpdate {
            thread_id: Some(infinity_protocol::ThreadRef::local(group_id.to_owned())),
            view_type: view_type.to_owned(),
            content,
        };
        observer::broadcast_to_thread(&self.subscriber_map, group_id, &msg, None);
    }

    /// Create a brand new session with the given working directory and model.
    /// The model is not validated here; if it is no longer available when the
    /// agent runs, the thread worker falls back to the default model.
    pub async fn create_session(
        &self,
        cwd: &Path,
        model: ModelRef,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<ThreadId, BoxError> {
        let session_id = self.id_source.generate();
        {
            let mut store = self.session_store.lock().await;
            store.create(&session_id, cwd.to_path_buf());
            let _ = store.save();
        }
        // Ensure the root thread metadata exists before setting last_updated,
        // otherwise set_last_updated is a no-op and the session broadcasts with
        // an empty timestamp (sorting it to the bottom of the session list).
        self.conversation_store
            .ensure_root_thread(&session_id)
            .await
            .map_err(|e| format!("failed to ensure root thread: {e}"))?;
        // Persist the selected model on the root thread so restarts keep it.
        self.conversation_store.set_thread_model(&session_id, model);
        self.conversation_store
            .set_last_updated(&session_id, &chrono::Utc::now().to_rfc3339());
        emit(self.build_connected(&session_id, &session_id)).await;
        // Nothing to start: the agent system is always running, and the
        // session's RAP servers boot lazily on its first step.
        Ok(session_id)
    }

    /// Resume a persisted session, recovering its cwd from the session store.
    /// Does NOT boot the agent loop — that happens lazily on first user input
    /// via `send_input`. This just emits `Connected` so the client can attach.
    pub async fn resume_session(
        &self,
        session_id: &ThreadId<str>,
        thread_id: &ThreadId<str>,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<(), BoxError> {
        emit(self.build_connected(session_id, thread_id)).await;
        Ok(())
    }

    fn build_connected(
        &self,
        session_id: &ThreadId<str>,
        thread_id: &ThreadId<str>,
    ) -> DaemonMessage {
        // Resolve the thread's own selected model (falling back to the global
        // default if it is no longer available).
        let selected = self.conversation_store.get_thread_model(thread_id);
        let (model_ref, entry, _) = self.catalog.resolve(&selected);
        DaemonMessage::Connected {
            root_thread_id: infinity_protocol::ThreadRef::local(session_id.to_owned()),
            thread_id: infinity_protocol::ThreadRef::local(thread_id.to_owned()),
            model_name: entry.display_name.clone(),
            context_window: entry.context_window,
            title: self.conversation_store.get_thread_title(session_id),
            total_tokens_used: self.conversation_store.get_total_tokens_used(session_id),
            provider_id: model_ref.provider_id,
        }
    }

    /// Attach a client's message sender to a thread for receiving display
    /// events. The request is routed to the thread's driver (spawning one if
    /// none is live) and resolves once the subscriber is installed — its
    /// replay sent and its registration completed — so input sent afterwards
    /// is guaranteed to be observed by the client.
    ///
    /// `keeps_session_alive`: when `false`, this subscriber will not prevent
    /// the session from going idle (and its RAP servers being stopped).
    pub async fn attach_client(
        &self,
        thread_id: &ThreadId<str>,
        tx: mpsc::UnboundedSender<DaemonMessage>,
        wants_replay: bool,
        keeps_session_alive: bool,
    ) {
        let installed = self
            .subscribe
            .subscribe(
                thread_id,
                SubscribeRequest {
                    tx: tx.clone(),
                    wants_replay,
                    keeps_session_alive,
                },
            )
            .await;
        if !installed {
            // Only possible while the daemon itself is shutting down.
            tracing::warn!("failed to attach client to thread {thread_id}");
        }
    }

    /// Send an input message to the agent system.
    ///
    /// All input surfaces enqueue blindly. The router drops event-style input
    /// for unknown or stopped threads, while user text may create or resume a
    /// thread. A resulting `Live` lifecycle transition clears the root
    /// session's idle and stopped flags, including when a child thread wakes.
    pub async fn send_input(&self, msg: (InputMessage, Option<String>)) -> bool {
        let (input, dedup) = msg;
        let dedup_id = dedup.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.sender
            .send_to_input_queue(input, &dedup_id)
            .await
            .is_ok()
    }

    /// The agent system's input sender, used by the RAP callback bridge to
    /// forward converted callbacks directly into the input queue. The router
    /// owns thread-existence and stopped-thread admission.
    pub fn input_sender(&self) -> ChannelSender {
        self.sender.clone()
    }

    /// Switch the model used for future requests on a specific thread. The
    /// switch affects only `thread_id` — it does not propagate to child
    /// threads.
    ///
    /// The selection is validated against the catalog and persisted in the
    /// conversation store; every completion round resolves the persisted
    /// selection when it starts (via [`CatalogModelSource`]), so the switch
    /// takes effect on the thread's next round — an in-flight completion
    /// finishes on the model it started with.
    ///
    /// The [`DaemonMessage::ModelSwitched`] confirmation is broadcast to the
    /// thread's subscribers, and delivered to `requester` as well when the
    /// requester is not already among those subscribers — so the requesting
    /// client sees the confirmation exactly once whether or not it is
    /// attached.
    pub fn switch_model(
        &self,
        thread_id: &ThreadId<str>,
        model: ModelRef,
        requester: Option<&mpsc::UnboundedSender<DaemonMessage>>,
    ) -> Result<(), String> {
        let Some(entry) = self.catalog.find(&model) else {
            return Err(format!(
                "unknown model {}/{}",
                model.provider_id, model.model_id
            ));
        };
        if !self.conversation_store.has_thread(thread_id) {
            return Err(format!("thread {thread_id} not found"));
        }

        // Persist so future rounds (and daemon restarts) resolve the new
        // selection.
        self.conversation_store
            .set_thread_model(thread_id, model.clone());

        let msg = DaemonMessage::ModelSwitched {
            thread_id: infinity_protocol::ThreadRef::local(thread_id.to_owned()),
            model_name: entry.display_name.clone(),
            context_window: entry.context_window,
            provider_id: model.provider_id.clone(),
        };

        // Broadcast the confirmation to the thread's subscribers; deliver
        // directly to the requester only if the broadcast did not already
        // reach it, so it sees the confirmation exactly once.
        let requester_reached =
            observer::broadcast_to_thread(&self.subscriber_map, thread_id, &msg, requester);
        if !requester_reached && let Some(requester) = requester {
            let _ = requester.send(msg);
        }
        Ok(())
    }

    /// List all sessions — active ones plus persisted ones from the cache.
    pub async fn list_sessions(
        &self,
        subscribe: Option<mpsc::UnboundedSender<DaemonMessage>>,
    ) -> HashMap<infinity_protocol::ThreadRef, SessionInfo> {
        if let Some(tx) = subscribe {
            self.broadcast_clients
                .lock()
                .expect("bug: mutex poisoned")
                .push(tx);
        }

        let store = self.session_store.lock().await;
        let mut result: HashMap<infinity_protocol::ThreadRef, SessionInfo> = HashMap::new();

        for (id, entry) in &store.sessions {
            let threads = self.conversation_store.get_open_subthreads(id);
            result.insert(
                infinity_protocol::ThreadRef::local(id.to_owned()),
                SessionInfo {
                    title: self.conversation_store.get_thread_title(id),
                    last_updated: self.conversation_store.get_last_updated(id),
                    total_tokens_used: self.conversation_store.get_total_tokens_used(id),
                    status: entry.status(self.state_store.has_pending_choices_for_session(id)),
                    threads,
                    remote: None,
                },
            );
        }

        // Include remote sessions
        if let Some(ref rd) = self.remote_daemons {
            result.extend(rd.all_remote_sessions());
        }

        result
    }

    /// Shut down a session: stop its RAP servers, drop its cached toolset
    /// (so a later restart re-reads the RAP config), clear pending choices,
    /// and mark it shut down in the store.
    ///
    /// The agent system keeps running — a thread that is mid-completion
    /// finishes and flushes its turn, then parks. Sending new user input
    /// clears the flag and picks the session back up (its servers reboot
    /// lazily); RAP callbacks arriving while shut down are ignored.
    #[tracing::instrument(skip(self))]
    pub async fn cleanup_session(&self, session_id: &ThreadId<str>) {
        if let Err(error) = self
            .state_store
            .clear_pending_choices_for_session(session_id)
            .await
        {
            tracing::error!(%error, "failed to clear pending choices for {session_id}");
        }
        self.rap_manager.evict_session(session_id).await;
        let mut store = self.session_store.lock().await;
        if store.sessions.contains_key(session_id) {
            store.mark_shut_down(session_id);
            let _ = store.save();
            tracing::info!("Cleanup complete");
        } else {
            tracing::warn!("Session not found");
        }
    }

    /// Returns true if the session has no active threads: none with a live
    /// driver and none waiting on subscription events.
    pub fn is_session_idle(&self, session_id: &ThreadId<str>) -> bool {
        !session_has_active_threads(&self.active_threads, &self.conversation_store, session_id)
    }

    /// Request an idle re-evaluation for a session (e.g. after a client
    /// disconnect): if none of its threads are live and no keep-alive client
    /// remains, its RAP servers are stopped.
    pub fn send_idle_ping(&self, session_id: &ThreadId<str>) {
        let _ = self.idle_eval_tx.send(session_id.to_owned());
    }
}

/// Whether any active thread belongs to `session_id` (live driver or waiting
/// on subscription events). The single definition of "session activity",
/// shared by [`SessionManager::is_session_idle`] and
/// [`evaluate_session_idle`] so the two can never disagree.
fn session_has_active_threads(
    active_threads: &ActiveThreadSet,
    conversation_store: &PersistentConversationStore,
    session_id: &ThreadId<str>,
) -> bool {
    active_threads
        .lock()
        .expect("bug: mutex poisoned")
        .iter()
        .any(|thread_id| conversation_store.get_root_thread_id(thread_id) == *session_id)
}

/// Decide whether `session_id` has gone idle and release its resources.
///
/// Runs on every thread-driver exit and on explicit idle pings. "Idle" means
/// the session has no active threads: none with a live driver and none
/// waiting on subscription events. The session is then flagged idle in the
/// store, and, if no keep-alive client is attached, its RAP servers are
/// stopped. Stopping is always safe (never a race): a message that arrives a
/// moment later simply respawns a driver, and the first tool interaction
/// boots the servers back up.
async fn evaluate_session_idle(
    session_id: &ThreadId<str>,
    conversation_store: &PersistentConversationStore,
    session_store: &SessionStoreHandle,
    subscriber_map: &SubscriberMap,
    active_threads: &ActiveThreadSet,
    rap_manager: &SessionRapManager,
) {
    if session_has_active_threads(active_threads, conversation_store, session_id) {
        return;
    }

    // Mark idle in the store so listing shows Idle status.
    {
        let mut store = session_store.lock().await;
        if !store.sessions.contains_key(session_id) {
            return;
        }
        store.mark_idle(session_id);
        let _ = store.save();
    }

    // If a keep-alive client is attached to any of the session's threads,
    // keep its servers warm.
    let has_clients = {
        let smap = subscriber_map.lock().expect("bug: mutex poisoned");
        smap.iter()
            .filter(|(thread_id, _)| {
                conversation_store.get_root_thread_id(thread_id) == *session_id
            })
            .any(|(_, subs)| {
                subs.lock()
                    .expect("bug: mutex poisoned")
                    .iter()
                    .any(|sub| !sub.tx.is_closed() && sub.keeps_session_alive)
            })
    };
    if !has_clients {
        tracing::info!(
            "Session {session_id} is idle with no keep-alive clients; stopping its RAP servers"
        );
        rap_manager.shutdown_session(session_id).await;
    } else {
        tracing::info!("Session {session_id} is idle but a client is still connected");
    }
}
