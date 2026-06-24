//! Runtime state accessible from dataflow `q!()` closures.
//!
//! Initialized by the Slack sidecar's `create()` function.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::session_store::SessionStore;

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
}

static RUNTIME: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();

/// Initialize runtime state. Called once from the Slack sidecar's `create()`.
pub fn init(config: &'static Config, sessions: Arc<Mutex<SessionStore>>) {
    let _ = RUNTIME.set(Runtime {
        config,
        sessions,
        pending_input: Arc::new(Mutex::new(HashMap::new())),
        channels: Arc::new(Mutex::new(HashMap::new())),
        had_tool_call: Arc::new(Mutex::new(HashMap::new())),
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
        }));
        Runtime {
            config,
            sessions: Arc::new(Mutex::new(SessionStore::empty())),
            pending_input: Arc::new(Mutex::new(HashMap::new())),
            channels: Arc::new(Mutex::new(HashMap::new())),
            had_tool_call: Arc::new(Mutex::new(HashMap::new())),
        }
    });
}

/// Get the runtime state. Panics if not yet initialized.
pub fn get() -> &'static Runtime {
    RUNTIME.get().expect("bug: runtime not initialized")
}
