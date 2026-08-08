//! Simple in-memory implementations of the platform traits.
//!
//! These are fully functional [`ConversationStore`] / [`StateStore`]
//! implementations backed by `HashMap`s, suitable for tests, examples, and
//! embedded runtimes that do not need their own persistence. Production
//! embeddings usually bring their own stores (the Lambda binding uses Aurora
//! DSQL + DynamoDB; the Infinity Code daemon adds file persistence, titles,
//! and per-thread model tracking on top of an in-memory core).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::message::InfinityMessage;
use crate::traits::{ConversationStore, StateStore};

/// Error type for the in-memory stores. The operations themselves are
/// infallible; this exists to satisfy the trait signatures.
#[derive(Debug)]
pub struct InMemoryStoreError(pub String);

impl std::fmt::Display for InMemoryStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InMemoryStoreError {}

#[derive(Clone)]
struct ThreadInfo {
    parent_thread_id: Option<String>,
    root_thread_id: String,
    /// Number of parent messages the child inherits (history cutoff).
    spawn_message_order: Option<i64>,
    spawn_tool_call_id: Option<String>,
    closed: bool,
    is_subscription_event: bool,
    is_compaction: bool,
}

#[derive(Clone)]
struct CompactionSummary {
    summary: String,
    up_to_order: i64,
}

/// An in-memory [`ConversationStore`]: threads, message history, the thread
/// tree, and compaction summaries, all in process memory.
#[derive(Clone, Default)]
pub struct InMemoryConversationStore {
    #[expect(clippy::type_complexity, reason = "shared state")]
    messages: Arc<Mutex<HashMap<String, Vec<(InfinityMessage, String)>>>>,
    threads: Arc<Mutex<HashMap<String, ThreadInfo>>>,
    compaction_summaries: Arc<Mutex<HashMap<String, Vec<CompactionSummary>>>>,
}

impl InMemoryConversationStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    type Error = InMemoryStoreError;

    async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), Self::Error> {
        let mut threads = self.threads.lock().expect("bug: mutex poisoned");
        threads
            .entry(thread_id.to_owned())
            .or_insert_with(|| ThreadInfo {
                parent_thread_id: None,
                root_thread_id: thread_id.to_owned(),
                spawn_message_order: None,
                spawn_tool_call_id: None,
                closed: false,
                is_subscription_event: false,
                is_compaction: false,
            });
        Ok(())
    }

    async fn load_history_up_to(
        &self,
        session_id: &str,
        start_from: Option<i64>,
        up_to: Option<i64>,
    ) -> Result<Vec<InfinityMessage>, Self::Error> {
        let msgs = self.messages.lock().expect("bug: mutex poisoned");
        Ok(msgs
            .get(session_id)
            .map(|v| {
                let start = start_from.unwrap_or(0) as usize;
                let end = up_to.map(|u| u as usize).unwrap_or(v.len()).min(v.len());
                let start = start.min(end);
                v[start..end].iter().map(|(m, _)| m.clone()).collect()
            })
            .unwrap_or_default())
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: Vec<(InfinityMessage, String)>,
    ) -> Result<(), Self::Error> {
        let mut store = self.messages.lock().expect("bug: mutex poisoned");
        store
            .entry(session_id.to_owned())
            .or_default()
            .extend(messages);
        Ok(())
    }

    async fn spawn_thread(
        &self,
        parent_thread_id: &str,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
        spawn_order_override: Option<usize>,
    ) -> Result<String, Self::Error> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let msgs = self.messages.lock().expect("bug: mutex poisoned");
        let spawn_message_order = spawn_order_override
            .unwrap_or_else(|| msgs.get(parent_thread_id).map(|v| v.len()).unwrap_or(0))
            as i64;
        drop(msgs);

        let mut threads = self.threads.lock().expect("bug: mutex poisoned");
        let root = threads
            .get(parent_thread_id)
            .map(|t| t.root_thread_id.clone())
            .unwrap_or_else(|| parent_thread_id.to_owned());
        threads.insert(
            new_id.clone(),
            ThreadInfo {
                parent_thread_id: Some(parent_thread_id.to_owned()),
                root_thread_id: root,
                spawn_message_order: Some(spawn_message_order),
                spawn_tool_call_id: Some(spawn_tool_call_id.to_owned()),
                closed: false,
                is_subscription_event: is_for_subscription_event,
                is_compaction: false,
            },
        );
        Ok(new_id)
    }

    async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).map(|t| t.closed).unwrap_or(false))
    }

    async fn close_thread(&self, thread_id: &str) -> Result<(), Self::Error> {
        let mut threads = self.threads.lock().expect("bug: mutex poisoned");
        if let Some(t) = threads.get_mut(thread_id) {
            t.closed = true;
        }
        Ok(())
    }

    async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_subscription_event)
            .unwrap_or(false))
    }

    async fn get_thread_parent_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).and_then(|t| {
            match (&t.parent_thread_id, &t.spawn_tool_call_id) {
                (Some(p), Some(tc)) => Some((p.clone(), tc.clone())),
                _ => None,
            }
        }))
    }

    async fn get_ancestor_chain(&self, thread_id: &str) -> Result<Vec<(String, i64)>, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        let mut result = Vec::new();
        let mut current = thread_id.to_owned();
        while let Some(info) = threads.get(&current) {
            let Some(parent) = info.parent_thread_id.clone() else {
                break;
            };
            let order = info.spawn_message_order.unwrap_or(0);
            result.push((parent.clone(), order));
            current = parent;
        }
        result.reverse();
        Ok(result)
    }

    async fn mark_thread_as_compaction(&self, thread_id: &str) -> Result<(), Self::Error> {
        let mut threads = self.threads.lock().expect("bug: mutex poisoned");
        if let Some(t) = threads.get_mut(thread_id) {
            t.is_compaction = true;
        }
        Ok(())
    }

    async fn is_compaction_thread(&self, thread_id: &str) -> Result<bool, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_compaction)
            .unwrap_or(false))
    }

    async fn get_thread_spawn_order(&self, thread_id: &str) -> Result<Option<i64>, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).and_then(|t| t.spawn_message_order))
    }

    async fn save_compaction_summary(
        &self,
        thread_id: &str,
        summary: &str,
        up_to_order: i64,
    ) -> Result<(), Self::Error> {
        let mut cs = self
            .compaction_summaries
            .lock()
            .expect("bug: mutex poisoned");
        cs.entry(thread_id.to_owned())
            .or_default()
            .push(CompactionSummary {
                summary: summary.to_owned(),
                up_to_order,
            });
        Ok(())
    }

    async fn load_latest_compaction_summary_up_to(
        &self,
        thread_id: &str,
        up_to_order: Option<i64>,
    ) -> Result<Option<(String, i64)>, Self::Error> {
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
}

