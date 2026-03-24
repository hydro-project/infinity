use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use infinity_agent_core::batch_processor::DisplayEvent;
use infinity_agent_core::event_processor;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::tools::config::ToolsConfig;
use infinity_agent_core::tools::sleep::SleepUntilEventOrInputTool;
use infinity_agent_core::tools::thread::{
    CloseThreadTool, ReportToParentTool, SendMessageToChildTool, SpawnThreadTool,
};
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_protocol::{DaemonMessage, SessionInfo, TokenUsage};
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::{CompletionModel, GetTokenUsage};
use rig::message::{ToolResultContent, UserContent};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config;
use crate::mcp_proxy;
use crate::memory_store::{InMemoryConversationStore, InMemoryMessageSender, InMemoryStateStore};
use crate::model_picker::{self, ModelProvider};
use crate::rap_callback;
use crate::rap_tools;
use crate::session_store;
use crate::set_title_tool;
use crate::sleep_tools::{SleepTool, SleepUntilTool};
use infinity_agent_core::traits::{ConversationStore, StateStore};

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
    pub spawned_servers: Vec<tokio::process::Child>,
    pub agent_task: Option<JoinHandle<()>>,
    pub total_tokens_used: usize,
    pub model_name: String,
    pub context_window: usize,
    /// Set of group_ids with running thread workers. Empty means idle.
    pub active_workers: ActiveWorkers,
    /// Receives a notification whenever all thread workers have exited (session becomes idle).
    pub idle_rx: mpsc::UnboundedReceiver<()>,
}

pub type SessionStoreHandle = Arc<tokio::sync::Mutex<session_store::SessionStore>>;

/// Shared map of session_id → ActiveWorkers for determining session status.
pub type SessionWorkersMap = Arc<std::sync::Mutex<HashMap<String, ActiveWorkers>>>;

/// Manages all active sessions.
pub struct SessionManager {
    pub sessions: HashMap<String, Session>,
    callback_url: String,
    route_map: Option<RouteMap>,
    session_store: SessionStoreHandle,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    pub default_model_name: String,
    pub default_context_window: usize,
    pub available_models: Vec<model_picker::ModelEntry>,
    /// Connected clients that receive broadcast updates.
    broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>>,
    /// Shared map for broadcast task to determine session status.
    session_workers: SessionWorkersMap,
}

/// Shared routing table for callback messages.
pub type RouteMap =
    Arc<std::sync::Mutex<HashMap<String, mpsc::UnboundedSender<(InputMessage, String)>>>>;

