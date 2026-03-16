use async_trait::async_trait;
use rig::OneOrMany;
use rig::message::{AssistantContent, Message};

use crate::message::InputMessage;

/// Persistent conversation history storage (DSQL in Lambda, in-memory for CLI).
#[async_trait]
pub trait ConversationStore: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), Self::Error>;

    /// Load history for a session. `start_from` (exclusive) and `up_to`
    /// (inclusive) are optional bounds on message order. `None` means unbounded.
    async fn load_history_up_to(
        &self,
        session_id: &str,
        start_from: Option<i64>,
        up_to: Option<i64>,
    ) -> Result<Vec<Message>, Self::Error>;

    /// Load full history for a thread including ancestor context and compaction.
    /// Walks backwards through ancestors to find the most recent compaction
    /// summary, skipping all earlier ancestors (their content is in the summary).
    /// Returns `(history, leaf_compacted_up_to)` where the second element is
    /// the absolute store index the leaf thread's compaction covers, if any.
    async fn load_history_with_ancestors(
        &self,
        thread_id: &str,
    ) -> Result<(Vec<Message>, Option<i64>), Self::Error> {
        // Check the thread itself first
        if let Ok(Some((summary, compacted_up_to))) = self
            .load_latest_compaction_summary_up_to(thread_id, None)
            .await
        {
            let mut combined = vec![Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text(&format!(
                    "[Compacted conversation summary]\n{}",
                    summary
                ))),
            }];
            combined.extend(
                self.load_history_up_to(thread_id, Some(compacted_up_to), None)
                    .await?,
            );
            return Ok((combined, Some(compacted_up_to)));
        }

        let ancestors = self.get_ancestor_chain(thread_id).await?;

        // Walk backwards to find the first ancestor with a compaction summary
        let mut compaction_idx = None;
        let mut compaction_summary = None;
        for i in (0..ancestors.len()).rev() {
            let (ref tid, cutoff) = ancestors[i];
            if let Ok(Some((summary, compacted_up_to))) = self
                .load_latest_compaction_summary_up_to(tid, Some(cutoff))
                .await
            {
                compaction_idx = Some(i);
                compaction_summary = Some((summary, compacted_up_to));
                break;
            }
        }

        let mut combined = Vec::new();

        if let (Some(idx), Some((summary, compacted_up_to))) = (compaction_idx, compaction_summary)
        {
            // Prepend the compaction summary (covers all ancestors before idx + ancestor idx's messages up to compacted_up_to)
            combined.push(Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text(&format!(
                    "[Compacted conversation summary]\n{}",
                    summary
                ))),
            });
            // Load remaining messages from the compacted ancestor (after summary, up to cutoff)
            let (_, cutoff) = &ancestors[idx];
            combined.extend(
                self.load_history_up_to(&ancestors[idx].0, Some(compacted_up_to), Some(*cutoff))
                    .await?,
            );
            // Load subsequent ancestors normally
            for (tid, cutoff) in &ancestors[idx + 1..] {
                combined.extend(self.load_history_up_to(tid, None, Some(*cutoff)).await?);
            }
        } else {
            // No compaction anywhere — load all ancestors
            for (tid, cutoff) in &ancestors {
                combined.extend(self.load_history_up_to(tid, None, Some(*cutoff)).await?);
            }
        }

        combined.extend(self.load_history_up_to(thread_id, None, None).await?);
        Ok((combined, None))
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: Vec<(Message, String)>,
    ) -> Result<(), Self::Error>;

    async fn spawn_thread(
        &self,
        parent_thread_id: &str,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
    ) -> Result<String, Self::Error>;

    async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, Self::Error>;

    async fn close_thread(&self, thread_id: &str) -> Result<(), Self::Error>;

    async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, Self::Error>;

    async fn get_thread_parent_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>, Self::Error>;

    async fn get_ancestor_chain(&self, thread_id: &str) -> Result<Vec<(String, i64)>, Self::Error>;

    // ── Compaction support ──

    async fn mark_thread_as_compaction(&self, thread_id: &str) -> Result<(), Self::Error>;

    async fn is_compaction_thread(&self, thread_id: &str) -> Result<bool, Self::Error>;

    async fn get_thread_spawn_order(&self, thread_id: &str) -> Result<Option<i64>, Self::Error>;

    async fn save_compaction_summary(
        &self,
        thread_id: &str,
        summary: &str,
        up_to_order: i64,
    ) -> Result<(), Self::Error>;

    /// Load the latest compaction summary. When `up_to_order` is `Some(n)`,
    /// only return summaries whose `up_to_order` is <= n.
    async fn load_latest_compaction_summary_up_to(
        &self,
        thread_id: &str,
        up_to_order: Option<i64>,
    ) -> Result<Option<(String, i64)>, Self::Error>;
}

/// Key-value state store for processed message IDs, metadata, toolset caches
/// (DynamoDB in Lambda, in-memory HashMap for CLI).
#[async_trait]
pub trait StateStore: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn get_processed_ids(
        &self,
        thread_id: &str,
    ) -> Result<
        (
            std::collections::HashSet<String>,
            std::collections::HashSet<String>,
        ),
        Self::Error,
    >;

    async fn add_processed_message_ids(
        &self,
        thread_id: &str,
        message_ids: Vec<String>,
    ) -> Result<(), Self::Error>;

    async fn add_processed_tool_calls(
        &self,
        thread_id: &str,
        tool_call_ids: Vec<String>,
    ) -> Result<(), Self::Error>;

    async fn get_metadata(
        &self,
        root_thread_id: &str,
    ) -> Result<Option<serde_json::Value>, Self::Error>;

    async fn set_metadata(
        &self,
        root_thread_id: &str,
        metadata: serde_json::Value,
    ) -> Result<(), Self::Error>;

    /// Return the list of active subscription tool_call_ids for a specific thread.
    async fn get_active_subscriptions(&self, thread_id: &str) -> Result<Vec<String>, Self::Error>;

    /// Record a new active subscription (tool_call_id) for a specific thread.
    async fn add_active_subscription(
        &self,
        thread_id: &str,
        tool_call_id: &str,
    ) -> Result<(), Self::Error>;

    /// Remove an active subscription (tool_call_id) from a specific thread.
    async fn remove_active_subscription(
        &self,
        thread_id: &str,
        tool_call_id: &str,
    ) -> Result<(), Self::Error>;
}

/// Abstraction over input message delivery (SQS in Lambda, channel/direct in CLI).
#[async_trait]
pub trait InputSender: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Send a message to the input queue for processing.
    async fn send_to_input_queue(
        &self,
        message: InputMessage,
        group_id: &str,
        dedup_id: &str,
    ) -> Result<(), Self::Error>;
}

/// Abstraction over HTTP client for tool invocation.
#[async_trait]
pub trait HttpClient: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn post(&self, url: &str, body: &str) -> Result<u16, Self::Error>;
    async fn get(&self, url: &str) -> Result<(u16, Vec<u8>), Self::Error>;
}

/// Abstraction over toolset manifest caching.
#[async_trait]
pub trait ToolsetCache: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn get_cached(&self, cache_key: &str) -> Result<Option<String>, Self::Error>;

    async fn put_cache(&self, cache_key: &str, json: &str) -> Result<(), Self::Error>;
}
