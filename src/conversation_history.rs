use aws_config::BehaviorVersion;
use aws_sdk_dsql::{
    Client as DsqlClient,
    auth_token::{AuthTokenGenerator, Config},
};
use aws_types::region::Region;
use lambda_runtime::{Error, tracing};
use rig::message::Message;
use serde_json;
use sqlx::{Pool, Postgres, Row, postgres::PgConnectOptions};

#[derive(Clone)]
pub struct ConversationHistoryStore {
    pool: Pool<Postgres>,
}

impl ConversationHistoryStore {
    pub async fn new(_dsql_client: &DsqlClient, cluster_endpoint: &str) -> Result<Self, Error> {
        // Get AWS region from environment or default to us-east-1
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());

        tracing::info!(
            "Generating DSQL auth token for cluster: {} in region: {}",
            cluster_endpoint,
            region
        );

        // Generate auth token using AuthTokenGenerator
        let sdk_config = aws_config::load_defaults(BehaviorVersion::latest()).await;
        let dsql_config = Config::builder()
            .hostname(cluster_endpoint)
            .region(Region::new(region))
            .build()
            .map_err(|e| Error::from(format!("Failed to build DSQL auth config: {}", e)))?;

        let signer = AuthTokenGenerator::new(dsql_config);

        let password_token = signer
            .db_connect_admin_auth_token(&sdk_config)
            .await
            .map_err(|e| Error::from(format!("Failed to generate DSQL auth token: {}", e)))?;

        // Create connection options with proper DSQL configuration
        let connection_options = PgConnectOptions::new()
            .host(cluster_endpoint)
            .port(5432)
            .database("postgres")
            .username("admin")
            .password(password_token.as_str())
            .ssl_mode(sqlx::postgres::PgSslMode::Require);

        tracing::info!("Connecting to DSQL cluster at: {}", cluster_endpoint);

        // Create connection pool
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect_with(connection_options)
            .await
            .map_err(|e| {
                tracing::error!("DSQL connection failed: {}", e);
                Error::from(format!(
                    "Failed to connect to DSQL cluster '{}': {}. \
                    Make sure the cluster is running and IAM permissions are configured correctly.",
                    cluster_endpoint, e
                ))
            })?;

