use std::{collections::HashMap, path::PathBuf};

use chrono::Utc;
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
    pub total_tokens_used: usize,
    pub last_updated: String,
    #[serde(default)]
    pub title: Option<String>,
    pub cwd: PathBuf,
    /// When true, only user text input should re-awaken the agent.
    /// Set on explicit shutdown; cleared on next user input.
    #[serde(default)]
    pub shut_down: bool,
    /// When true, the session was cleaned up because it went idle (not explicit shutdown).
    /// Shows as "Idle" in session list instead of "Stopped".
    #[serde(default)]
    pub idle_cleaned: bool,
    /// Pending user choice requests (transient, not persisted).
    #[serde(skip)]
    pub pending_choices: Vec<PendingChoice>,
}

impl SessionEntry {
    pub fn status(&self) -> infinity_protocol::SessionStatus {
        if !self.pending_choices.is_empty() {
            infinity_protocol::SessionStatus::WaitingForChoice
        } else if self.shut_down {
            infinity_protocol::SessionStatus::Stopped
        } else if self.idle_cleaned {
            infinity_protocol::SessionStatus::Idle
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
                    total_tokens_used: usize,
                    last_updated: String,
                    #[serde(default)]
                    title: Option<String>,
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
                                    total_tokens_used: e.total_tokens_used,
                                    last_updated: e.last_updated,
                                    title: e.title,
                                    cwd: std::env::current_dir().unwrap(),
                                    shut_down: false,
                                    idle_cleaned: false,
                                    pending_choices: Vec::new(),
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

    pub fn update(&mut self, session_id: &str, total_tokens_used: usize, title: Option<String>) {
        let now = Utc::now().to_rfc3339();
        let entry = self.sessions.get_mut(session_id).unwrap();
        entry.total_tokens_used = total_tokens_used;
        entry.last_updated = now;
        if title.is_some() {
            entry.title = title;
        }
        self.notify(session_id);
    }

    pub fn create(&mut self, session_id: &str, cwd: PathBuf) {
        self.sessions.insert(
            session_id.to_string(),
            SessionEntry {
                total_tokens_used: 0,
                last_updated: Utc::now().to_rfc3339(),
                title: None,
                cwd,
                shut_down: false,
                idle_cleaned: false,
                pending_choices: Vec::new(),
            },
        );
    }

    pub fn get_cwd(&self, session_id: &str) -> &PathBuf {
        &self.sessions.get(session_id).unwrap().cwd
    }

    pub fn set_title(&mut self, session_id: &str, title: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.title = Some(title.to_string());
            self.notify(session_id);
        } else {
            self.update(session_id, 0, Some(title.to_string()));
        }
    }

    pub fn get_title(&self, session_id: &str) -> Option<String> {
        self.sessions.get(session_id).and_then(|e| e.title.clone())
    }

    pub fn mark_shut_down(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.shut_down = true;
        }
    }

    pub fn clear_shut_down(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.shut_down = false;
            entry.idle_cleaned = false;
        }
    }

    pub fn mark_idle_cleaned(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.idle_cleaned = true;
        }
    }

    pub fn is_idle_cleaned(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|e| e.idle_cleaned)
            .unwrap_or(false)
    }

    pub fn is_shut_down(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|e| e.shut_down)
            .unwrap_or(false)
    }
}
