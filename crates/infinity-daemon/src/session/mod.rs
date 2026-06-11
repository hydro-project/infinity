use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use infinity_agent_core::message::InputMessage;
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::thread::{
    CloseThreadTool, ReportToParentTool, SendMessageToChildTool, SpawnThreadTool,
};
use infinity_agent_core::traits::ConversationStore;
use infinity_protocol::{DaemonMessage, ModelRef, SessionInfo};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config;
use crate::mcp_proxy;
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use crate::models::ModelCatalog;
use crate::rap_tools;
use crate::session_store;
use crate::set_title_tool;
use crate::sleep_tools::{SleepTool, SleepUntilTool};

pub mod agent_loop;
pub mod display;
pub mod thread_worker;

pub use agent_loop::agent_loop;
pub use thread_worker::thread_worker;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Re-export from thread_worker.
pub use thread_worker::{SubscribeRequest, ThreadSubscribers};

/// Message sent to the agent loop — either user input or a subscribe request.
pub enum AgentMessage {
    Input(Box<InputMessage>, String),
    Subscribe {
        thread_id: String,
        request: SubscribeRequest,
    },
}

/// Maps thread_id → subscriber list (for inheriting to children and idle detection).
pub type SubscriberMap = Arc<std::sync::Mutex<HashMap<String, ThreadSubscribers>>>;

pub type ActiveThreads = Arc<std::sync::Mutex<HashSet<String>>>;

/// A single agent session managed by the daemon.
/// The session_id doubles as the root thread_id.
pub struct Session {
    pub session_id: String,
    pub cwd: PathBuf,
    pub active_threads: ActiveThreads,
    pub agent_tx: mpsc::UnboundedSender<AgentMessage>,
    pub agent_task: JoinHandle<()>,
    /// Send to signal the agent loop to shut down.
    pub shutdown_tx: Option<oneshot::Sender<()>>,
    /// Send to ping the agent to attempt to idle.
    pub idle_tx: mpsc::UnboundedSender<()>,
    /// Per-thread subscriber lists for broadcasting display events.
    pub subscriber_map: SubscriberMap,
}

pub type SessionStoreHandle = Arc<tokio::sync::Mutex<session_store::SessionStore>>;

/// Manages all active sessions.
pub struct SessionManager {
    pub sessions: HashMap<String, Session>,
    /// Base URL for the RAP callback server.
    pub callback_url: String,
    pub session_store: SessionStoreHandle,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    /// Registered model providers and their available models.
    pub catalog: Arc<ModelCatalog>,
    /// Connected clients that receive broadcast updates.
    broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>>,
    /// Remote daemon connections.
    pub remote_daemons: Option<crate::remote::RemoteDaemons>,
}

