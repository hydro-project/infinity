use async_trait::async_trait;
use infinity_agent_core::message::{InfinityMessage, InputMessage};
use infinity_agent_core::traits::{ConversationStore, InputSender, StateStore};
use infinity_protocol::ModelRef;
use rig::message::Message;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::session_store::PendingChoice;

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
    messages: Arc<Mutex<HashMap<String, Vec<(InfinityMessage, String)>>>>,
    /// thread_id -> ThreadInfo
    threads: Arc<Mutex<HashMap<String, ThreadInfo>>>,
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
    /// Transient pending user choice requests, keyed by root thread id.
    pending_choices: Arc<Mutex<HashMap<String, Vec<PendingChoice>>>>,
    /// Per-thread active views, keyed by thread_id → (view_type → content).
    /// Persisted separately to `{thread_id}.views.json`.
    views: Arc<Mutex<HashMap<String, HashMap<String, serde_json::Value>>>>,
    /// The global default model, used for new threads and backfilled into
    /// metadata serialized before models were tracked per-thread.
    default_model: ModelRef,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ThreadInfo {
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
    #[serde(default)]
    total_tokens_used: usize,
    #[serde(default)]
    last_updated: String,
    /// The model selected for this specific thread. There is no parent-thread
    /// fallback: every thread gets the global default at creation time.
    /// Metadata serialized before models were tracked per-thread lacks this
    /// field; it is backfilled with the store's default model on load (see
    /// [`InMemoryConversationStore::backfill_selected_model`]).
    #[serde(default = "unset_model_ref")]
    selected_model: ModelRef,
}

/// Serde default marking `selected_model` as absent in old serialized
/// metadata; replaced with the store's default model on load. The empty
/// provider id is a safe sentinel because the daemon's `ModelCatalog` asserts
/// that registered provider ids are never empty.
fn unset_model_ref() -> ModelRef {
    ModelRef {
        provider_id: String::new(),
        model_id: String::new(),
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct CompactionSummary {
    summary: String,
    up_to_order: i64,
}

/// Per-thread snapshot written to `{dir}/{thread_id}.json`.
#[derive(Serialize)]
pub(crate) struct ThreadSnapshot {
    messages: Vec<(InfinityMessage, String)>,
    #[serde(default)]
    compaction_summaries: Vec<CompactionSummary>,
}

/// Helper struct for deserializing the new format directly.
#[derive(Deserialize)]
struct NewThreadSnapshot {
    messages: Vec<(InfinityMessage, String)>,
    #[serde(default)]
    compaction_summaries: Vec<CompactionSummary>,
}

/// Helper struct for deserializing the old format (bare rig Messages + display_as sidecar).
#[derive(Deserialize)]
struct OldThreadSnapshot {
    #[serde(default)]
    messages: Vec<(Message, String)>,
    #[serde(default, deserialize_with = "deserialize_legacy_display_as_map")]
    display_as: HashMap<String, Vec<rap_protocol::DisplaySegment>>,
    #[serde(default)]
    compaction_summaries: Vec<CompactionSummary>,
}

/// Custom deserializer for ThreadSnapshot that handles both old format
/// (bare rig Messages + display_as map) and new format (InfinityMessage).
impl<'de> Deserialize<'de> for ThreadSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw: serde_json::Value = Deserialize::deserialize(deserializer)?;

        // Try new format first, fall back to old format
        if let Ok(new) = serde_json::from_value::<NewThreadSnapshot>(raw.clone()) {
            return Ok(ThreadSnapshot {
                messages: new.messages,
                compaction_summaries: new.compaction_summaries,
            });
        }

        let old: OldThreadSnapshot =
            serde_json::from_value(raw).map_err(serde::de::Error::custom)?;

        let messages = old
            .messages
            .into_iter()
            .map(|(msg, id)| {
                let mut inf = InfinityMessage::from_rig_message(msg);
                if let InfinityMessage::ToolResult {
                    ref result,
                    ref mut display_segments,
                } = inf
                    && let Some(segs) = old.display_as.get(&result.id)
                {
                    *display_segments = Some(segs.clone());
                }
                (inf, id)
            })
            .collect();

        Ok(ThreadSnapshot {
            messages,
            compaction_summaries: old.compaction_summaries,
        })
    }
}

