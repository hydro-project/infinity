use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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
    SwitchModel {
        session_id: String,
        model_id: String,
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
    /// Batch replay of history messages, sent on connect/load.
    Replay {
        history: Vec<DaemonMessage>,
        pending_choices: Vec<DaemonMessage>,
    },
    /// Sent immediately on socket connection with session list and default model info.
    Welcome {
        sessions: HashMap<String, SessionInfo>,
        available_models: Vec<ModelInfo>,
        default_model_name: String,
        default_context_window: usize,
        provider_name: String,
    },
    /// Broadcast: one or more sessions were created or updated.
    SessionsUpdated {
        sessions: HashMap<String, SessionInfo>,
    },
    /// The agent is not idle — the client should show the full quit picker UI.
    DisconnectNotIdle,
    /// The agent was idle and has been detached — the client can proceed with
    /// its pending action (quit, switch, new session) without showing a picker.
    DetachedIdle,
}

// ── Supporting types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Stopped,
    WaitingForChoice,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub display_name: String,
    pub model_id: String,
    pub context_window: usize,
}
