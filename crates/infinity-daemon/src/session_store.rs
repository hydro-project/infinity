use std::{collections::HashMap, path::PathBuf};

use infinity_protocol::DaemonMessage;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// A pending user choice request awaiting user response.
#[derive(Clone, Debug)]
pub struct PendingChoice {
    pub id: String,
    pub message: DaemonMessage,
    pub response_url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionEntry {
    pub cwd: PathBuf,
    /// When true, only user text input should re-awaken the agent.
    #[serde(default)]
    pub shut_down: bool,
    /// When true, the agent is idle (no active work).
    #[serde(default)]
    pub idle: bool,
}

impl SessionEntry {
    pub fn status(&self, has_pending_choices: bool) -> infinity_protocol::SessionStatus {
        if has_pending_choices {
            infinity_protocol::SessionStatus::WaitingForChoice
        } else if self.idle {
            infinity_protocol::SessionStatus::Idle
        } else if self.shut_down {
            infinity_protocol::SessionStatus::Stopped
        } else {
            infinity_protocol::SessionStatus::Running
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct SessionStore {
    pub sessions: HashMap<String, SessionEntry>,
    #[serde(skip)]
    path: String,
    #[serde(skip)]
    change_tx: Option<mpsc::UnboundedSender<String>>,
}

impl SessionStore {
    pub fn load(path: &str, change_tx: mpsc::UnboundedSender<String>) -> Self {
        let sessions = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| {
                // Try new HashMap format first
                if let Ok(store) = serde_json::from_str::<Self>(&s) {
                    return Some(store.sessions);
                }
                // Fall back to legacy Vec format
                #[derive(Deserialize)]
                struct LegacyEntry {
                    thread_id: String,
                }
                #[derive(Deserialize)]
                struct LegacyStore {
                    sessions: Vec<LegacyEntry>,
                }
                if let Ok(legacy) = serde_json::from_str::<LegacyStore>(&s) {
                    let map = legacy
                        .sessions
                        .into_iter()
                        .map(|e| {
                            (
                                e.thread_id,
                                SessionEntry {
                                    cwd: std::env::current_dir()
                                        .expect("failed to get current directory"),
                                    shut_down: false,
                                    idle: false,
                                },
                            )
                        })
                        .collect();
                    return Some(map);
                }
                None
            })
            .unwrap_or_default();

        Self {
            sessions,
            path: path.to_string(),
            change_tx: Some(change_tx),
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }

    pub fn notify(&self, session_id: &str) {
        if let Some(ref tx) = self.change_tx {
            let _ = tx.send(session_id.to_string());
        }
    }

    pub fn create(&mut self, session_id: &str, cwd: PathBuf) {
        self.sessions.insert(
            session_id.to_string(),
            SessionEntry {
                cwd,
                shut_down: false,
                idle: false,
            },
        );
        self.notify(session_id);
    }

    pub fn get_cwd(&self, session_id: &str) -> &PathBuf {
        &self
            .sessions
            .get(session_id)
            .expect("bug: session not found in store")
            .cwd
    }

    pub fn mark_shut_down(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.shut_down = true;
            self.notify(session_id);
        }
    }

    pub fn clear_shut_down(&mut self, session_id: &str) {
        tracing::trace!("Clearing shut down status for {}", session_id);
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.shut_down = false;
            self.notify(session_id);
        }
    }

    pub fn mark_idle(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id)
            && !entry.idle
        {
            entry.idle = true;
            self.notify(session_id);
        }
    }

    pub fn clear_idle(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id)
            && entry.idle
        {
            entry.idle = false;
            self.notify(session_id);
        }
    }

    pub fn is_idle(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|e| e.idle)
            .unwrap_or(false)
    }

    pub fn is_shut_down(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|e| e.shut_down)
            .unwrap_or(false)
    }
}
