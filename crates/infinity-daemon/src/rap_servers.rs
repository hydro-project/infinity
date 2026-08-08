//! Lazy, daemon-managed RAP tool servers.
//!
//! The daemon runs one agent system for its whole lifetime; sessions are just
//! groups of threads sharing a working directory. What *does* start and stop
//! with session activity are the session's RAP tool servers. This module
//! makes that lifecycle lazy:
//!
//! - A session's toolset is resolved the first time one of its threads runs a
//!   step (via [`SessionRapManager`]'s [`ThreadConfigSource`] impl): the
//!   configured servers are booted, their manifests fetched, and one
//!   [`ManagedRapTool`] built per remote tool definition.
//! - When the session goes idle (no live thread drivers, no keep-alive
//!   client), the daemon calls [`SessionRapManager::shutdown_session`], which
//!   stops the server processes but keeps the cached tool definitions.
//! - The next tool invocation boots the server back up transparently
//!   ([`ManagedRapServer::invoke_endpoint`]), so shutting servers down is
//!   always safe — there is no teardown/wakeup race to coordinate.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use infinity_agent_core::system::{ChannelSender, ThreadConfig, ThreadConfigSource};
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::config::ToolSetConfig;
use infinity_protocol::DaemonMessage;
use rap_client::http::{HttpClient, SimpleHttpClient};
use rap_client::notifier::RapNotifier;
use rap_protocol::{RapInvocation, ToolsetManifest};

use crate::memory_store::InMemoryConversationStore;
use crate::session::observer::SubscriberMap;
use crate::session::{SessionStoreHandle, spawn_rap_server};
use crate::{config, mcp_proxy, set_title_tool};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ── ManagedRapServer ─────────────────────────────────────────────────────────

/// How to bring a configured RAP server up.
enum ServerSpec {
    /// A command spawned as a child process (`RAP_EMBEDDED=1`).
    Command { command: String, cwd: PathBuf },
    /// A stdio MCP server proxied behind an in-process RAP server. The proxy
    /// owns the child process; it is booted once and kept for the daemon's
    /// lifetime.
    Mcp {
        name: String,
        command: Vec<String>,
        env: HashMap<String, String>,
    },
    /// A remote MCP server proxied behind an in-process RAP server.
    HttpMcp {
        name: String,
        url: String,
        headers: HashMap<String, String>,
    },
    /// A pre-existing external server URL; never started or stopped by us.
    External { url: String },
}

#[expect(
    clippy::large_enum_variant,
    reason = "at most a handful of servers exist per session"
)]
enum ServerState {
    Down,
    Up {
        base_url: String,
        /// The invocation endpoint from the server's manifest.
        endpoint: String,
        /// The child process for command-based servers (killed on shutdown).
        child: Option<tokio::process::Child>,
    },
}

/// A handle to one configured RAP server that the daemon can boot lazily and
/// shut down at will. Tools hold an `Arc` to it and call
/// [`invoke_endpoint`](Self::invoke_endpoint) per invocation, so a server
/// stopped while its session idled reboots transparently on the next call.
pub struct ManagedRapServer {
    /// Config id (for migration bookkeeping), when present.
    pub config_id: Option<String>,
    session_id: String,
    spec: ServerSpec,
    state: tokio::sync::Mutex<ServerState>,
}

impl ManagedRapServer {
    fn new(session_id: String, config_id: Option<String>, spec: ServerSpec) -> Self {
        Self {
            config_id,
            session_id,
            spec,
            state: tokio::sync::Mutex::new(ServerState::Down),
        }
    }

    /// The server's base URL if it is currently up.
    pub async fn current_base_url(&self) -> Option<String> {
        match &*self.state.lock().await {
            ServerState::Up { base_url, .. } => Some(base_url.clone()),
            ServerState::Down => None,
        }
    }

    /// Whether the server is currently up.
    pub async fn is_up(&self) -> bool {
        matches!(&*self.state.lock().await, ServerState::Up { .. })
    }