impl SessionManager {
    pub async fn new(state_dir: PathBuf, callback_url: String) -> Result<Self, BoxError> {
        // Register model providers. Each provider gets a stable unique id;
        // the first model of the first provider is the global default.
        let catalog = Arc::new(
            ModelCatalog::new(vec![(
                "bedrock".to_owned(),
                Arc::new(infinity_provider_bedrock::BedrockProvider::from_env()) as _,
            )])
            .await?,
        );

        std::fs::create_dir_all(&state_dir).ok();
        let sessions_path = state_dir.join("sessions.json");
        let (change_tx, mut change_rx) = mpsc::unbounded_channel::<String>();
        let change_tx_for_conv = change_tx.clone();
        let session_store = Arc::new(tokio::sync::Mutex::new(session_store::SessionStore::load(
            &sessions_path.to_string_lossy(),
            change_tx,
        )));
        let broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let threads_dir = state_dir.join("threads");
        std::fs::create_dir_all(&threads_dir).ok();
        let mut conversation_store =
            InMemoryConversationStore::new_with_dir(&threads_dir, catalog.default_ref().clone());
        conversation_store.set_change_tx(change_tx_for_conv);
        let state_store = InMemoryStateStore::new(state_dir.join("state"));

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
                                status: e.status(cs.has_pending_choices(&session_id)),
                                threads,
                                remote: None,
                            }
                        }
                        None => continue,
                    };
                    drop(store);
                    let mut sessions = HashMap::new();
                    sessions.insert(session_id, info);
                    let msg = DaemonMessage::SessionsUpdated { sessions };
                    bc.lock()
                        .expect("bug: mutex poisoned")
                        .retain(|tx| tx.send(msg.clone()).is_ok());
                }
            },
        ));

        Ok(Self {
            sessions: HashMap::new(),
            callback_url,
            session_store,
            conversation_store,
            state_store,
            catalog,
            broadcast_clients,
            remote_daemons: None,
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

    pub fn conversation_store(&self) -> &InMemoryConversationStore {
        &self.conversation_store
    }

    /// Broadcast a message to all connected clients.
    pub fn broadcast(&self, msg: DaemonMessage) {
        self.broadcast_clients
            .lock()
            .expect("bug: mutex poisoned")
            .retain(|tx| tx.send(msg.clone()).is_ok());
    }

    /// Handle a view_update RAP callback: persist the view and broadcast to subscribers.
    pub fn handle_view_update(&self, group_id: &str, view_type: &str, content: serde_json::Value) {
        self.conversation_store
            .set_view(group_id, view_type, content.clone());

        let session_id = self.conversation_store.get_root_thread_id(group_id);
        let msg = DaemonMessage::ViewUpdate {
            thread_id: Some(group_id.to_owned()),
            view_type: view_type.to_owned(),
            content,
        };

        if let Some(session) = self.sessions.get(&session_id) {
            let smap = session.subscriber_map.lock().expect("bug: mutex poisoned");
            if let Some(subs) = smap.get(group_id) {
                let subs = subs.lock().expect("bug: mutex poisoned");
                for tx in subs.iter() {
                    let _ = tx.send(msg.clone());
                }
            }
        }
    }

    /// Create a brand new session with the given working directory and model.
    /// The model is not validated here; if it is no longer available when the
    /// agent runs, the thread worker falls back to the default model.
    pub async fn create_session(
        &mut self,
        cwd: &Path,
        model: ModelRef,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<String, BoxError> {
        let session_id = uuid::Uuid::new_v4().to_string();
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
        self.start_session(session_id.clone(), cwd, emit).await?;
        Ok(session_id)
    }

    /// Resume a persisted session, recovering its cwd from the session store.
    /// Does NOT boot the agent loop — that happens lazily on first user input
    /// via `send_input`. This just emits `Connected` so the client can attach.
    pub async fn resume_session(
        &mut self,
        session_id: &str,
        thread_id: &str,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<(), BoxError> {
        emit(self.build_connected(session_id, thread_id)).await;
        Ok(())
    }

    fn build_connected(&self, session_id: &str, thread_id: &str) -> DaemonMessage {
        // Resolve the thread's own selected model (falling back to the global
        // default if it is no longer available).
        let selected = self.conversation_store.get_thread_model(thread_id);
        let (_, entry, _) = self.catalog.resolve(&selected);
        DaemonMessage::Connected {
            session_id: session_id.to_owned(),
            thread_id: thread_id.to_owned(),
            model_name: entry.display_name.clone(),
            context_window: entry.context_window,
            title: self.conversation_store.get_thread_title(session_id),
            total_tokens_used: self.conversation_store.get_total_tokens_used(session_id),
        }
    }

    /// Internal: spin up the agent loop for a session.
    async fn start_session(
        &mut self,
        session_id: String,
        cwd: &Path,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<(), BoxError> {
        if self.sessions.contains_key(&session_id) {
            return Ok(());
        }

        let active_threads = Arc::new(std::sync::Mutex::new(HashSet::new()));

        let (agent_tx, agent_rx) = mpsc::unbounded_channel::<AgentMessage>();
        // Adapter: InMemoryMessageSender needs a (InputMessage, String) sender.
        // Create one that wraps into AgentMessage::Input.
        let (input_tx, mut input_adapter_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
        let agent_tx_for_adapter = agent_tx.clone();
        tokio::task::spawn_local(rap_protocol::log_panic("input_adapter", async move {
            while let Some((msg, id)) = input_adapter_rx.recv().await {
                if agent_tx_for_adapter
                    .send(AgentMessage::Input(Box::new(msg), id))
                    .is_err()
                {
                    break;
                }
            }
        }));
        let sender = InMemoryMessageSender::new(input_tx.clone());

        // Load RAP config
        let sid = session_id.clone();
        let info = |text: String| DaemonMessage::Info {
            thread_id: Some(sid.clone()),
            text,
        };

        let boot_result = boot_rap_servers(cwd, &mut async |text: String| {
            emit(info(text)).await;
        })
        .await;
        let booted = match boot_result {
            Ok(b) => b,
            Err(e) => {
                emit(info(format!("Warning: failed to boot RAP servers: {e}"))).await;
                BootedRapServers {
                    server_ports: HashMap::new(),
                    server_ids: HashMap::new(),
                    spawned_servers: Vec::new(),
                    urls: Vec::new(),
                }
            }
        };
        let spawned_servers = booted.spawned_servers;
        let urls = booted.urls;

        let rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> = if !urls.is_empty() {
            let servers_with_ids: Vec<(String, Option<String>)> = urls
                .iter()
                .map(|u| {
                    let id = booted.server_ids.get(u).cloned();
                    (u.clone(), id)
                })
                .collect();
            match rap_tools::load_rap_tools(&servers_with_ids).await {
                Ok(loaded) => {
                    emit(info(format!("Loaded {} RAP tool(s)", loaded.tools.len()))).await;

                    emit(info(String::new())).await;

                    loaded.tools
                }
                Err(e) => {
                    emit(info(format!("Warning: failed to load RAP tools: {e}"))).await;
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let extra_system_prompt = Some(format!(
            "The user's current working directory is: {cwd:?}\n\n\
             Use the `set_title` tool to give the current thread a short, descriptive title. \
             Set it once at the start when the user's intent becomes clear, and update it only \
             when the overall scope of work changes significantly. Do not call it repeatedly \
             for minor follow-ups within the same task."
        ));

        let state_store = self.state_store.clone();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let (idle_tx, agent_handle, subscriber_map) = self.start_agent_loop(
            session_id.clone(),
            agent_rx,
            self.conversation_store.clone(),
            state_store,
            sender,
            self.callback_url.clone(),
            rap_tools,
            urls,
            extra_system_prompt,
            active_threads.clone(),
            shutdown_rx,
            spawned_servers,
        );

        let session = Session {
            session_id: session_id.clone(),
            cwd: cwd.to_path_buf(),
            active_threads,
            agent_tx,
            agent_task: agent_handle,
            shutdown_tx: Some(shutdown_tx),
            idle_tx,
            subscriber_map,
        };
        self.sessions.insert(session_id.clone(), session);
        Ok(())
    }

    /// Attach a client's message sender to a thread for receiving display events.
    /// If the session is alive, sends a subscribe request through the agent loop.
    /// Otherwise, loads history directly from the conversation store and sends
    /// a Replay message to the client.
    pub async fn attach_client(
        &mut self,
        thread_id: &str,
        tx: mpsc::UnboundedSender<DaemonMessage>,
        wants_replay: bool,
    ) {
        let session_id = self.conversation_store.get_root_thread_id(thread_id);

        let is_alive = self
            .sessions
            .get(&session_id)
            .is_some_and(|s| !s.agent_task.is_finished());

        if is_alive {
            let agent_tx = self
                .sessions
                .get(&session_id)
                .expect("bug: session missing after is_alive check")
                .agent_tx
                .clone();
            let _ = agent_tx.send(AgentMessage::Subscribe {
                thread_id: thread_id.to_owned(),
                request: (tx.clone(), wants_replay),
            });
        } else if wants_replay {
            // Session not alive — load history from the conversation store directly.
            let history = self
                .conversation_store
                .load_history_up_to(thread_id, None, None)
                .await
                .unwrap_or_default();
            let history: Vec<DaemonMessage> = history
                .iter()
                .filter_map(|m| display::history_message_to_daemon(m, thread_id, &history))
                .collect();
            let choices = self
                .conversation_store
                .get_pending_choice_messages(&session_id);
            let views = self.conversation_store.get_views(thread_id);
            if !history.is_empty() || !choices.is_empty() || !views.is_empty() {
                let _ = tx.send(DaemonMessage::Replay {
                    history,
                    pending_choices: choices,
                    views,
                });
            }
        }
    }

    /// Send user input text to a session's agent loop.
    /// If the session was shut down and no agent loop is running, this
    /// clears the shut_down flag and starts a new agent loop first.
    pub async fn send_input(
        &mut self,
        thread_id: &str,
        msg: (InputMessage, Option<String>),
        client_tx: Option<mpsc::UnboundedSender<DaemonMessage>>,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> bool {
        // Resolve thread ID to root session ID in case a child thread ID was provided.
        let session_id = self.conversation_store.get_root_thread_id(thread_id);
        let session_id = session_id.as_str();

        // If session task finished or was never started, check if we need to restart.
        let needs_restart = if let Some(session) = self.sessions.get(session_id) {
            session.agent_task.is_finished()
        } else {
            // Session isn't running in memory. It needs a restart if it exists in the
            // store at all — either it was shut down cleanly, or the daemon restarted
            // while it was still running (in which case shut_down may be false).
            let store = self.session_store.lock().await;
            store.sessions.contains_key(session_id)
        };

        // if client_tx is None, that means this is for a RAP callback, but if the agent was shut down,
        // we should ignore the callback (the agent will not idle if any tool calls or subscriptions are active)
        if needs_restart && client_tx.is_some() {
            // Remove stale session if present.
            self.sessions.remove(session_id);
            {
                let mut store = self.session_store.lock().await;
                store.clear_shut_down(session_id);
                store.clear_idle(session_id);
                let _ = store.save();
            }
            let cwd = self.session_store.lock().await.get_cwd(session_id).clone();
            if let Err(e) = self.start_session(session_id.to_owned(), &cwd, emit).await {
                tracing::error!("failed to restart session: {e}");
                return false;
            }
            // Re-attach client to the new session.
            if let Some(tx) = client_tx {
                self.attach_client(session_id, tx, false).await;
            }
        }

        if let Some(session) = self.sessions.get(session_id) {
            // Clear idle since the agent is about to do work.
            {
                let mut store = self.session_store.lock().await;
                store.clear_idle(session_id);
                let _ = store.save();
            }
            session
                .agent_tx
                .send(AgentMessage::Input(
                    Box::new(msg.0),
                    msg.1.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                ))
                .is_ok()
        } else {
            false
        }
    }

    /// List all sessions — active ones plus persisted ones from the cache.
    pub async fn list_sessions(
        &self,
        subscribe: Option<mpsc::UnboundedSender<DaemonMessage>>,
    ) -> HashMap<String, SessionInfo> {
        if let Some(tx) = subscribe {
            self.broadcast_clients
                .lock()
                .expect("bug: mutex poisoned")
                .push(tx);
        }

        let store = self.session_store.lock().await;
        let mut result: HashMap<String, SessionInfo> = HashMap::new();

        for (id, entry) in &store.sessions {
            let threads = self.conversation_store.get_open_subthreads(id);
            result.insert(
                id.clone(),
                SessionInfo {
                    title: self.conversation_store.get_thread_title(id),
                    last_updated: self.conversation_store.get_last_updated(id),
                    total_tokens_used: self.conversation_store.get_total_tokens_used(id),
                    status: entry.status(self.conversation_store.has_pending_choices(id)),
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

    /// Clean up a session: signal shutdown, wait for agent task to finish
    /// (which handles RAP server cleanup and marking the session store).
    #[tracing::instrument(skip(self))]
    pub async fn cleanup_session(&mut self, session_id: &str) {
        if let Some(mut session) = self.sessions.remove(session_id) {
            // Signal shutdown; the spawned future will kill servers and mark shut_down.
            tracing::debug!("Session found, sending `shutdown_tx`");
            if let Some(tx) = session.shutdown_tx.take() {
                let _ = tx.send(());
            }
            if let Err(ref e) = session.agent_task.await {
                if e.is_panic() {
                    tracing::error!("session agent task panicked: {e}");
                } else {
                    tracing::warn!("session agent task cancelled: {e}");
                }
            }

            // Ensure shut_down is set (task may have already finished as idle).
            tracing::debug!("Setting `shut_down`");
            let mut store = self.session_store.lock().await;
            if !store.is_shut_down(session_id) {
                store.mark_shut_down(session_id);
                let _ = store.save();
            }
            tracing::info!("Cleanup complete");
        } else {
            tracing::warn!("Session not found");
        }
    }

    /// Returns true if the session has no running thread workers.
    pub fn is_session_idle(&self, session_id: &str) -> bool {
        match self.sessions.get(session_id) {
            Some(session) => {
                session.agent_task.is_finished()
                    || session
                        .active_threads
                        .lock()
                        .expect("bug: mutex poisoned")
                        .is_empty()
            }
            None => true,
        }
    }

    /// Ping the agent to attempt to idle (e.g. after client disconnect).
    pub fn send_idle_ping(&self, session_id: &str) {
        if let Some(session) = self.sessions.get(session_id) {
            let _ = session.idle_tx.send(());
        }
    }

    /// Start the agent loop for a session.
    #[expect(
        clippy::too_many_arguments,
        reason = "session setup requires many dependencies"
    )]
    fn start_agent_loop(
        &self,
        session_id: String,
        agent_rx: mpsc::UnboundedReceiver<AgentMessage>,
        conversation_store: InMemoryConversationStore,
        state_store: InMemoryStateStore,
        sender: InMemoryMessageSender,
        callback_url: String,
        rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
        tool_server_urls: Vec<String>,
        extra_system_prompt: Option<String>,
        active_threads: ActiveThreads,
        shutdown_rx: oneshot::Receiver<()>,
        spawned_servers: Vec<tokio::process::Child>,
    ) -> (mpsc::UnboundedSender<()>, JoinHandle<()>, SubscriberMap) {
        spawn_agent_loop(
            session_id,
            self.session_store.clone(),
            agent_rx,
            self.catalog.clone(),
            conversation_store,
            state_store,
            sender,
            callback_url,
            rap_tools,
            tool_server_urls,
            extra_system_prompt,
            active_threads,
            shutdown_rx,
            spawned_servers,
        )
    }
}

// ── Agent loop (mirrors CLI main.rs agent_loop/thread_worker) ───────────────

#[expect(
    clippy::too_many_arguments,
    reason = "agent loop requires many dependencies"
)]
fn spawn_agent_loop(
    session_id: String,
    session_store: SessionStoreHandle,
    agent_rx: mpsc::UnboundedReceiver<AgentMessage>,
    catalog: Arc<ModelCatalog>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
    tool_server_urls: Vec<String>,
    extra_system_prompt: Option<String>,
    active_threads: ActiveThreads,
    shutdown_rx: oneshot::Receiver<()>,
    spawned_servers: Vec<tokio::process::Child>,
) -> (mpsc::UnboundedSender<()>, JoinHandle<()>, SubscriberMap) {
    let rap_notifier = if tool_server_urls.is_empty() {
        None
    } else {
        Some(rap_client::notifier::RapNotifier::new(
            tool_server_urls,
            rap_tools::SimpleHttpClient::new(),
        ))
    };

    let mut tool_impls: Vec<Box<dyn Tool<InMemoryMessageSender>>> = vec![
        Box::new(SleepUntilEventOrInputTool),
        Box::new(SleepTool),
        Box::new(SleepUntilTool),
        Box::new(SpawnThreadTool {
            conversation_store: conversation_store.clone(),
        }),
        Box::new(ReportToParentTool {
            conversation_store: conversation_store.clone(),
        }),
        Box::new(CloseThreadTool {
            conversation_store: conversation_store.clone(),
            rap_notifier: rap_notifier.clone(),
        }),
        Box::new(SendMessageToChildTool {
            conversation_store: conversation_store.clone(),
        }),
        Box::new(
            infinity_agent_core::tools::cancel_subscription::CancelSubscriptionTool {
                state_store: state_store.clone(),
                rap_notifier: rap_notifier.clone(),
            },
        ),
        Box::new(set_title_tool::SetTitleTool {
            conversation_store: conversation_store.clone(),
        }),
    ];
    tool_impls.extend(rap_tools);

    let tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>> = Arc::new(tool_impls);
    let extra_system_prompt = Arc::new(extra_system_prompt);
    let (idle_tx, idle_rx) = mpsc::unbounded_channel();

    let subscriber_map = Arc::new(std::sync::Mutex::new(HashMap::new()));

    let agent_fut = agent_loop(
        session_id.clone(),
        agent_rx,
        catalog,
        conversation_store.clone(),
        state_store,
        sender,
        callback_url,
        tool_impls,
        extra_system_prompt,
        rap_notifier,
        subscriber_map.clone(),
        active_threads.clone(),
        idle_tx.clone(),
    );

    let handle = tokio::task::spawn_local(session_wrapper(
        agent_fut,
        session_id,
        session_store,
        conversation_store,
        subscriber_map.clone(),
        active_threads,
        idle_rx,
        shutdown_rx,
        spawned_servers,
    ));
    (idle_tx, handle, subscriber_map)
}

/// Wrapper that owns the RAP servers and handles cleanup.
/// Runs agent_loop, selects on shutdown signal and idle notifications.
/// When done, gracefully kills servers and marks the session store.
#[tracing::instrument(skip_all, fields(session_id))]
#[expect(
    clippy::too_many_arguments,
    reason = "session wrapper requires many dependencies"
)]
async fn session_wrapper(
    agent_fut: impl Future<Output = ()>,
    session_id: String,
    session_store: SessionStoreHandle,
    conversation_store: InMemoryConversationStore,
    subscriber_map: SubscriberMap,
    active_threads: ActiveThreads,
    mut idle_rx: mpsc::UnboundedReceiver<()>,
    mut shutdown_rx: oneshot::Receiver<()>,
    mut spawned_servers: Vec<tokio::process::Child>,
) {
    // Determine why we exited: idle (no client) vs explicit shutdown.
    let idle_exited;
    tokio::pin!(agent_fut);
    loop {
        tokio::select! {
            _ = &mut agent_fut => {
                // agent_loop returned (rx closed). Wait for workers to drain.
                while idle_rx.try_recv().is_ok() {}
                idle_exited = {
                    let smap = subscriber_map.lock().expect("bug: mutex poisoned");
                    smap.values().all(|subs| subs.lock().expect("bug: mutex poisoned").iter().all(|tx| tx.is_closed()))
                };
                break;
            }
            _ = &mut shutdown_rx => {
                tracing::info!("Received `shutdown_rx`.");
                conversation_store.clear_pending_choices(&session_id);
                idle_exited = false;
                break;
            }
            _ = idle_rx.recv() => {
                // idle_tx means "might be idle" — check active threads.
                if !active_threads.lock().expect("bug: active_threads mutex poisoned").is_empty() {
                    continue;
                }

                // Mark idle in the store immediately so listing shows Idle status.
                {
                    let mut store = session_store.lock().await;
                    store.mark_idle(&session_id);
                    let _ = store.save();
                }

                // If no client attached, exit the loop entirely.
                let has_clients = {
                    let smap = subscriber_map.lock().expect("bug: mutex poisoned");
                    smap.values().any(|subs| subs.lock().expect("bug: mutex poisoned").iter().any(|tx| !tx.is_closed()))
                };
                if !has_clients {
                    tracing::info!("Exiting agent {} due to idle", session_id);
                    idle_exited = true;
                    break;
                } else {
                    tracing::info!("Agent {} is idle but client is still connected", session_id);
                }
                // Client still attached — keep running.
            }
        }
    }
    tracing::info!("Idle loop exited, cleaning up RAP servers");

    // Gracefully kill RAP servers.
    for child in spawned_servers.iter_mut() {
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;

            let child_id = child.id();
            tracing::trace!(child_id, "Killing RAP server, sending SIGINT");
            if let Some(id) = child_id {
                // Negative ID kills the entire process group.
                let _ = signal::kill(Pid::from_raw(-(id as i32)), Signal::SIGINT);
            }
        }
        #[cfg(not(unix))]
        {
            tracing::trace!("Killing RAP server");
            let _ = child.start_kill();
        }
        let _ = child.wait().await;
        tracing::trace!("Child exited");
    }

    // Mark session store.
    let mut store = session_store.lock().await;
    if !idle_exited {
        store.mark_shut_down(&session_id);
    }
    let _ = store.save();

    tracing::info!("Done");
}

async fn spawn_rap_server(
    command: &str,
    cwd: &Path,
) -> Result<(tokio::process::Child, u16), BoxError> {
    use infinity_agent_core::tools::config::CommandServerReady;
    use std::process::Stdio;
    use tokio::io::AsyncBufReadExt;

    let working_dir = cwd.join(".infinity");
    std::fs::create_dir_all(&working_dir).ok();

    let mut child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .env("RAP_EMBEDDED", "1")
        .current_dir(&working_dir)
        // Ensure all children are in the the same process group. We will send SIGINT to the entire
        // group during shutdown.
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn '{command}': {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("failed to read startup line: {e}"))?;

    if line.is_empty() {
        let _ = child.kill().await;
        return Err("server exited before emitting port".into());
    }

    let ready: CommandServerReady = serde_json::from_str(line.trim())
        .map_err(|e| format!("invalid startup JSON: {e} (got: {line})"))?;
    Ok((child, ready.port))
}

/// Result of booting RAP servers.
pub struct BootedRapServers {
    /// config_id → port for each server (only servers with an ID).
    pub server_ports: HashMap<String, u16>,
    /// URL → config_id mapping for servers that have an ID.
    pub server_ids: HashMap<String, String>,
    /// Spawned child processes (command-based servers only; MCP proxies are managed internally).
    pub spawned_servers: Vec<tokio::process::Child>,
    /// All server URLs (including pre-existing toolset_server URLs from config).
    pub urls: Vec<String>,
}

/// Boot RAP servers at the given cwd using local + user config.
/// The `emit` callback streams individual progress messages as servers launch.
pub async fn boot_rap_servers(
    cwd: &Path,
    emit: &mut impl AsyncFnMut(String),
) -> Result<BootedRapServers, BoxError> {
    let cwd_rap = cwd.join(".infinity").join("rap.json");
    let local_config = cwd_rap
        .exists()
        .then(|| config::load_config(&cwd_rap))
        .transpose()?;
    let user_config = config::user_config_path()
        .and_then(|p| p.exists().then(|| config::load_config(&p)).transpose())?;

    let config = match (local_config, user_config) {
        (None, None) => {
            emit("Neither local nor user RAP configs exist, using empty config".into()).await;
            return Ok(BootedRapServers {
                server_ports: HashMap::new(),
                server_ids: HashMap::new(),
                spawned_servers: Vec::new(),
                urls: Vec::new(),
            });
        }
        (None, Some(c)) => {
            emit("Using user config".into()).await;
            c
        }
        (Some(c), None) => {
            emit("Using local config".into()).await;
            c
        }
        (Some(mut l), Some(u)) => {
            emit("Both local and user RAP configs exist, merging".into()).await;
            l.merge(u);
            l
        }
    };

    let mut server_ports = HashMap::new();
    let mut server_ids = HashMap::new();
    let mut spawned_servers = Vec::new();
    let mut urls: Vec<String> = Vec::new();

    for (server_url, id) in config.toolset_server_urls() {
        urls.push(server_url.clone());
        if let Some(id) = id {
            server_ids.insert(server_url, id);
        }
    }

    for (cmd, id) in config.toolset_commands() {
        emit(format!("Launching RAP server: {cmd}")).await;
        match spawn_rap_server(&cmd, cwd).await {
            Ok((child, port)) => {
                emit(format!("RAP server ready on port {port}")).await;
                let url = format!("http://127.0.0.1:{port}");
                if let Some(id) = id {
                    server_ports.insert(id.clone(), port);
                    server_ids.insert(url.clone(), id);
                }
                urls.push(url);
                spawned_servers.push(child);
            }
            Err(e) => {
                emit(format!("Warning: failed to launch RAP server '{cmd}': {e}")).await;
            }
        }
    }
    for (name, cmd, env, id) in config.mcp_servers() {
        emit(format!("Starting MCP proxy for '{name}'")).await;
        match mcp_proxy::start_mcp_proxy(name.clone(), cmd, env).await {
            Ok(port) => {
                emit(format!("MCP proxy '{name}' ready on port {port}")).await;
                let url = format!("http://127.0.0.1:{port}");
                if let Some(id) = id {
                    server_ports.insert(id.clone(), port);
                    server_ids.insert(url.clone(), id);
                }
                urls.push(url);
            }
            Err(e) => {
                emit(format!("Warning: failed to start MCP proxy '{name}': {e}")).await;
            }
        }
    }
    for (name, mcp_url, headers, id) in config.http_mcp_servers() {
        emit(format!("Starting HTTP MCP proxy for '{name}'")).await;
        match mcp_proxy::start_http_mcp_proxy(name.clone(), mcp_url, headers).await {
            Ok(port) => {
                emit(format!("HTTP MCP proxy '{name}' ready on port {port}")).await;
                let url = format!("http://127.0.0.1:{port}");
                if let Some(id) = id {
                    server_ports.insert(id.clone(), port);
                    server_ids.insert(url.clone(), id);
                }
                urls.push(url);
            }
            Err(e) => {
                emit(format!(
                    "Warning: failed to start HTTP MCP proxy '{name}': {e}"
                ))
                .await;
            }
        }
    }

    Ok(BootedRapServers {
        server_ports,
        server_ids,
        spawned_servers,
        urls,
    })
}
