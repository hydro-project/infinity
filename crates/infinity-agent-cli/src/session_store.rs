use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionEntry {
    pub thread_id: String,
    pub total_tokens_used: usize,
    /// ISO 8601 timestamp string (stored as string to avoid chrono serde feature).
    pub last_updated: String,
    #[serde(default)]
    pub title: Option<String>,
}

impl SessionEntry {
    /// Parse the `last_updated` field into a `DateTime<Utc>`.
    pub fn last_updated_dt(&self) -> Option<DateTime<Utc>> {
        self.last_updated.parse().ok()
    }

    /// Format the `last_updated` field for display.
    pub fn last_updated_display(&self) -> String {
        self.last_updated_dt()
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| self.last_updated.clone())
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct SessionStore {
    pub sessions: Vec<SessionEntry>,
}

impl SessionStore {
    pub fn load(path: &str) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Insert or update a session entry, then re-sort by most recent first.
    pub fn upsert(&mut self, thread_id: &str, total_tokens_used: usize, title: Option<String>) {
        let now = Utc::now().to_rfc3339();
        if let Some(entry) = self.sessions.iter_mut().find(|e| e.thread_id == thread_id) {
            entry.total_tokens_used = total_tokens_used;
            entry.last_updated = now;
            if title.is_some() {
                entry.title = title;
            }
        } else {
            self.sessions.push(SessionEntry {
                thread_id: thread_id.to_string(),
                total_tokens_used,
                last_updated: now,
                title,
            });
        }
        // Sort by last_updated descending (ISO 8601 strings sort lexicographically).
        self.sessions
            .sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
    }
}
