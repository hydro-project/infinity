use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_util::codec::LengthDelimitedCodec;

/// Maximum frame size for daemon ↔ client communication (256 MiB).
///
/// The default `LengthDelimitedCodec` limit is 8 MiB, which is too small for
/// messages that carry large tool outputs, file contents, or replayed
/// conversation history. We use a generous 256 MiB limit to avoid "frame size
/// too big" errors in practice.
const MAX_FRAME_LENGTH: usize = 256 * 1024 * 1024;

/// Create a [`LengthDelimitedCodec`] configured with the project-wide maximum
/// frame size. Use this instead of `LengthDelimitedCodec::new()` to ensure
/// both client and daemon agree on the limit.
pub fn length_delimited_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_LENGTH)
        .new_codec()
}

/// Returns the path to the daemon unix socket: `~/.infinity/daemon.sock`.
pub fn socket_path() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
        .join("daemon.sock")
}

/// Returns the path to the daemon PID file: `~/.infinity/daemon.pid`.
pub fn pid_path() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
        .join("daemon.pid")
}

/// Returns the base directory for daemon state: `~/.infinity/`.
pub fn state_dir() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
}

/// Returns the path to the remotes config: `~/.infinity/remotes.json`.
pub fn remotes_config_path() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
        .join("remotes.json")
}

// ── Client → Daemon ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Create a new session with the given working directory.
    CreateSession {
        cwd: PathBuf,
        /// Optional target location. `None` means local.
        /// Otherwise, the name of a remote.
        #[serde(default)]
        location: Option<String>,
        /// Optional model to use for the new session. `None` uses the
        /// daemon's default model.
        #[serde(default)]
        model: Option<ModelRef>,
    },
    /// Connect to an existing session (optionally a specific thread).
    Connect {
        session_id: String,
        thread_id: Option<String>,
    },
    UserInput {
        session_id: String,
        text: String,
    },
    /// Disconnect from the session while letting the agent continue to run in the background.
    Disconnect,
    /// Immediately attempt to detach. If the agent is idle, the daemon shuts
    /// down the session (closing the display channel). If not idle, the daemon
    /// responds with `DisconnectNotIdle` so the client can show a picker.
    SoftDetach {
        session_id: String,
    },
    /// Disconnects from the session and shuts down the agent so that it can only be woken bu
    /// new user inputs.
    ShutdownSession {
        session_id: String,
    },
    /// Archive a session (shut it down and hide from the main list).
    ArchiveSession {
        session_id: String,
    },
    SwitchModel {
        session_id: String,
        model: ModelRef,
    },
    /// Notify the daemon that a user choice was answered so it can be
    /// removed from the pending replay list.
    UserChoiceAnswered {
        choice_id: String,
        selected: usize,
    },
    /// Trigger compaction for the given session.
    TriggerCompaction {
        session_id: String,
    },
    /// Request migration of a session to a different host.
    RequestMigrate {
        session_id: String,
        /// `None` means local, `Some(name)` means a remote.
        #[serde(default)]
        to: Option<String>,
        dest_cwd: PathBuf,
    },
    /// Daemon-to-daemon: request a session to emigrate. Includes destination RAP URLs
    /// so source RAP servers can migrate their state.
    Emigrate {
        session_id: String,
        /// config_id → destination URL
        dest_rap_urls: HashMap<String, String>,
    },
    /// Daemon-to-daemon: immigration is complete, source can clean up.
    EmigrateDone {
        session_id: String,
    },
    /// Daemon-to-daemon: import a serialized session at the given cwd.
    ImportSession {
        session_id: String,
        cwd: PathBuf,
        session_data: String,
    },
    /// Daemon-to-daemon: boot RAP servers at the given cwd and return their ports.
    BootRapServers {
        cwd: PathBuf,
    },
    /// Request directory listing for path completion.
    ListDirectory {
        path: String,
        /// Target remote name. `None` means list on the local filesystem.
        #[serde(default)]
        on: Option<String>,
    },
}