impl SessionManager {
    pub async fn new(state_dir: std::path::PathBuf) -> Result<Self, BoxError> {
        std::fs::create_dir_all(&state_dir).ok();
        let sessions_path = state_dir.join("sessions.json");
        let (change_tx, mut change_rx) = mpsc::unbounded_channel::<String>();
        let session_store = Arc::new(tokio::sync::Mutex::new(session_store::SessionStore::load(
            &sessions_path.to_string_lossy(),
            change_tx,
        )));
        let broadcast_clients: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<DaemonMessage>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let session_workers: SessionWorkersMap = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // Task: listen for session store changes and broadcast to clients
        let bc = broadcast_clients.clone();
        let ss = session_store.clone();
        let sw = session_workers.clone();
        tokio::task::spawn_local(async move {
            while let Some(session_id) = change_rx.recv().await {
                let store = ss.lock().await;
                let info = match store.sessions.get(&session_id) {
                    Some(e) => SessionInfo {
                        title: e.title.clone(),
                        last_updated: e.last_updated.clone(),
                        total_tokens_used: e.total_tokens_used,
                        status: session_status(&sw, &session_id, e.shut_down),
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

        // Start shared callback server
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<(InputMessage, String)>();
        let callback_url = rap_callback::start_callback_server(input_tx.clone())
            .await
            .map_err(|e| format!("Failed to start callback server: {e}"))?;
        tracing::info!("shared callback server started");

        // Router task
        let routes: RouteMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let routes_clone = routes.clone();
        tokio::task::spawn_local(async move {
            while let Some((msg, dedup)) = input_rx.recv().await {
                let group_id = msg.group_id.clone();
                let routes = routes_clone.lock().unwrap();
                if let Some(tx) = routes.get(&group_id) {
                    let _ = tx.send((msg, dedup));
                } else {
                    for tx in routes.values() {
                        let _ = tx.send((msg.clone(), dedup.clone()));
                    }
                }
            }
        });

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
            route_map: Some(routes),
            session_store,
            conversation_store,
            state_store,
            default_model_name,
            default_context_window,
            available_models,
            broadcast_clients,
            session_workers,
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
        if self.sessions.contains_key(session_id) {
            Ok(())
        } else if self.session_store.lock().await.is_shut_down(session_id) {
            // Session was shut down — send replay but don't start agent loop.
            self.send_replay(session_id, &client_tx).await;
            Ok(())
        } else {
            let cwd = self.session_store.lock().await.get_cwd(session_id).clone();
            self.start_session(session_id.to_string(), &cwd, client_tx, emit)
                .await
        }
    }

    /// Send history replay to a client without requiring a running Session.
    async fn send_replay(&self, session_id: &str, tx: &mpsc::UnboundedSender<DaemonMessage>) {
        if let Ok((history, _)) = self
            .conversation_store
            .load_history_with_ancestors(session_id)
            .await
        {
            let msgs: Vec<DaemonMessage> = history
                .iter()
                .filter_map(|m| history_message_to_daemon(m, session_id, &self.conversation_store))
                .collect();
            if !msgs.is_empty() {
                let _ = tx.send(DaemonMessage::Replay(msgs));
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

        if let Some(ref route_map) = self.route_map {
            route_map
                .lock()
                .unwrap()
                .insert(session_id.clone(), input_tx.clone());
        }

        // Load RAP config
        let cwd_rap = Path::new(&cwd).join(".infinity").join("rap.json");
        let mut config = if cwd_rap.exists() {
            config::load_config(&cwd_rap)
        } else {
            ToolsConfig::empty()
        };
        if let Ok(user_path) = config::user_config_path()
            && user_path.exists()
        {
            config.merge(config::load_config(&user_path));
            emit(DaemonMessage::Info(format!(
                "Merged user config from {}",
                user_path.display()
            )))
            .await;
        }

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
            )
            .await?;

        let session = Session {
            session_id: session_id.clone(),
            cwd: cwd.to_path_buf(),
            client_tx_handle,
            input_tx,
            spawned_servers,
            agent_task: Some(agent_handle),
            total_tokens_used: 0,
            model_name,
            context_window,
            active_workers: active_workers.clone(),
            idle_rx,
        };
        self.session_workers
            .lock()
            .unwrap()
            .insert(session_id.clone(), active_workers);
        self.sessions.insert(session_id.clone(), session);
        Ok(())
    }

    /// Attach a client's message sender to a session for receiving display events.
    pub async fn attach_client(
        &mut self,
        session_id: &str,
        tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> bool {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if let Ok((history, _)) = self
                .conversation_store
                .load_history_with_ancestors(&session.session_id)
                .await
            {
                let msgs: Vec<DaemonMessage> = history
                    .iter()
                    .filter_map(|m| {
                        history_message_to_daemon(m, session_id, &self.conversation_store)
                    })
                    .collect();
                if !msgs.is_empty() {
                    let _ = tx.send(DaemonMessage::Replay(msgs));
                }
            }
            *session.client_tx_handle.lock().unwrap() = Some(tx);
            true
        } else {
            false
        }
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
        text: String,
        client_tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> bool {
        // If session isn't running but was shut down, restart it on user input.
        if !self.sessions.contains_key(session_id)
            && self.session_store.lock().await.is_shut_down(session_id)
        {
            {
                let mut store = self.session_store.lock().await;
                store.clear_shut_down(session_id);
                let _ = store.save();
            }
            let cwd = self.session_store.lock().await.get_cwd(session_id).clone();
            let mut emit = async |_msg: DaemonMessage| {};
            if let Err(e) = self
                .start_session(session_id.to_string(), &cwd, client_tx, &mut emit)
                .await
            {
                tracing::error!("failed to restart shut-down session: {e}");
                return false;
            }
        }

        if let Some(session) = self.sessions.get(session_id) {
            let msg = InputMessage {
                content: InputMessageContent::User(UserContent::text(&text)),
                group_id: session.session_id.clone(),
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            };
            session
                .input_tx
                .send((msg, uuid::Uuid::new_v4().to_string()))
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
                    status: session_status(&self.session_workers, id, entry.shut_down),
                },
            );
        }

        result
    }

    /// Clean up a session: kill RAP servers, abort agent task.
    pub async fn cleanup_session(&mut self, session_id: &str) {
        // Mark as shut down so stale tool results don't re-awaken the agent.
        {
            let mut store = self.session_store.lock().await;
            store.mark_shut_down(session_id);
            let _ = store.save();
        }
        if let Some(mut session) = self.sessions.remove(session_id) {
            self.session_workers.lock().unwrap().remove(session_id);
            if let Some(ref route_map) = self.route_map {
                route_map.lock().unwrap().remove(&session.session_id);
            }
            for mut child in session.spawned_servers.drain(..) {
                #[cfg(unix)]
                {
                    use nix::sys::signal::{self, Signal};
                    use nix::unistd::Pid;
                    if let Some(id) = child.id() {
                        let _ = signal::kill(Pid::from_raw(id as i32), Signal::SIGINT);
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = child.start_kill();
                }
                let _ = child.wait().await;
            }
            if let Some(task) = session.agent_task.take() {
                task.abort();
            }
        }
    }

    /// Returns true if the session has no running thread workers.
    pub fn is_session_idle(&self, session_id: &str) -> bool {
        match self.sessions.get(session_id) {
            Some(session) => session.active_workers.lock().unwrap().is_empty(),
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
) -> JoinHandle<()>
where
    Mdl: CompletionModel + 'static,
{
    let additional_request_params = Arc::new(std::sync::RwLock::new(additional_params));
    let active_model_id: Arc<std::sync::RwLock<Option<String>>> =
        Arc::new(std::sync::RwLock::new(None));

    // The display_tx here is a dummy — we don't have a terminal.
    // Instead we use a channel that a bridge task reads from.
    // For now, the display events are just logged. The bridge is set up
    // when a client attaches via the Session's client_tx.
    // TODO: wire display bridge per-session

    let rap_notifier = if tool_server_urls.is_empty() {
        None
    } else {
        Some(infinity_agent_core::rap_notifier::RapNotifier::new(
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

    let handle = tokio::task::spawn_local(agent_loop(
        session_id,
        session_store,
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
        client_tx_handle,
        active_workers,
        idle_tx,
    ));
    handle
}

#[expect(clippy::too_many_arguments)]
async fn agent_loop<Mdl>(
    session_id: String,
    session_store: SessionStoreHandle,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    model: Arc<Mdl>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>>,
    extra_system_prompt: Arc<Option<String>>,
    rap_notifier: Option<
        infinity_agent_core::rap_notifier::RapNotifier<rap_tools::SimpleHttpClient>,
    >,
    additional_request_params: Arc<std::sync::RwLock<Option<serde_json::Value>>>,
    active_model_id: Arc<std::sync::RwLock<Option<String>>>,
    client_tx_handle: ClientTxHandle,
    active_workers: ActiveWorkers,
    idle_tx: mpsc::UnboundedSender<()>,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
    let (display_tx, mut display_rx) =
        mpsc::unbounded_channel::<DisplayEvent<Mdl::StreamingResponse>>();

    // Display bridge: convert DisplayEvent → DaemonMessage, forward to client, persist session
    let bridge_handle = client_tx_handle.clone();
    let bridge_session_id = session_id.clone();
    let bridge_session_store = session_store.clone();
    tokio::task::spawn_local(async move {
        while let Some(evt) = display_rx.recv().await {
            if let DisplayEvent::ResponseDone(ref prefix, ref r) = evt
                && prefix.is_none()
                && let Some(r) = r
            {
                let tokens = r.token_usage().map_or(0, |u| u.total_tokens as usize);
                let mut store = bridge_session_store.lock().await;
                store.update(&bridge_session_id, tokens, None);
                let _ = store.save();
            }

            if let Some(dm) = display_event_to_daemon(evt) {
                let guard = bridge_handle.lock().unwrap();
                if let Some(ref tx) = *guard {
                    let _ = tx.send(dm);
                }
            }
        }
    });

    let mut thread_txs: HashMap<String, mpsc::UnboundedSender<(InputMessage, String)>> =
        HashMap::new();

    while let Some((input_msg, message_id)) = rx.recv().await {
        let group_id = input_msg.group_id.clone();

        // Try to send to existing worker; if channel is closed (worker returned),
        // remove the stale entry so a new worker is spawned below.
        if let Some(tx) = thread_txs.get(&group_id) {
            if tx.send((input_msg.clone(), message_id.clone())).is_ok() {
                continue;
            }
            thread_txs.remove(&group_id);
        }

        let (tx, worker_rx) = mpsc::unbounded_channel();
        let aw = active_workers.clone();
        let gid = group_id.clone();
        let itx = idle_tx.clone();
        tokio::task::spawn_local(thread_worker(
            gid,
            worker_rx,
            display_tx.clone(),
            model.clone(),
            conversation_store.clone(),
            state_store.clone(),
            sender.clone(),
            callback_url.clone(),
            tool_impls.clone(),
            extra_system_prompt.as_ref().clone(),
            rap_notifier.clone(),
            additional_request_params.clone(),
            active_model_id.clone(),
            aw,
            itx,
        ));
        let _ = tx.send((input_msg, message_id));
        thread_txs.insert(group_id, tx);
    }
}

fn is_user_text_input(msg: &InputMessage) -> bool {
    msg.synthetic.is_none()
        && matches!(
            &msg.content,
            InputMessageContent::User(UserContent::Text(_))
        )
}

#[expect(clippy::too_many_arguments)]
async fn thread_worker<Mdl>(
    active_group_id: String,
    mut rx: mpsc::UnboundedReceiver<(InputMessage, String)>,
    display_tx: mpsc::UnboundedSender<DisplayEvent<Mdl::StreamingResponse>>,
    model: Arc<Mdl>,
    conversation_store: InMemoryConversationStore,
    state_store: InMemoryStateStore,
    sender: InMemoryMessageSender,
    callback_url: String,
    tool_impls: Arc<Vec<Box<dyn Tool<InMemoryMessageSender>>>>,
    extra_system_prompt: Option<String>,
    rap_notifier: Option<
        infinity_agent_core::rap_notifier::RapNotifier<rap_tools::SimpleHttpClient>,
    >,
    additional_request_params: Arc<std::sync::RwLock<Option<serde_json::Value>>>,
    active_model_id: Arc<std::sync::RwLock<Option<String>>>,
    active_workers: ActiveWorkers,
    idle_tx: mpsc::UnboundedSender<()>,
) where
    Mdl: CompletionModel + Send + Sync + 'static,
{
    active_workers
        .lock()
        .unwrap()
        .insert(active_group_id.clone());

    let _guard = WorkerGuard {
        active_workers: active_workers.clone(),
        group_id: active_group_id.clone(),
        idle_tx,
    };

    let current_history = std::cell::RefCell::new(
        match event_processor::HistoryManager::new_with_history(
            conversation_store.clone(),
            state_store.clone(),
            active_group_id.clone(),
        )
        .await
        {
            Ok(h) => h,
            Err(e) => {
                let _ = display_tx.send(DisplayEvent::Info(format!("Error: {e}")));
                return;
            }
        },
    );

    let tool_names: std::collections::HashSet<String> =
        tool_impls.iter().map(|t| t.name().to_string()).collect();
    let tool_defs: Vec<rig::completion::ToolDefinition> = tool_impls
        .iter()
        .map(|t| rig::completion::ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters(),
        })
        .collect();

    let tool_context = ToolContext {
        message_sender: sender.clone(),
        group_id: active_group_id.clone(),
        input_queue_arn: String::new(),
        callback_url,
        user_id: None,
        thread_stack: current_history.borrow().get_thread_stack(),
    };
    let tool_registry: std::collections::HashMap<String, &dyn Tool<InMemoryMessageSender>> =
        tool_impls
            .iter()
            .map(|t| (t.name().to_string(), t.as_ref()))
            .collect();

    let mut pending_non_interrupt_items = vec![];
    let mut completion_fut = None;
    let mut completion_cancel_tx: Option<oneshot::Sender<()>> = None;

    loop {
        let inputs_before_pending = if let Some(mut_fut) = completion_fut.as_mut() {
            tokio::select! {
                _ = mut_fut => {
                    // if the LLM completed first, simply loop back and collect a batch in the else branch
                    let _ = completion_fut.take().unwrap();
                    continue;
                },
                first = rx.recv() => {
                    let Some(first) = first else {
                        return;
                    };
                    // Drain all immediately-available events before running the LLM.
                    let mut batch = vec![first];
                    while let Ok(item) = rx.try_recv() {
                        batch.push(item);
                    }

                    if batch.iter().any(|(msg, _)| is_user_text_input(msg))
                    {
                        let _ = completion_cancel_tx.take().unwrap().send(());
                        let completion_fut_taken = completion_fut.take().unwrap();
                        completion_fut_taken.await;

                        let (mut user_inputs, non_user_inputs): (Vec<_>, Vec<_>) = batch
                            .into_iter()
                            .partition(|(msg, _)| is_user_text_input(msg));

                        if let InputMessageContent::User(UserContent::Text(text)) = &mut user_inputs[0].0.content {
                            text.text = format!("<interrupt>{}", text.text);
                        } else {
                            panic!("user_inputs should only have user text");
                        }

                        pending_non_interrupt_items.extend(non_user_inputs);
                        user_inputs
                    } else {
                        pending_non_interrupt_items.extend(batch);
                        // if nothing is an interrupt-causing event, simply loop back and continue waiting
                        // for a real interrupt or the LLM to complete naturally
                        continue;
                    }
                }
            }
        } else {
            let mut batch = vec![];

            if pending_non_interrupt_items.is_empty() {
                let first_res = rx.try_recv();
                let mut first = if let Ok(first_res) = first_res {
                    Some(first_res)
                } else {
                    let last_is_tool_call = {
                        let hist = current_history.borrow();

                        hist.history.last().is_some_and(|msg| matches!(
                            msg,
                            rig::message::Message::Assistant { content, .. }
                                if matches!(content.first(), rig::message::AssistantContent::ToolCall(_))
                        ))
                    };
                    let has_subs = state_store
                        .get_active_subscriptions(&active_group_id)
                        .await
                        .map(|s| !s.is_empty())
                        .unwrap_or(false);

                    if !last_is_tool_call && !has_subs {
                        // Idle — return so the worker can be respawned later.
                        tracing::info!("Thread {} going to idle", &active_group_id);
                        return;
                    } else {
                        None
                    }
                };

                // only block if there are no pending items
                if first.is_none() {
                    first = rx.recv().await;
                }
                let Some(first) = first else {
                    return;
                };
                batch.push(first);
            }

            while let Ok(item) = rx.try_recv() {
                batch.push(item);
            }

            batch
        };

        let all_inputs: Vec<_> = inputs_before_pending
            .into_iter()
            .chain(pending_non_interrupt_items.drain(..))
            .collect();

        for (m, _) in &all_inputs {
            if let (Some(da), InputMessageContent::User(UserContent::ToolResult(res))) =
                (&m.display_as, &m.content)
            {
                conversation_store.save_display_as(&active_group_id, &res.id, da);
            }
        }

        let params = additional_request_params.read().unwrap().clone();
        let mid = active_model_id.read().unwrap().clone();

        let result = infinity_agent_core::batch_processor::process_batch(
            all_inputs.into_iter(),
            &current_history,
            &conversation_store,
            &display_tx,
            &active_group_id,
            model.as_ref(),
            &tool_names,
            &tool_defs,
            &tool_registry,
            tool_context.clone(),
            &extra_system_prompt,
            params,
            mid,
            rap_notifier.as_ref(),
        )
        .await;

        if let Some((fut, ct)) = result {
            completion_cancel_tx = Some(ct);
            completion_fut = Some(fut);
        }
    }
}

/// RAII guard that removes the group_id from active_workers on drop.
/// When the set becomes empty, sends a notification on idle_tx.
struct WorkerGuard {
    active_workers: ActiveWorkers,
    group_id: String,
    idle_tx: mpsc::UnboundedSender<()>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut set = self.active_workers.lock().unwrap();
        set.remove(&self.group_id);
        if set.is_empty() {
            let _ = self.idle_tx.send(());
        }
    }
}

// ── Display bridge ──────────────────────────────────────────────────────────

fn display_event_to_daemon<R: GetTokenUsage>(evt: DisplayEvent<R>) -> Option<DaemonMessage> {
    Some(match evt {
        DisplayEvent::StartOutput { prefix } => DaemonMessage::StartOutput { prefix },
        DisplayEvent::TextChunk { prefix, chunk } => DaemonMessage::TextChunk { prefix, chunk },
        DisplayEvent::ToolCall {
            name,
            args,
            prefix,
            display_script,
        } => DaemonMessage::ToolCall {
            name,
            args: args.to_string(),
            prefix,
            display_script,
        },
        DisplayEvent::ToolResult {
            text,
            display_as,
            prefix,
        } => DaemonMessage::ToolResult {
            text,
            display_as,
            prefix,
        },
        DisplayEvent::Info(s) => DaemonMessage::Info(s),
        DisplayEvent::ResponseDone(thread_id, r) => {
            let token_usage = r.and_then(|r| r.token_usage()).map(|u| TokenUsage {
                input_tokens: Some(u.input_tokens),
                output_tokens: Some(u.output_tokens),
            });
            DaemonMessage::ResponseDone {
                thread_id,
                token_usage,
            }
        }
        DisplayEvent::UserInput(s) => DaemonMessage::UserInputEcho(s),
        DisplayEvent::SubscriptionEvent { name, text, prefix } => {
            DaemonMessage::SubscriptionEvent { name, text, prefix }
        }
        DisplayEvent::OAuthRequired { auth_url } => DaemonMessage::OAuthRequired { auth_url },
        DisplayEvent::ThinkingStart { prefix } => DaemonMessage::ThinkingStart { prefix },
        DisplayEvent::ThinkingEnd { prefix } => DaemonMessage::ThinkingEnd { prefix },
        DisplayEvent::ThinkingChunk { prefix, chunk } => {
            DaemonMessage::ThinkingChunk { prefix, chunk }
        }
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn session_status(
    workers_map: &SessionWorkersMap,
    session_id: &str,
    shut_down: bool,
) -> infinity_protocol::SessionStatus {
    use infinity_protocol::SessionStatus;
    match workers_map.lock().unwrap().get(session_id) {
        Some(aw) if !aw.lock().unwrap().is_empty() => SessionStatus::Running,
        Some(_) => SessionStatus::Idle,
        None if shut_down => SessionStatus::Stopped,
        None => SessionStatus::Stopped,
    }
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

fn history_message_to_daemon(
    msg: &rig::message::Message,
    tid: &str,
    store: &InMemoryConversationStore,
) -> Option<DaemonMessage> {
    use rig::message::{AssistantContent, Message};
    match msg {
        Message::User { content } => match content.first() {
            UserContent::Text(text) => Some(DaemonMessage::UserInputEcho(text.text.clone())),
            UserContent::ToolResult(res) => {
                if let ToolResultContent::Text(t) = res.content.first() {
                    let display_as = store.get_display_as(tid, &res.id);
                    Some(DaemonMessage::ToolResult {
                        text: t.to_string(),
                        display_as,
                        prefix: None,
                    })
                } else {
                    None
                }
            }
            _ => None,
        },
        Message::Assistant { content, .. } => match content.first() {
            AssistantContent::Text(text) => Some(DaemonMessage::TextChunk {
                prefix: None,
                chunk: text.text.clone(),
            }),
            AssistantContent::ToolCall(call) => Some(DaemonMessage::ToolCall {
                name: call.function.name.clone(),
                args: call.function.arguments.to_string(),
                prefix: None,
                display_script: None,
            }),
            _ => None,
        },
    }
}
