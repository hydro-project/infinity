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
use rap_protocol::ThreadId;
use serde::{Deserialize, Serialize};

use crate::message::InfinityMessage;
use crate::system::UserChoice;
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

/// One thread's bookkeeping in an [`InMemoryConversationStore`].
///
/// Public and serializable so embeddings that layer persistence on top (like
/// the Infinity Code daemon) can snapshot and restore threads through
/// [`InMemoryConversationStore::thread_info`] /
/// [`set_thread_info`](InMemoryConversationStore::set_thread_info) without
/// re-implementing the store semantics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub parent_thread_id: Option<ThreadId>,
    pub root_thread_id: ThreadId,
    /// Number of parent messages the child inherits (history cutoff).
    pub spawn_message_order: Option<i64>,
    pub spawn_tool_call_id: Option<String>,
    pub closed: bool,
    pub is_subscription_event: bool,
    #[serde(default)]
    pub is_compaction: bool,
}

/// One compaction summary entry (public and serializable for the same
/// snapshot/restore purposes as [`ThreadInfo`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionSummary {
    pub summary: String,
    pub up_to_order: i64,
}

/// An in-memory [`ConversationStore`]: threads, message history, the thread
/// tree, and compaction summaries, all in process memory.
#[derive(Clone, Default)]
pub struct InMemoryConversationStore {
    #[expect(clippy::type_complexity, reason = "shared state")]
    messages: Arc<Mutex<HashMap<ThreadId, Vec<(InfinityMessage, String)>>>>,
    threads: Arc<Mutex<HashMap<ThreadId, ThreadInfo>>>,
    compaction_summaries: Arc<Mutex<HashMap<ThreadId, Vec<CompactionSummary>>>>,
}

impl InMemoryConversationStore {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Snapshot/restore accessors ──
    //
    // Synchronous inherent methods for embeddings that layer persistence (or
    // other bookkeeping) on top of this store: they can export a thread's
    // state piece by piece, persist it however they like, and restore it
    // later — while every trait-level semantic (spawn orders, ancestor
    // chains, compaction cutoffs) stays implemented here, once.

    /// Snapshot one thread's bookkeeping, if the thread exists.
    pub fn thread_info(&self, thread_id: &ThreadId<str>) -> Option<ThreadInfo> {
        self.threads
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .cloned()
    }

    /// Restore (or overwrite) one thread's bookkeeping.
    pub fn set_thread_info(&self, thread_id: &ThreadId<str>, info: ThreadInfo) {
        self.threads
            .lock()
            .expect("bug: mutex poisoned")
            .insert(thread_id.to_owned(), info);
    }

    /// Snapshot one thread's messages (with their dedup IDs), if any were
    /// ever appended.
    pub fn thread_messages(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Option<Vec<(InfinityMessage, String)>> {
        self.messages
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .cloned()
    }

    /// Restore one thread's messages. Returns `true` if messages for the
    /// thread were already present (and have been overwritten).
    pub fn set_thread_messages(
        &self,
        thread_id: &ThreadId<str>,
        messages: Vec<(InfinityMessage, String)>,
    ) -> bool {
        self.messages
            .lock()
            .expect("bug: mutex poisoned")
            .insert(thread_id.to_owned(), messages)
            .is_some()
    }

    /// Snapshot one thread's compaction summaries.
    pub fn thread_compaction_summaries(&self, thread_id: &ThreadId<str>) -> Vec<CompactionSummary> {
        self.compaction_summaries
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Restore one thread's compaction summaries. Returns `true` if
    /// summaries for the thread were already present (and overwritten).
    pub fn set_thread_compaction_summaries(
        &self,
        thread_id: &ThreadId<str>,
        summaries: Vec<CompactionSummary>,
    ) -> bool {
        self.compaction_summaries
            .lock()
            .expect("bug: mutex poisoned")
            .insert(thread_id.to_owned(), summaries)
            .is_some()
    }

    /// Spawn a child thread under `parent_thread_id` with a caller-chosen ID
    /// (the trait-level [`spawn_thread`](ConversationStore::spawn_thread)
    /// generates a UUID and delegates here). The spawn order — how much of
    /// the parent's history the child inherits — is the parent's current
    /// message count unless overridden.
    pub fn spawn_thread_with_id(
        &self,
        new_thread_id: &ThreadId<str>,
        parent_thread_id: &ThreadId<str>,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
        spawn_order_override: Option<usize>,
    ) {
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
            new_thread_id.to_owned(),
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
    }
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    type Error = InMemoryStoreError;

    async fn ensure_root_thread(&self, thread_id: &ThreadId<str>) -> Result<(), Self::Error> {
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

    async fn thread_exists(&self, thread_id: &ThreadId<str>) -> Result<bool, Self::Error> {
        Ok(self
            .threads
            .lock()
            .expect("bug: mutex poisoned")
            .contains_key(thread_id))
    }

    async fn load_history_up_to(
        &self,
        session_id: &ThreadId<str>,
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
        session_id: &ThreadId<str>,
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
        parent_thread_id: &ThreadId<str>,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
        spawn_order_override: Option<usize>,
    ) -> Result<ThreadId, Self::Error> {
        let new_id = ThreadId::from(uuid::Uuid::new_v4().to_string());
        self.spawn_thread_with_id(
            &new_id,
            parent_thread_id,
            spawn_tool_call_id,
            is_for_subscription_event,
            spawn_order_override,
        );
        Ok(new_id)
    }

    async fn is_thread_closed(&self, thread_id: &ThreadId<str>) -> Result<bool, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).map(|t| t.closed).unwrap_or(false))
    }