    /// Boot the server if it is down and return its manifest. Used both for
    /// the initial toolset discovery and for lazy reboots.
    pub async fn ensure_up(&self) -> Result<ToolsetManifest, BoxError> {
        let mut state = self.state.lock().await;
        let base_url = match &*state {
            ServerState::Up { base_url, .. } => base_url.clone(),
            ServerState::Down => {
                let (base_url, child) = match &self.spec {
                    ServerSpec::Command { command, cwd } => {
                        tracing::info!(
                            "Booting RAP server for session {}: {command}",
                            self.session_id
                        );
                        let (child, port) = spawn_rap_server(command, cwd).await?;
                        (format!("http://127.0.0.1:{port}"), Some(child))
                    }
                    ServerSpec::Mcp { name, command, env } => {
                        let port =
                            mcp_proxy::start_mcp_proxy(name.clone(), command.clone(), env.clone())
                                .await?;
                        (format!("http://127.0.0.1:{port}"), None)
                    }
                    ServerSpec::HttpMcp { name, url, headers } => {
                        let port = mcp_proxy::start_http_mcp_proxy(
                            name.clone(),
                            url.clone(),
                            headers.clone(),
                        )
                        .await?;
                        (format!("http://127.0.0.1:{port}"), None)
                    }
                    ServerSpec::External { url } => (url.clone(), None),
                };
                *state = ServerState::Up {
                    base_url: base_url.clone(),
                    endpoint: String::new(),
                    child,
                };
                base_url
            }
        };

        // Fetch the manifest (the invocation endpoint changes across reboots
        // since command servers get a fresh port).
        let manifest = fetch_manifest(&base_url, &self.session_id).await?;
        if let ServerState::Up { endpoint, .. } = &mut *state {
            *endpoint = manifest.endpoint.clone();
        }
        Ok(manifest)
    }

    /// The invocation endpoint, booting the server first if necessary.
    pub async fn invoke_endpoint(&self) -> Result<String, BoxError> {
        {
            let state = self.state.lock().await;
            if let ServerState::Up { endpoint, .. } = &*state
                && !endpoint.is_empty()
            {
                return Ok(endpoint.clone());
            }
        }
        Ok(self.ensure_up().await?.endpoint)
    }

    /// Stop the server. Safe to call at any time: the next invocation (or
    /// manifest fetch) boots it back up. MCP proxies and external servers are
    /// left running — only command-based child processes are stopped.
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        match &self.spec {
            ServerSpec::Command { .. } => {
                if let ServerState::Up { child, .. } = &mut *state
                    && let Some(mut child) = child.take()
                {
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{self, Signal};
                        use nix::unistd::Pid;
                        if let Some(id) = child.id() {
                            // Negative ID signals the entire process group.
                            let _ = signal::kill(Pid::from_raw(-(id as i32)), Signal::SIGINT);
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.start_kill();
                    }
                    let _ = child.wait().await;
                    tracing::info!("Stopped RAP server for session {}", self.session_id);
                }
                *state = ServerState::Down;
            }
            // In-process proxies and external servers are not ours to stop.
            ServerSpec::Mcp { .. } | ServerSpec::HttpMcp { .. } | ServerSpec::External { .. } => {}
        }
    }
}

async fn fetch_manifest(base_url: &str, _session_id: &str) -> Result<ToolsetManifest, BoxError> {
    let http = SimpleHttpClient::new();
    let url = format!("{}/.well-known/rap-toolset", base_url.trim_end_matches('/'));
    let (status, body) = http
        .get(&url)
        .await
        .map_err(|e| format!("failed to fetch toolset manifest from {url}: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("toolset manifest fetch from {url} returned status {status}").into());
    }
    let manifest: ToolsetManifest = serde_json::from_slice(&body)
        .map_err(|e| format!("invalid toolset manifest from {url}: {e}"))?;
    Ok(manifest)
}

// ── ManagedRapTool ───────────────────────────────────────────────────────────

/// A RAP tool bound to a [`ManagedRapServer`]: each invocation resolves the
/// server's current endpoint, booting it first if it was shut down.
pub struct ManagedRapTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub display_script: Option<String>,
    pub server: Arc<ManagedRapServer>,
    pub http_client: SimpleHttpClient,
}

#[async_trait]
impl Tool<ChannelSender> for ManagedRapTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn display_script(&self) -> Option<&str> {
        self.display_script.as_deref()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &infinity_agent_core::tools::ToolContext<ChannelSender>,
    ) -> Result<(), BoxError> {
        let endpoint = self.server.invoke_endpoint().await?;

        let thread_ancestors = if context.thread_stack.len() > 1 {
            Some(context.thread_stack[..context.thread_stack.len() - 1].to_vec())
        } else {
            None
        };
        let invocation = RapInvocation {
            operation: self.name.clone(),
            arguments: args,
            id,
            call_id,
            callback_url: context.callback_url.clone(),
            group_id: context.group_id.clone(),
            user_id: context.user_id.clone(),
            thread_ancestors,
        };
        let body = serde_json::to_string(&invocation)?;
        let status = self
            .http_client
            .post(&endpoint, &body)
            .await
            .map_err(|e| Box::new(e) as BoxError)?;
        if !(200..300).contains(&status) {
            tracing::warn!("RAP tool {} returned status {}", self.name, status);
        }
        tracing::info!("Invoked RAP tool {} (status: {})", self.name, status);
        Ok(())
    }
}

// ── SessionRapManager ────────────────────────────────────────────────────────

