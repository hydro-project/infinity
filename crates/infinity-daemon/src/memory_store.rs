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
    /// thread_id -> tool_result_id -> display_as segments.
    /// Persisted separately because rig's `Message` type does not carry display_as.
    #[expect(clippy::type_complexity, reason = "shared state")]
    display_as_map: Arc<Mutex<HashMap<String, HashMap<String, Vec<rap_protocol::DisplaySegment>>>>>,
    /// thread_id -> compaction summaries
    compaction_summaries: Arc<Mutex<HashMap<String, Vec<CompactionSummary>>>>,
    /// Directory where per-thread JSON files are stored. `None` disables persistence.
    dir: Option<PathBuf>,
    /// Tracks which thread IDs have had their full data loaded from disk.
    loaded: Arc<Mutex<HashSet<String>>>,
    /// Tracks which thread IDs have had their metadata loaded from disk.
    metadata_loaded: Arc<Mutex<HashSet<String>>>,
    /// Optional sender to notify session store of changes (for SessionsUpdated broadcasts).
    change_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
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
    #[serde(default)]
    children: Vec<String>,
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
    #[serde(default, deserialize_with = "deserialize_display_as_map")]
    display_as: HashMap<String, Vec<rap_protocol::DisplaySegment>>,
    #[serde(default)]
    compaction_summaries: Vec<CompactionSummary>,
}

