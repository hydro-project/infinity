//! The daemon's stores: the core in-memory stores wrapped with per-thread
//! JSON-file persistence and daemon-only bookkeeping.
//!
//! All shared [`ConversationStore`] / [`StateStore`] semantics (spawn orders,
//! ancestor chains, compaction cutoffs, dedup sets, subscriptions, metadata)
//! live in `infinity_agent_core::stores` — the trait impls here are pure
//! delegation. This module layers on top:
//!
//! - lazy per-thread loading from and saving to `{dir}/{thread_id}*.json`,
//! - daemon-only per-thread extras ([`ThreadExtras`]: title, children,
//!   token totals, timestamps, the selected model),
//! - transient and persisted views,
//! - session serialization for migration.

use async_trait::async_trait;
use infinity_agent_core::ThreadId;
use infinity_agent_core::message::InfinityMessage;
use infinity_agent_core::stores::{
    self as core_stores, CompactionSummary, ThreadInfo, ThreadState,
};
use infinity_agent_core::system::UserChoice;
use infinity_agent_core::traits::{ConversationStore, StateStore};
use infinity_protocol::ModelRef;
use infinity_provider_protocol::message::Message;
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

impl From<core_stores::InMemoryStoreError> for MemoryError {
    fn from(e: core_stores::InMemoryStoreError) -> Self {
        MemoryError(e.to_string())
    }
}

// ── Persistent conversation store backed by shared in-memory semantics ──

#[derive(Clone)]
pub struct PersistentConversationStore {
    /// The shared store semantics (threads, messages, compaction summaries).
    core: core_stores::InMemoryConversationStore,
    /// Daemon-only per-thread bookkeeping. Invariant: a thread has an entry
    /// here iff it has one in `core` (both are created together by the
    /// spawn/load/import paths of this wrapper).
    extras: Arc<Mutex<HashMap<ThreadId, ThreadExtras>>>,
    /// Directory where per-thread JSON files are stored. `None` disables persistence.
    dir: Option<PathBuf>,
    /// Tracks which thread IDs have had their full data loaded from disk.
    loaded: Arc<Mutex<HashSet<ThreadId>>>,
    /// Tracks which thread IDs have had their metadata loaded from disk.
    metadata_loaded: Arc<Mutex<HashSet<ThreadId>>>,
    /// Optional sender to notify session store of changes (for SessionsUpdated broadcasts).
    change_tx: Option<tokio::sync::mpsc::UnboundedSender<ThreadId>>,
    /// Per-thread active views, keyed by thread_id → (view_type → content).
    /// Persisted separately to `{thread_id}.views.json`.
    views: Arc<Mutex<HashMap<ThreadId, HashMap<String, serde_json::Value>>>>,
    /// The global default model, used for new threads and backfilled into
    /// metadata serialized before models were tracked per-thread.
    default_model: ModelRef,
    /// Source of new thread ids (deterministic in tests).
    id_source: Arc<dyn crate::ids::IdSource>,
}

/// Daemon-only per-thread bookkeeping, layered on top of the core store's
/// [`ThreadInfo`].
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ThreadExtras {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    children: Vec<ThreadId>,
    #[serde(default)]
    total_tokens_used: usize,
    #[serde(default)]
    last_updated: String,
    /// The model selected for this specific thread. There is no parent-thread
    /// fallback: every thread gets the global default at creation time.
    /// Metadata serialized before models were tracked per-thread lacks this
    /// field; it is backfilled with the store's default model on load (see
    /// [`PersistentConversationStore::backfill_selected_model`]).
    #[serde(default = "unset_model_ref")]
    selected_model: ModelRef,
}

impl ThreadExtras {
    fn new(selected_model: ModelRef) -> Self {
        Self {
            title: None,
            children: Vec::new(),
            total_tokens_used: 0,
            last_updated: String::new(),
            selected_model,
        }
    }
}

/// The serialization shape of a thread's metadata (`{thread_id}.meta.json`
/// and the `metadata` field of [`SerializedThread`]): the core
/// [`ThreadInfo`] and the daemon's [`ThreadExtras`] flattened into one
/// object, byte-compatible with the format written before the core/daemon
/// store split.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ThreadMeta {
    #[serde(flatten)]
    pub(crate) info: ThreadInfo,
    #[serde(flatten)]
    pub(crate) extras: ThreadExtras,
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

/// Helper struct for deserializing the old format (legacy bare Messages + display_as sidecar).
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
/// (legacy bare Messages + display_as map) and new format (InfinityMessage).
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
                let mut inf = InfinityMessage::from_message(msg);
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
    pub metadata: ThreadMeta,
    pub snapshot: ThreadSnapshot,
    #[serde(default)]
    pub views: HashMap<String, serde_json::Value>,
}

