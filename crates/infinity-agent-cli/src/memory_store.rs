use async_trait::async_trait;
use infinity_agent_core::message::InputMessage;
use infinity_agent_core::traits::{ConversationStore, InputSender, StateStore};
use rig::message::Message;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ── Error type ──

#[derive(Debug)]
pub struct MemoryError(pub String);
impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for MemoryError {}

// ── In-memory conversation store with per-thread file persistence ──

#[derive(Clone)]
pub struct InMemoryConversationStore {
    /// session_id -> ordered messages
    #[expect(clippy::type_complexity, reason = "shared state")]
    messages: Arc<Mutex<HashMap<String, Vec<(Message, String)>>>>,
    /// thread_id -> ThreadInfo
    threads: Arc<Mutex<HashMap<String, ThreadInfo>>>,
    /// thread_id -> tool_result_id -> display_as text.
    /// Persisted separately because rig's `Message` type does not carry display_as.
    display_as_map: Arc<Mutex<HashMap<String, HashMap<String, String>>>>,
    /// thread_id -> compaction summaries
    compaction_summaries: Arc<Mutex<HashMap<String, Vec<CompactionSummary>>>>,
    /// Directory where per-thread JSON files are stored. `None` disables persistence.
    dir: Option<PathBuf>,
    /// Tracks which thread IDs have already been loaded (or attempted) from disk.
    loaded: Arc<Mutex<HashSet<String>>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ThreadInfo {
    parent_thread_id: Option<String>,
    root_thread_id: String,
    spawn_message_order: Option<i64>,
    spawn_tool_call_id: Option<String>,
    closed: bool,
    is_subscription_event: bool,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    is_compaction: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct CompactionSummary {
    summary: String,
    up_to_order: i64,
}

/// Per-thread snapshot written to `{dir}/{thread_id}.json`.
#[derive(Serialize, Deserialize)]
struct ThreadSnapshot {
    messages: Vec<(Message, String)>,
    thread_info: ThreadInfo,
    #[serde(default)]
    display_as: HashMap<String, String>,
    #[serde(default)]
    compaction_summaries: Vec<CompactionSummary>,
}

impl InMemoryConversationStore {
    /// Create a store that persists each thread to its own JSON file under `dir`.
    pub fn new_with_dir(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).ok();
        Self {
            messages: Arc::new(Mutex::new(HashMap::new())),
            threads: Arc::new(Mutex::new(HashMap::new())),
            display_as_map: Arc::new(Mutex::new(HashMap::new())),
            compaction_summaries: Arc::new(Mutex::new(HashMap::new())),
            dir: Some(dir),
            loaded: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Write a single thread's data to `{dir}/{thread_id}.json`.
    /// No-op when persistence is disabled.
    fn save_thread(&self, thread_id: &str) {
        let Some(ref dir) = self.dir else { return };
        let messages = self.messages.lock().unwrap();
        let threads = self.threads.lock().unwrap();
        let display_as_map = self.display_as_map.lock().unwrap();
        let compaction_summaries = self.compaction_summaries.lock().unwrap();

        let snapshot = ThreadSnapshot {
            messages: messages.get(thread_id).cloned().unwrap_or_default(),
            thread_info: threads.get(thread_id).cloned().unwrap_or(ThreadInfo {
                parent_thread_id: None,
                root_thread_id: thread_id.to_string(),
                spawn_message_order: None,
                spawn_tool_call_id: None,
                closed: false,
                is_subscription_event: false,
                title: None,
                is_compaction: false,
            }),
            display_as: display_as_map.get(thread_id).cloned().unwrap_or_default(),
            compaction_summaries: compaction_summaries
                .get(thread_id)
                .cloned()
                .unwrap_or_default(),
        };

        let path = dir.join(format!("{}.json", thread_id));
        if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
            std::fs::write(path, json).ok();
        }
    }

    /// Ensure a thread's data is loaded from disk into the in-memory caches.
    /// No-op when persistence is disabled or the thread was already loaded.
    fn ensure_thread_loaded(&self, thread_id: &str) {
        let Some(ref dir) = self.dir else { return };

        // Fast-path: already loaded.
        let mut loaded = self.loaded.lock().unwrap();
        if loaded.contains(thread_id) {
            return;
        }

        // keep it locked while we read to prevent concurrent loading of the same thread

        // Try to read the per-thread file.
        let path = dir.join(format!("{}.json", thread_id));
        if let Ok(json) = std::fs::read_to_string(&path) {
            if let Ok(snapshot) = serde_json::from_str::<ThreadSnapshot>(&json) {
                let mut messages = self.messages.lock().unwrap();
                let mut threads = self.threads.lock().unwrap();
                let mut display_as_map = self.display_as_map.lock().unwrap();
                let mut compaction_summaries = self.compaction_summaries.lock().unwrap();

                assert!(
                    messages
                        .insert(thread_id.to_string(), snapshot.messages)
                        .is_none()
                );
                assert!(
                    threads
                        .insert(thread_id.to_string(), snapshot.thread_info)
                        .is_none()
                );
                assert!(
                    display_as_map
                        .insert(thread_id.to_string(), snapshot.display_as)
                        .is_none()
                );
                assert!(
                    compaction_summaries
                        .insert(thread_id.to_string(), snapshot.compaction_summaries)
                        .is_none()
                );
            }
        }

        // Mark as loaded even if the file didn't exist — avoids repeated fs checks.
        loaded.insert(thread_id.to_string());
    }

    /// Record the display_as text for a tool result so it survives persistence.
    pub fn save_display_as(&self, thread_id: &str, tool_result_id: &str, display_as: &str) {
        self.ensure_thread_loaded(thread_id);
        {
            let mut map = self.display_as_map.lock().unwrap();
            map.entry(thread_id.to_string())
                .or_default()
                .insert(tool_result_id.to_string(), display_as.to_string());
        }
        self.save_thread(thread_id);
    }

    /// Look up a previously stored display_as for a tool result.
    pub fn get_display_as(&self, thread_id: &str, tool_result_id: &str) -> Option<String> {
        self.ensure_thread_loaded(thread_id);
        let map = self.display_as_map.lock().unwrap();
        map.get(thread_id)
            .and_then(|inner| inner.get(tool_result_id).cloned())
    }

    /// Set a friendly title for a thread.
    pub fn set_title(&self, thread_id: &str, title: &str) {
        self.ensure_thread_loaded(thread_id);
        {
            let mut threads = self.threads.lock().unwrap();
            if let Some(t) = threads.get_mut(thread_id) {
                t.title = Some(title.to_string());
            }
        }
        self.save_thread(thread_id);
    }

    /// Get the friendly title for a thread, if set.
    pub fn get_title(&self, thread_id: &str) -> Option<String> {
        self.ensure_thread_loaded(thread_id);
        let threads = self.threads.lock().unwrap();
        threads.get(thread_id).and_then(|t| t.title.clone())
    }
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    type Error = MemoryError;

    async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let inserted = {
            let mut threads = self.threads.lock().unwrap();
            if threads.contains_key(thread_id) {
                false
            } else {
                threads.insert(
                    thread_id.to_string(),
                    ThreadInfo {
                        parent_thread_id: None,
                        root_thread_id: thread_id.to_string(),
                        spawn_message_order: None,
                        spawn_tool_call_id: None,
                        closed: false,
                        is_subscription_event: false,
                        title: None,
                        is_compaction: false,
                    },
                );
                true
            }
        };
        if inserted {
            self.save_thread(thread_id);
        }
        Ok(())
    }

    async fn load_history_up_to(
        &self,
        session_id: &str,
        start_from: Option<i64>,
        up_to: Option<i64>,
    ) -> Result<Vec<Message>, MemoryError> {
        self.ensure_thread_loaded(session_id);
        let msgs = self.messages.lock().unwrap();
        Ok(msgs
            .get(session_id)
            .map(|v| {
                let start = start_from.unwrap_or(0) as usize;
                let end = up_to.map(|u| u as usize).unwrap_or(v.len());
                v[start..end].iter().map(|(m, _)| m.clone()).collect()
            })
            .unwrap_or_default())
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: Vec<(Message, String)>,
    ) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(session_id);
        {
            let mut store = self.messages.lock().unwrap();
            let entry = store.entry(session_id.to_string()).or_default();
            entry.extend(messages);
        }
        self.save_thread(session_id);
        Ok(())
    }