/// Deserialize display_as map, handling both the old `String` format and the
/// new `Vec<DisplaySegment>` format for backward compatibility.
fn deserialize_display_as_map<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<rap_protocol::DisplaySegment>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;

    let raw: HashMap<String, Value> = HashMap::deserialize(deserializer)?;
    let mut result = HashMap::new();
    for (k, v) in raw {
        let segments = match v {
            // Old format: plain string → wrap in a single Text segment
            Value::String(s) => vec![rap_protocol::DisplaySegment::Text(s)],
            // New format: array of segments
            Value::Array(_) => serde_json::from_value(v).unwrap_or_default(),
            _ => Vec::new(),
        };
        result.insert(k, segments);
    }
    Ok(result)
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
            metadata_loaded: Arc::new(Mutex::new(HashSet::new())),
            change_tx: None,
        }
    }

    /// Set the change notification sender. Called after construction.
    pub fn set_change_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        self.change_tx = Some(tx);
    }

    /// Notify that a session's thread tree changed.
    fn notify_session(&self, thread_id: &str) {
        if let Some(ref tx) = self.change_tx {
            let root = self.get_root_thread_id(thread_id);
            let _ = tx.send(root);
        }
    }

    /// Write a single thread's metadata to `{dir}/{thread_id}.meta.json`.
    fn save_thread_metadata(&self, thread_id: &str) {
        let Some(ref dir) = self.dir else { return };
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        if let Some(info) = threads.get(thread_id) {
            let path = dir.join(format!("{}.meta.json", thread_id));
            if let Ok(json) = serde_json::to_string_pretty(info) {
                std::fs::write(path, json).ok();
            }
        }
    }

    /// Write a single thread's data to `{dir}/{thread_id}.json` and metadata to `.meta.json`.
    /// No-op when persistence is disabled.
    fn save_thread(&self, thread_id: &str) {
        let Some(ref dir) = self.dir else { return };
        let messages = self.messages.lock().expect("bug: mutex poisoned");
        let display_as_map = self.display_as_map.lock().expect("bug: mutex poisoned");
        let compaction_summaries = self
            .compaction_summaries
            .lock()
            .expect("bug: mutex poisoned");

        let snapshot = ThreadSnapshot {
            messages: messages.get(thread_id).cloned().unwrap_or_default(),
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
        self.save_thread_metadata(thread_id);
    }

    /// Ensure a thread's metadata (ThreadInfo) is loaded from disk.
    /// Tries `.meta.json` first; falls back to extracting from the full `.json` snapshot
    /// and writes the `.meta.json` for future fast loads.
    fn ensure_thread_metadata_loaded(&self, thread_id: &str) {
        let Some(ref dir) = self.dir else { return };

        let mut meta_loaded = self.metadata_loaded.lock().expect("bug: mutex poisoned");
        if meta_loaded.contains(thread_id) {
            return;
        }

        // Try the fast metadata file first.
        let meta_path = dir.join(format!("{}.meta.json", thread_id));
        if let Ok(json) = std::fs::read_to_string(&meta_path)
            && let Ok(info) = serde_json::from_str::<ThreadInfo>(&json)
        {
            self.threads
                .lock()
                .expect("bug: mutex poisoned")
                .entry(thread_id.to_string())
                .or_insert(info);
            meta_loaded.insert(thread_id.to_string());
            return;
        }

        // Fall back: extract thread_info from the full snapshot file.
        let full_path = dir.join(format!("{}.json", thread_id));
        if let Ok(json) = std::fs::read_to_string(&full_path)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&json)
            && let Some(info_val) = val.get("thread_info")
            && let Ok(info) = serde_json::from_value::<ThreadInfo>(info_val.clone())
        {
            self.threads
                .lock()
                .expect("bug: mutex poisoned")
                .entry(thread_id.to_string())
                .or_insert(info);
            // Migrate: write the .meta.json for next time.
            self.save_thread_metadata(thread_id);
        }

        meta_loaded.insert(thread_id.to_string());
    }

    /// Ensure a thread's full data (messages, display_as, compaction summaries) is loaded.
    /// Calls `ensure_thread_metadata_loaded` first.
    fn ensure_thread_loaded(&self, thread_id: &str) {
        self.ensure_thread_metadata_loaded(thread_id);

        let Some(ref dir) = self.dir else { return };

        let mut loaded = self.loaded.lock().expect("bug: mutex poisoned");
        if loaded.contains(thread_id) {
            return;
        }

        let path = dir.join(format!("{}.json", thread_id));
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(snapshot) = serde_json::from_str::<ThreadSnapshot>(&json)
        {
            let mut messages = self.messages.lock().expect("bug: mutex poisoned");
            let mut display_as_map = self.display_as_map.lock().expect("bug: mutex poisoned");
            let mut compaction_summaries = self
                .compaction_summaries
                .lock()
                .expect("bug: mutex poisoned");

            assert!(
                messages
                    .insert(thread_id.to_string(), snapshot.messages)
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

        loaded.insert(thread_id.to_string());
    }

    /// Record the display_as segments for a tool result so it survives persistence.
    pub fn save_display_as(
        &self,
        thread_id: &str,
        tool_result_id: &str,
        display_as: &[rap_protocol::DisplaySegment],
    ) {
        self.ensure_thread_loaded(thread_id);
        {
            let mut map = self.display_as_map.lock().expect("bug: mutex poisoned");
            map.entry(thread_id.to_string())
                .or_default()
                .insert(tool_result_id.to_string(), display_as.to_vec());
        }
        self.save_thread(thread_id);
    }

    /// Look up previously stored display_as segments for a tool result.
    pub fn get_display_as(
        &self,
        thread_id: &str,
        tool_result_id: &str,
    ) -> Option<Vec<rap_protocol::DisplaySegment>> {
        self.ensure_thread_loaded(thread_id);
        let map = self.display_as_map.lock().expect("bug: mutex poisoned");
        map.get(thread_id)
            .and_then(|inner| inner.get(tool_result_id).cloned())
    }

    /// Resolve a thread ID to its root thread ID (i.e. the session ID).
    pub fn get_root_thread_id(&self, thread_id: &str) -> String {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        threads
            .get(thread_id)
            .map(|t| t.root_thread_id.clone())
            .unwrap_or_else(|| thread_id.to_string())
    }

    /// Get the parent thread ID, if any.
    pub fn get_thread_parent_id(&self, thread_id: &str) -> Option<String> {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        threads
            .get(thread_id)
            .and_then(|t| t.parent_thread_id.clone())
    }

    /// Set the title for a thread.
    pub fn set_thread_title(&self, thread_id: &str, title: &str) {
        self.ensure_thread_metadata_loaded(thread_id);
        {
            let mut threads = self.threads.lock().expect("bug: mutex poisoned");
            if let Some(t) = threads.get_mut(thread_id) {
                t.title = Some(title.to_string());
            }
        }
        self.save_thread_metadata(thread_id);
        self.notify_session(thread_id);
    }

    /// List open (non-closed) subthreads that are descendants of `parent_id`
    /// within the given session. Walks the children tree via metadata.
    pub fn get_open_subthreads(&self, parent_id: &str) -> Vec<infinity_protocol::SubthreadInfo> {
        self.ensure_thread_metadata_loaded(parent_id);
        let mut result = Vec::new();
        let mut queue = vec![parent_id.to_string()];
        while let Some(pid) = queue.pop() {
            let children = {
                let threads = self.threads.lock().expect("bug: mutex poisoned");
                threads
                    .get(&pid)
                    .map(|t| t.children.clone())
                    .unwrap_or_default()
            };
            for child_id in children {
                self.ensure_thread_metadata_loaded(&child_id);
                let threads = self.threads.lock().expect("bug: mutex poisoned");
                if let Some(info) = threads.get(&child_id)
                    && !info.closed
                    && !info.is_compaction
                {
                    result.push(infinity_protocol::SubthreadInfo {
                        thread_id: child_id.clone(),
                        parent_thread_id: pid.clone(),
                        title: info.title.clone(),
                    });
                    queue.push(child_id);
                }
            }
        }
        result
    }
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    type Error = MemoryError;

    async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let inserted = {
            let mut threads = self.threads.lock().expect("bug: mutex poisoned");
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
                        children: Vec::new(),
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
        let msgs = self.messages.lock().expect("bug: mutex poisoned");
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
        tracing::trace!("Appending messages {:?} to store", &messages);
        {
            let mut store = self.messages.lock().expect("bug: mutex poisoned");
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
            let threads = self.threads.lock().expect("bug: mutex poisoned");
            let msgs = self.messages.lock().expect("bug: mutex poisoned");
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
            let mut loaded = self.loaded.lock().expect("bug: mutex poisoned");
            let mut meta_loaded = self.metadata_loaded.lock().expect("bug: mutex poisoned");

            {
                let mut messages = self.messages.lock().expect("bug: mutex poisoned");
                messages.insert(new_id.clone(), vec![]);
            }

            {
                let mut threads = self.threads.lock().expect("bug: mutex poisoned");
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
                        children: Vec::new(),
                    },
                );
                // Add to parent's children list.
                if let Some(parent) = threads.get_mut(parent_thread_id) {
                    parent.children.push(new_id.clone());
                }
            }

            {
                let mut display_as_map = self.display_as_map.lock().expect("bug: mutex poisoned");
                display_as_map.insert(new_id.clone(), HashMap::new());
            }

            loaded.insert(new_id.clone());
            meta_loaded.insert(new_id.clone());
        }

        self.save_thread(&new_id);
        self.save_thread_metadata(parent_thread_id);
        self.notify_session(parent_thread_id);
        Ok(new_id)
    }

    async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).map(|t| t.closed).unwrap_or(false))
    }

    async fn close_thread(&self, thread_id: &str) -> Result<(), MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        {
            let mut threads = self.threads.lock().expect("bug: mutex poisoned");
            if let Some(t) = threads.get_mut(thread_id) {
                t.closed = true;
            }
        }
        self.save_thread_metadata(thread_id);
        self.notify_session(thread_id);
        Ok(())
    }

    async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_subscription_event)
            .unwrap_or(false))
    }

    async fn get_thread_parent_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
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
            self.ensure_thread_metadata_loaded(&current);
            let info = {
                let threads = self.threads.lock().expect("bug: mutex poisoned");
                threads.get(&current).cloned()
            };
            match info {
                Some(t) if t.parent_thread_id.is_some() => {
                    let parent = t
                        .parent_thread_id
                        .expect("bug: parent_thread_id was None after is_some check");
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
            let mut cs = self
                .compaction_summaries
                .lock()
                .expect("bug: mutex poisoned");
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
        let cs = self
            .compaction_summaries
            .lock()
            .expect("bug: mutex poisoned");
        Ok(cs.get(thread_id).and_then(|v| {
            v.iter()
                .rev()
                .find(|s| up_to_order.is_none_or(|n| s.up_to_order <= n))
                .map(|s| (s.summary.clone(), s.up_to_order))
        }))
    }

    async fn is_compaction_thread(&self, thread_id: &str) -> Result<bool, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_compaction)
            .unwrap_or(false))
    }

    async fn mark_thread_as_compaction(&self, thread_id: &str) -> Result<(), MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        {
            let mut threads = self.threads.lock().expect("bug: mutex poisoned");
            if let Some(t) = threads.get_mut(thread_id) {
                t.is_compaction = true;
            }
        }
        self.save_thread_metadata(thread_id);
        Ok(())
    }

    async fn get_thread_spawn_order(&self, thread_id: &str) -> Result<Option<i64>, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).and_then(|t| t.spawn_message_order))
    }
}