impl PersistentConversationStore {
    /// Create a store that persists each thread to its own JSON file under `dir`.
    /// `default_model` is assigned to new threads and backfilled into metadata
    /// that predates per-thread model tracking. `id_source` generates ids for
    /// newly spawned threads (deterministic in tests).
    pub fn new_with_dir(
        dir: impl AsRef<Path>,
        default_model: ModelRef,
        id_source: Arc<dyn crate::ids::IdSource>,
    ) -> Self {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).ok();
        Self {
            core: core_stores::InMemoryConversationStore::new(),
            extras: Arc::new(Mutex::new(HashMap::new())),
            dir: Some(dir),
            loaded: Arc::new(Mutex::new(HashSet::new())),
            metadata_loaded: Arc::new(Mutex::new(HashSet::new())),
            change_tx: None,
            views: Arc::new(Mutex::new(HashMap::new())),
            default_model,
            id_source,
        }
    }

    /// Replace an unset `selected_model` (from metadata serialized before
    /// models were tracked per-thread) with the store's default model.
    fn backfill_selected_model(&self, extras: &mut ThreadExtras) {
        if extras.selected_model.provider_id.is_empty() {
            extras.selected_model = self.default_model.clone();
        }
    }

    /// Set the change notification sender. Called after construction.
    pub fn set_change_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<ThreadId>) {
        self.change_tx = Some(tx);
    }

    /// Migration: if a thread's last_updated / total_tokens_used is empty, try to
    /// restore them from the legacy `sessions.json` (parent of the threads dir).
    fn migrate_from_session_store(&self, thread_id: &ThreadId<str>, threads_dir: &Path) {
        {
            let extras = self.extras.lock().expect("bug: mutex poisoned");
            match extras.get(thread_id) {
                Some(e) if e.last_updated.is_empty() => {}
                _ => return,
            }
        }

        let sessions_path = threads_dir.join("../sessions.json");
        let Ok(json) = std::fs::read_to_string(&sessions_path) else {
            return;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&json) else {
            return;
        };
        let Some(entry) = val.get("sessions").and_then(|s| s.get(thread_id.as_str())) else {
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

        {
            let mut extras = self.extras.lock().expect("bug: mutex poisoned");
            if let Some(e) = extras.get_mut(thread_id) {
                e.last_updated = last_updated;
                e.total_tokens_used = total_tokens_used;
            }
        }
        self.save_thread_metadata(thread_id);
    }

    /// Notify that a session's thread tree changed.
    fn notify_session(&self, thread_id: &ThreadId<str>) {
        if let Some(ref tx) = self.change_tx {
            let root = self.get_root_thread_id(thread_id);
            let _ = tx.send(root);
        }
    }

    /// Combine the core store's info and the daemon extras into the
    /// serialization shape, if the thread exists in both.
    fn thread_meta(&self, thread_id: &ThreadId<str>) -> Option<ThreadMeta> {
        let info = self.core.thread_info(thread_id)?;
        let extras = self
            .extras
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .cloned()?;
        Some(ThreadMeta { info, extras })
    }

    /// Restore a thread's metadata from its serialization shape (splitting
    /// it between the core store and the daemon extras).
    fn restore_thread_meta(&self, thread_id: &ThreadId<str>, mut meta: ThreadMeta) {
        self.backfill_selected_model(&mut meta.extras);
        self.core.set_thread_info(thread_id, meta.info);
        self.extras
            .lock()
            .expect("bug: mutex poisoned")
            .insert(thread_id.to_owned(), meta.extras);
    }

    /// Write a single thread's metadata to `{dir}/{thread_id}.meta.json`.
    fn save_thread_metadata(&self, thread_id: &ThreadId<str>) {
        let Some(ref dir) = self.dir else { return };
        if let Some(meta) = self.thread_meta(thread_id) {
            let path = dir.join(format!("{}.meta.json", thread_id));
            if let Ok(json) = serde_json::to_string_pretty(&meta) {
                std::fs::write(path, json).ok();
            }
        }
    }

    /// Write a single thread's data to `{dir}/{thread_id}.json` and metadata to `.meta.json`.
    /// No-op when persistence is disabled.
    fn save_thread(&self, thread_id: &ThreadId<str>) {
        let Some(ref dir) = self.dir else { return };
        let snapshot = ThreadSnapshot {
            messages: self.core.thread_messages(thread_id).unwrap_or_default(),
            compaction_summaries: self.core.thread_compaction_summaries(thread_id),
        };

        let path = dir.join(format!("{}.json", thread_id));
        if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
            std::fs::write(path, json).ok();
        }
        self.save_thread_metadata(thread_id);
    }

    /// Ensure a thread's metadata (core info + extras) is loaded from disk.
    /// Tries `.meta.json` first; falls back to extracting from the full `.json` snapshot
    /// and writes the `.meta.json` for future fast loads.
    fn ensure_thread_metadata_loaded(&self, thread_id: &ThreadId<str>) {
        let Some(ref dir) = self.dir else { return };

        let mut meta_loaded = self.metadata_loaded.lock().expect("bug: mutex poisoned");
        if meta_loaded.contains(thread_id) {
            return;
        }

        // Never overwrite a thread that already exists in memory (e.g. it
        // was created via ensure_root_thread before anything was persisted).
        let already_in_memory = self.core.thread_info(thread_id).is_some();

        // Try the fast metadata file first.
        let meta_path = dir.join(format!("{}.meta.json", thread_id));
        if let Ok(json) = std::fs::read_to_string(&meta_path)
            && let Ok(meta) = serde_json::from_str::<ThreadMeta>(&json)
        {
            if !already_in_memory {
                self.restore_thread_meta(thread_id, meta);
            }
        } else {
            // Fall back: extract thread_info from the full snapshot file.
            let full_path = dir.join(format!("{}.json", thread_id));
            if let Ok(json) = std::fs::read_to_string(&full_path)
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(&json)
                && let Some(info_val) = val.get("thread_info")
                && let Ok(meta) = serde_json::from_value::<ThreadMeta>(info_val.clone())
                && !already_in_memory
            {
                self.restore_thread_meta(thread_id, meta);
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
    fn ensure_thread_loaded(&self, thread_id: &ThreadId<str>) {
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
            assert!(
                !self.core.set_thread_messages(thread_id, snapshot.messages),
                "bug: thread {thread_id} messages loaded twice"
            );
            assert!(
                !self
                    .core
                    .set_thread_compaction_summaries(thread_id, snapshot.compaction_summaries),
                "bug: thread {thread_id} compaction summaries loaded twice"
            );
        }

        loaded.insert(thread_id.to_owned());

        self.load_views(thread_id);
    }

    /// Whether metadata exists for this thread (in memory or on disk).
    pub fn has_thread(&self, thread_id: &ThreadId<str>) -> bool {
        self.ensure_thread_metadata_loaded(thread_id);
        self.core.thread_info(thread_id).is_some()
    }

    /// Resolve a thread ID to its root thread ID (i.e. the session ID).
    pub fn get_root_thread_id(&self, thread_id: &ThreadId<str>) -> ThreadId {
        self.ensure_thread_metadata_loaded(thread_id);
        self.core
            .thread_info(thread_id)
            .map(|t| t.root_thread_id)
            .unwrap_or_else(|| thread_id.to_owned())
    }

    /// Get the parent thread ID, if any.
    pub fn get_thread_parent_id(&self, thread_id: &ThreadId<str>) -> Option<ThreadId> {
        self.ensure_thread_metadata_loaded(thread_id);
        self.core
            .thread_info(thread_id)
            .and_then(|t| t.parent_thread_id)
    }

    /// Set the title for a thread.
    pub fn set_thread_title(&self, thread_id: &ThreadId<str>, title: &str) {
        self.ensure_thread_metadata_loaded(thread_id);
        {
            let mut extras = self.extras.lock().expect("bug: mutex poisoned");
            if let Some(e) = extras.get_mut(thread_id) {
                e.title = Some(title.to_owned());
            }
        }
        self.save_thread_metadata(thread_id);
        self.notify_session(thread_id);
    }

    /// Get the model selected for this specific thread. Does NOT fall back to
    /// the parent thread — every thread is assigned a model at creation time.
    pub fn get_thread_model(&self, thread_id: &ThreadId<str>) -> ModelRef {
        self.ensure_thread_metadata_loaded(thread_id);
        let extras = self.extras.lock().expect("bug: mutex poisoned");
        extras
            .get(thread_id)
            .map(|e| e.selected_model.clone())
            .expect("bug: thread metadata missing in get_thread_model")
    }

    /// Set the model selected for a thread.
    pub fn set_thread_model(&self, thread_id: &ThreadId<str>, model: ModelRef) {
        self.ensure_thread_metadata_loaded(thread_id);
        {
            let mut extras = self.extras.lock().expect("bug: mutex poisoned");
            if let Some(e) = extras.get_mut(thread_id) {
                e.selected_model = model;
            }
        }
        self.save_thread_metadata(thread_id);
    }

    /// List every thread in a session, including the root and descendants that
    /// are closed or used for compaction. Session-level state must still account
    /// for those threads even though they are omitted from the visible thread
    /// list.
    pub fn get_session_thread_ids(&self, root_id: &ThreadId<str>) -> Vec<ThreadId> {
        let mut result = Vec::new();
        let mut queue = vec![root_id.to_owned()];
        let mut visited = HashSet::new();

        while let Some(thread_id) = queue.pop() {
            if !visited.insert(thread_id.to_owned()) {
                continue;
            }
            self.ensure_thread_metadata_loaded(&thread_id);
            let children = {
                let extras = self.extras.lock().expect("bug: mutex poisoned");
                extras
                    .get(&thread_id)
                    .map(|entry| entry.children.clone())
                    .unwrap_or_default()
            };
            result.push(thread_id);
            queue.extend(children);
        }

        result
    }

    /// List open (non-closed) subthreads that are descendants of `parent_id`
    /// within the given session. Walks the children tree via metadata.
    pub fn get_open_subthreads(
        &self,
        parent_id: &ThreadId<str>,
    ) -> Vec<infinity_protocol::SubthreadInfo> {
        self.ensure_thread_metadata_loaded(parent_id);
        let mut result = Vec::new();
        let mut queue = vec![parent_id.to_owned()];
        while let Some(pid) = queue.pop() {
            let children = {
                let extras = self.extras.lock().expect("bug: mutex poisoned");
                extras
                    .get(&pid)
                    .map(|e| e.children.clone())
                    .unwrap_or_default()
            };
            for child_id in children {
                self.ensure_thread_metadata_loaded(&child_id);
                let Some(info) = self.core.thread_info(&child_id) else {
                    continue;
                };
                if !info.closed && !info.is_compaction {
                    let title = {
                        let extras = self.extras.lock().expect("bug: mutex poisoned");
                        extras.get(&child_id).and_then(|e| e.title.clone())
                    };
                    result.push(infinity_protocol::SubthreadInfo {
                        thread_id: child_id.to_string(),
                        parent_thread_id: pid.to_string(),
                        title,
                    });
                    queue.push(child_id);
                }
            }
        }
        result
    }

    pub fn get_total_tokens_used(&self, thread_id: &ThreadId<str>) -> usize {
        self.ensure_thread_metadata_loaded(thread_id);
        self.extras
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .map(|e| e.total_tokens_used)
            .unwrap_or(0)
    }

    pub fn set_total_tokens_used(&self, thread_id: &ThreadId<str>, tokens: usize) {
        self.ensure_thread_metadata_loaded(thread_id);
        if let Some(e) = self
            .extras
            .lock()
            .expect("bug: mutex poisoned")
            .get_mut(thread_id)
        {
            e.total_tokens_used = tokens;
        }
        self.save_thread_metadata(thread_id);
        self.notify_session(thread_id);
    }

    pub fn get_last_updated(&self, thread_id: &ThreadId<str>) -> String {
        self.ensure_thread_metadata_loaded(thread_id);
        self.extras
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .map(|e| e.last_updated.clone())
            .unwrap_or_default()
    }

    pub fn set_last_updated(&self, thread_id: &ThreadId<str>, ts: &str) {
        self.ensure_thread_metadata_loaded(thread_id);
        if let Some(e) = self
            .extras
            .lock()
            .expect("bug: mutex poisoned")
            .get_mut(thread_id)
        {
            e.last_updated = ts.to_owned();
        }
        self.save_thread_metadata(thread_id);
    }

    /// Write views to `{dir}/{thread_id}.views.json`.
    fn save_views(&self, thread_id: &ThreadId<str>) {
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
    fn load_views(&self, thread_id: &ThreadId<str>) {
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
    pub fn set_view(&self, thread_id: &ThreadId<str>, view_type: &str, content: serde_json::Value) {
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
    pub fn get_views(&self, thread_id: &ThreadId<str>) -> HashMap<String, serde_json::Value> {
        self.ensure_thread_loaded(thread_id);
        self.views
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn get_thread_title(&self, thread_id: &ThreadId<str>) -> Option<String> {
        self.ensure_thread_metadata_loaded(thread_id);
        self.extras
            .lock()
            .expect("bug: mutex poisoned")
            .get(thread_id)
            .and_then(|e| e.title.clone())
    }

    /// Serialize all threads in a session tree to a JSON string.
    pub fn serialize_session(&self, root_thread_id: &ThreadId<str>) -> String {
        let mut threads: HashMap<ThreadId, SerializedThread> = HashMap::new();
        let mut queue = vec![root_thread_id.to_owned()];
        while let Some(tid) = queue.pop() {
            self.ensure_thread_loaded(&tid);
            let Some(metadata) = self.thread_meta(&tid) else {
                continue;
            };
            queue.extend(metadata.extras.children.clone());
            let snapshot = ThreadSnapshot {
                messages: self.core.thread_messages(&tid).unwrap_or_default(),
                compaction_summaries: self.core.thread_compaction_summaries(&tid),
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
        let threads: HashMap<ThreadId, SerializedThread> = serde_json::from_str(data)
            .map_err(|e| MemoryError(format!("failed to deserialize session: {e}")))?;
        for (tid, st) in threads {
            self.restore_thread_meta(&tid, st.metadata);
            self.core.set_thread_messages(&tid, st.snapshot.messages);
            self.core
                .set_thread_compaction_summaries(&tid, st.snapshot.compaction_summaries);
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
impl ConversationStore for PersistentConversationStore {
    type Error = MemoryError;

    async fn ensure_root_thread(&self, thread_id: &ThreadId<str>) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(thread_id);
        let inserted = self.core.thread_info(thread_id).is_none();
        if inserted {
            self.core.ensure_root_thread(thread_id).await?;
            self.extras.lock().expect("bug: mutex poisoned").insert(
                thread_id.to_owned(),
                ThreadExtras::new(self.default_model.clone()),
            );
            self.save_thread(thread_id);
        }
        Ok(())
    }

    async fn thread_exists(&self, thread_id: &ThreadId<str>) -> Result<bool, MemoryError> {
        Ok(self.has_thread(thread_id))
    }

    async fn load_history_up_to(
        &self,
        session_id: &ThreadId<str>,
        start_from: Option<i64>,
        up_to: Option<i64>,
    ) -> Result<Vec<InfinityMessage>, MemoryError> {
        self.ensure_thread_loaded(session_id);
        Ok(self
            .core
            .load_history_up_to(session_id, start_from, up_to)
            .await?)
    }

    async fn append_messages(
        &self,
        session_id: &ThreadId<str>,
        messages: Vec<(InfinityMessage, String)>,
    ) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(session_id);
        tracing::trace!("Appending messages to store");
        self.core.append_messages(session_id, messages).await?;
        self.save_thread(session_id);
        Ok(())
    }

    async fn spawn_thread(
        &self,
        parent_thread_id: &ThreadId<str>,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
        spawn_order_override: Option<usize>,
    ) -> Result<ThreadId, MemoryError> {
        self.ensure_thread_loaded(parent_thread_id);
        let new_id = ThreadId::from(self.id_source.generate());
        {
            // Hold the load-tracking locks while creating the thread so a
            // concurrent load cannot interleave with the creation.
            let mut loaded = self.loaded.lock().expect("bug: mutex poisoned");
            let mut meta_loaded = self.metadata_loaded.lock().expect("bug: mutex poisoned");

            // The spawn-order and root-resolution semantics live in the core
            // store; the daemon only picks the ID and layers its extras.
            self.core.spawn_thread_with_id(
                &new_id,
                parent_thread_id,
                spawn_tool_call_id,
                is_for_subscription_event,
                spawn_order_override,
            );
            self.core.set_thread_messages(&new_id, Vec::new());

            {
                let mut extras = self.extras.lock().expect("bug: mutex poisoned");
                // The child inherits the parent's model selection.
                let parent_model = extras
                    .get(parent_thread_id)
                    .map(|e| e.selected_model.clone())
                    .unwrap_or_else(|| self.default_model.clone());
                extras.insert(new_id.clone(), ThreadExtras::new(parent_model));
                if let Some(parent) = extras.get_mut(parent_thread_id) {
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

    async fn is_thread_closed(&self, thread_id: &ThreadId<str>) -> Result<bool, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        Ok(self.core.is_thread_closed(thread_id).await?)
    }

    async fn close_thread(&self, thread_id: &ThreadId<str>) -> Result<(), MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        self.core.close_thread(thread_id).await?;
        self.save_thread_metadata(thread_id);
        self.notify_session(thread_id);
        Ok(())
    }

    async fn is_subscription_event_thread(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<bool, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        Ok(self.core.is_subscription_event_thread(thread_id).await?)
    }

    async fn get_thread_parent_info(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Option<(ThreadId, String)>, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        Ok(self.core.get_thread_parent_info(thread_id).await?)
    }

    async fn get_ancestor_chain(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<(ThreadId, i64)>, MemoryError> {
        // Lazily load the metadata of every ancestor, then delegate the walk
        // (and its ordering semantics) to the core store.
        let mut current = thread_id.to_owned();
        loop {
            self.ensure_thread_metadata_loaded(&current);
            match self
                .core
                .thread_info(&current)
                .and_then(|t| t.parent_thread_id)
            {
                Some(parent) => current = parent,
                None => break,
            }
        }
        Ok(self.core.get_ancestor_chain(thread_id).await?)
    }

    async fn save_compaction_summary(
        &self,
        thread_id: &ThreadId<str>,
        summary: &str,
        up_to_order: i64,
    ) -> Result<(), MemoryError> {
        self.ensure_thread_loaded(thread_id);
        self.core
            .save_compaction_summary(thread_id, summary, up_to_order)
            .await?;
        self.save_thread(thread_id);
        Ok(())
    }

    async fn load_latest_compaction_summary_up_to(
        &self,
        thread_id: &ThreadId<str>,
        up_to_order: Option<i64>,
    ) -> Result<Option<(String, i64)>, MemoryError> {
        self.ensure_thread_loaded(thread_id);
        Ok(self
            .core
            .load_latest_compaction_summary_up_to(thread_id, up_to_order)
            .await?)
    }

    async fn is_compaction_thread(&self, thread_id: &ThreadId<str>) -> Result<bool, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        Ok(self.core.is_compaction_thread(thread_id).await?)
    }

    async fn mark_thread_as_compaction(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<(), MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        self.core.mark_thread_as_compaction(thread_id).await?;
        self.save_thread_metadata(thread_id);
        Ok(())
    }

    async fn get_thread_spawn_order(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Option<i64>, MemoryError> {
        self.ensure_thread_metadata_loaded(thread_id);
        Ok(self.core.get_thread_spawn_order(thread_id).await?)
    }
}

// ── Persistent state store backed by shared in-memory semantics ──

#[derive(Clone)]
pub struct PersistentStateStore {
    /// The shared store semantics (processed IDs, metadata, subscriptions).
    core: core_stores::InMemoryStateStore,
    /// Directory where per-thread state JSON files are stored.
    dir: PathBuf,
    /// Tracks which keys have already been loaded (or attempted) from disk.
    loaded: Arc<Mutex<HashSet<ThreadId>>>,
    // Used to resolve child threads for stopped-session policy.
    conversation_store: PersistentConversationStore,
    session_store: Arc<tokio::sync::Mutex<crate::session_store::SessionStore>>,
}

impl PersistentStateStore {
    pub fn new(
        dir: impl AsRef<Path>,
        conversation_store: PersistentConversationStore,
        session_store: Arc<tokio::sync::Mutex<crate::session_store::SessionStore>>,
    ) -> Self {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).ok();
        Self {
            core: core_stores::InMemoryStateStore::new(),
            dir,
            loaded: Arc::new(Mutex::new(HashSet::new())),
            conversation_store,
            session_store,
        }
    }

    pub fn has_pending_choices(&self, thread_id: &ThreadId<str>) -> bool {
        self.ensure_loaded(thread_id);
        !self
            .core
            .thread_state(thread_id)
            .pending_user_choices
            .is_empty()
    }

    /// Whether any thread in the session rooted at `session_id` has a pending
    /// user choice. The exact-thread query above remains available for routing
    /// choice responses to the thread that requested them.
    pub fn has_pending_choices_for_session(&self, session_id: &ThreadId<str>) -> bool {
        self.conversation_store
            .get_session_thread_ids(session_id)
            .iter()
            .any(|thread_id| self.has_pending_choices(thread_id))
    }

    pub fn pending_choice(&self, thread_id: &ThreadId<str>, choice_id: &str) -> Option<UserChoice> {
        self.ensure_loaded(thread_id);
        self.core
            .thread_state(thread_id)
            .pending_user_choices
            .into_iter()
            .find(|choice| choice.id == choice_id)
    }

    pub async fn clear_pending_choices(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<(), MemoryError> {
        let choices = self.get_pending_user_choices(thread_id).await?;
        for choice in choices {
            self.remove_pending_user_choice(thread_id, &choice.id)
                .await?;
        }
        Ok(())
    }

    /// Clear pending choices from the root and every descendant, including
    /// closed and compaction threads that are not shown in the session UI.
    pub async fn clear_pending_choices_for_session(
        &self,
        session_id: &ThreadId<str>,
    ) -> Result<(), MemoryError> {
        for thread_id in self.conversation_store.get_session_thread_ids(session_id) {
            self.clear_pending_choices(&thread_id).await?;
        }
        Ok(())
    }

    /// Write a single key's state data to `{dir}/{key}.state.json`. The file
    /// is the serialized core [`ThreadState`] snapshot.
    fn save_key(&self, key: &ThreadId<str>) {
        let snapshot = self.core.thread_state(key);
        let path = self.dir.join(format!("{}.state.json", key));
        if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
            std::fs::write(path, json).ok();
        }
    }

    /// Ensure a key's data is loaded from disk into the core store.
    fn ensure_loaded(&self, key: &ThreadId<str>) {
        let mut loaded = self.loaded.lock().expect("bug: mutex poisoned");
        if loaded.contains(key) {
            return;
        }

        let path = self.dir.join(format!("{}.state.json", key));
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(snapshot) = serde_json::from_str::<ThreadState>(&json)
        {
            self.core.set_thread_state(key, snapshot);
        }

        loaded.insert(key.to_owned());
    }
}

#[async_trait]
impl StateStore for PersistentStateStore {
    type Error = MemoryError;

    async fn get_processed_ids(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<HashSet<String>, MemoryError> {
        self.ensure_loaded(thread_id);
        Ok(self.core.get_processed_ids(thread_id).await?)
    }

    async fn add_processed_message_ids(
        &self,
        thread_id: &ThreadId<str>,
        message_ids: Vec<String>,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(thread_id);
        self.core
            .add_processed_message_ids(thread_id, message_ids)
            .await?;
        self.save_key(thread_id);
        Ok(())
    }

    async fn get_metadata(
        &self,
        root_thread_id: &ThreadId<str>,
    ) -> Result<Option<serde_json::Value>, MemoryError> {
        self.ensure_loaded(root_thread_id);
        Ok(self.core.get_metadata(root_thread_id).await?)
    }

    async fn set_metadata(
        &self,
        root_thread_id: &ThreadId<str>,
        metadata: serde_json::Value,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(root_thread_id);
        self.core.set_metadata(root_thread_id, metadata).await?;
        self.save_key(root_thread_id);
        Ok(())
    }

    async fn get_active_subscriptions(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<String>, MemoryError> {
        self.ensure_loaded(thread_id);
        Ok(self.core.get_active_subscriptions(thread_id).await?)
    }

    async fn add_active_subscription(
        &self,
        thread_id: &ThreadId<str>,
        tool_call_id: &str,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(thread_id);
        self.core
            .add_active_subscription(thread_id, tool_call_id)
            .await?;
        self.save_key(thread_id);
        Ok(())
    }

    async fn remove_active_subscription(
        &self,
        thread_id: &ThreadId<str>,
        tool_call_id: &str,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(thread_id);
        self.core
            .remove_active_subscription(thread_id, tool_call_id)
            .await?;
        self.save_key(thread_id);
        Ok(())
    }

    async fn add_pending_user_choice(
        &self,
        thread_id: &ThreadId<str>,
        choice: UserChoice,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(thread_id);
        self.core.add_pending_user_choice(thread_id, choice).await?;
        self.save_key(thread_id);
        self.conversation_store.notify_session(thread_id);
        Ok(())
    }

    async fn remove_pending_user_choice(
        &self,
        thread_id: &ThreadId<str>,
        choice_id: &str,
    ) -> Result<(), MemoryError> {
        self.ensure_loaded(thread_id);
        self.core
            .remove_pending_user_choice(thread_id, choice_id)
            .await?;
        self.save_key(thread_id);
        self.conversation_store.notify_session(thread_id);
        Ok(())
    }

    async fn get_pending_user_choices(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<UserChoice>, MemoryError> {
        self.ensure_loaded(thread_id);
        Ok(self.core.get_pending_user_choices(thread_id).await?)
    }

    async fn is_thread_stopped(&self, thread_id: &ThreadId<str>) -> Result<bool, MemoryError> {
        let session_id = self.conversation_store.get_root_thread_id(thread_id);
        Ok(self.session_store.lock().await.is_shut_down(&session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinity_agent_core::traits::ConversationStore;
    use infinity_provider_protocol::message::{AssistantContent, Message, UserContent};

    fn test_model() -> ModelRef {
        ModelRef {
            provider_id: "test".to_owned(),
            model_id: "test-model".to_owned(),
        }
    }

    fn user_msg(text: &str) -> InfinityMessage {
        InfinityMessage::from_message(Message::User {
            content: vec![UserContent::text(text)],
        })
    }

    /// The `.meta.json` format written before the core/daemon store split
    /// (all fields in one flat object) must keep deserializing, and the
    /// flattened [`ThreadMeta`] must serialize the same flat key set.
    #[test]
    fn thread_meta_format_is_flat_and_backward_compatible() {
        let old_json = serde_json::json!({
            "parent_thread_id": "p",
            "root_thread_id": "r",
            "spawn_message_order": 3,
            "spawn_tool_call_id": "tc",
            "closed": false,
            "is_subscription_event": false,
            "title": "t",
            "is_compaction": false,
            "children": ["c1"],
            "total_tokens_used": 42,
            "last_updated": "2024",
            "selected_model": {"provider_id": "prov", "model_id": "m"}
        });
        let meta: ThreadMeta =
            serde_json::from_value(old_json.clone()).expect("old flat format parses");
        assert_eq!(meta.info.root_thread_id.as_str(), "r");
        assert_eq!(meta.info.spawn_message_order, Some(3));
        assert_eq!(meta.extras.total_tokens_used, 42);
        assert_eq!(meta.extras.selected_model.provider_id, "prov");
        let round = serde_json::to_value(&meta).expect("serialize ThreadMeta");
        assert_eq!(
            round, old_json,
            "flattened ThreadMeta must keep the flat on-disk format"
        );
    }
    fn asst_msg(text: &str) -> InfinityMessage {
        InfinityMessage::from_message(Message::Assistant {
            content: vec![AssistantContent::text(text)],
        })
    }

    /// Parent has messages, child spawned at index 2. load_history_with_ancestors
    /// should return parent[0..2] + child messages.
    #[tokio::test]
    async fn ancestors_basic_cutoff() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = PersistentConversationStore::new_with_dir(
            dir.path(),
            test_model(),
            Arc::new(crate::ids::UuidIdSource),
        );
        store
            .ensure_root_thread(ThreadId::from_ref("root"))
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                ThreadId::from_ref("root"),
                vec![(user_msg("p1"), "m1".into()), (asst_msg("p2"), "m2".into())],
            )
            .await
            .expect("append root messages");

        let child = store
            .spawn_thread(ThreadId::from_ref("root"), "tc-1", false, None)
            .await
            .expect("spawn child thread");

        store
            .append_messages(
                ThreadId::from_ref("root"),
                vec![(user_msg("p3"), "m3".into()), (asst_msg("p4"), "m4".into())],
            )
            .await
            .expect("append root messages after spawn");

        store
            .append_messages(&child, vec![(user_msg("c1"), "m5".into())])
            .await
            .expect("append child messages");

        let (history, _, _) = store
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
        let store = PersistentConversationStore::new_with_dir(
            dir.path(),
            test_model(),
            Arc::new(crate::ids::UuidIdSource),
        );
        store
            .ensure_root_thread(ThreadId::from_ref("root"))
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                ThreadId::from_ref("root"),
                vec![(user_msg("r1"), "m1".into())],
            )
            .await
            .expect("append root messages");

        let child = store
            .spawn_thread(ThreadId::from_ref("root"), "tc-1", false, None)
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

        let (history, _, _) = store
            .load_history_with_ancestors(&grandchild)
            .await
            .expect("load history with ancestors");
        assert_eq!(history.len(), 4);
    }

    /// Compaction on root: should return [summary] + messages after compaction point.
    #[tokio::test]
    async fn ancestors_with_compaction_on_self() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = PersistentConversationStore::new_with_dir(
            dir.path(),
            test_model(),
            Arc::new(crate::ids::UuidIdSource),
        );
        store
            .ensure_root_thread(ThreadId::from_ref("root"))
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                ThreadId::from_ref("root"),
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
            .save_compaction_summary(ThreadId::from_ref("root"), "summary of old stuff", 2)
            .await
            .expect("save compaction summary");

        let (history, compacted_up_to, _) = store
            .load_history_with_ancestors(ThreadId::from_ref("root"))
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
        let store = PersistentConversationStore::new_with_dir(
            dir.path(),
            test_model(),
            Arc::new(crate::ids::UuidIdSource),
        );
        store
            .ensure_root_thread(ThreadId::from_ref("root"))
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                ThreadId::from_ref("root"),
                vec![
                    (user_msg("old1"), "m1".into()),
                    (asst_msg("old2"), "m2".into()),
                    (user_msg("recent"), "m3".into()),
                ],
            )
            .await
            .expect("append root messages");

        store
            .save_compaction_summary(ThreadId::from_ref("root"), "compacted root", 2)
            .await
            .expect("save compaction summary");

        let child = store
            .spawn_thread(ThreadId::from_ref("root"), "tc-1", false, None)
            .await
            .expect("spawn child thread");
        store
            .append_messages(&child, vec![(user_msg("c1"), "m4".into())])
            .await
            .expect("append child messages");

        let (history, _, _) = store
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
        let store = PersistentConversationStore::new_with_dir(
            dir.path(),
            test_model(),
            Arc::new(crate::ids::UuidIdSource),
        );
        store
            .ensure_root_thread(ThreadId::from_ref("root"))
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                ThreadId::from_ref("root"),
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
            .save_compaction_summary(ThreadId::from_ref("root"), "early summary", 2)
            .await
            .expect("save early compaction summary");
        store
            .save_compaction_summary(ThreadId::from_ref("root"), "later summary", 4)
            .await
            .expect("save later compaction summary");

        let child = store
            .spawn_thread(ThreadId::from_ref("root"), "tc-1", false, None)
            .await
            .expect("spawn child thread");
        store
            .append_messages(&child, vec![(user_msg("c1"), "m6".into())])
            .await
            .expect("append child messages");

        let (history, _, _) = store
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
        let store = PersistentConversationStore::new_with_dir(
            dir.path(),
            test_model(),
            Arc::new(crate::ids::UuidIdSource),
        );
        store
            .ensure_root_thread(ThreadId::from_ref("root"))
            .await
            .expect("ensure root thread");
        store
            .append_messages(
                ThreadId::from_ref("root"),
                vec![(user_msg("r1"), "m1".into()), (asst_msg("r2"), "m2".into())],
            )
            .await
            .expect("append root messages");
        store
            .save_compaction_summary(ThreadId::from_ref("root"), "root compaction", 2)
            .await
            .expect("save root compaction summary");

        let child = store
            .spawn_thread(ThreadId::from_ref("root"), "tc-1", false, None)
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

        let (history, compacted_up_to, _) = store
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
