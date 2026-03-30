use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use infinity_agent_core::message::InputMessage;
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::config::ToolsConfig;
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::thread::{
    CloseThreadTool, ReportToParentTool, SendMessageToChildTool, SpawnThreadTool,
};
use infinity_protocol::{DaemonMessage, SessionInfo};
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::CompletionModel;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config;
use crate::mcp_proxy;
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use crate::model_picker::{self, ModelProvider};
use crate::rap_tools;
use crate::session_store;
use crate::set_title_tool;
use crate::sleep_tools::{SleepTool, SleepUntilTool};
use infinity_agent_core::traits::ConversationStore;

pub mod agent_loop;
pub mod display;
pub mod thread_worker;

pub use agent_loop::agent_loop;
use display::history_message_to_daemon;
pub use thread_worker::thread_worker;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Shared handle to the currently-attached client sender for a session.
/// The display bridge reads from this; attach/detach_client writes to it.
pub type ClientTxHandle = Arc<std::sync::Mutex<Option<mpsc::UnboundedSender<DaemonMessage>>>>;

/// Tracks which thread group_ids have a running worker task.
pub type ActiveWorkers = Arc<std::sync::Mutex<HashSet<String>>>;

/// A single agent session managed by the daemon.
/// The session_id doubles as the root thread_id.
pub struct Session {
    pub session_id: String,
    pub cwd: PathBuf,
    pub client_tx_handle: ClientTxHandle,
    pub input_tx: mpsc::UnboundedSender<(InputMessage, String)>,
    pub agent_task: JoinHandle<()>,
    pub total_tokens_used: usize,
    pub model_name: String,
    pub context_window: usize,
    /// Set of group_ids with running thread workers. Empty means idle.
    pub active_workers: ActiveWorkers,
    /// Send to signal the agent loop to shut down.
    pub shutdown_tx: Option<oneshot::Sender<()>>,
}

pub type SessionStoreHandle = Arc<tokio::sync::Mutex<session_store::SessionStore>>;

/// Shared map of session_id → ActiveWorkers for determining session status.
pub type SessionWorkersMap = Arc<std::sync::Mutex<HashMap<String, ActiveWorkers>>>;

/// Manages all active sessions.
pub struct SessionManager {
    pub sessions: HashMap<String, Session>,
    /// Base URL for the RAP callback server.
    pub callback_url: String,
    pub session_store: SessionStoreHandle,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    pub default_model_name: String,
    pub default_context_window: usize,
    pub available_models: Vec<model_picker::ModelEntry>,
    /// Connected clients that receive broadcast updates.
    broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>>,
}

/// Shared routing table for callback messages.
pub type RouteMap =
    Arc<std::sync::Mutex<HashMap<String, mpsc::UnboundedSender<(InputMessage, String)>>>>;

impl SessionManager {
    pub async fn new(
        state_dir: std::path::PathBuf,
        callback_url: String,
    ) -> Result<Self, BoxError> {
        std::fs::create_dir_all(&state_dir).ok();
        let sessions_path = state_dir.join("sessions.json");
        let (change_tx, mut change_rx) = mpsc::unbounded_channel::<String>();
        let session_store = Arc::new(tokio::sync::Mutex::new(session_store::SessionStore::load(
            &sessions_path.to_string_lossy(),
            change_tx,
        )));
        let broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        // Task: listen for session store changes and broadcast to clients
        let bc = broadcast_clients.clone();
        let ss = session_store.clone();
        tokio::task::spawn_local(async move {
            while let Some(session_id) = change_rx.recv().await {
                let store = ss.lock().await;
                let info = match store.sessions.get(&session_id) {
                    Some(e) => SessionInfo {
                        title: e.title.clone(),
                        last_updated: e.last_updated.clone(),
                        total_tokens_used: e.total_tokens_used,
                        status: e.status(),
                    },
                    None => continue,
                };
                drop(store);
                let mut sessions = std::collections::HashMap::new();
                sessions.insert(session_id, info);
                let msg = DaemonMessage::SessionsUpdated { sessions };
                bc.lock().unwrap().retain(|tx| tx.send(msg.clone()).is_ok());
            }
        });

        let threads_dir = state_dir.join("threads");
        std::fs::create_dir_all(&threads_dir).ok();
        let conversation_store = InMemoryConversationStore::new_with_dir(&threads_dir);
        let state_store = InMemoryStateStore::new(state_dir.join("state"));

        // Detect model provider
        let provider = model_picker::BedrockProvider;
        let models = provider.available_models();
        let idx = provider.default_model_index();
        let (default_model_name, default_context_window, available_models) = (
            models[idx].display_name.clone(),
            models[idx].context_window,
            models,
        );

        Ok(Self {
            sessions: HashMap::new(),
            callback_url,
            session_store,
            conversation_store,
            state_store,
            default_model_name,
            default_context_window,
            available_models,
            broadcast_clients,
        })
    }