// ── In-memory state store with per-thread file persistence ──

/// Per-thread snapshot written to `{dir}/{thread_id}.state.json`.
#[derive(Serialize, Deserialize)]
struct StateThreadSnapshot {
    #[serde(default)]
    processed_message_ids: HashSet<String>,
    #[serde(default)]
    processed_tool_call_ids: HashSet<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    subscriptions: HashSet<String>,
}

#[derive(Clone)]
pub struct InMemoryStateStore {
    #[expect(clippy::type_complexity, reason = "shared state")]
    processed_ids: Arc<Mutex<HashMap<String, (HashSet<String>, HashSet<String>)>>>,
    metadata: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// Per-thread active subscriptions: thread_id → set of tool_call_ids.
    subscriptions: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// Directory where per-thread state JSON files are stored.
    dir: PathBuf,
    /// Tracks which keys have already been loaded (or attempted) from disk.
    loaded: Arc<Mutex<HashSet<String>>>,
}

impl InMemoryStateStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).ok();
        Self {
            processed_ids: Arc::new(Mutex::new(HashMap::new())),
            metadata: Arc::new(Mutex::new(HashMap::new())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            dir,
            loaded: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Write a single key's state data to `{dir}/{key}.state.json`.
    fn save_key(&self, key: &str) {
        let processed_ids = self.processed_ids.lock().expect("bug: mutex poisoned");
        let metadata = self.metadata.lock().expect("bug: mutex poisoned");
        let subscriptions = self.subscriptions.lock().expect("bug: mutex poisoned");

        let (msg_ids, tc_ids) = processed_ids
            .get(key)
            .cloned()
            .unwrap_or_else(|| (HashSet::new(), HashSet::new()));

        let snapshot = StateThreadSnapshot {
            processed_message_ids: msg_ids,
            processed_tool_call_ids: tc_ids,
            metadata: metadata.get(key).cloned(),
            subscriptions: subscriptions.get(key).cloned().unwrap_or_default(),
        };

        let path = self.dir.join(format!("{}.state.json", key));
        if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
            std::fs::write(path, json).ok();
        }
    }

    /// Ensure a key's data is loaded from disk into the in-memory caches.
    fn ensure_loaded(&self, key: &str) {
        let mut loaded = self.loaded.lock().expect("bug: mutex poisoned");
        if loaded.contains(key) {
            return;
        }

        let path = self.dir.join(format!("{}.state.json", key));
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(snapshot) = serde_json::from_str::<StateThreadSnapshot>(&json)
        {
            let mut processed_ids = self.processed_ids.lock().expect("bug: mutex poisoned");
            let mut metadata = self.metadata.lock().expect("bug: mutex poisoned");
            let mut subscriptions = self.subscriptions.lock().expect("bug: mutex poisoned");

            processed_ids.insert(
                key.to_string(),
                (
                    snapshot.processed_message_ids,
                    snapshot.processed_tool_call_ids,
                ),
            );
            if let Some(meta) = snapshot.metadata {
                metadata.insert(key.to_string(), meta);
            }
            if !snapshot.subscriptions.is_empty() {
                subscriptions.insert(key.to_string(), snapshot.subscriptions);
            }
        }

        loaded.insert(key.to_string());
    }
}

