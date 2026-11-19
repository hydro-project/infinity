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
//!
//! Migration flows boot the same configured servers eagerly (outside any
//! session) via [`boot_migration_servers`], pairing source and destination
//! servers by config id and consulting each manifest's `needsMigration`
//! flag.
//!
//! Command servers configured with `"lifetime": "daemon"` are the exception
//! to all of the above: [`DaemonServers`] runs one shared process per
//! command for the daemon's whole lifetime (booted at daemon start for
//! user-level entries), session idles never stop it, and migrations leave it
//! alone.

use infinity_agent_core::ThreadId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::system::{ThreadConfig, ThreadConfigSource};
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::config::{ServerLifetime, ToolSetConfig};
use infinity_agent_core::tools::rap_tool::{
    RapInvocationParams, RapToolDescriptor, invoke_rap_tool,
};
use infinity_protocol::DaemonMessage;
use rap_client::http::{HttpClient, SimpleHttpClient};
use rap_client::notifier::RapNotifier;
use rap_protocol::ToolsetManifest;

use crate::memory_store::PersistentConversationStore;
use crate::session::SessionStoreHandle;
use crate::session::observer::SubscriberMap;
use crate::{config, mcp_proxy, set_title_tool};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ── ManagedRapServer ─────────────────────────────────────────────────────────

/// How to bring a configured RAP server up.
enum ServerSpec {
    /// A command spawned as a child process (`RAP_EMBEDDED=1`) in
    /// `working_dir`, with `extra_env` added to its environment.
    Command {
        command: String,
        working_dir: PathBuf,
        extra_env: Vec<(&'static str, String)>,
    },
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
        /// The server's manifest, fetched once per boot. (The invocation
        /// endpoint changes across reboots since command servers get a
        /// fresh port.)
        manifest: ToolsetManifest,
        /// The child process for command-based servers (killed on shutdown).
        child: Option<tokio::process::Child>,
        /// The accept-loop task of an in-process MCP proxy. Stored for
        /// ownership: unlike command servers, MCP servers may hold state
        /// that lives only as long as their process (RAP servers persist
        /// state via the thread ID, but MCP has no such contract), so
        /// [`shutdown`](ManagedRapServer::shutdown) leaves proxies running
        /// rather than restarting them across session idles. The task is
        /// only ever aborted to tear down a half-booted proxy (manifest
        /// fetch failure) before it is stored here.
        #[expect(dead_code, reason = "held for ownership; never polled")]
        task: Option<tokio::task::JoinHandle<()>>,
    },
}

/// A handle to one configured RAP server that the daemon can boot lazily and
/// shut down at will. Tools hold an `Arc` to it and call
/// [`invoke_endpoint`](Self::invoke_endpoint) per invocation, so a server
/// stopped while its session idled reboots transparently on the next call.
pub struct ManagedRapServer {
    /// Config id (for migration bookkeeping), when present.
    pub config_id: Option<String>,
    session_id: ThreadId,
    spec: ServerSpec,
    /// Daemon-lifetime servers ignore [`shutdown`](Self::shutdown); only
    /// [`stop`](Self::stop) (daemon exit) ends them.
    lifetime: ServerLifetime,
    state: tokio::sync::Mutex<ServerState>,
    /// The base URL while `Up`, readable without waiting on `state` (which
    /// a boot holds for as long as it takes).
    live_url: std::sync::Mutex<Option<String>>,
}

impl ManagedRapServer {
    fn new(session_id: ThreadId, config_id: Option<String>, spec: ServerSpec) -> Self {
        Self {
            config_id,
            session_id,
            spec,
            lifetime: ServerLifetime::Session,
            state: tokio::sync::Mutex::new(ServerState::Down),
            live_url: Default::default(),
        }
    }

    /// The server's base URL if it is currently up.
    pub fn current_base_url(&self) -> Option<String> {
        self.live_url.lock().expect("bug: poisoned").clone()
    }

    fn set_live_url(&self, url: Option<String>) {
        *self.live_url.lock().expect("bug: poisoned") = url;
    }