/// A session's resolved toolset: the tool instances shared by all of the
/// session's threads plus the managed servers behind them.
struct SessionToolset {
    tools: Vec<Rc<dyn Tool<ChannelSender>>>,
    servers: Vec<Arc<ManagedRapServer>>,
    extra_system_prompt: String,
}

/// Daemon-wide manager of per-session toolsets and their RAP servers. Also
/// the agent system's [`ThreadConfigSource`]: each thread resolves to its
/// session's toolset.
#[derive(Clone)]
pub struct SessionRapManager {
    toolsets: Rc<tokio::sync::Mutex<HashMap<String, Rc<SessionToolset>>>>,
    conversation_store: InMemoryConversationStore,
    session_store: SessionStoreHandle,
    user_rap_config: Option<PathBuf>,
    /// Used to surface boot progress to the session's subscribers.
    subscriber_map: SubscriberMap,
    /// Sessions whose servers have been shut down at least once (observable
    /// for tests and diagnostics).
    shutdown_log: Rc<std::cell::RefCell<Vec<String>>>,
}

impl SessionRapManager {
    pub fn new(
        conversation_store: InMemoryConversationStore,
        session_store: SessionStoreHandle,
        user_rap_config: Option<PathBuf>,
        subscriber_map: SubscriberMap,
    ) -> Self {
        Self {
            toolsets: Rc::new(tokio::sync::Mutex::new(HashMap::new())),
            conversation_store,
            session_store,
            user_rap_config,
            subscriber_map,
            shutdown_log: Rc::new(std::cell::RefCell::new(Vec::new())),
        }
    }

    /// Send an informational message to the session's subscribers.
    fn info(&self, session_id: &str, text: String) {
        let msg = DaemonMessage::Info {
            thread_id: Some(session_id.to_owned()),
            text,
        };
        let smap = self.subscriber_map.lock().expect("bug: mutex poisoned");
        if let Some(subs) = smap.get(session_id) {
            let mut subs = subs.lock().expect("bug: mutex poisoned");
            subs.retain(|sub| sub.tx.send(msg.clone()).is_ok());
        }
    }

    /// Get (or lazily build) the toolset for a session: read the RAP config
    /// for the session's cwd, boot the servers, fetch their manifests, and
    /// build the tool instances.
    async fn get_or_init(&self, session_id: &str) -> Result<Rc<SessionToolset>, BoxError> {
        let mut toolsets = self.toolsets.lock().await;
        if let Some(ts) = toolsets.get(session_id) {
            return Ok(ts.clone());
        }

        let cwd = {
            let store = self.session_store.lock().await;
            store.get_cwd(session_id).clone()
        };

        let mut servers: Vec<Arc<ManagedRapServer>> = Vec::new();
        match collect_server_specs(session_id, &cwd, self.user_rap_config.as_deref()) {
            Ok((source_info, specs)) => {
                self.info(session_id, source_info);
                for server in specs {
                    servers.push(Arc::new(server));
                }
            }
            Err(e) => {
                self.info(
                    session_id,
                    format!("Warning: failed to read RAP config: {e}"),
                );
            }
        }

        // Boot each server once to discover its tools.
        let http_client = SimpleHttpClient::new();
        let mut tools: Vec<Rc<dyn Tool<ChannelSender>>> = Vec::new();
        let mut tool_count = 0usize;
        for server in &servers {
            match server.ensure_up().await {
                Ok(manifest) => {
                    for def in manifest.tools {
                        tool_count += 1;
                        tools.push(Rc::new(ManagedRapTool {
                            name: def.name,
                            description: def.description,
                            parameters: def.input_schema,
                            display_script: def.display_script,
                            server: server.clone(),
                            http_client: http_client.clone(),
                        }));
                    }
                }
                Err(e) => {
                    self.info(
                        session_id,
                        format!("Warning: failed to start RAP server: {e}"),
                    );
                }
            }
        }
        if tool_count > 0 {
            self.info(session_id, format!("Loaded {tool_count} RAP tool(s)"));
        }

        tools.push(Rc::new(set_title_tool::SetTitleTool {
            conversation_store: self.conversation_store.clone(),
        }));

        let extra_system_prompt = format!(
            "The user's current working directory is: {cwd:?}\n\n\
             Use the `set_title` tool to give the current thread a short, descriptive title. \
             Set it once at the start when the user's intent becomes clear, and update it only \
             when the overall scope of work changes significantly. Do not call it repeatedly \
             for minor follow-ups within the same task."
        );

        let toolset = Rc::new(SessionToolset {
            tools,
            servers,
            extra_system_prompt,
        });
        toolsets.insert(session_id.to_owned(), toolset.clone());
        Ok(toolset)
    }