#[async_trait]
impl StateStore for InMemoryStateStore {
    type Error = MemoryError;

    async fn get_processed_ids(
        &self,
        thread_id: &str,
    ) -> Result<(HashSet<String>, HashSet<String>), MemoryError> {
        self.ensure_loaded(thread_id);
        let store = self.processed_ids.lock().expect("bug: mutex poisoned");
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
        self.ensure_loaded(thread_id);
        {
            let mut store = self.processed_ids.lock().expect("bug: mutex poisoned");
            let entry = store
                .entry(thread_id.to_string())
                .or_insert_with(|| (HashSet::new(), HashSet::new()));
            entry.0.extend(message_ids);
        }
        self.save_key(thread_id);
        Ok(())
    }

    async fn add_processed_tool_calls(
        &self,
        thread_id: &str,
        tool_call_ids: Vec<String>,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(thread_id);
        {
            let mut store = self.processed_ids.lock().expect("bug: mutex poisoned");
            let entry = store
                .entry(thread_id.to_string())
                .or_insert_with(|| (HashSet::new(), HashSet::new()));
            entry.1.extend(tool_call_ids);
        }
        self.save_key(thread_id);
        Ok(())
    }

    async fn get_metadata(
        &self,
        root_thread_id: &str,
    ) -> Result<Option<serde_json::Value>, MemoryError> {
        self.ensure_loaded(root_thread_id);
        let store = self.metadata.lock().expect("bug: mutex poisoned");
        Ok(store.get(root_thread_id).cloned())
    }