// ── Daemon → Client ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonMessage {
    Connected {
        session_id: String,
        thread_id: String,
        model_name: String,
        context_window: usize,
        title: Option<String>,
        total_tokens_used: usize,
        #[serde(default)]
        provider_id: String,
    },
    StartOutput {
        thread_id: Option<String>,
    },
    TextChunk {
        thread_id: Option<String>,
        chunk: String,
    },
    ToolCall {
        name: String,
        args: String,
        thread_id: Option<String>,
        display_as: Option<String>,
    },
    ToolResult {
        /// Prioritized display segments. Clients use the first type they support.
        segments: Vec<rap_protocol::DisplaySegment>,
        thread_id: Option<String>,
    },
    Info {
        thread_id: Option<String>,
        text: String,
    },
    ResponseDone {
        thread_id: Option<String>,
        token_usage: Option<TokenUsage>,
    },
    UserInputEcho {
        thread_id: Option<String>,
        text: String,
    },
    SubscriptionEvent {
        name: String,
        text: String,
        thread_id: Option<String>,
    },
    OAuthRequired {
        thread_id: Option<String>,
        auth_url: String,
    },
    UserChoiceRequired {
        thread_id: Option<String>,
        id: String,
        prompt: String,
        choices: Vec<String>,
        default: usize,
    },
    UserChoiceComplete {
        choice_id: String,
    },
    ThinkingStart {
        thread_id: Option<String>,
    },
    ThinkingEnd {
        thread_id: Option<String>,
    },
    ThinkingChunk {
        thread_id: Option<String>,
        chunk: String,
    },
    CompactionApplied {
        thread_id: Option<String>,
    },
    Error {
        thread_id: Option<String>,
        text: String,
    },
    /// A view update pushed by a RAP tool server.
    ViewUpdate {
        thread_id: Option<String>,
        view_type: String,
        content: serde_json::Value,
    },
    /// Batch replay of history messages, sent on connect/load.
    Replay {
        history: Vec<DaemonMessage>,
        pending_choices: Vec<DaemonMessage>,
        #[serde(default)]
        views: HashMap<String, serde_json::Value>,
        /// Whether a completion is currently in flight for this thread. When
        /// false, clients should treat the end of the replay as an implicit
        /// ResponseDone (a trailing unresolved ToolCall in the history still
        /// implies a "waiting for tool result" state); when true, the
        /// spinner state implied by the end of the history is live and more
        /// events will follow.
        #[serde(default)]
        in_progress: bool,
    },
    /// Sent immediately on socket connection with session list and default model info.
    Welcome {
        sessions: HashMap<String, SessionInfo>,
        available_models: Vec<ModelInfo>,
        default_model_name: String,
        default_context_window: usize,
        provider_name: String,
        #[serde(default)]
        remotes: Vec<RemoteInfo>,
    },
    /// Broadcast: one or more sessions were created or updated.
    SessionsUpdated {
        sessions: HashMap<String, SessionInfo>,
    },
    /// Broadcast: remote connection statuses changed.
    RemotesUpdated {
        remotes: Vec<RemoteInfo>,
    },
    /// The agent is not idle — the client should show the full quit picker UI.
    DisconnectNotIdle,
    /// The agent was idle and has been detached — the client can proceed with
    /// its pending action (quit, switch, new session) without showing a picker.
    DetachedIdle,
    /// Response to Emigrate: serialized session data (thread tree as JSON).
    EmigrateResult {
        session_id: String,
        session_data: String,
    },
    MigrateStarted {
        session_id: String,
    },
    MigrateComplete {
        session_id: String,
        new_session_id: String,
    },
    MigrateError {
        session_id: String,
        error: String,
    },
    /// Response to ImportSession.
    ImportComplete {
        session_id: String,
    },
    /// Response to BootRapServers: maps config ID → local port for servers needing migration.
    RapServersBooted {
        /// config_id → port on the remote host (only servers with needsMigration)
        server_ports: HashMap<String, u16>,
    },
    /// Response to ListDirectory: directory entries for path completion.
    DirectoryListing {
        /// The path that was requested (for matching responses to requests).
        request_path: String,
        /// Directory entries (names only, directories have trailing `/`).
        entries: Vec<String>,
        /// The remote that was queried, if any.
        #[serde(default)]
        on: Option<String>,
    },
}

// ── Supporting types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Stopped,
    WaitingForChoice,
    Migrating,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub title: Option<String>,
    pub last_updated: String,
    pub total_tokens_used: usize,
    pub status: SessionStatus,
    #[serde(default)]
    pub threads: Vec<SubthreadInfo>,
    /// If set, this session lives on a remote daemon with this name.
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubthreadInfo {
    pub thread_id: String,
    pub parent_thread_id: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Total tokens including cached input. When prompt caching is active,
    /// `input_tokens` only reflects uncached input, so consumers should prefer
    /// `total_tokens` (falling back to `input + output` if absent for
    /// backwards compatibility).
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub display_name: String,
    /// The provider this model belongs to.
    #[serde(default)]
    pub provider_id: String,
    pub model_id: String,
    pub context_window: usize,
}

/// Globally unique reference to a model: a provider id plus the model's
/// provider-scoped id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub name: String,
    pub status: String,
}