    /// Stop the session's RAP servers (keeping the cached toolset, so a later
    /// tool invocation reboots them transparently). Safe to call at any time.
    pub async fn shutdown_session(&self, session_id: &str) {
        let toolset = {
            let toolsets = self.toolsets.lock().await;
            toolsets.get(session_id).cloned()
        };
        if let Some(toolset) = toolset {
            for server in &toolset.servers {
                server.shutdown().await;
            }
        }
        self.shutdown_log.borrow_mut().push(session_id.to_owned());
    }

    /// Stop the session's servers and drop its cached toolset entirely, so
    /// the next use re-reads the RAP config and re-fetches manifests. Used
    /// when a session is explicitly shut down.
    pub async fn evict_session(&self, session_id: &str) {
        self.shutdown_session(session_id).await;
        self.toolsets.lock().await.remove(session_id);
    }

    /// Stop every managed server (daemon exit).
    pub async fn shutdown_all(&self) {
        let session_ids: Vec<String> = self.toolsets.lock().await.keys().cloned().collect();
        for session_id in session_ids {
            self.shutdown_session(&session_id).await;
        }
    }

    /// Whether any of the session's managed servers is currently up.
    pub async fn session_servers_up(&self, session_id: &str) -> bool {
        let toolset = {
            let toolsets = self.toolsets.lock().await;
            toolsets.get(session_id).cloned()
        };
        if let Some(toolset) = toolset {
            for server in &toolset.servers {
                if server.is_up().await {
                    return true;
                }
            }
        }
        false
    }

    /// How many times [`shutdown_session`](Self::shutdown_session) has been
    /// invoked for `session_id` (diagnostics / tests).
    pub fn times_shut_down(&self, session_id: &str) -> usize {
        self.shutdown_log
            .borrow()
            .iter()
            .filter(|s| s.as_str() == session_id)
            .count()
    }
}

#[async_trait(?Send)]
impl ThreadConfigSource<ChannelSender, SimpleHttpClient> for SessionRapManager {
    async fn resolve(
        &self,
        thread_id: &str,
    ) -> Result<ThreadConfig<ChannelSender, SimpleHttpClient>, BoxError> {
        let session_id = self.conversation_store.get_root_thread_id(thread_id);
        let toolset = self.get_or_init(&session_id).await?;

        // Best-effort lifecycle notifications go to the servers that are
        // currently up (a server that is down has nothing in flight).
        let mut urls = Vec::new();
        for server in &toolset.servers {
            if let Some(url) = server.current_base_url().await {
                urls.push(url);
            }
        }
        Ok(ThreadConfig {
            tools: toolset.tools.clone(),
            extra_system_prompt: Some(toolset.extra_system_prompt.clone()),
            rap_notifier: Some(RapNotifier::new(urls, SimpleHttpClient::new())),
        })
    }
}

/// Read the merged RAP config for `cwd` and build the (un-booted) server
/// handles. Also returns a human-readable line describing which config
/// source(s) were used, for display to the session's subscribers.
fn collect_server_specs(
    session_id: &str,
    cwd: &std::path::Path,
    user_config_path: Option<&std::path::Path>,
) -> Result<(String, Vec<ManagedRapServer>), BoxError> {
    let cwd_rap = cwd.join(".infinity").join("rap.json");
    let local_config = cwd_rap
        .exists()
        .then(|| config::load_config(&cwd_rap))
        .transpose()?;
    let user_config = user_config_path
        .and_then(|p| p.exists().then(|| config::load_config(p)))
        .transpose()?;

    let (source_info, merged) = match (local_config, user_config) {
        (None, None) => {
            return Ok((
                "Neither local nor user RAP configs exist, using empty config".into(),
                Vec::new(),
            ));
        }
        (None, Some(c)) => ("Using user config".into(), c),
        (Some(c), None) => ("Using local config".into(), c),
        (Some(mut l), Some(u)) => {
            l.merge(u);
            ("Both local and user RAP configs exist, merging".into(), l)
        }
    };

    let mut servers = Vec::new();
    for entry in merged.tool_sets {
        let (id, spec) = match entry {
            ToolSetConfig::ToolsetServer { server_url, id } => {
                (id, ServerSpec::External { url: server_url })
            }
            ToolSetConfig::ToolsetCommand { command, id, .. } => (
                id,
                ServerSpec::Command {
                    command,
                    cwd: cwd.to_path_buf(),
                },
            ),
            ToolSetConfig::McpServer {
                name,
                command,
                env,
                id,
            } => (id, ServerSpec::Mcp { name, command, env }),
            ToolSetConfig::HttpMcpServer {
                name,
                url,
                headers,
                id,
            } => (id, ServerSpec::HttpMcp { name, url, headers }),
        };
        servers.push(ManagedRapServer::new(session_id.to_owned(), id, spec));
    }
    Ok((source_info, servers))
}