/// An in-memory [`StateStore`]: processed IDs, metadata, and active
/// subscription tracking in process memory.
#[derive(Clone, Default)]
pub struct InMemoryStateStore {
    #[expect(clippy::type_complexity, reason = "shared state")]
    processed_ids: Arc<Mutex<HashMap<String, (HashSet<String>, HashSet<String>)>>>,
    metadata: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    subscriptions: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

impl InMemoryStateStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StateStore for InMemoryStateStore {
    type Error = InMemoryStoreError;

    async fn get_processed_ids(
        &self,
        thread_id: &str,
    ) -> Result<(HashSet<String>, HashSet<String>), Self::Error> {
        let store = self.processed_ids.lock().expect("bug: mutex poisoned");
        Ok(store.get(thread_id).cloned().unwrap_or_default())
    }

    async fn add_processed_message_ids(
        &self,
        thread_id: &str,
        message_ids: Vec<String>,
    ) -> Result<(), Self::Error> {
        let mut store = self.processed_ids.lock().expect("bug: mutex poisoned");
        store
            .entry(thread_id.to_owned())
            .or_default()
            .0
            .extend(message_ids);
        Ok(())
    }

    async fn add_processed_tool_calls(
        &self,
        thread_id: &str,
        tool_call_ids: Vec<String>,
    ) -> Result<(), Self::Error> {
        let mut store = self.processed_ids.lock().expect("bug: mutex poisoned");
        store
            .entry(thread_id.to_owned())
            .or_default()
            .1
            .extend(tool_call_ids);
        Ok(())
    }

    async fn get_metadata(
        &self,
        root_thread_id: &str,
    ) -> Result<Option<serde_json::Value>, Self::Error> {
        let store = self.metadata.lock().expect("bug: mutex poisoned");
        Ok(store.get(root_thread_id).cloned())
    }

    async fn set_metadata(
        &self,
        root_thread_id: &str,
        metadata: serde_json::Value,
    ) -> Result<(), Self::Error> {
        let mut store = self.metadata.lock().expect("bug: mutex poisoned");
        store.insert(root_thread_id.to_owned(), metadata);
        Ok(())
    }

    async fn get_active_subscriptions(&self, thread_id: &str) -> Result<Vec<String>, Self::Error> {
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
    ) -> Result<(), Self::Error> {
        let mut store = self.subscriptions.lock().expect("bug: mutex poisoned");
        store
            .entry(thread_id.to_owned())
            .or_default()
            .insert(tool_call_id.to_owned());
        Ok(())
    }

    async fn remove_active_subscription(
        &self,
        thread_id: &str,
        tool_call_id: &str,
    ) -> Result<(), Self::Error> {
        let mut store = self.subscriptions.lock().expect("bug: mutex poisoned");
        if let Some(s) = store.get_mut(thread_id) {
            s.remove(tool_call_id);
        }
        Ok(())
    }
}