    async fn close_thread(&self, thread_id: &ThreadId<str>) -> Result<(), Self::Error> {
        let mut threads = self.threads.lock().expect("bug: mutex poisoned");
        if let Some(t) = threads.get_mut(thread_id) {
            t.closed = true;
        }
        Ok(())
    }

    async fn is_subscription_event_thread(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<bool, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_subscription_event)
            .unwrap_or(false))
    }

    async fn get_thread_parent_info(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Option<(ThreadId, String)>, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).and_then(|t| {
            match (&t.parent_thread_id, &t.spawn_tool_call_id) {
                (Some(p), Some(tc)) => Some((p.clone(), tc.clone())),
                _ => None,
            }
        }))
    }

    async fn get_ancestor_chain(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<(ThreadId, i64)>, Self::Error> {
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

    async fn mark_thread_as_compaction(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<(), Self::Error> {
        let mut threads = self.threads.lock().expect("bug: mutex poisoned");
        if let Some(t) = threads.get_mut(thread_id) {
            t.is_compaction = true;
        }
        Ok(())
    }

    async fn is_compaction_thread(&self, thread_id: &ThreadId<str>) -> Result<bool, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads
            .get(thread_id)
            .map(|t| t.is_compaction)
            .unwrap_or(false))
    }

    async fn get_thread_spawn_order(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Option<i64>, Self::Error> {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        Ok(threads.get(thread_id).and_then(|t| t.spawn_message_order))
    }

    async fn save_compaction_summary(
        &self,
        thread_id: &ThreadId<str>,
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
        thread_id: &ThreadId<str>,
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

/// Serializable snapshot of one thread's [`InMemoryStateStore`] entry, for
/// embeddings that layer persistence on top (export/restore).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ThreadState {
    #[serde(default)]
    pub processed_message_ids: HashSet<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub subscriptions: HashSet<String>,
    #[serde(default)]
    pub pending_user_choices: Vec<UserChoice>,
}

/// An in-memory [`StateStore`]: processed IDs, metadata, and active
/// subscription tracking in process memory.
#[derive(Clone, Default)]
pub struct InMemoryStateStore {
    processed_ids: Arc<Mutex<HashMap<ThreadId, HashSet<String>>>>,
    metadata: Arc<Mutex<HashMap<ThreadId, serde_json::Value>>>,
    subscriptions: Arc<Mutex<HashMap<ThreadId, HashSet<String>>>>,
    pending_user_choices: Arc<Mutex<HashMap<ThreadId, Vec<UserChoice>>>>,
}

impl InMemoryStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot one thread's state (see [`ThreadState`]).
    pub fn thread_state(&self, thread_id: &ThreadId<str>) -> ThreadState {
        let processed = self.processed_ids.lock().expect("bug: mutex poisoned");
        let metadata = self.metadata.lock().expect("bug: mutex poisoned");
        let subscriptions = self.subscriptions.lock().expect("bug: mutex poisoned");
        let pending_user_choices = self
            .pending_user_choices
            .lock()
            .expect("bug: mutex poisoned");
        ThreadState {
            processed_message_ids: processed.get(thread_id).cloned().unwrap_or_default(),
            metadata: metadata.get(thread_id).cloned(),
            subscriptions: subscriptions.get(thread_id).cloned().unwrap_or_default(),
            pending_user_choices: pending_user_choices
                .get(thread_id)
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// Restore one thread's state (see [`ThreadState`]).
    pub fn set_thread_state(&self, thread_id: &ThreadId<str>, state: ThreadState) {
        self.processed_ids
            .lock()
            .expect("bug: mutex poisoned")
            .insert(thread_id.to_owned(), state.processed_message_ids);
        if let Some(meta) = state.metadata {
            self.metadata
                .lock()
                .expect("bug: mutex poisoned")
                .insert(thread_id.to_owned(), meta);
        }
        if !state.subscriptions.is_empty() {
            self.subscriptions
                .lock()
                .expect("bug: mutex poisoned")
                .insert(thread_id.to_owned(), state.subscriptions);
        }
        if !state.pending_user_choices.is_empty() {
            self.pending_user_choices
                .lock()
                .expect("bug: mutex poisoned")
                .insert(thread_id.to_owned(), state.pending_user_choices);
        }
    }
}

#[async_trait]
impl StateStore for InMemoryStateStore {
    type Error = InMemoryStoreError;

    async fn get_processed_ids(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<HashSet<String>, Self::Error> {
        let store = self.processed_ids.lock().expect("bug: mutex poisoned");
        Ok(store.get(thread_id).cloned().unwrap_or_default())
    }

    async fn add_processed_message_ids(
        &self,
        thread_id: &ThreadId<str>,
        message_ids: Vec<String>,
    ) -> Result<(), Self::Error> {
        let mut store = self.processed_ids.lock().expect("bug: mutex poisoned");
        store
            .entry(thread_id.to_owned())
            .or_default()
            .extend(message_ids);
        Ok(())
    }

    async fn get_metadata(
        &self,
        root_thread_id: &ThreadId<str>,
    ) -> Result<Option<serde_json::Value>, Self::Error> {
        let store = self.metadata.lock().expect("bug: mutex poisoned");
        Ok(store.get(root_thread_id).cloned())
    }

    async fn set_metadata(
        &self,
        root_thread_id: &ThreadId<str>,
        metadata: serde_json::Value,
    ) -> Result<(), Self::Error> {
        let mut store = self.metadata.lock().expect("bug: mutex poisoned");
        store.insert(root_thread_id.to_owned(), metadata);
        Ok(())
    }

    async fn get_active_subscriptions(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<String>, Self::Error> {
        let store = self.subscriptions.lock().expect("bug: mutex poisoned");
        Ok(store
            .get(thread_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default())
    }

    async fn add_active_subscription(
        &self,
        thread_id: &ThreadId<str>,
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
        thread_id: &ThreadId<str>,
        tool_call_id: &str,
    ) -> Result<(), Self::Error> {
        let mut store = self.subscriptions.lock().expect("bug: mutex poisoned");
        if let Some(s) = store.get_mut(thread_id) {
            s.remove(tool_call_id);
        }
        Ok(())
    }

    async fn add_pending_user_choice(
        &self,
        thread_id: &ThreadId<str>,
        choice: UserChoice,
    ) -> Result<(), Self::Error> {
        let mut store = self
            .pending_user_choices
            .lock()
            .expect("bug: mutex poisoned");
        let choices = store.entry(thread_id.to_owned()).or_default();
        if let Some(existing) = choices.iter_mut().find(|existing| existing.id == choice.id) {
            *existing = choice;
        } else {
            choices.push(choice);
        }
        Ok(())
    }

    async fn remove_pending_user_choice(
        &self,
        thread_id: &ThreadId<str>,
        choice_id: &str,
    ) -> Result<(), Self::Error> {
        if let Some(choices) = self
            .pending_user_choices
            .lock()
            .expect("bug: mutex poisoned")
            .get_mut(thread_id)
        {
            choices.retain(|choice| choice.id != choice_id);
        }
        Ok(())
    }

    async fn get_pending_user_choices(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<UserChoice>, Self::Error> {
        Ok(self
            .pending_user_choices
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .cloned()
            .unwrap_or_default())
    }
}