    async fn set_metadata(
        &self,
        root_thread_id: &str,
        metadata: serde_json::Value,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(root_thread_id);
        {
            let mut store = self.metadata.lock().expect("bug: mutex poisoned");
            store.insert(root_thread_id.to_string(), metadata);
        }
        self.save_key(root_thread_id);
        Ok(())
    }

    async fn get_active_subscriptions(&self, thread_id: &str) -> Result<Vec<String>, MemoryError> {
        self.ensure_loaded(thread_id);
        let store = self.subscriptions.lock().expect("bug: mutex poisoned");
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
        self.ensure_loaded(thread_id);
        {
            let mut store = self.subscriptions.lock().expect("bug: mutex poisoned");
            store
                .entry(thread_id.to_string())
                .or_default()
                .insert(tool_call_id.to_string());
        }
        self.save_key(thread_id);
        Ok(())
    }

    async fn remove_active_subscription(
        &self,
        thread_id: &str,
        tool_call_id: &str,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(thread_id);
        {
            let mut store = self.subscriptions.lock().expect("bug: mutex poisoned");
            if let Some(subs) = store.get_mut(thread_id) {
                subs.remove(tool_call_id);
            }
        }
        self.save_key(thread_id);
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

#[cfg(test)]
#[allow(clippy::collapsible_if)]
mod tests {
    use super::*;
    use infinity_agent_core::traits::ConversationStore;
    use rig::OneOrMany;
    use rig::message::{AssistantContent, Message, UserContent};

    fn user_msg(text: &str) -> Message {
        Message::User {
            content: OneOrMany::one(UserContent::text(text)),
        }
    }
    fn asst_msg(text: &str) -> Message {
        Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text(text)),
        }
    }