    /// Boot the server if it is down (or its process has died) and return
    /// its manifest (cached for the lifetime of the boot). If the server
    /// boots but its manifest cannot be fetched, the boot is torn down and
    /// the server stays down, so `Up` always has a usable manifest.
    pub async fn ensure_up(&self) -> Result<ToolsetManifest, BoxError> {
        let mut state = self.state.lock().await;
        if let ServerState::Up {
            child: Some(child), ..
        } = &mut *state
            && let Ok(Some(status)) = child.try_wait()
        {
            tracing::warn!(
                "RAP server for session {} exited ({status}); restarting it",
                self.session_id
            );
            *state = ServerState::Down;
            self.set_live_url(None);
        }
        if let ServerState::Up { manifest, .. } = &*state {
            return Ok(manifest.clone());
        }

        let (base_url, mut child, task) = match &self.spec {
            ServerSpec::Command {
                command,
                working_dir,
                extra_env,
            } => {
                tracing::info!(
                    "Booting RAP server for session {}: {command}",
                    self.session_id
                );
                let (child, port) =
                    spawn_rap_server(command, working_dir, extra_env, self.lifetime).await?;
                (format!("http://127.0.0.1:{port}"), Some(child), None)
            }
            ServerSpec::Mcp { name, command, env } => {
                let (port, task) =
                    mcp_proxy::start_mcp_proxy(name.clone(), command.clone(), env.clone()).await?;
                (format!("http://127.0.0.1:{port}"), None, Some(task))
            }
            ServerSpec::HttpMcp { name, url, headers } => {
                let (port, task) =
                    mcp_proxy::start_http_mcp_proxy(name.clone(), url.clone(), headers.clone())
                        .await?;
                (format!("http://127.0.0.1:{port}"), None, Some(task))
            }
            ServerSpec::External { url } => (url.clone(), None, None),
        };

        // Like the startup line, a server that never answers must not hold
        // the boot lock forever.
        let fetched = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            fetch_manifest(&base_url),
        )
        .await
        .unwrap_or_else(|_| {
            Err(format!("toolset manifest fetch from {base_url} timed out").into())
        });
        match fetched {
            Ok(manifest) => {
                self.set_live_url(Some(base_url));
                *state = ServerState::Up {
                    manifest: manifest.clone(),
                    child,
                    task,
                };
                Ok(manifest)
            }
            Err(e) => {
                // Tear the boot down; the next call retries from scratch.
                if let Some(child) = &mut child {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
                if let Some(task) = task {
                    task.abort();
                }
                Err(e)
            }
        }
    }

    /// The invocation endpoint, booting the server first if necessary.
    pub async fn invoke_endpoint(&self) -> Result<String, BoxError> {
        Ok(self.ensure_up().await?.endpoint)
    }

    /// Stop the server. Safe to call at any time: the next invocation (or
    /// manifest fetch) boots it back up. Only command-based child processes
    /// are stopped — RAP servers persist their state keyed by thread ID, so
    /// a restart is transparent. MCP servers have no such contract (one may
    /// hold state that lives only as long as its process), so MCP proxies
    /// are left running; external servers are not ours to stop. Neither are
    /// daemon-lifetime servers: they outlive every session.
    pub async fn shutdown(&self) {
        if self.lifetime == ServerLifetime::Daemon {
            return;
        }
        self.stop().await;
    }

    /// Stop a command server's process regardless of its lifetime.
    async fn stop(&self) {
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
                self.set_live_url(None);
            }
            // In-process proxies and external servers are not ours to stop.
            ServerSpec::Mcp { .. } | ServerSpec::HttpMcp { .. } | ServerSpec::External { .. } => {}
        }
    }
}

