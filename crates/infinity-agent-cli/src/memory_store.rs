use async_trait::async_trait;
use infinity_agent_core::message::InputMessage;
use infinity_agent_core::traits::{ConversationStore, InputSender, StateStore};
use rig::message::Message;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

// ── In-memory conversation store ──

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
}

#[derive(Clone, Serialize, Deserialize)]
struct ThreadInfo {
    parent_thread_id: Option<String>,
    root_thread_id: String,
    spawn_message_order: Option<i64>,
    spawn_tool_call_id: Option<String>,
    closed: bool,
    is_subscription_event: bool,
}

impl InMemoryConversationStore {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(HashMap::new())),
            threads: Arc::new(Mutex::new(HashMap::new())),
            display_as_map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Serialize the store to a JSON file.
    pub fn save_to_file(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let messages = self.messages.lock().unwrap();
        let threads = self.threads.lock().unwrap();
        let display_as_map = self.display_as_map.lock().unwrap();
        let snapshot = StoreSnapshot {
            messages: messages.clone(),
            threads: threads.clone(),
            display_as_map: display_as_map.clone(),
        };
        let json = serde_json::to_string_pretty(&snapshot)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load the store from a JSON file, replacing current contents.
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let json = std::fs::read_to_string(path)?;
        let snapshot: StoreSnapshot = serde_json::from_str(&json)?;
        Ok(Self {
            messages: Arc::new(Mutex::new(snapshot.messages)),
            threads: Arc::new(Mutex::new(snapshot.threads)),
            display_as_map: Arc::new(Mutex::new(snapshot.display_as_map)),
        })
    }

    /// Record the display_as text for a tool result so it survives persistence.
    pub fn save_display_as(&self, thread_id: &str, tool_result_id: &str, display_as: &str) {
        let mut map = self.display_as_map.lock().unwrap();
        map.entry(thread_id.to_string())
            .or_default()
            .insert(tool_result_id.to_string(), display_as.to_string());
    }

    /// Look up a previously stored display_as for a tool result.
    pub fn get_display_as(&self, thread_id: &str, tool_result_id: &str) -> Option<String> {
        let map = self.display_as_map.lock().unwrap();
        map.get(thread_id)
            .and_then(|inner| inner.get(tool_result_id).cloned())
    }
}

#[derive(Serialize, Deserialize)]
struct StoreSnapshot {
    messages: HashMap<String, Vec<(Message, String)>>,
    threads: HashMap<String, ThreadInfo>,
    /// thread_id -> tool_result_id -> display_as text.
    /// Uses `serde(default)` so older store files without this field still load.
    #[serde(default)]
    display_as_map: HashMap<String, HashMap<String, String>>,
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    type Error = MemoryError;

    async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), MemoryError> {
        let mut threads = self.threads.lock().unwrap();
        threads.entry(thread_id.to_string()).or_insert(ThreadInfo {
            parent_thread_id: None,
            root_thread_id: thread_id.to_string(),
            spawn_message_order: None,
            spawn_tool_call_id: None,
            closed: false,
            is_subscription_event: false,
        });
        Ok(())
    }

    async fn load_history(&self, session_id: &str) -> Result<Vec<Message>, MemoryError> {
        let msgs = self.messages.lock().unwrap();
        Ok(msgs
            .get(session_id)
            .map(|v| v.iter().map(|(m, _)| m.clone()).collect())
            .unwrap_or_default())
    }

    async fn load_history_up_to(
        &self,
        session_id: &str,
        up_to_order: i64,
    ) -> Result<Vec<Message>, MemoryError> {
        let msgs = self.messages.lock().unwrap();
        Ok(msgs
            .get(session_id)
            .map(|v| {
                v.iter()
                    .take(up_to_order as usize)
                    .map(|(m, _)| m.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn load_history_with_ancestors(
        &self,
        thread_id: &str,
    ) -> Result<Vec<Message>, MemoryError> {
        let ancestors = self.get_ancestor_chain(thread_id).await?;
        let mut combined = Vec::new();
        for (tid, cutoff) in &ancestors {
            combined.extend(self.load_history_up_to(tid, *cutoff).await?);
        }
        combined.extend(self.load_history(thread_id).await?);
        Ok(combined)
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: Vec<(Message, String)>,
    ) -> Result<(), MemoryError> {
        let mut store = self.messages.lock().unwrap();
        let entry = store.entry(session_id.to_string()).or_default();
        entry.extend(messages);
        Ok(())
    }

    async fn spawn_thread(
        &self,
        parent_thread_id: &str,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
    ) -> Result<String, MemoryError> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let mut threads = self.threads.lock().unwrap();
        let msgs = self.messages.lock().unwrap();
        let spawn_message_order = msgs
            .get(parent_thread_id)
            .map(|v| v.len() as i64)
            .unwrap_or(0);
        let root = threads
            .get(parent_thread_id)
            .map(|t| t.root_thread_id.clone())
            .unwrap_or_else(|| parent_thread_id.to_string());
        threads.insert(
            new_id.clone(),
            ThreadInfo {
                parent_thread_id: Some(parent_thread_id.to_string()),
                root_thread_id: root,
                spawn_message_order: Some(spawn_message_order),
                spawn_tool_call_id: Some(spawn_tool_call_id.to_string()),
                closed: false,
                is_subscription_event: is_for_subscription_event,
            },
        );
        Ok(new_id)
    }

    async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, MemoryError> {
        let threads = self.threads.lock().unwrap();
        Ok(threads.get(thread_id).map(|t| t.closed).unwrap_or(false))
    }

    async fn close_thread(&self, thread_id: &str) -> Result<(), MemoryError> {
        let mut threads = self.threads.lock().unwrap();
        if let Some(t) = threads.get_mut(thread_id) {
            t.closed = true;
        }
        Ok(())
    }

    async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, MemoryError> {
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
        let threads = self.threads.lock().unwrap();
        Ok(threads.get(thread_id).and_then(|t| {
            match (&t.parent_thread_id, &t.spawn_tool_call_id) {
                (Some(p), Some(tc)) => Some((p.clone(), tc.clone())),
                _ => None,
            }
        }))
    }

    async fn get_ancestor_chain(&self, thread_id: &str) -> Result<Vec<(String, i64)>, MemoryError> {
        let threads = self.threads.lock().unwrap();
        let mut result = Vec::new();
        let mut current = thread_id.to_string();
        loop {
            let info = threads.get(&current);
            match info.and_then(|t| t.parent_thread_id.as_ref()) {
                Some(parent) => {
                    let order = threads
                        .get(&current)
                        .and_then(|t| t.spawn_message_order)
                        .unwrap_or(0);
                    result.push((parent.clone(), order));
                    current = parent.clone();
                }
                None => break,
            }
        }
        result.reverse();
        Ok(result)
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
