//! Runtime state accessible from dataflow `q!()` closures.
//!
//! Initialized at startup by the bot binary (or by [`ensure_test_init`] in
//! tests).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use infinity_protocol::{ModelInfo, ModelRef};

use crate::config::Config;
use crate::session_store::SessionStore;

/// Per-thread FIFO of in-flight tool tasks `(task_id, title, details)`.
type ToolTaskQueue = VecDeque<(String, String, String)>;

/// Shared runtime state for the dataflow.
pub struct Runtime {
    pub config: &'static Config,
    pub sessions: Arc<Mutex<SessionStore>>,
    /// Per-thread pending input text (stashed until Connected arrives).
    pub pending_input: Arc<Mutex<HashMap<String, String>>>,
    /// Per-thread channel mapping (thread_ts → channel_id).
    pub channels: Arc<Mutex<HashMap<String, String>>>,
    /// Per-thread flag: true if a tool call happened in the current response turn.
    pub had_tool_call: Arc<Mutex<HashMap<String, bool>>>,
    /// Tracks posted choice-button messages so they can be dismissed when the
    /// choice is resolved without a Slack button click (e.g. timeout/auto-select,
    /// another client answering, or interruption).
    /// Key: choice_id, Value: (channel, message_ts).
    pub choice_messages: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// The current default model for new sessions. Overrides the daemon's
    /// default when set. Can be changed at runtime via Slack commands.
    pub default_model: Arc<Mutex<Option<ModelRef>>>,
    /// Available models fetched from the daemon's Welcome message.
    pub available_models: Arc<Mutex<Vec<ModelInfo>>>,
    /// Per-thread FIFO of in-flight tool tasks `(task_id, title)` for
    /// streaming task updates. Pushed on ToolCall, popped on ToolResult;
    /// drained (marked complete) when the stream stops.
    pub tool_tasks: Arc<Mutex<HashMap<String, ToolTaskQueue>>>,
    /// Monotonic id source for streaming task updates.
    pub tool_task_seq: Arc<AtomicU64>,
    /// Last title set on each Slack thread (dedup for SetThreadTitle).
    pub thread_titles: Arc<Mutex<HashMap<String, String>>>,
    /// Users who have already received suggested prompts this run.
    pub app_home_seen: Arc<Mutex<HashSet<String>>>,
}

static RUNTIME: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();

/// Initialize runtime state. Called once at startup by the bot binary.
pub fn init(config: &'static Config, sessions: Arc<Mutex<SessionStore>>) {
    let default_model = config.default_model.clone();
    let _ = RUNTIME.set(Runtime {
        config,
        sessions,
        pending_input: Arc::new(Mutex::new(HashMap::new())),
        channels: Arc::new(Mutex::new(HashMap::new())),
        had_tool_call: Arc::new(Mutex::new(HashMap::new())),
        choice_messages: Arc::new(Mutex::new(HashMap::new())),
        default_model: Arc::new(Mutex::new(default_model)),
        available_models: Arc::new(Mutex::new(Vec::new())),
        tool_tasks: Arc::new(Mutex::new(HashMap::new())),
        tool_task_seq: Arc::new(AtomicU64::new(0)),
        thread_titles: Arc::new(Mutex::new(HashMap::new())),
        app_home_seen: Arc::new(Mutex::new(HashSet::new())),
    });
}

/// Initialize with defaults if not already initialized (for tests).
pub fn ensure_test_init() {
    let _ = RUNTIME.get_or_init(|| {
        let config: &'static Config = Box::leak(Box::new(Config {
            bot_token: String::new(),
            app_token: String::new(),
            default_cwd: std::path::PathBuf::from("/tmp"),
            allowed_users: vec![],
            default_model: None,
            path: None,
        }));
        Runtime {
            config,
            sessions: Arc::new(Mutex::new(SessionStore::empty())),
            pending_input: Arc::new(Mutex::new(HashMap::new())),
            channels: Arc::new(Mutex::new(HashMap::new())),
            had_tool_call: Arc::new(Mutex::new(HashMap::new())),
            choice_messages: Arc::new(Mutex::new(HashMap::new())),
            default_model: Arc::new(Mutex::new(None)),
            available_models: Arc::new(Mutex::new(Vec::new())),
            tool_tasks: Arc::new(Mutex::new(HashMap::new())),
            tool_task_seq: Arc::new(AtomicU64::new(0)),
            thread_titles: Arc::new(Mutex::new(HashMap::new())),
            app_home_seen: Arc::new(Mutex::new(HashSet::new())),
        }
    });
}

/// Get the runtime state. Panics if not yet initialized.
pub fn get() -> &'static Runtime {
    RUNTIME.get().expect("bug: runtime not initialized")
}