    /// Parent has messages, child spawned at index 2. load_history_with_ancestors
    /// should return parent[0..2] + child messages.
    #[tokio::test]
    async fn ancestors_basic_cutoff() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path());
        store
            .ensure_root_thread("root")
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                "root",
                vec![(user_msg("p1"), "m1".into()), (asst_msg("p2"), "m2".into())],
            )
            .await
            .expect("append root messages");

        let child = store
            .spawn_thread("root", "tc-1", false)
            .await
            .expect("spawn child thread");

        store
            .append_messages(
                "root",
                vec![(user_msg("p3"), "m3".into()), (asst_msg("p4"), "m4".into())],
            )
            .await
            .expect("append root messages after spawn");

        store
            .append_messages(&child, vec![(user_msg("c1"), "m5".into())])
            .await
            .expect("append child messages");

        let (history, _) = store
            .load_history_with_ancestors(&child)
            .await
            .expect("load history with ancestors");
        assert_eq!(history.len(), 3);
        if let Message::User { content } = &history[0] {
            assert!(matches!(content.first(), UserContent::Text(t) if t.text == "p1"));
        }
        if let Message::User { content } = &history[2] {
            assert!(matches!(content.first(), UserContent::Text(t) if t.text == "c1"));
        }
    }

    /// Three-level chain: root → child → grandchild.
    #[tokio::test]
    async fn ancestors_three_levels() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path());
        store
            .ensure_root_thread("root")
            .await
            .expect("ensure root thread");
        store
            .append_messages("root", vec![(user_msg("r1"), "m1".into())])
            .await
            .expect("append root messages");

        let child = store
            .spawn_thread("root", "tc-1", false)
            .await
            .expect("spawn child thread");
        store
            .append_messages(
                &child,
                vec![(user_msg("c1"), "m2".into()), (asst_msg("c2"), "m3".into())],
            )
            .await
            .expect("append child messages");

        let grandchild = store
            .spawn_thread(&child, "tc-2", false)
            .await
            .expect("spawn grandchild thread");
        store
            .append_messages(&grandchild, vec![(user_msg("g1"), "m4".into())])
            .await
            .expect("append grandchild messages");

        let (history, _) = store
            .load_history_with_ancestors(&grandchild)
            .await
            .expect("load history with ancestors");
        assert_eq!(history.len(), 4);
    }

    /// Compaction on root: should return [summary] + messages after compaction point.
    #[tokio::test]
    async fn ancestors_with_compaction_on_self() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path());
        store
            .ensure_root_thread("root")
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                "root",
                vec![
                    (user_msg("old1"), "m1".into()),
                    (asst_msg("old2"), "m2".into()),
                    (user_msg("new1"), "m3".into()),
                    (asst_msg("new2"), "m4".into()),
                ],
            )
            .await
            .expect("append root messages");

        store
            .save_compaction_summary("root", "summary of old stuff", 2)
            .await
            .expect("save compaction summary");

        let (history, compacted_up_to) = store
            .load_history_with_ancestors("root")
            .await
            .expect("load history with ancestors");
        assert_eq!(history.len(), 3);
        assert_eq!(compacted_up_to, Some(2));
        if let Message::Assistant { content, .. } = &history[0] {
            if let AssistantContent::Text(t) = content.first() {
                assert!(t.text.contains("summary of old stuff"));
            }
        }
    }

    /// Compaction on parent: child should use parent's compaction summary.
    #[tokio::test]
    async fn ancestors_with_compaction_on_parent() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path());
        store
            .ensure_root_thread("root")
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                "root",
                vec![
                    (user_msg("old1"), "m1".into()),
                    (asst_msg("old2"), "m2".into()),
                    (user_msg("recent"), "m3".into()),
                ],
            )
            .await
            .expect("append root messages");

        store
            .save_compaction_summary("root", "compacted root", 2)
            .await
            .expect("save compaction summary");

        let child = store
            .spawn_thread("root", "tc-1", false)
            .await
            .expect("spawn child thread");
        store
            .append_messages(&child, vec![(user_msg("c1"), "m4".into())])
            .await
            .expect("append child messages");

        let (history, _) = store
            .load_history_with_ancestors(&child)
            .await
            .expect("load history with ancestors");
        assert_eq!(history.len(), 3);
        if let Message::Assistant { content, .. } = &history[0] {
            if let AssistantContent::Text(t) = content.first() {
                assert!(t.text.contains("compacted root"));
            }
        }
    }

    /// Two compactions on root — should pick the latest that fits within cutoff.
    #[tokio::test]
    async fn ancestors_multiple_compactions_picks_latest() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path());
        store
            .ensure_root_thread("root")
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                "root",
                vec![
                    (user_msg("a"), "m1".into()),
                    (asst_msg("b"), "m2".into()),
                    (user_msg("c"), "m3".into()),
                    (asst_msg("d"), "m4".into()),
                    (user_msg("e"), "m5".into()),
                ],
            )
            .await
            .expect("append root messages");

        store
            .save_compaction_summary("root", "early summary", 2)
            .await
            .expect("save early compaction summary");
        store
            .save_compaction_summary("root", "later summary", 4)
            .await
            .expect("save later compaction summary");

        let child = store
            .spawn_thread("root", "tc-1", false)
            .await
            .expect("spawn child thread");
        store
            .append_messages(&child, vec![(user_msg("c1"), "m6".into())])
            .await
            .expect("append child messages");

        let (history, _) = store
            .load_history_with_ancestors(&child)
            .await
            .expect("load history with ancestors");
        assert_eq!(history.len(), 3);
        if let Message::Assistant { content, .. } = &history[0] {
            if let AssistantContent::Text(t) = content.first() {
                assert!(t.text.contains("later summary"));
            }
        }
    }

    /// Both parent and leaf have compactions. The leaf's compaction should be
    /// used exclusively — ancestors are skipped entirely.
    #[tokio::test]
    async fn leaf_compaction_takes_priority_over_ancestor() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path());
        store
            .ensure_root_thread("root")
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                "root",
                vec![(user_msg("r1"), "m1".into()), (asst_msg("r2"), "m2".into())],
            )
            .await
            .expect("append root messages");
        store
            .save_compaction_summary("root", "root compaction", 2)
            .await
            .expect("save root compaction summary");

        let child = store
            .spawn_thread("root", "tc-1", false)
            .await
            .expect("spawn child thread");
        store
            .append_messages(
                &child,
                vec![
                    (user_msg("c1"), "m3".into()),
                    (asst_msg("c2"), "m4".into()),
                    (user_msg("c3"), "m5".into()),
                    (asst_msg("c4"), "m6".into()),
                ],
            )
            .await
            .expect("append child messages");
        store
            .save_compaction_summary(&child, "child compaction", 2)
            .await
            .expect("save child compaction summary");

        let (history, compacted_up_to) = store
            .load_history_with_ancestors(&child)
            .await
            .expect("load history with ancestors");
        // Should be: [child compaction summary] + c3 + c4 = 3
        // No ancestor messages at all — leaf compaction short-circuits.
        assert_eq!(history.len(), 3);
        assert_eq!(compacted_up_to, Some(2));
        if let Message::Assistant { content, .. } = &history[0] {
            if let AssistantContent::Text(t) = content.first() {
                assert!(
                    t.text.contains("child compaction"),
                    "should use child's compaction, got: {}",
                    t.text
                );
                assert!(
                    !t.text.contains("root compaction"),
                    "should NOT contain root compaction"
                );
            }
        }
        // The remaining messages should be the child's post-compaction messages
        if let Message::User { content } = &history[1] {
            assert!(matches!(content.first(), UserContent::Text(t) if t.text == "c3"));
        }
    }
}
