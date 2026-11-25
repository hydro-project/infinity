use infinity_agent_core::ThreadId;
use std::path::Path;
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::persist::{self, PersistError, SESSIONS_VERSION};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionEntry {
    pub cwd: PathBuf,
    /// When true, only user text input should re-awaken the agent.
    #[serde(default)]
    pub shut_down: bool,
    /// When true, the agent is idle (no active work).
    #[serde(default)]
    pub idle: bool,
    /// When true, the session has been migrated away and should not be displayed.
    #[serde(default)]
    pub archived: bool,
}

impl SessionEntry {
    pub fn status(&self, has_pending_choices: bool) -> infinity_protocol::SessionStatus {
        if self.archived {
            infinity_protocol::SessionStatus::Archived
        } else if has_pending_choices {
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

/// The session index (`{state_dir}/sessions.json`): every session's working
/// directory and lifecycle flags, keyed by root thread id. Written atomically
/// (see [`crate::persist::write_atomic`]) so a crash mid-save never leaves a
/// truncated index.
#[derive(Serialize, Deserialize)]
pub struct SessionStore {
    /// Format version (see [`SESSIONS_VERSION`]); absent in files written
    /// before versioning.
    #[serde(default)]
    pub version: u32,
    pub sessions: HashMap<ThreadId, SessionEntry>,
    #[serde(skip)]
    path: String,
    #[serde(skip)]
    change_tx: Option<mpsc::UnboundedSender<ThreadId>>,
}

impl SessionStore {
    /// Load the index from `path`. A missing file yields an empty store. An
    /// unreadable one is logged and also yields an empty store — but a file
    /// written by a newer daemon (unsupported version) is a hard error, since
    /// saving over it would destroy data this daemon cannot interpret.
    pub fn load(path: &str, change_tx: mpsc::UnboundedSender<ThreadId>) -> Self {
        let sessions = match Self::read_sessions(path) {
            Ok(sessions) => sessions,
            Err(error @ PersistError::UnsupportedVersion { .. }) => {
                panic!("cannot load session index: {error}");
            }
            Err(error) => {
                tracing::error!(%error, "failed to load session index; starting empty");
                HashMap::new()
            }
        };

        Self {
            version: SESSIONS_VERSION,
            sessions,
            path: path.to_owned(),
            change_tx: Some(change_tx),
        }
    }

    fn read_sessions(path: &str) -> Result<HashMap<ThreadId, SessionEntry>, PersistError> {
        let path = Path::new(path);
        let Some(raw) = persist::read_json::<serde_json::Value>(path)? else {
            return Ok(HashMap::new());
        };
        // Current (and pre-versioning) HashMap format.
        if let Ok(store) = serde_json::from_value::<Self>(raw.clone()) {
            persist::check_version(path, store.version, SESSIONS_VERSION)?;
            return Ok(store.sessions);
        }
        // Legacy Vec format.
        #[derive(Deserialize)]
        struct LegacyEntry {
            thread_id: ThreadId,
        }
        #[derive(Deserialize)]
        struct LegacyStore {
            sessions: Vec<LegacyEntry>,
        }
        let legacy: LegacyStore =
            serde_json::from_value(raw).map_err(|e| PersistError::Corrupt {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        Ok(legacy
            .sessions
            .into_iter()
            .map(|e| {
                (
                    e.thread_id,
                    SessionEntry {
                        cwd: std::env::current_dir().expect("failed to get current directory"),
                        shut_down: false,
                        idle: false,
                        archived: false,
                    },
                )
            })
            .collect())
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        persist::write_json_atomic(Path::new(&self.path), self)?;
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
                idle: false,
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

    pub fn mark_idle(&mut self, session_id: &ThreadId<str>) {
        if let Some(entry) = self.sessions.get_mut(session_id)
            && !entry.idle
        {
            entry.idle = true;
            self.notify(session_id);
        }
    }

    pub fn clear_idle(&mut self, session_id: &ThreadId<str>) -> bool {
        if let Some(entry) = self.sessions.get_mut(session_id)
            && entry.idle
        {
            entry.idle = false;
            self.notify(session_id);
            return true;
        }
        false
    }

    pub fn is_idle(&self, session_id: &ThreadId<str>) -> bool {
        self.sessions
            .get(session_id)
            .map(|e| e.idle)
            .unwrap_or(false)
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