async fn fetch_manifest(base_url: &str) -> Result<ToolsetManifest, BoxError> {
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

// ── Migration boots ──────────────────────────────────────────────────────────

/// A RAP server booted eagerly for a migration flow. Migration servers are
/// ordinary configured RAP servers — the same specs the session runtime
/// boots lazily — brought up outside any session so the migration protocol
/// can be spoken to the ones whose manifest declares `needsMigration`.
///
/// Owns the underlying server: dropping this kills a command server's child
/// process (`kill_on_drop`), which bounds the servers' lifetime to the
/// migration flow. MCP proxies detach and keep running, as everywhere else.
pub struct MigrationServer {
    /// The server's config id, when its config entry has one (migration
    /// pairing between source and destination is keyed by these ids).
    pub config_id: Option<String>,
    /// The server's reachable base URL.
    pub url: String,
    /// The local port, for servers we booted ourselves (`None` for external
    /// `toolset_server` URLs).
    pub port: Option<u16>,
    /// Whether the server's manifest declared `needsMigration: true`.
    pub needs_migration: bool,
    _server: ManagedRapServer,
}

/// Boot every server in `cwd`'s merged RAP config and fetch its manifest.
/// Servers that fail to boot are skipped with a warning, matching the
/// session runtime's tolerance for individual server failures.
/// Daemon-lifetime servers are skipped: they are shared by every session on
/// this daemon, so there is no per-session state to migrate, and booting a
/// second instance would fight the shared one.
pub async fn boot_migration_servers(
    cwd: &Path,
    user_config_path: Option<&Path>,
) -> Result<Vec<MigrationServer>, BoxError> {
    let (_source_info, configured) = collect_server_specs(cwd, user_config_path)?;
    let mut servers = Vec::new();
    for configured in configured {
        let ConfiguredServer::Session { config_id, spec } = configured else {
            continue;
        };
        let server = ManagedRapServer::new(ThreadId::from("migration"), config_id, spec);
        let manifest = match server.ensure_up().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("failed to boot RAP server for migration: {e}");
                continue;
            }
        };
        let url = server
            .current_base_url()
            .expect("bug: server is up right after ensure_up");
        // Only servers we booted ourselves have a meaningful local port
        // (external toolset_server URLs are excluded from port maps, since
        // migration tunnels forward local ports).
        let port = if matches!(server.spec, ServerSpec::External { .. }) {
            None
        } else {
            url.rsplit_once(':')
                .and_then(|(_, p)| p.trim_end_matches('/').parse().ok())
        };
        servers.push(MigrationServer {
            config_id: server.config_id.clone(),
            url,
            port,
            needs_migration: manifest.needs_migration,
            _server: server,
        });
    }
    Ok(servers)
}

/// The `config_id → port` map for the servers that need migration — the
/// payload of [`DaemonMessage::RapServersBooted`].
pub fn migration_server_ports(servers: &[MigrationServer]) -> HashMap<String, u16> {
    servers
        .iter()
        .filter(|s| s.needs_migration)
        .filter_map(|s| Some((s.config_id.clone()?, s.port?)))
        .collect()
}