    async fn spawn_thread(
        &self,
        parent_thread_id: &str,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
    ) -> Result<String, MemoryError> {
        self.ensure_thread_loaded(parent_thread_id);
        let new_id = uuid::Uuid::new_v4().to_string();
        let spawn_message_order;
        let root;
        {
            let threads = self.threads.lock().unwrap();
            let msgs = self.messages.lock().unwrap();
            spawn_message_order = msgs
                .get(parent_thread_id)
                .map(|v| v.len() as i64)
                .unwrap_or(0);
            root = threads
                .get(parent_thread_id)
                .map(|t| t.root_thread_id.clone())
                .unwrap_or_else(|| parent_thread_id.to_string());
        }
        {
            let mut loaded = self.loaded.lock().unwrap();

            {
                let mut messages = self.messages.lock().unwrap();
                messages.insert(new_id.clone(), vec![]);
            }

            {
                let mut threads = self.threads.lock().unwrap();
                threads.insert(
                    new_id.clone(),
                    ThreadInfo {
                        parent_thread_id: Some(parent_thread_id.to_string()),
                        root_thread_id: root,
                        spawn_message_order: Some(spawn_message_order),
                        spawn_tool_call_id: Some(spawn_tool_call_id.to_string()),
                        closed: false,
                        is_subscription_event: is_for_subscription_event,
                        title: None,
                        is_compaction: false,
                    },
                );
            }

            {
                let mut display_as_map = self.display_as_map.lock().unwrap();
                display_as_map.insert(new_id.clone(), HashMap::new());
            }

            loaded.insert(new_id.clone());
        }

        self.save_thread(&new_id);
        Ok(new_id)
    }