/// Deserialize the legacy display_as map, handling both old String and new Vec<DisplaySegment> formats.
fn deserialize_legacy_display_as_map<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<rap_protocol::DisplaySegment>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: HashMap<String, serde_json::Value> = HashMap::deserialize(deserializer)?;
    let mut result = HashMap::new();
    for (k, v) in raw {
        let segments = match v {
            serde_json::Value::String(s) => vec![rap_protocol::DisplaySegment::Text(s)],
            serde_json::Value::Array(_) => {
                serde_json::from_value(v).map_err(serde::de::Error::custom)?
            }
            _ => continue,
        };
        result.insert(k, segments);
    }
    Ok(result)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SerializedThread {
    pub metadata: ThreadInfo,
    pub snapshot: ThreadSnapshot,
    #[serde(default)]
    pub views: HashMap<String, serde_json::Value>,
}

impl InMemoryConversationStore {
    /// Create a store that persists each thread to its own JSON file under `dir`.
    /// `default_model` is assigned to new threads and backfilled into metadata
    /// that predates per-thread model tracking.
    pub fn new_with_dir(dir: impl AsRef<Path>, default_model: ModelRef) -> Self {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).ok();
        Self {
            messages: Arc::new(Mutex::new(HashMap::new())),
            threads: Arc::new(Mutex::new(HashMap::new())),
            compaction_summaries: Arc::new(Mutex::new(HashMap::new())),
            dir: Some(dir),
            loaded: Arc::new(Mutex::new(HashSet::new())),
            metadata_loaded: Arc::new(Mutex::new(HashSet::new())),
            change_tx: None,
            pending_choices: Arc::new(Mutex::new(HashMap::new())),
            views: Arc::new(Mutex::new(HashMap::new())),
            default_model,
        }
    }

    /// Replace an unset `selected_model` (from metadata serialized before
    /// models were tracked per-thread) with the store's default model.
    fn backfill_selected_model(&self, info: &mut ThreadInfo) {
        if info.selected_model.provider_id.is_empty() {
            info.selected_model = self.default_model.clone();
        }
    }

    /// Set the change notification sender. Called after construction.
    pub fn set_change_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        self.change_tx = Some(tx);
    }

    /// Migration: if a thread's last_updated / total_tokens_used is empty, try to
    /// restore them from the legacy `sessions.json` (parent of the threads dir).
    fn migrate_from_session_store(&self, thread_id: &str, threads_dir: &Path) {
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        let Some(info) = threads.get(thread_id) else {
            return;
        };
        if !info.last_updated.is_empty() {
            return;
        }
        drop(threads);

        let sessions_path = threads_dir.join("../sessions.json");
        let Ok(json) = std::fs::read_to_string(&sessions_path) else {
            return;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&json) else {
            return;
        };
        let Some(entry) = val.get("sessions").and_then(|s| s.get(thread_id)) else {
            return;
        };

        let last_updated = entry
            .get("last_updated")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let total_tokens_used = entry
            .get("total_tokens_used")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        if last_updated.is_empty() && total_tokens_used == 0 {
            return;
        }

        let mut threads = self.threads.lock().expect("bug: mutex poisoned");
        if let Some(info) = threads.get_mut(thread_id) {
            info.last_updated = last_updated;
            info.total_tokens_used = total_tokens_used;
        }
        drop(threads);
        self.save_thread_metadata(thread_id);
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
        let compaction_summaries = self
            .compaction_summaries
            .lock()
            .expect("bug: mutex poisoned");

        let snapshot = ThreadSnapshot {
            messages: messages.get(thread_id).cloned().unwrap_or_default(),
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
            && let Ok(mut info) = serde_json::from_str::<ThreadInfo>(&json)
        {
            self.backfill_selected_model(&mut info);
            self.threads
                .lock()
                .expect("bug: mutex poisoned")
                .entry(thread_id.to_owned())
                .or_insert(info);
        } else {
            // Fall back: extract thread_info from the full snapshot file.
            let full_path = dir.join(format!("{}.json", thread_id));
            if let Ok(json) = std::fs::read_to_string(&full_path)
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(&json)
                && let Some(info_val) = val.get("thread_info")
                && let Ok(mut info) = serde_json::from_value::<ThreadInfo>(info_val.clone())
            {
                self.backfill_selected_model(&mut info);
                self.threads
                    .lock()
                    .expect("bug: mutex poisoned")
                    .entry(thread_id.to_owned())
                    .or_insert(info);
                // Migrate: write the .meta.json for next time.
                self.save_thread_metadata(thread_id);
            }
        }

        // Migration: restore title/last_updated from legacy sessions.json
        self.migrate_from_session_store(thread_id, dir);

        meta_loaded.insert(thread_id.to_owned());
    }

    /// Ensure a thread's full data (messages, compaction summaries) is loaded.
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
            let mut compaction_summaries = self
                .compaction_summaries
                .lock()
                .expect("bug: mutex poisoned");

            assert!(
                messages
                    .insert(thread_id.to_owned(), snapshot.messages)
                    .is_none()
            );
            assert!(
                compaction_summaries
                    .insert(thread_id.to_owned(), snapshot.compaction_summaries)
                    .is_none()
            );
        }

        loaded.insert(thread_id.to_owned());

        self.load_views(thread_id);
    }

    /// Resolve a thread ID to its root thread ID (i.e. the session ID).
    pub fn get_root_thread_id(&self, thread_id: &str) -> String {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        threads
            .get(thread_id)
            .map(|t| t.root_thread_id.clone())
            .unwrap_or_else(|| thread_id.to_owned())
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
                t.title = Some(title.to_owned());
            }
        }
        self.save_thread_metadata(thread_id);
        self.notify_session(thread_id);
    }

    /// Get the model selected for this specific thread. Does NOT fall back to
    /// the parent thread — every thread is assigned a model at creation time.
    pub fn get_thread_model(&self, thread_id: &str) -> ModelRef {
        self.ensure_thread_metadata_loaded(thread_id);
        let threads = self.threads.lock().expect("bug: mutex poisoned");
        threads
            .get(thread_id)
            .map(|t| t.selected_model.clone())
            .expect("bug: thread metadata missing in get_thread_model")
    }

    /// Set the model selected for a thread.
    pub fn set_thread_model(&self, thread_id: &str, model: ModelRef) {
        self.ensure_thread_metadata_loaded(thread_id);
        {
            let mut threads = self.threads.lock().expect("bug: mutex poisoned");
            if let Some(t) = threads.get_mut(thread_id) {
                t.selected_model = model;
            }
        }
        self.save_thread_metadata(thread_id);
    }

    /// List open (non-closed) subthreads that are descendants of `parent_id`
    /// within the given session. Walks the children tree via metadata.
    pub fn get_open_subthreads(&self, parent_id: &str) -> Vec<infinity_protocol::SubthreadInfo> {
        self.ensure_thread_metadata_loaded(parent_id);
        let mut result = Vec::new();
        let mut queue = vec![parent_id.to_owned()];
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

    pub fn get_total_tokens_used(&self, thread_id: &str) -> usize {
        self.ensure_thread_metadata_loaded(thread_id);
        self.threads
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .map(|t| t.total_tokens_used)
            .unwrap_or(0)
    }

    pub fn set_total_tokens_used(&self, thread_id: &str, tokens: usize) {
        self.ensure_thread_metadata_loaded(thread_id);
        if let Some(t) = self
            .threads
            .lock()
            .expect("bug: mutex poisoned")
            .get_mut(thread_id)
        {
            t.total_tokens_used = tokens;
        }
        self.save_thread_metadata(thread_id);
        self.notify_session(thread_id);
    }

    pub fn get_last_updated(&self, thread_id: &str) -> String {
        self.ensure_thread_metadata_loaded(thread_id);
        self.threads
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .map(|t| t.last_updated.clone())
            .unwrap_or_default()
    }

    pub fn set_last_updated(&self, thread_id: &str, ts: &str) {
        self.ensure_thread_metadata_loaded(thread_id);
        if let Some(t) = self
            .threads
            .lock()
            .expect("bug: mutex poisoned")
            .get_mut(thread_id)
        {
            t.last_updated = ts.to_owned();
        }
        self.save_thread_metadata(thread_id);
    }

    /// Write views to `{dir}/{thread_id}.views.json`.
    fn save_views(&self, thread_id: &str) {
        let Some(ref dir) = self.dir else { return };
        let views = self.views.lock().expect("bug: mutex poisoned");
        let path = dir.join(format!("{}.views.json", thread_id));
        match views.get(thread_id) {
            Some(v) if !v.is_empty() => {
                if let Ok(json) = serde_json::to_string_pretty(v) {
                    std::fs::write(path, json).ok();
                }
            }
            _ => {
                let _ = std::fs::remove_file(path); // file might already not exist
            }
        }
    }

    /// Load views from `{dir}/{thread_id}.views.json`.
    fn load_views(&self, thread_id: &str) {
        tracing::info!("Loading views for thread {thread_id}");
        let Some(ref dir) = self.dir else { return };
        let path = dir.join(format!("{}.views.json", thread_id));
        match std::fs::read_to_string(&path) {
            Ok(json) => match serde_json::from_str::<HashMap<String, serde_json::Value>>(&json) {
                Ok(v) => {
                    self.views
                        .lock()
                        .expect("bug: mutex poisoned")
                        .insert(thread_id.to_owned(), v);
                }
                Err(e) => {
                    tracing::error!("Failed to deserialize views: {e}");
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // file does not exist, okay
            Err(e) => {
                tracing::error!("Failed to load views file: {e}");
            }
        }
    }

    /// Update a view for a thread and persist.
    pub fn set_view(&self, thread_id: &str, view_type: &str, content: serde_json::Value) {
        {
            let mut views = self.views.lock().expect("bug: mutex poisoned");
            views
                .entry(thread_id.to_owned())
                .or_default()
                .insert(view_type.to_owned(), content);
        }
        self.save_views(thread_id);
    }

    /// Get all views for a thread.
    pub fn get_views(&self, thread_id: &str) -> HashMap<String, serde_json::Value> {
        self.ensure_thread_loaded(thread_id);
        self.views
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn get_thread_title(&self, thread_id: &str) -> Option<String> {
        self.ensure_thread_metadata_loaded(thread_id);
        self.threads
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .and_then(|t| t.title.clone())
    }

    pub fn add_pending_choice(&self, root_thread_id: &str, choice: PendingChoice) {
        self.pending_choices
            .lock()
            .expect("bug: mutex poisoned")
            .entry(root_thread_id.to_owned())
            .or_default()
            .push(choice);
        self.notify_session(root_thread_id);
    }

    pub fn remove_pending_choice(
        &self,
        root_thread_id: &str,
        choice_id: &str,
    ) -> Option<PendingChoice> {
        let mut map = self.pending_choices.lock().expect("bug: mutex poisoned");
        if let Some(choices) = map.get_mut(root_thread_id)
            && let Some(pos) = choices.iter().position(|c| c.id == choice_id)
        {
            return Some(choices.remove(pos));
        }
        None
    }

    pub fn get_pending_choice_messages(
        &self,
        root_thread_id: &str,
    ) -> Vec<infinity_protocol::DaemonMessage> {
        self.pending_choices
            .lock()
            .expect("bug: mutex poisoned")
            .get(root_thread_id)
            .map(|v| v.iter().map(|c| c.message.clone()).collect())
            .unwrap_or_default()
    }

    pub fn has_pending_choices(&self, root_thread_id: &str) -> bool {
        self.pending_choices
            .lock()
            .expect("bug: mutex poisoned")
            .get(root_thread_id)
            .is_some_and(|v| !v.is_empty())
    }

    pub fn clear_pending_choices(&self, root_thread_id: &str) {
        self.pending_choices
            .lock()
            .expect("bug: mutex poisoned")
            .remove(root_thread_id);
    }

    /// Serialize all threads in a session tree to a JSON string.
    pub fn serialize_session(&self, root_thread_id: &str) -> String {
        let mut threads: HashMap<String, SerializedThread> = HashMap::new();
        let mut queue = vec![root_thread_id.to_owned()];
        while let Some(tid) = queue.pop() {
            self.ensure_thread_loaded(&tid);
            let metadata = {
                self.threads
                    .lock()
                    .expect("bug: mutex poisoned")
                    .get(&tid)
                    .cloned()
            };
            let Some(metadata) = metadata else { continue };
            queue.extend(metadata.children.clone());
            let snapshot = {
                let msgs = self.messages.lock().expect("bug: mutex poisoned");
                let cs = self
                    .compaction_summaries
                    .lock()
                    .expect("bug: mutex poisoned");
                ThreadSnapshot {
                    messages: msgs.get(&tid).cloned().unwrap_or_default(),
                    compaction_summaries: cs.get(&tid).cloned().unwrap_or_default(),
                }
            };
            let views = self
                .views
                .lock()
                .expect("bug: mutex poisoned")
                .get(&tid)
                .cloned()
                .unwrap_or_default();
            threads.insert(
                tid,
                SerializedThread {
                    metadata,
                    snapshot,
                    views,
                },
            );
        }
        serde_json::to_string(&threads).expect("bug: serde serialization failed")
    }

    /// Import a serialized session into the store.
    pub fn import_session(&self, data: &str) -> Result<(), MemoryError> {
        let threads: HashMap<String, SerializedThread> = serde_json::from_str(data)
            .map_err(|e| MemoryError(format!("failed to deserialize session: {e}")))?;
        for (tid, st) in threads {
            let mut metadata = st.metadata;
            self.backfill_selected_model(&mut metadata);
            self.threads
                .lock()
                .expect("bug: mutex poisoned")
                .insert(tid.clone(), metadata);
            self.messages
                .lock()
                .expect("bug: mutex poisoned")
                .insert(tid.clone(), st.snapshot.messages);
            self.compaction_summaries
                .lock()
                .expect("bug: mutex poisoned")
                .insert(tid.clone(), st.snapshot.compaction_summaries);
            if !st.views.is_empty() {
                self.views
                    .lock()
                    .expect("bug: mutex poisoned")
                    .insert(tid.clone(), st.views);
            }
            self.loaded
                .lock()
                .expect("bug: mutex poisoned")
                .insert(tid.clone());
            self.metadata_loaded
                .lock()
                .expect("bug: mutex poisoned")
                .insert(tid.clone());
            self.save_thread(&tid);
            self.save_views(&tid);
        }
        Ok(())
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
                    thread_id.to_owned(),
                    ThreadInfo {
                        parent_thread_id: None,
                        root_thread_id: thread_id.to_owned(),
                        spawn_message_order: None,
                        spawn_tool_call_id: None,
                        closed: false,
                        is_subscription_event: false,
                        title: None,
                        is_compaction: false,
                        children: Vec::new(),
                        total_tokens_used: 0,
                        last_updated: String::new(),
                        selected_model: self.default_model.clone(),
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
    ) -> Result<Vec<InfinityMessage>, MemoryError> {
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
        messages: Vec<(InfinityMessage, String)>,
    ) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(session_id);
        tracing::trace!("Appending messages to store");
        {
            let mut store = self.messages.lock().expect("bug: mutex poisoned");
            let entry = store.entry(session_id.to_owned()).or_default();
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
        spawn_order_override: Option<usize>,
    ) -> Result<String, MemoryError> {
        self.ensure_thread_loaded(parent_thread_id);
        let new_id = uuid::Uuid::new_v4().to_string();
        let spawn_message_order;
        let root;
        {
            let threads = self.threads.lock().expect("bug: mutex poisoned");
            let msgs = self.messages.lock().expect("bug: mutex poisoned");
            spawn_message_order = spawn_order_override
                .unwrap_or_else(|| msgs.get(parent_thread_id).map(|v| v.len()).unwrap_or(0))
                as i64;
            root = threads
                .get(parent_thread_id)
                .map(|t| t.root_thread_id.clone())
                .unwrap_or_else(|| parent_thread_id.to_owned());
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
                        parent_thread_id: Some(parent_thread_id.to_owned()),
                        root_thread_id: root,
                        spawn_message_order: Some(spawn_message_order),
                        spawn_tool_call_id: Some(spawn_tool_call_id.to_owned()),
                        closed: false,
                        is_subscription_event: is_for_subscription_event,
                        title: None,
                        is_compaction: false,
                        children: Vec::new(),
                        total_tokens_used: 0,
                        last_updated: String::new(),
                        selected_model: self.default_model.clone(),
                    },
                );
                // Add to parent's children list.
                if let Some(parent) = threads.get_mut(parent_thread_id) {
                    parent.children.push(new_id.clone());
                }
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
        let mut current = thread_id.to_owned();
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
            cs.entry(thread_id.to_owned())
                .or_default()
                .push(CompactionSummary {
                    summary: summary.to_owned(),
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
                key.to_owned(),
                (
                    snapshot.processed_message_ids,
                    snapshot.processed_tool_call_ids,
                ),
            );
            if let Some(meta) = snapshot.metadata {
                metadata.insert(key.to_owned(), meta);
            }
            if !snapshot.subscriptions.is_empty() {
                subscriptions.insert(key.to_owned(), snapshot.subscriptions);
            }
        }

        loaded.insert(key.to_owned());
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
                .entry(thread_id.to_owned())
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
                .entry(thread_id.to_owned())
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
            store.insert(root_thread_id.to_owned(), metadata);
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
                .entry(thread_id.to_owned())
                .or_default()
                .insert(tool_call_id.to_owned());
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
            .send((message, dedup_id.to_owned()))
            .map_err(|e| MemoryError(format!("channel send failed: {}", e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinity_agent_core::traits::ConversationStore;
    use rig::OneOrMany;
    use rig::message::{AssistantContent, Message, UserContent};

    fn test_model() -> ModelRef {
        ModelRef {
            provider_id: "test".to_owned(),
            model_id: "test-model".to_owned(),
        }
    }

    fn user_msg(text: &str) -> InfinityMessage {
        InfinityMessage::from_rig_message(Message::User {
            content: OneOrMany::one(UserContent::text(text)),
        })
    }
    fn asst_msg(text: &str) -> InfinityMessage {
        InfinityMessage::from_rig_message(Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text(text)),
        })
    }

    /// Parent has messages, child spawned at index 2. load_history_with_ancestors
    /// should return parent[0..2] + child messages.
    #[tokio::test]
    async fn ancestors_basic_cutoff() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path(), test_model());
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
            .spawn_thread("root", "tc-1", false, None)
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
        if let InfinityMessage::User {
            content: UserContent::Text(t),
        } = &history[0]
        {
            assert_eq!(t.text, "p1");
        }
        if let InfinityMessage::User {
            content: UserContent::Text(t),
        } = &history[2]
        {
            assert_eq!(t.text, "c1");
        }
    }

    /// Three-level chain: root → child → grandchild.
    #[tokio::test]
    async fn ancestors_three_levels() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path(), test_model());
        store
            .ensure_root_thread("root")
            .await
            .expect("ensure root thread");
        store
            .append_messages("root", vec![(user_msg("r1"), "m1".into())])
            .await
            .expect("append root messages");

        let child = store
            .spawn_thread("root", "tc-1", false, None)
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
            .spawn_thread(&child, "tc-2", false, None)
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
        let store = InMemoryConversationStore::new_with_dir(dir.path(), test_model());
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
        if let InfinityMessage::Assistant {
            content: AssistantContent::Text(t),
        } = &history[0]
        {
            assert!(t.text.contains("summary of old stuff"));
        }
    }

    /// Compaction on parent: child should use parent's compaction summary.
    #[tokio::test]
    async fn ancestors_with_compaction_on_parent() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path(), test_model());
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
            .spawn_thread("root", "tc-1", false, None)
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
        if let InfinityMessage::Assistant {
            content: AssistantContent::Text(t),
        } = &history[0]
        {
            assert!(t.text.contains("compacted root"));
        }
    }

    /// Two compactions on root — should pick the latest that fits within cutoff.
    #[tokio::test]
    async fn ancestors_multiple_compactions_picks_latest() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path(), test_model());
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
            .spawn_thread("root", "tc-1", false, None)
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
        if let InfinityMessage::Assistant {
            content: AssistantContent::Text(t),
        } = &history[0]
        {
            assert!(t.text.contains("later summary"));
        }
    }

    /// Both parent and leaf have compactions. The leaf's compaction should be
    /// used exclusively — ancestors are skipped entirely.
    #[tokio::test]
    async fn leaf_compaction_takes_priority_over_ancestor() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = InMemoryConversationStore::new_with_dir(dir.path(), test_model());
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
            .spawn_thread("root", "tc-1", false, None)
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
        if let InfinityMessage::Assistant {
            content: AssistantContent::Text(t),
        } = &history[0]
        {
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
        // The remaining messages should be the child's post-compaction messages
        if let InfinityMessage::User {
            content: UserContent::Text(t),
        } = &history[1]
        {
            assert_eq!(t.text, "c3");
        }
    }
}
