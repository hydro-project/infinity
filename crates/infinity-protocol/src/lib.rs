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

// ── Client → Daemon ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Create a new session with the given working directory.
    CreateSession {
        cwd: PathBuf,
    },
    /// Connect to an existing session.
    Connect {
        session_id: String,
    },
    UserInput {
        session_id: String,
        text: String,
    },
    /// Disconnect from the session while letting the agent continue to run in the background.
    Disconnect {
        session_id: String,
    },
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
    LoadSession {
        target_session_id: String,
    },
    SwitchModel {
        session_id: String,
        model_id: String,
    },
}

// ── Daemon → Client ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonMessage {
    Connected {
        session_id: String,
        model_name: String,
        context_window: usize,
        title: Option<String>,
        total_tokens_used: usize,
    },
    StartOutput {
        prefix: Option<String>,
    },
    TextChunk {
        prefix: Option<String>,
        chunk: String,
    },
    ToolCall {
        name: String,
        args: String,
        prefix: Option<String>,
        display_script: Option<String>,
    },
    ToolResult {
        text: String,
        display_as: Option<String>,
        prefix: Option<String>,
    },
    Info(String),
    ResponseDone {
        thread_id: Option<String>,
        token_usage: Option<TokenUsage>,
    },
    UserInputEcho(String),
    SubscriptionEvent {
        name: String,
        text: String,
        prefix: Option<String>,
    },
    OAuthRequired {
        auth_url: String,
    },
    ThinkingStart {
        prefix: Option<String>,
    },
    ThinkingEnd {
        prefix: Option<String>,
    },
    ThinkingChunk {
        prefix: Option<String>,
        chunk: String,
    },
    Error(String),
    /// Batch replay of history messages, sent on connect/load.
    Replay(Vec<DaemonMessage>),
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
}

// ── Supporting types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub title: Option<String>,
    pub last_updated: String,
    pub total_tokens_used: usize,
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