    async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let threads = self.threads.lock().unwrap();
        Ok(threads.get(thread_id).map(|t| t.closed).unwrap_or(false))
    }

    async fn close_thread(&self, thread_id: &str) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(thread_id);
        {
            let mut threads = self.threads.lock().unwrap();
            if let Some(t) = threads.get_mut(thread_id) {
                t.closed = true;
            }
        }
        self.save_thread(thread_id);
        Ok(())
    }

    async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let threads = self.threads.lock().unwrap();
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_subscription_event)
            .unwrap_or(false))
    }

    async fn get_thread_parent_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>, MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let threads = self.threads.lock().unwrap();
        Ok(threads.get(thread_id).and_then(|t| {
            match (&t.parent_thread_id, &t.spawn_tool_call_id) {
                (Some(p), Some(tc)) => Some((p.clone(), tc.clone())),
                _ => None,
            }
        }))
    }

    async fn get_ancestor_chain(&self, thread_id: &str) -> Result<Vec<(String, i64)>, MemoryError> {
        let mut result = Vec::new();
        let mut current = thread_id.to_string();
        loop {
            self.ensure_thread_loaded(&current);
            let info = {
                let threads = self.threads.lock().unwrap();
                threads.get(&current).cloned()
            };
            match info {
                Some(t) if t.parent_thread_id.is_some() => {
                    let parent = t.parent_thread_id.unwrap();
                    let order = t.spawn_message_order.unwrap_or(0);
                    result.push((parent.clone(), order));
                    current = parent;
                }
                _ => break,
            }
        }
        result.reverse();
        Ok(result)
    }

    async fn save_compaction_summary(
        &self,
        thread_id: &str,
        summary: &str,
        up_to_order: i64,
    ) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(thread_id);
        {
            let mut cs = self.compaction_summaries.lock().unwrap();
            cs.entry(thread_id.to_string())
                .or_default()
                .push(CompactionSummary {
                    summary: summary.to_string(),
                    up_to_order,
                });
        }
        self.save_thread(thread_id);
        Ok(())
    }

    async fn load_latest_compaction_summary_up_to(
        &self,
        thread_id: &str,
        up_to_order: Option<i64>,
    ) -> Result<Option<(String, i64)>, MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let cs = self.compaction_summaries.lock().unwrap();
        Ok(cs.get(thread_id).and_then(|v| {
            v.iter()
                .rev()
                .find(|s| up_to_order.map_or(true, |n| s.up_to_order <= n))
                .map(|s| (s.summary.clone(), s.up_to_order))
        }))
    }

    async fn is_compaction_thread(&self, thread_id: &str) -> Result<bool, MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let threads = self.threads.lock().unwrap();
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_compaction)
            .unwrap_or(false))
    }

    async fn mark_thread_as_compaction(&self, thread_id: &str) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(thread_id);
        {
            let mut threads = self.threads.lock().unwrap();
            if let Some(t) = threads.get_mut(thread_id) {
                t.is_compaction = true;
            }
        }
        self.save_thread(thread_id);
        Ok(())
    }

    async fn get_thread_spawn_order(&self, thread_id: &str) -> Result<Option<i64>, MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let threads = self.threads.lock().unwrap();
        Ok(threads.get(thread_id).and_then(|t| t.spawn_message_order))
    }
}

