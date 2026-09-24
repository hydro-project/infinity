use infinity_agent_core::ThreadId;
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionEntry {
    pub cwd: PathBuf,
    /// When true, the session was explicitly shut down and is quiescent
    /// (nothing running); only user text input re-awakens the agent. Set by
    /// `SessionManager::cleanup_session` *after* the session's drivers have
    /// been stopped, and cleared when new user input arrives — so the flag
    /// is only ever true while the session is actually not running.
    #[serde(default)]
    pub shut_down: bool,
    /// When true, the session has been migrated away and should not be displayed.
    #[serde(default)]
    pub archived: bool,
}

impl SessionEntry {
    /// Derive the session's status. `Running`/`Idle` come from live runtime
    /// state (`is_active`: does the session have active threads right now?),
    /// never from a persisted flag — persisted runtime state would go stale
    /// the moment the daemon restarts. Only durable facts (`archived`,
    /// pending choices, the enforced `shut_down` flag) are read from storage.
    pub fn status(
        &self,
        has_pending_choices: bool,
        is_active: bool,
    ) -> infinity_protocol::SessionStatus {
        if self.archived {
            infinity_protocol::SessionStatus::Archived
        } else if has_pending_choices {
            infinity_protocol::SessionStatus::WaitingForChoice
        } else if is_active {
            infinity_protocol::SessionStatus::Running
        } else if self.shut_down {
            infinity_protocol::SessionStatus::Stopped
        } else {
            infinity_protocol::SessionStatus::Idle
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct SessionStore {
    pub sessions: HashMap<ThreadId, SessionEntry>,
    #[serde(skip)]
    path: String,
    #[serde(skip)]
    change_tx: Option<mpsc::UnboundedSender<ThreadId>>,
}

impl SessionStore {
    pub fn load(path: &str, change_tx: mpsc::UnboundedSender<ThreadId>) -> Self {
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
                    thread_id: ThreadId,
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
                                    archived: false,
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
            path: path.to_owned(),
            change_tx: Some(change_tx),
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }

    pub fn notify(&self, session_id: &ThreadId<str>) {
        if let Some(ref tx) = self.change_tx {
            let _ = tx.send(session_id.to_owned());
        }
    }

    pub fn create(&mut self, session_id: &ThreadId<str>, cwd: PathBuf) {
        self.sessions.insert(
            session_id.to_owned(),
            SessionEntry {
                cwd,
                shut_down: false,
                archived: false,
            },
        );
        self.notify(session_id);
    }

    pub fn get_cwd(&self, session_id: &ThreadId<str>) -> &PathBuf {
        &self
            .sessions
            .get(session_id)
            .expect("bug: session not found in store")
            .cwd
    }

    pub fn mark_shut_down(&mut self, session_id: &ThreadId<str>) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.shut_down = true;
            self.notify(session_id);
        }
    }

    pub fn clear_shut_down(&mut self, session_id: &ThreadId<str>) -> bool {
        tracing::trace!("Clearing shut down status for {}", session_id);
        if let Some(entry) = self.sessions.get_mut(session_id)
            && entry.shut_down
        {
            entry.shut_down = false;
            self.notify(session_id);
            return true;
        }
        false
    }

    pub fn is_shut_down(&self, session_id: &ThreadId<str>) -> bool {
        self.sessions
            .get(session_id)
            .map(|e| e.shut_down)
            .unwrap_or(false)
    }

    pub fn mark_archived(&mut self, session_id: &ThreadId<str>) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.archived = true;
            self.notify(session_id);
        }
    }
}
