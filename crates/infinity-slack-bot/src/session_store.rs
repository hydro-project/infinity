use std::collections::HashMap;
use std::path::PathBuf;

use crate::BoxError;

/// A pending approval/choice waiting for user response.
#[derive(Clone)]
pub struct PendingChoice {
    pub choice_id: String,
    pub choices: Vec<String>,
}

/// Persists the Slack thread_ts → Infinity session_id mapping to disk.
pub struct SessionStore {
    map: HashMap<String, String>,
    /// thread_ts → pending choice (not persisted; transient)
    pending_choices: HashMap<String, PendingChoice>,
    path: PathBuf,
}

impl SessionStore {
    /// Load from disk, or start empty if the file doesn't exist.
    pub fn load(path: PathBuf) -> Result<Self, BoxError> {
        let map = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e.into()),
        };
        tracing::info!(
            "loaded {} session mappings from {}",
            map.len(),
            path.display()
        );
        Ok(Self {
            map,
            pending_choices: HashMap::new(),
            path,
        })
    }

    pub fn get(&self, thread_ts: &str) -> Option<&String> {
        self.map.get(thread_ts)
    }

    pub fn insert(&mut self, thread_ts: String, session_id: String) {
        self.map.insert(thread_ts, session_id);
        if let Err(e) = self.save() {
            tracing::error!("failed to persist session map: {e}");
        }
    }

    /// Insert without persisting to disk (for use in tests or transient state).
    pub fn insert_ephemeral(&mut self, thread_ts: String, session_id: String) {
        self.map.insert(thread_ts, session_id);
    }

    /// Create an empty store with no backing file (for tests).
    pub fn empty() -> Self {
        Self {
            map: HashMap::new(),
            pending_choices: HashMap::new(),
            path: PathBuf::new(),
        }
    }

    pub fn values(&self) -> impl Iterator<Item = &String> {
        self.map.values()
    }

    pub fn set_pending_choice(&mut self, thread_ts: String, choice: PendingChoice) {
        self.pending_choices.insert(thread_ts, choice);
    }

    pub fn take_pending_choice(&mut self, thread_ts: &str) -> Option<PendingChoice> {
        self.pending_choices.remove(thread_ts)
    }

    fn save(&self) -> Result<(), BoxError> {
        let json = serde_json::to_string_pretty(&self.map)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "slack_sessions_test_{}_{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let path = tmp_path("missing");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::load(path.clone()).expect("load should succeed");
        assert!(store.get("any").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn insert_persists_to_disk() {
        let path = tmp_path("insert");
        let _ = std::fs::remove_file(&path);

        {
            let mut store = SessionStore::load(path.clone()).expect("load should succeed");
            store.insert("thread1".into(), "session1".into());
        }

        // Reload from disk
        let store = SessionStore::load(path.clone()).expect("reload should succeed");
        assert_eq!(store.get("thread1").expect("key should exist"), "session1");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_existing_file() {
        let path = tmp_path("existing");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, r#"{"t1":"s1","t2":"s2"}"#).expect("write should succeed");

        let store = SessionStore::load(path.clone()).expect("load should succeed");
        assert_eq!(store.get("t1").expect("t1 should exist"), "s1");
        assert_eq!(store.get("t2").expect("t2 should exist"), "s2");
        assert!(store.get("t3").is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn values_iterates_all_sessions() {
        let path = tmp_path("values");
        let _ = std::fs::remove_file(&path);

        let mut store = SessionStore::load(path.clone()).expect("load should succeed");
        store.insert("t1".into(), "s1".into());
        store.insert("t2".into(), "s2".into());

        let mut vals: Vec<&String> = store.values().collect();
        vals.sort();
        assert_eq!(vals, vec!["s1", "s2"]);

        let _ = std::fs::remove_file(&path);
    }
}
