use async_trait::async_trait;
use rig::message::Message;

/// Persistent conversation history storage (DSQL in Lambda, in-memory for CLI).
#[async_trait]
pub trait ConversationStore: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), Self::Error>;

    async fn load_history(&self, session_id: &str) -> Result<Vec<Message>, Self::Error>;

    async fn load_history_up_to(
        &self,
        session_id: &str,
        up_to_order: i64,
    ) -> Result<Vec<Message>, Self::Error>;

    async fn load_history_with_ancestors(
        &self,
        thread_id: &str,
    ) -> Result<Vec<Message>, Self::Error>;

    async fn append_messages(
        &self,
        session_id: &str,
        messages: Vec<(Message, String)>,
    ) -> Result<(), Self::Error>;

    async fn get_current_message_order(&self, session_id: &str) -> Result<i64, Self::Error>;

    async fn spawn_thread(
        &self,
        parent_thread_id: &str,
        spawn_message_order: i64,
        spawn_tool_call_id: &str,
    ) -> Result<String, Self::Error>;

    async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, Self::Error>;

    async fn close_thread(&self, thread_id: &str) -> Result<(), Self::Error>;

    async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, Self::Error>;

    async fn mark_as_subscription_event(&self, thread_id: &str) -> Result<(), Self::Error>;

    async fn get_thread_parent_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>, Self::Error>;

    async fn get_ancestor_chain(&self, thread_id: &str) -> Result<Vec<(String, i64)>, Self::Error>;
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
}

/// Abstraction over message delivery (SQS in Lambda, channel/direct in CLI).
#[async_trait]
pub trait MessageSender: Send + Sync + Clone {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Send a message to the input queue for processing.
    async fn send_to_input_queue(
        &self,
        body: &str,
        group_id: &str,
        dedup_id: &str,
    ) -> Result<(), Self::Error>;

    /// Send a message to the output queue (user-facing responses).
    async fn send_to_output(&self, body: &str) -> Result<(), Self::Error>;
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