    /// Create a brand new session with the given working directory.
    pub async fn create_session(
        &mut self,
        cwd: &Path,
        client_tx: mpsc::UnboundedSender<DaemonMessage>,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<String, BoxError> {
        let session_id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = self.session_store.lock().await;
            store.create(&session_id, cwd.to_path_buf());
            let _ = store.save();
        }
        emit(self.build_connected(&session_id).await).await;
        self.start_session(session_id.clone(), cwd, client_tx, emit)
            .await?;
        Ok(session_id)
    }

    /// Resume a persisted session, recovering its cwd from the session store.
    /// If the session was previously shut down, this only sends the Connected
    /// message and replays history — the agent loop is NOT started until user
    /// input arrives via `send_input`.
    pub async fn resume_session(
        &mut self,
        session_id: &str,
        client_tx: mpsc::UnboundedSender<DaemonMessage>,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<(), BoxError> {
        emit(self.build_connected(session_id).await).await;

        // Check if session is alive (in map with running task).
        let is_alive = self
            .sessions
            .get(session_id)
            .is_some_and(|s| !s.agent_task.is_finished());

        if is_alive {
            return Ok(());
        }

        // Session not alive — check if shut down or idle-cleaned.
        let store = self.session_store.lock().await;
        let is_stopped = store.is_shut_down(session_id) || store.is_idle_cleaned(session_id);
        drop(store);
        if is_stopped {
            self.send_replay(session_id, &client_tx).await;
            Ok(())
        } else {
            let cwd = self.session_store.lock().await.get_cwd(session_id).clone();
            self.start_session(session_id.to_string(), &cwd, client_tx, emit)
                .await
        }
    }

    /// Send history replay to a client, including any pending choices from the store.
    async fn send_replay(&self, session_id: &str, tx: &mpsc::UnboundedSender<DaemonMessage>) {
        if let Ok((history, _)) = self
            .conversation_store
            .load_history_with_ancestors(session_id)
            .await
        {
            let history: Vec<DaemonMessage> = history
                .iter()
                .filter_map(|m| history_message_to_daemon(m, session_id, &self.conversation_store))
                .collect();
            let store = self.session_store.lock().await;
            let choices: Vec<DaemonMessage> = store
                .sessions
                .get(session_id)
                .map(|e| {
                    e.pending_choices
                        .iter()
                        .map(|c| c.message.clone())
                        .collect()
                })
                .unwrap_or_default();
            drop(store);
            if !history.is_empty() || !choices.is_empty() {
                let _ = tx.send(DaemonMessage::Replay {
                    history,
                    pending_choices: choices,
                });
            }
        }
    }

    async fn build_connected(&self, session_id: &str) -> DaemonMessage {
        let store = self.session_store.lock().await;
        let entry = store.sessions.get(session_id);
        DaemonMessage::Connected {
            session_id: session_id.to_string(),
            model_name: self.default_model_name.clone(),
            context_window: self.default_context_window,
            title: entry.and_then(|e| e.title.clone()),
            total_tokens_used: entry.map(|e| e.total_tokens_used).unwrap_or(0),
        }
    }

    /// Internal: spin up the agent loop for a session.
    /// If `client_tx` is provided, it's attached immediately so info events flow during setup.
    async fn start_session(
        &mut self,
        session_id: String,
        cwd: &Path,
        client_tx: mpsc::UnboundedSender<DaemonMessage>,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> Result<(), BoxError> {
        if self.sessions.contains_key(&session_id) {
            return Ok(());
        }

        let client_tx_handle: ClientTxHandle = Arc::new(std::sync::Mutex::new(Some(client_tx)));

        let (input_tx, input_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
        let sender = InMemoryMessageSender::new(input_tx.clone());

        // Load RAP config
        let cwd_rap = Path::new(&cwd).join(".infinity").join("rap.json");
        let local_config = cwd_rap
            .exists()
            .then(|| config::load_config(&cwd_rap))
            .transpose();
        let user_config = config::user_config_path().and_then(|user_path| {
            user_path
                .exists()
                .then(|| config::load_config(&user_path))
                .transpose()
        });
        tracing::debug!(?local_config, ?user_config);

        let local_config = match local_config {
            Ok(config) => config,
            Err(e) => {
                emit(DaemonMessage::Error(format!(
                    "Failed to load local RAP config {}: {e}",
                    cwd_rap.display(),
                )))
                .await;
                None
            }
        };
        let user_config = match user_config {
            Ok(config) => config,
            Err(e) => {
                emit(DaemonMessage::Error(format!(
                    "Failed to load user RAP config {:?}: {e}",
                    config::user_config_path()
                )))
                .await;
                None
            }
        };

        let (config, msg) = match (local_config, user_config) {
            (None, None) => (
                ToolsConfig::empty(),
                "Neither local nor user RAP configs exist, using empty config",
            ),
            (None, Some(user_config)) => (user_config, "Using user config"),
            (Some(local_config), None) => (local_config, "Using local config"),
            (Some(mut local_config), Some(user_config)) => {
                local_config.merge(user_config);
                (
                    local_config,
                    "Both local and user RAP configs exist, merging",
                )
            }
        };
        // Notify user about configuration discovery.
        emit(DaemonMessage::Info(msg.to_string())).await;

        // Spawn servers and load tools
        let mut spawned_servers = Vec::new();
        let mut urls = config.toolset_server_urls();

        for cmd in config.toolset_commands() {
            emit(DaemonMessage::Info(format!("Launching RAP server: {cmd}"))).await;
            match spawn_rap_server(&cmd, cwd).await {
                Ok((child, port)) => {
                    emit(DaemonMessage::Info(format!(
                        "RAP server ready on port {port}"
                    )))
                    .await;
                    urls.push(format!("http://127.0.0.1:{port}"));
                    spawned_servers.push(child);
                }
                Err(e) => {
                    emit(DaemonMessage::Info(format!(
                        "Warning: failed to launch RAP server '{cmd}': {e}"
                    )))
                    .await
                }
            }
        }
        for (name, cmd, env) in config.mcp_servers() {
            emit(DaemonMessage::Info(format!(
                "Starting MCP proxy for '{name}'"
            )))
            .await;
            match mcp_proxy::start_mcp_proxy(name.clone(), cmd, env).await {
                Ok(port) => {
                    emit(DaemonMessage::Info(format!(
                        "MCP proxy '{name}' ready on port {port}"
                    )))
                    .await;
                    urls.push(format!("http://127.0.0.1:{port}"));
                }
                Err(e) => {
                    emit(DaemonMessage::Info(format!(
                        "Warning: failed to start MCP proxy '{name}': {e}"
                    )))
                    .await
                }
            }
        }
        for (name, mcp_url, headers) in config.http_mcp_servers() {
            emit(DaemonMessage::Info(format!(
                "Starting HTTP MCP proxy for '{name}'"
            )))
            .await;
            match mcp_proxy::start_http_mcp_proxy(name.clone(), mcp_url, headers).await {
                Ok(port) => {
                    emit(DaemonMessage::Info(format!(
                        "HTTP MCP proxy '{name}' ready on port {port}"
                    )))
                    .await;
                    urls.push(format!("http://127.0.0.1:{port}"));
                }
                Err(e) => {
                    emit(DaemonMessage::Info(format!(
                        "Warning: failed to start HTTP MCP proxy '{name}': {e}"
                    )))
                    .await
                }
            }
        }

        let rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>> = if !urls.is_empty() {
            match rap_tools::load_rap_tools(&urls).await {
                Ok(tools) => {
                    emit(DaemonMessage::Info(format!(
                        "Loaded {} RAP tool(s)",
                        tools.len()
                    )))
                    .await;

                    emit(DaemonMessage::Info("".to_string())).await;

                    tools
                }
                Err(e) => {
                    emit(DaemonMessage::Info(format!(
                        "Warning: failed to load RAP tools: {e}"
                    )))
                    .await;
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

        let active_workers: ActiveWorkers = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let (model_name, context_window, agent_handle) = self
            .start_agent_loop(
                session_id.clone(),
                input_rx,
                self.conversation_store.clone(),
                state_store,
                sender,
                self.callback_url.clone(),
                rap_tools,
                urls,
                extra_system_prompt,
                client_tx_handle.clone(),
                active_workers.clone(),
                idle_tx,
                idle_rx,
                shutdown_rx,
                spawned_servers,
            )
            .await?;

        let session = Session {
            session_id: session_id.clone(),
            cwd: cwd.to_path_buf(),
            client_tx_handle,
            input_tx,
            agent_task: agent_handle,
            total_tokens_used: 0,
            model_name,
            context_window,
            active_workers: active_workers.clone(),
            shutdown_tx: Some(shutdown_tx),
        };
        self.sessions.insert(session_id.clone(), session);
        Ok(())
    }

    /// Attach a client's message sender to a session for receiving display events.
    pub async fn attach_client(
        &mut self,
        session_id: &str,
        tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> bool {
        if !self.sessions.contains_key(session_id) {
            return false;
        }
        self.send_replay(session_id, &tx).await;
        if let Some(session) = self.sessions.get_mut(session_id) {
            *session.client_tx_handle.lock().unwrap() = Some(tx);
        }
        true
    }

    /// Detach the client from a session (session keeps running headless).
    pub fn detach_client(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            *session.client_tx_handle.lock().unwrap() = None;
        }
    }

    /// Send user input text to a session's agent loop.
    /// If the session was shut down and no agent loop is running, this
    /// clears the shut_down flag and starts a new agent loop first.
    pub async fn send_input(
        &mut self,
        session_id: &str,
        msg: (InputMessage, Option<String>),
        client_tx: Option<mpsc::UnboundedSender<DaemonMessage>>,
        emit: &mut impl AsyncFnMut(DaemonMessage),
    ) -> bool {
        // Resolve thread ID to root session ID in case a child thread ID was provided.
        let session_id = self.conversation_store.get_root_thread_id(session_id);
        let session_id = session_id.as_str();

        // If session task finished (idle-cleaned) or was never started, check if we need to restart.
        let needs_restart = if let Some(session) = self.sessions.get(session_id) {
            session.agent_task.is_finished()
        } else {
            let store = self.session_store.lock().await;
            store.is_shut_down(session_id) || store.is_idle_cleaned(session_id)
        };

        // if client_tx is None, that means this is for a RAP callback, but if the agent was shut down or idled,
        // we should ignore the callback (the agent will not idle if any tool calls or subscriptions are active)
        if needs_restart && let Some(client_tx) = client_tx {
            // Remove stale session if present.
            self.sessions.remove(session_id);
            {
                let mut store = self.session_store.lock().await;
                store.clear_shut_down(session_id);
                let _ = store.save();
            }
            let cwd = self.session_store.lock().await.get_cwd(session_id).clone();
            if let Err(e) = self
                .start_session(session_id.to_string(), &cwd, client_tx, emit)
                .await
            {
                tracing::error!("failed to restart session: {e}");
                return false;
            }
        }

        if let Some(session) = self.sessions.get(session_id) {
            session
                .input_tx
                .send((
                    msg.0,
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
    ) -> std::collections::HashMap<String, SessionInfo> {
        if let Some(tx) = subscribe {
            self.broadcast_clients.lock().unwrap().push(tx);
        }

        let store = self.session_store.lock().await;
        let mut result: std::collections::HashMap<String, SessionInfo> =
            std::collections::HashMap::new();

        for (id, entry) in &store.sessions {
            result.insert(
                id.clone(),
                SessionInfo {
                    title: entry.title.clone(),
                    last_updated: entry.last_updated.clone(),
                    total_tokens_used: entry.total_tokens_used,
                    status: entry.status(),
                },
            );
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
            let _ = session.agent_task.await;

            // Ensure shut_down is set (task may have already finished as idle_cleaned).
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
                    || session.active_workers.lock().unwrap().is_empty()
            }
            None => true,
        }
    }

    /// Start the agent loop for a session. Returns (model_name, context_window).
    async fn start_agent_loop(
        &self,
        session_id: String,
        input_rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
        conversation_store: InMemoryConversationStore,
        state_store: InMemoryStateStore,
        sender: InMemoryMessageSender,
        callback_url: String,
        rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
        tool_server_urls: Vec<String>,
        extra_system_prompt: Option<String>,
        client_tx_handle: ClientTxHandle,
        active_workers: ActiveWorkers,
        idle_tx: mpsc::UnboundedSender<()>,
        idle_rx: mpsc::UnboundedReceiver<()>,
        shutdown_rx: oneshot::Receiver<()>,
        spawned_servers: Vec<tokio::process::Child>,
    ) -> Result<(String, usize, JoinHandle<()>), BoxError> {
        let default = &self.available_models[0]; // already validated in new()
        let model_name = default.display_name.clone();
        let context_window = default.context_window;

        let handle = {
            let client = rig_bedrock::client::Client::from_env();
            let model = client.completion_model(&default.model_id);

            spawn_agent_loop(
                session_id,
                self.session_store.clone(),
                input_rx,
                model,
                conversation_store,
                state_store,
                sender,
                callback_url,
                rap_tools,
                tool_server_urls,
                extra_system_prompt,
                default.additional_request_params.clone(),
                client_tx_handle,
                active_workers,
                idle_tx,
                idle_rx,
                shutdown_rx,
                spawned_servers,
                context_window,
            )
        };
        Ok((model_name, context_window, handle))
    }
}

// ── Agent loop (mirrors CLI main.rs agent_loop/thread_worker) ───────────────

fn spawn_agent_loop<Mdl>(
    session_id: String,
    session_store: SessionStoreHandle,
    input_rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    model: Mdl,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    rap_tools: Vec<Box<dyn Tool<InMemoryMessageSender>>>,
    tool_server_urls: Vec<String>,
    extra_system_prompt: Option<String>,
    additional_params: Option<serde_json::Value>,
    client_tx_handle: ClientTxHandle,
    active_workers: ActiveWorkers,
    idle_tx: mpsc::UnboundedSender<()>,
    idle_rx: mpsc::UnboundedReceiver<()>,
    shutdown_rx: oneshot::Receiver<()>,
    spawned_servers: Vec<tokio::process::Child>,
    context_window: usize,
) -> JoinHandle<()>
where
    Mdl: CompletionModel + 'static,
{
    let additional_request_params = Arc::new(std::sync::RwLock::new(additional_params));
    let active_model_id: Arc<std::sync::RwLock<Option<String>>> =
        Arc::new(std::sync::RwLock::new(None));

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
            session_store: session_store.clone(),
        }),
    ];
    tool_impls.extend(rap_tools);

    let tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>> = Arc::new(tool_impls);
    let model = Arc::new(model);
    let extra_system_prompt = Arc::new(extra_system_prompt);

    let agent_fut = agent_loop(
        session_id.clone(),
        session_store.clone(),
        input_rx,
        model,
        conversation_store,
        state_store,
        sender,
        callback_url,
        tool_impls,
        extra_system_prompt,
        rap_notifier,
        additional_request_params,
        active_model_id,
        client_tx_handle.clone(),
        active_workers,
        idle_tx,
        context_window,
    );

    tokio::task::spawn_local(session_wrapper(
        agent_fut,
        session_id,
        session_store,
        client_tx_handle,
        idle_rx,
        shutdown_rx,
        spawned_servers,
    ))
}

/// Wrapper that owns the RAP servers and handles cleanup.
/// Runs agent_loop, selects on shutdown signal and idle notifications.
/// When done, gracefully kills servers and marks the session store.
#[expect(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, fields(session_id))]
async fn session_wrapper(
    agent_fut: impl Future<Output = ()>,
    session_id: String,
    session_store: SessionStoreHandle,
    client_tx_handle: ClientTxHandle,
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
                while idle_rx.recv().await.is_some() {
                    break;
                }
                idle_exited = client_tx_handle.lock().unwrap().is_none();
                break;
            }
            _ = &mut shutdown_rx => {
                tracing::info!("Received `shutdown_rx`.");
                if let Some(entry) = session_store.lock().await.sessions.get_mut(&session_id) {
                    entry.pending_choices.clear();
                }
                idle_exited = false;
                break;
            }
            _ = idle_rx.recv() => {
                // All workers idle. If no client attached, exit.
                if client_tx_handle.lock().unwrap().is_none() {
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
    if idle_exited {
        store.mark_idle_cleaned(&session_id);
    } else {
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