/// Spawn a command-based RAP server (`RAP_EMBEDDED=1`) and wait for it to
/// report its port on stdout. Used both by the lazy per-session server
/// management and by the eager migration boots above.
///
/// A daemon-lifetime server's stdin is a pipe whose write end lives as long
/// as the returned child handle (so as long as this daemon): the server
/// sees EOF when the daemon exits, even by crashing, and should exit then
/// instead of lingering as an orphan.
async fn spawn_rap_server(
    command: &str,
    working_dir: &Path,
    extra_env: &[(&'static str, String)],
    lifetime: ServerLifetime,
) -> Result<(tokio::process::Child, u16), BoxError> {
    use infinity_agent_core::tools::config::CommandServerReady;
    use std::process::Stdio;
    use tokio::io::AsyncBufReadExt;

    std::fs::create_dir_all(working_dir).ok();

    let mut child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .env("RAP_EMBEDDED", "1")
        .envs(extra_env.iter().map(|(k, v)| (*k, v)))
        .current_dir(working_dir)
        // Ensure all children are in the the same process group. We will send SIGINT to the entire
        // group during shutdown.
        .process_group(0)
        .stdin(match lifetime {
            ServerLifetime::Daemon => Stdio::piped(),
            ServerLifetime::Session => Stdio::inherit(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn '{command}': {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = String::new();
    // A server that never reports its port would otherwise hold its boot
    // lock (and every caller waiting on it) forever.
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        reader.read_line(&mut line),
    )
    .await
    {
        Ok(read) => {
            read.map_err(|e| format!("failed to read startup line: {e}"))?;
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err(format!("'{command}' did not report its port within 60s").into());
        }
    }

    if line.is_empty() {
        let _ = child.kill().await;
        return Err("server exited before emitting port".into());
    }

    let ready: CommandServerReady = serde_json::from_str(line.trim())
        .map_err(|e| format!("invalid startup JSON: {e} (got: {line})"))?;
    Ok((child, ready.port))
}

// ── ManagedRapTool ───────────────────────────────────────────────────────────

/// A RAP tool bound to a [`ManagedRapServer`]: each invocation resolves the
/// server's current endpoint, booting it first if it was shut down.
pub struct ManagedRapTool {
    pub descriptor: RapToolDescriptor,
    pub server: Arc<ManagedRapServer>,
    pub http_client: SimpleHttpClient,
}

#[async_trait]
impl Tool<ChannelSender> for ManagedRapTool {
    fn name(&self) -> &str {
        &self.descriptor.name
    }

    fn description(&self) -> &str {
        &self.descriptor.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.descriptor.parameters.clone()
    }

    fn display_script(&self) -> Option<&str> {
        self.descriptor.display_script.as_deref()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: rap_protocol::ToolCallId,
        call_id: Option<rap_protocol::ProviderCallId>,
        context: &infinity_agent_core::tools::ToolContext<ChannelSender>,
    ) -> Result<(), BoxError> {
        let endpoint = self.server.invoke_endpoint().await?;
        invoke_rap_tool(
            &self.http_client,
            RapInvocationParams {
                endpoint: &endpoint,
                operation: &self.descriptor.name,
                arguments: args,
                id,
                call_id,
                callback_url: None,
            },
            context,
        )
        .await
    }
}

// ── DaemonServers ────────────────────────────────────────────────────────────

/// The daemon's shared `lifetime: daemon` command servers, one per command
/// string. Each process is spawned in the daemon's state directory with
/// `RAP_SERVER_LIFETIME=daemon` and `RAP_CALLBACK_URL` (the daemon's callback
/// URL, which changes on every daemon restart, so long-lived servers learn
/// the current one without waiting for an invocation).
#[derive(Clone)]
pub struct DaemonServers {
    state_dir: PathBuf,
    callback_url: String,
    servers: Rc<std::cell::RefCell<HashMap<String, Arc<ManagedRapServer>>>>,
    /// Set by [`stop_all`](Self::stop_all) so [`supervise`](Self::supervise)
    /// does not restart servers while the daemon exits.
    stopped: Rc<std::cell::Cell<bool>>,
}

impl DaemonServers {
    pub fn new(state_dir: PathBuf, callback_url: String) -> Self {
        Self {
            state_dir,
            callback_url,
            servers: Default::default(),
            stopped: Default::default(),
        }
    }

    /// The shared server for `command`, created (un-booted) on first use.
    fn get_or_create(&self, config_id: Option<String>, command: String) -> Arc<ManagedRapServer> {
        let mut servers = self.servers.borrow_mut();
        servers
            .entry(command.clone())
            .or_insert_with(|| {
                Arc::new(ManagedRapServer {
                    config_id,
                    session_id: ThreadId::from("daemon"),
                    spec: ServerSpec::Command {
                        command,
                        working_dir: self.state_dir.clone(),
                        extra_env: vec![
                            ("RAP_SERVER_LIFETIME", "daemon".to_owned()),
                            ("RAP_CALLBACK_URL", self.callback_url.clone()),
                        ],
                    },
                    lifetime: ServerLifetime::Daemon,
                    state: tokio::sync::Mutex::new(ServerState::Down),
                    live_url: Default::default(),
                })
            })
            .clone()
    }

    /// Boot the daemon-lifetime servers in the user-level RAP config, so they
    /// receive events before any session runs. (Daemon-lifetime entries in a
    /// project-level config boot when a session in that project first runs.)
    pub async fn boot_user_servers(&self, user_config_path: &Path) {
        if !user_config_path.exists() {
            return;
        }
        let config = match config::load_config(user_config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to read {}: {e}", user_config_path.display());
                return;
            }
        };
        for entry in config.tool_sets {
            if let ToolSetConfig::ToolsetCommand {
                command,
                id,
                lifetime: ServerLifetime::Daemon,
                ..
            } = entry
                && let Err(e) = self.get_or_create(id, command.clone()).ensure_up().await
            {
                tracing::warn!("failed to boot daemon-lifetime RAP server '{command}': {e}");
            }
        }
    }

    /// Keep the shared servers running: every interval, restart any whose
    /// process has died (they must keep receiving events even when no
    /// session invokes them). Runs forever.
    pub async fn supervise(&self, interval: std::time::Duration) {
        loop {
            tokio::time::sleep(interval).await;
            if self.stopped.get() {
                return;
            }
            let servers: Vec<_> = self.servers.borrow().values().cloned().collect();
            for server in servers {
                if let Err(e) = server.ensure_up().await {
                    tracing::warn!("failed to restart daemon-lifetime RAP server: {e}");
                }
            }
        }
    }

    /// Tell every shared server that `session_id` will not be used again
    /// (it was stopped, archived, or migrated away), so it can drop
    /// per-session state such as event subscriptions. Runs in the
    /// background, starting any server that is down first (a server that
    /// misses this keeps the session's state forever), so a slow boot never
    /// holds up the caller.
    fn notify_session_closed(&self, session_id: &ThreadId<str>) {
        let servers: Vec<_> = self.servers.borrow().values().cloned().collect();
        let stopped = self.stopped.clone();
        let session_id = session_id.to_owned();
        tokio::task::spawn_local(rap_protocol::log_panic(
            "notify_session_closed",
            async move {
                // Each server is told as soon as it is up, concurrently, so
                // one slow boot does not delay the others (a close arriving
                // after the session is resumed would close inboxes it
                // reopened).
                futures_util::future::join_all(servers.into_iter().map(|server| {
                    let stopped = stopped.clone();
                    let session_id = session_id.clone();
                    async move {
                        if stopped.get() {
                            return;
                        }
                        if let Err(e) = server.ensure_up().await {
                            tracing::warn!(
                                "cannot tell a daemon-lifetime server {session_id} closed: {e}"
                            );
                            return;
                        }
                        let urls: Vec<String> = server.current_base_url().into_iter().collect();
                        RapNotifier::new(urls, SimpleHttpClient::new())
                            .notify_thread_closed(&session_id)
                            .await;
                    }
                }))
                .await;
            },
        ));
    }

    /// Stop every shared server (daemon exit).
    async fn stop_all(&self) {
        self.stopped.set(true);
        let servers: Vec<_> = self.servers.borrow().values().cloned().collect();
        for server in servers {
            server.stop().await;
        }
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
    toolsets: Rc<tokio::sync::Mutex<HashMap<ThreadId, Rc<SessionToolset>>>>,
    conversation_store: PersistentConversationStore,
    session_store: SessionStoreHandle,
    user_rap_config: Option<PathBuf>,
    /// Shared daemon-lifetime servers, referenced by every session's toolset
    /// that configures them.
    pub daemon_servers: DaemonServers,
    /// Used to surface boot progress to the session's subscribers.
    subscriber_map: SubscriberMap,
    /// Sessions whose servers have been shut down at least once (observable
    /// for tests and diagnostics).
    shutdown_log: Rc<std::cell::RefCell<Vec<ThreadId>>>,
}

impl SessionRapManager {
    pub fn new(
        conversation_store: PersistentConversationStore,
        session_store: SessionStoreHandle,
        user_rap_config: Option<PathBuf>,
        subscriber_map: SubscriberMap,
        daemon_servers: DaemonServers,
    ) -> Self {
        Self {
            toolsets: Rc::new(tokio::sync::Mutex::new(HashMap::new())),
            conversation_store,
            session_store,
            user_rap_config,
            daemon_servers,
            subscriber_map,
            shutdown_log: Rc::new(std::cell::RefCell::new(Vec::new())),
        }
    }

    /// Send an informational message to the session's subscribers.
    fn info(&self, session_id: &ThreadId<str>, text: String) {
        let msg = DaemonMessage::Info {
            thread_id: Some(infinity_protocol::ThreadRef::local(session_id.to_owned())),
            text,
        };
        crate::session::observer::broadcast_to_thread(&self.subscriber_map, session_id, &msg, None);
    }

    /// Get (or lazily build) the toolset for a session: read the RAP config
    /// for the session's cwd, boot the servers, fetch their manifests, and
    /// build the tool instances.
    async fn get_or_init(
        &self,
        session_id: &ThreadId<str>,
    ) -> Result<Rc<SessionToolset>, BoxError> {
        let mut toolsets = self.toolsets.lock().await;
        if let Some(ts) = toolsets.get(session_id) {
            return Ok(ts.clone());
        }

        let cwd = {
            let store = self.session_store.lock().await;
            store.get_cwd(session_id).clone()
        };

        let mut servers: Vec<Arc<ManagedRapServer>> = Vec::new();
        match collect_server_specs(&cwd, self.user_rap_config.as_deref()) {
            Ok((source_info, configured)) => {
                self.info(session_id, source_info);
                for configured in configured {
                    servers.push(match configured {
                        ConfiguredServer::Session { config_id, spec } => Arc::new(
                            ManagedRapServer::new(session_id.to_owned(), config_id, spec),
                        ),
                        ConfiguredServer::Daemon { config_id, command } => {
                            self.daemon_servers.get_or_create(config_id, command)
                        }
                    });
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
                            descriptor: def.into(),
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
    pub async fn shutdown_session(&self, session_id: &ThreadId<str>) {
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
    /// when a session is explicitly shut down, archived, or migrated away;
    /// daemon-lifetime servers (which keep running) are told the session is
    /// closed.
    pub async fn evict_session(&self, session_id: &ThreadId<str>) {
        self.shutdown_session(session_id).await;
        self.toolsets.lock().await.remove(session_id);
        self.daemon_servers.notify_session_closed(session_id);
    }

    /// Stop every managed server, daemon-lifetime ones included (daemon exit).
    pub async fn shutdown_all(&self) {
        let session_ids: Vec<ThreadId> = self.toolsets.lock().await.keys().cloned().collect();
        for session_id in session_ids {
            self.shutdown_session(&session_id).await;
        }
        self.daemon_servers.stop_all().await;
    }

    /// How many times [`shutdown_session`](Self::shutdown_session) has been
    /// invoked for `session_id` (diagnostics / tests).
    pub fn times_shut_down(&self, session_id: &ThreadId<str>) -> usize {
        self.shutdown_log
            .borrow()
            .iter()
            .filter(|s| *s == session_id)
            .count()
    }
}

#[async_trait(?Send)]
impl ThreadConfigSource<ChannelSender, SimpleHttpClient> for SessionRapManager {
    async fn resolve(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<ThreadConfig<ChannelSender, SimpleHttpClient>, BoxError> {
        let session_id = self.conversation_store.get_root_thread_id(thread_id);
        let toolset = self.get_or_init(&session_id).await?;

        // Best-effort lifecycle notifications go to the servers that are up
        // when they are sent (a server that is down has nothing in flight),
        // at their address then: threads hold this config while they live,
        // and a daemon-lifetime server may restart on a new port meanwhile.
        let servers = toolset.servers.clone();
        let notifier = RapNotifier::new_live(
            move || {
                servers
                    .iter()
                    .filter_map(|s| s.current_base_url())
                    .collect()
            },
            SimpleHttpClient::new(),
        );
        Ok(ThreadConfig {
            tools: toolset.tools.clone(),
            extra_system_prompt: Some(toolset.extra_system_prompt.clone()),
            rap_notifier: Some(notifier),
        })
    }
}

/// A configured server, not yet booted.
enum ConfiguredServer {
    /// A server owned by one session (or one migration flow).
    Session {
        config_id: Option<String>,
        spec: ServerSpec,
    },
    /// A `lifetime: daemon` command, resolved through [`DaemonServers`].
    Daemon {
        config_id: Option<String>,
        command: String,
    },
}

/// Read the merged RAP config for `cwd` into (un-booted) server specs. Also
/// returns a human-readable line describing which config source(s) were
/// used, for display to the session's subscribers.
fn collect_server_specs(
    cwd: &Path,
    user_config_path: Option<&Path>,
) -> Result<(String, Vec<ConfiguredServer>), BoxError> {
    let (source_info, merged) = config::load_merged_rap_config(cwd, user_config_path)?;
    let Some(merged) = merged else {
        return Ok((source_info, Vec::new()));
    };

    let mut servers = Vec::new();
    for entry in merged.tool_sets {
        let (config_id, spec) = match entry {
            ToolSetConfig::ToolsetServer { server_url, id } => {
                (id, ServerSpec::External { url: server_url })
            }
            ToolSetConfig::ToolsetCommand {
                command,
                id,
                lifetime: ServerLifetime::Daemon,
                ..
            } => {
                servers.push(ConfiguredServer::Daemon {
                    config_id: id,
                    command,
                });
                continue;
            }
            ToolSetConfig::ToolsetCommand {
                command,
                id,
                lifetime: ServerLifetime::Session,
                ..
            } => (
                id,
                ServerSpec::Command {
                    command,
                    working_dir: cwd.join(".infinity"),
                    extra_env: Vec::new(),
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
        servers.push(ConfiguredServer::Session { config_id, spec });
    }
    Ok((source_info, servers))
}