// ── In-memory state store ──

#[derive(Clone)]
pub struct InMemoryStateStore {
    #[expect(clippy::type_complexity, reason = "shared state")]
    processed_ids: Arc<Mutex<HashMap<String, (HashSet<String>, HashSet<String>)>>>,
    metadata: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// Per-thread active subscriptions: thread_id → set of tool_call_ids.
    subscriptions: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

impl InMemoryStateStore {
    pub fn new() -> Self {
        Self {
            processed_ids: Arc::new(Mutex::new(HashMap::new())),
            metadata: Arc::new(Mutex::new(HashMap::new())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl StateStore for InMemoryStateStore {
    type Error = MemoryError;

    async fn get_processed_ids(
        &self,
        thread_id: &str,
    ) -> Result<(HashSet<String>, HashSet<String>), MemoryError> {
        let store = self.processed_ids.lock().unwrap();
        Ok(store
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| (HashSet::new(), HashSet::new())))
    }

    async fn add_processed_message_ids(
        &self,
        thread_id: &str,
        message_ids: Vec<String>,
    ) -> Result<(), MemoryError> {
        let mut store = self.processed_ids.lock().unwrap();
        let entry = store
            .entry(thread_id.to_string())
            .or_insert_with(|| (HashSet::new(), HashSet::new()));
        entry.0.extend(message_ids);
        Ok(())
    }

    async fn add_processed_tool_calls(
        &self,
        thread_id: &str,
        tool_call_ids: Vec<String>,
    ) -> Result<(), MemoryError> {
        let mut store = self.processed_ids.lock().unwrap();
        let entry = store
            .entry(thread_id.to_string())
            .or_insert_with(|| (HashSet::new(), HashSet::new()));
        entry.1.extend(tool_call_ids);
        Ok(())
    }

    async fn get_metadata(
        &self,
        root_thread_id: &str,
    ) -> Result<Option<serde_json::Value>, MemoryError> {
        let store = self.metadata.lock().unwrap();
        Ok(store.get(root_thread_id).cloned())
    }

    async fn set_metadata(
        &self,
        root_thread_id: &str,
        metadata: serde_json::Value,
    ) -> Result<(), MemoryError> {
        let mut store = self.metadata.lock().unwrap();
        store.insert(root_thread_id.to_string(), metadata);
        Ok(())
    }

    async fn get_active_subscriptions(&self, thread_id: &str) -> Result<Vec<String>, MemoryError> {
        let store = self.subscriptions.lock().unwrap();
        Ok(store
            .get(thread_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default())
    }

    async fn add_active_subscription(
        &self,
        thread_id: &str,
        tool_call_id: &str,
    ) -> Result<(), MemoryError> {
        let mut store = self.subscriptions.lock().unwrap();
        store
            .entry(thread_id.to_string())
            .or_default()
            .insert(tool_call_id.to_string());
        Ok(())
    }

    async fn remove_active_subscription(
        &self,
        thread_id: &str,
        tool_call_id: &str,
    ) -> Result<(), MemoryError> {
        let mut store = self.subscriptions.lock().unwrap();
        if let Some(subs) = store.get_mut(thread_id) {
            subs.remove(tool_call_id);
        }
        Ok(())
    }
}

// ── In-memory message sender (pushes to mpsc) ──

#[derive(Clone)]
pub struct InMemoryMessageSender {
    tx: tokio::sync::mpsc::UnboundedSender<(InputMessage, String)>,
}

impl InMemoryMessageSender {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<(InputMessage, String)>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl InputSender for InMemoryMessageSender {
    type Error = MemoryError;

    async fn send_to_input_queue(
        &self,
        message: InputMessage,
        _group_id: &str,
        dedup_id: &str,
    ) -> Result<(), MemoryError> {
        self.tx
            .send((message, dedup_id.to_string()))
            .map_err(|e| MemoryError(format!("channel send failed: {}", e)))?;
        Ok(())
    }
}