        // Initialize the conversation_history table if it doesn't exist
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS conversation_history (
                session_id VARCHAR(255) NOT NULL,
                message_order BIGINT NOT NULL,
                message_id VARCHAR(255) NOT NULL,
                message_data TEXT NOT NULL,
                created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
                PRIMARY KEY (session_id, message_order)
            )
            "#,
        )
        .execute(&pool)
        .await
        .map_err(|e| Error::from(format!("Failed to create table: {}", e)))?;

        // Create index for efficient querying using DSQL's ASYNC syntax
        sqlx::query(
            r#"
            CREATE INDEX ASYNC IF NOT EXISTS idx_conversation_history_session_order 
            ON conversation_history (session_id, message_order)
            "#,
        )
        .execute(&pool)
        .await
        .map_err(|e| Error::from(format!("Failed to create index: {}", e)))?;

        // Thread hierarchy table: tracks parent of each thread
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS thread_hierarchy (
                thread_id VARCHAR(255) PRIMARY KEY,
                parent_thread_id VARCHAR(255),
                root_thread_id VARCHAR(255) NOT NULL,
                spawn_message_order BIGINT,
                spawn_tool_call_id VARCHAR(255),
                closed BOOLEAN NOT NULL DEFAULT FALSE,
                is_subscription_event BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
            )
            "#,
        )
        .execute(&pool)
        .await
        .map_err(|e| Error::from(format!("Failed to create thread_hierarchy table: {}", e)))?;

        Ok(Self { pool })
    }

    pub async fn load_history(&self, session_id: &str) -> Result<Vec<Message>, Error> {
        let rows = sqlx::query(
            r#"
            SELECT message_data 
            FROM conversation_history 
            WHERE session_id = $1 
            ORDER BY message_order ASC
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to load history: {}", e)))?;

        let mut messages = Vec::new();
        for row in rows {
            let message_json_str: String = row.get("message_data");
            if let Ok(message) = serde_json::from_str::<Message>(&message_json_str) {
                messages.push(message);
            }
        }

        Ok(messages)
    }

    /// Register a root thread (e.g. a Slack conversation). Idempotent.
    pub async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), Error> {
        sqlx::query(
            r#"
            INSERT INTO thread_hierarchy (thread_id, parent_thread_id, root_thread_id)
            VALUES ($1, NULL, $1)
            ON CONFLICT (thread_id) DO NOTHING
            "#,
        )
        .bind(thread_id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to ensure root thread: {}", e)))?;
        Ok(())
    }

    /// Spawn a child thread. `spawn_message_order` is the message_order in the parent
    /// thread at which the spawn_thread tool call was recorded (i.e. the assistant ToolCall message).
    /// The child will only see parent messages up to and including this index.
    /// `spawn_tool_call_id` is the tool call ID of the spawn_thread invocation in the parent,
    /// used for sending synthetic subscription events back to the parent.
    pub async fn spawn_thread(
        &self,
        parent_thread_id: &str,
        spawn_message_order: i64,
        spawn_tool_call_id: &str,
    ) -> Result<String, Error> {
        let new_thread_id = uuid::Uuid::new_v4().to_string();

        // Look up the root of the parent
        let root_thread_id: String = sqlx::query_scalar(
            r#"SELECT root_thread_id FROM thread_hierarchy WHERE thread_id = $1"#,
        )
        .bind(parent_thread_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to find parent thread: {}", e)))?;

        sqlx::query(
            r#"
            INSERT INTO thread_hierarchy (thread_id, parent_thread_id, root_thread_id, spawn_message_order, spawn_tool_call_id)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(&new_thread_id)
        .bind(parent_thread_id)
        .bind(&root_thread_id)
        .bind(spawn_message_order)
        .bind(spawn_tool_call_id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to spawn thread: {}", e)))?;

        Ok(new_thread_id)
    }

    /// Check if a thread has been closed.
    pub async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, Error> {
        let row: Option<(bool,)> =
            sqlx::query_as(r#"SELECT closed FROM thread_hierarchy WHERE thread_id = $1"#)
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::from(format!("Failed to check thread closed status: {}", e)))?;

        Ok(row.map(|(closed,)| closed).unwrap_or(false))
    }

    /// Close a thread.
    pub async fn close_thread(&self, thread_id: &str) -> Result<(), Error> {
        sqlx::query(r#"UPDATE thread_hierarchy SET closed = TRUE WHERE thread_id = $1"#)
            .bind(thread_id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::from(format!("Failed to close thread: {}", e)))?;
        Ok(())
    }

    /// Check if a thread was created to handle a subscription event.
    pub async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, Error> {
        let row: Option<(bool,)> = sqlx::query_as(
            r#"SELECT is_subscription_event FROM thread_hierarchy WHERE thread_id = $1"#,
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to check subscription event status: {}", e)))?;

        Ok(row.map(|(v,)| v).unwrap_or(false))
    }

    /// Mark a thread as having been created for a subscription event.
    pub async fn mark_as_subscription_event(&self, thread_id: &str) -> Result<(), Error> {
        sqlx::query(
            r#"UPDATE thread_hierarchy SET is_subscription_event = TRUE WHERE thread_id = $1"#,
        )
        .bind(thread_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            Error::from(format!(
                "Failed to mark thread as subscription event: {}",
                e
            ))
        })?;
        Ok(())
    }

    /// Look up a thread's parent and spawn tool call ID.
    pub async fn get_thread_parent_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>, Error> {
        let row = sqlx::query(
            r#"SELECT parent_thread_id, spawn_tool_call_id FROM thread_hierarchy WHERE thread_id = $1"#,
        )
        .bind(thread_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to get thread info: {}", e)))?;

        let parent: Option<String> = row.get("parent_thread_id");
        let tool_call_id: Option<String> = row.get("spawn_tool_call_id");

        match (parent, tool_call_id) {
            (Some(p), Some(t)) => Ok(Some((p, t))),
            _ => Ok(None),
        }
    }

    /// Get the ancestor chain with truncation info.
    /// Returns Vec<AncestorLink> from root to the given thread (leaf).
    /// Get the ancestor chain (excluding the leaf thread itself).
    /// Returns Vec<(thread_id, spawn_message_order)> from root to the leaf's direct parent.
    /// Each entry's spawn_message_order is the truncation point for that thread's history.
    pub async fn get_ancestor_chain(&self, thread_id: &str) -> Result<Vec<(String, i64)>, Error> {
        let mut result: Vec<(String, i64)> = Vec::new();
        let mut current = thread_id.to_string();

        loop {
            let row = sqlx::query(
                r#"SELECT parent_thread_id, spawn_message_order FROM thread_hierarchy WHERE thread_id = $1"#,
            )
            .bind(&current)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::from(format!("Failed to get thread info: {}", e)))?;

            let parent: Option<String> = row.get("parent_thread_id");
            let spawn_order: Option<i64> = row.get("spawn_message_order");

            match parent {
                Some(p) => {
                    // spawn_order is always present for non-root threads
                    result.push((p.clone(), spawn_order.unwrap_or(0)));
                    current = p;
                }
                None => break,
            }
        }

        result.reverse();
        Ok(result)
    }

    /// Load history for a thread, with truncated parent histories as prefix.
    /// The leaf thread's own history is appended untruncated at the end.
    /// No synthetic messages are injected — the spawn_thread tool naturally produces
    /// the right tool results in both parent and child threads.
    pub async fn load_history_with_ancestors(
        &self,
        thread_id: &str,
    ) -> Result<Vec<Message>, Error> {
        let ancestors = self.get_ancestor_chain(thread_id).await?;

        let mut combined = Vec::new();

        for (tid, cutoff) in &ancestors {
            combined.extend(self.load_history_up_to(tid, *cutoff).await?);
        }

        // Leaf thread — full history, no truncation
        combined.extend(self.load_history(thread_id).await?);

        Ok(combined)
    }

    /// Load history for a session up to (and including) the given message_order.
    pub async fn load_history_up_to(
        &self,
        session_id: &str,
        up_to_order: i64,
    ) -> Result<Vec<Message>, Error> {
        let rows = sqlx::query(
            r#"
            SELECT message_data 
            FROM conversation_history 
            WHERE session_id = $1 AND message_order <= $2
            ORDER BY message_order ASC
            "#,
        )
        .bind(session_id)
        .bind(up_to_order)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to load history up to order: {}", e)))?;

        let mut messages = Vec::new();
        for row in rows {
            let message_json_str: String = row.get("message_data");
            if let Ok(message) = serde_json::from_str::<Message>(&message_json_str) {
                messages.push(message);
            }
        }

        Ok(messages)
    }

    /// Get the current max message_order for a session.
    pub async fn get_current_message_order(&self, session_id: &str) -> Result<i64, Error> {
        let max_order: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT COALESCE(MAX(message_order), 0) 
            FROM conversation_history 
            WHERE session_id = $1
            "#,
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to get current message order: {}", e)))?;

        Ok(max_order.unwrap_or(0))
    }

    pub async fn append_messages(
        &self,
        session_id: &str,
        messages: Vec<(Message, String)>, // (message, message_id)
    ) -> Result<(), Error> {
        if messages.is_empty() {
            return Ok(());
        }

        // Get the current max order for this session
        let max_order: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT COALESCE(MAX(message_order), 0) 
            FROM conversation_history 
            WHERE session_id = $1
            "#,
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::from(format!("Failed to get max order: {}", e)))?;

        let mut current_order = max_order.unwrap_or(0);

        // Prepare batch insert data
        let mut session_ids = Vec::new();
        let mut message_orders = Vec::new();
        let mut message_ids = Vec::new();
        let mut message_data = Vec::new();

        for (message, message_id) in messages {
            current_order += 1;
            let message_json_str = serde_json::to_string(&message)
                .map_err(|e| Error::from(format!("Failed to serialize message: {}", e)))?;

            session_ids.push(session_id);
            message_orders.push(current_order);
            message_ids.push(message_id);
            message_data.push(message_json_str);
        }

        // Single batch insert in transaction
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::from(format!("Failed to begin transaction: {}", e)))?;

        sqlx::query(
            r#"
            INSERT INTO conversation_history (session_id, message_order, message_id, message_data)
            SELECT * FROM UNNEST($1::text[], $2::bigint[], $3::text[], $4::text[])
            ON CONFLICT (session_id, message_order) DO NOTHING
            "#,
        )
        .bind(&session_ids)
        .bind(&message_orders)
        .bind(&message_ids)
        .bind(&message_data)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::from(format!("Failed to batch insert messages: {}", e)))?;

        tx.commit()
            .await
            .map_err(|e| Error::from(format!("Failed to commit transaction: {}", e)))?;

        Ok(())
    }
}
