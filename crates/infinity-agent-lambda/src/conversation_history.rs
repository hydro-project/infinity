use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_dsql::{
    Client as DsqlClient,
    auth_token::{AuthTokenGenerator, Config},
};
use aws_types::region::Region;
use infinity_agent_core::message::InfinityMessage;
use infinity_agent_core::traits::ConversationStore;
use lambda_runtime::Error;
use rig::message::Message;
use sqlx::{Pool, Postgres, Row, postgres::PgConnectOptions};

#[derive(Clone)]
pub struct DsqlConversationStore {
    pool: Pool<Postgres>,
}

#[derive(Debug)]
pub struct DsqlError(String);
impl std::fmt::Display for DsqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for DsqlError {}

impl From<String> for DsqlError {
    fn from(s: String) -> Self {
        DsqlError(s)
    }
}

impl DsqlConversationStore {
    pub async fn new(_dsql_client: &DsqlClient, cluster_endpoint: &str) -> Result<Self, Error> {
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_owned());

        tracing::info!(
            "Generating DSQL auth token for cluster: {} in region: {}",
            cluster_endpoint,
            region
        );

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

        let connection_options = PgConnectOptions::new()
            .host(cluster_endpoint)
            .port(5432)
            .database("postgres")
            .username("admin")
            .password(password_token.as_str())
            .ssl_mode(sqlx::postgres::PgSslMode::Require);

        tracing::info!("Connecting to DSQL cluster at: {}", cluster_endpoint);

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect_with(connection_options)
            .await
            .map_err(|e| {
                tracing::error!("DSQL connection failed: {}", e);
                Error::from(format!(
                    "Failed to connect to DSQL cluster '{}': {}.",
                    cluster_endpoint, e
                ))
            })?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS conversation_history (
                session_id VARCHAR(255) NOT NULL,
                message_order BIGINT NOT NULL,
                message_id VARCHAR(255) NOT NULL,
                message_data TEXT NOT NULL,
                created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
                PRIMARY KEY (session_id, message_order)
            )"#,
        )
        .execute(&pool)
        .await
        .map_err(|e| Error::from(format!("Failed to create table: {}", e)))?;

        sqlx::query(
            r#"CREATE INDEX ASYNC IF NOT EXISTS idx_conversation_history_session_order
            ON conversation_history (session_id, message_order)"#,
        )
        .execute(&pool)
        .await
        .map_err(|e| Error::from(format!("Failed to create index: {}", e)))?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS thread_hierarchy (
                thread_id VARCHAR(255) PRIMARY KEY,
                parent_thread_id VARCHAR(255),
                root_thread_id VARCHAR(255) NOT NULL,
                spawn_message_order BIGINT,
                spawn_tool_call_id VARCHAR(255),
                closed BOOLEAN NOT NULL DEFAULT FALSE,
                is_subscription_event BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
            )"#,
        )
        .execute(&pool)
        .await
        .map_err(|e| Error::from(format!("Failed to create thread_hierarchy table: {}", e)))?;

        sqlx::query(
            r#"ALTER TABLE thread_hierarchy ADD COLUMN IF NOT EXISTS is_compaction BOOLEAN NOT NULL DEFAULT FALSE"#,
        )
        .execute(&pool)
        .await
        .map_err(|e| Error::from(format!("Failed to add is_compaction column: {}", e)))?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS compaction_summaries (
                thread_id VARCHAR(255) NOT NULL,
                up_to_order BIGINT NOT NULL,
                summary TEXT NOT NULL,
                created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
                PRIMARY KEY (thread_id, up_to_order)
            )"#,
        )
        .execute(&pool)
        .await
        .map_err(|e| {
            Error::from(format!(
                "Failed to create compaction_summaries table: {}",
                e
            ))
        })?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl ConversationStore for DsqlConversationStore {
    type Error = DsqlError;

    async fn ensure_root_thread(&self, thread_id: &str) -> Result<(), DsqlError> {
        sqlx::query(
            r#"INSERT INTO thread_hierarchy (thread_id, parent_thread_id, root_thread_id)
            VALUES ($1, NULL, $1)
            ON CONFLICT (thread_id) DO NOTHING"#,
        )
        .bind(thread_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to ensure root thread: {}", e)))?;
        Ok(())
    }

    async fn load_history_up_to(
        &self,
        session_id: &str,
        start_from: Option<i64>,
        up_to: Option<i64>,
    ) -> Result<Vec<InfinityMessage>, DsqlError> {
        let mut query =
            String::from("SELECT message_data FROM conversation_history WHERE session_id = $1");
        let mut bind_idx = 2;

        if start_from.is_some() {
            query.push_str(&format!(" AND message_order > ${}", bind_idx));
            bind_idx += 1;
        }
        if up_to.is_some() {
            query.push_str(&format!(" AND message_order <= ${}", bind_idx));
        }
        query.push_str(" ORDER BY message_order ASC");

        let mut q = sqlx::query(&query).bind(session_id);
        if let Some(n) = start_from {
            q = q.bind(n);
        }
        if let Some(n) = up_to {
            q = q.bind(n);
        }

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DsqlError(format!("Failed to load history: {}", e)))?;

        let mut messages = Vec::new();
        for row in rows {
            let json_str: String = row.get("message_data");
            // Try new InfinityMessage format first, fall back to old rig Message
            if let Ok(msg) = serde_json::from_str::<InfinityMessage>(&json_str) {
                messages.push(msg);
            } else if let Ok(msg) = serde_json::from_str::<Message>(&json_str) {
                messages.push(InfinityMessage::from_rig_message(msg));
            }
        }
        Ok(messages)
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: Vec<(InfinityMessage, String)>,
    ) -> Result<(), DsqlError> {
        if messages.is_empty() {
            return Ok(());
        }

        let max_order: Option<i64> = sqlx::query_scalar(
            r#"SELECT COALESCE(MAX(message_order), 0)
            FROM conversation_history WHERE session_id = $1"#,
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to get max order: {}", e)))?;

        let mut current_order = max_order.unwrap_or(0);
        let mut session_ids = Vec::new();
        let mut message_orders = Vec::new();
        let mut message_ids_vec = Vec::new();
        let mut message_data = Vec::new();

        for (message, message_id) in messages {
            current_order += 1;
            let json_str = serde_json::to_string(&message)
                .map_err(|e| DsqlError(format!("Failed to serialize message: {}", e)))?;
            session_ids.push(session_id.to_owned());
            message_orders.push(current_order);
            message_ids_vec.push(message_id);
            message_data.push(json_str);
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DsqlError(format!("Failed to begin transaction: {}", e)))?;

        sqlx::query(
            r#"INSERT INTO conversation_history (session_id, message_order, message_id, message_data)
            SELECT * FROM UNNEST($1::text[], $2::bigint[], $3::text[], $4::text[])
            ON CONFLICT (session_id, message_order) DO NOTHING"#,
        )
        .bind(&session_ids)
        .bind(&message_orders)
        .bind(&message_ids_vec)
        .bind(&message_data)
        .execute(&mut *tx)
        .await
        .map_err(|e| DsqlError(format!("Failed to batch insert messages: {}", e)))?;

        tx.commit()
            .await
            .map_err(|e| DsqlError(format!("Failed to commit transaction: {}", e)))?;
        Ok(())
    }

    async fn spawn_thread(
        &self,
        parent_thread_id: &str,
        spawn_tool_call_id: &str,
        is_for_subscription_event: bool,
    ) -> Result<String, DsqlError> {
        let new_thread_id = uuid::Uuid::new_v4().to_string();

        let spawn_message_order: Option<i64> = sqlx::query_scalar(
            r#"SELECT COALESCE(MAX(message_order), 0)
            FROM conversation_history WHERE session_id = $1"#,
        )
        .bind(parent_thread_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to get current message order: {}", e)))?;
        let spawn_message_order = spawn_message_order.unwrap_or(0);

        let root_thread_id: String = sqlx::query_scalar(
            r#"SELECT root_thread_id FROM thread_hierarchy WHERE thread_id = $1"#,
        )
        .bind(parent_thread_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to find parent thread: {}", e)))?;

        sqlx::query(
            r#"INSERT INTO thread_hierarchy (thread_id, parent_thread_id, root_thread_id, spawn_message_order, spawn_tool_call_id, is_subscription_event)
            VALUES ($1, $2, $3, $4, $5, $6)"#,
        )
        .bind(&new_thread_id)
        .bind(parent_thread_id)
        .bind(&root_thread_id)
        .bind(spawn_message_order)
        .bind(spawn_tool_call_id)
        .bind(is_for_subscription_event)
        .execute(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to spawn thread: {}", e)))?;

        Ok(new_thread_id)
    }

    async fn is_thread_closed(&self, thread_id: &str) -> Result<bool, DsqlError> {
        let row: Option<(bool,)> =
            sqlx::query_as(r#"SELECT closed FROM thread_hierarchy WHERE thread_id = $1"#)
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DsqlError(format!("Failed to check thread closed: {}", e)))?;
        Ok(row.map(|(c,)| c).unwrap_or(false))
    }

    async fn close_thread(&self, thread_id: &str) -> Result<(), DsqlError> {
        sqlx::query(r#"UPDATE thread_hierarchy SET closed = TRUE WHERE thread_id = $1"#)
            .bind(thread_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DsqlError(format!("Failed to close thread: {}", e)))?;
        Ok(())
    }

    async fn is_subscription_event_thread(&self, thread_id: &str) -> Result<bool, DsqlError> {
        let row: Option<(bool,)> = sqlx::query_as(
            r#"SELECT is_subscription_event FROM thread_hierarchy WHERE thread_id = $1"#,
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to check subscription event: {}", e)))?;
        Ok(row.map(|(v,)| v).unwrap_or(false))
    }

    async fn get_thread_parent_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>, DsqlError> {
        let row = sqlx::query(
            r#"SELECT parent_thread_id, spawn_tool_call_id FROM thread_hierarchy WHERE thread_id = $1"#,
        )
        .bind(thread_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to get thread info: {}", e)))?;

        let parent: Option<String> = row.get("parent_thread_id");
        let tool_call_id: Option<String> = row.get("spawn_tool_call_id");
        match (parent, tool_call_id) {
            (Some(p), Some(t)) => Ok(Some((p, t))),
            _ => Ok(None),
        }
    }

    async fn get_ancestor_chain(&self, thread_id: &str) -> Result<Vec<(String, i64)>, DsqlError> {
        let mut result: Vec<(String, i64)> = Vec::new();
        let mut current = thread_id.to_owned();

        loop {
            let row = sqlx::query(
                r#"SELECT parent_thread_id, spawn_message_order FROM thread_hierarchy WHERE thread_id = $1"#,
            )
            .bind(&current)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DsqlError(format!("Failed to get thread info: {}", e)))?;

            let parent: Option<String> = row.get("parent_thread_id");
            let spawn_order: Option<i64> = row.get("spawn_message_order");

            match parent {
                Some(p) => {
                    result.push((p.clone(), spawn_order.unwrap_or(0)));
                    current = p;
                }
                None => break,
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
    ) -> Result<(), DsqlError> {
        sqlx::query(
            r#"INSERT INTO compaction_summaries (thread_id, up_to_order, summary)
            VALUES ($1, $2, $3)
            ON CONFLICT (thread_id, up_to_order) DO UPDATE SET summary = $3"#,
        )
        .bind(thread_id)
        .bind(up_to_order)
        .bind(summary)
        .execute(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to save compaction summary: {}", e)))?;
        Ok(())
    }

    async fn load_latest_compaction_summary_up_to(
        &self,
        thread_id: &str,
        up_to_order: Option<i64>,
    ) -> Result<Option<(String, i64)>, DsqlError> {
        let row = match up_to_order {
            Some(n) => {
                sqlx::query(
                    r#"SELECT summary, up_to_order FROM compaction_summaries
                    WHERE thread_id = $1 AND up_to_order <= $2
                    ORDER BY up_to_order DESC LIMIT 1"#,
                )
                .bind(thread_id)
                .bind(n)
                .fetch_optional(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    r#"SELECT summary, up_to_order FROM compaction_summaries
                    WHERE thread_id = $1 ORDER BY up_to_order DESC LIMIT 1"#,
                )
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await
            }
        }
        .map_err(|e| DsqlError(format!("Failed to load compaction summary: {}", e)))?;

        Ok(row.map(|r| {
            let summary: String = r.get("summary");
            let up_to_order: i64 = r.get("up_to_order");
            (summary, up_to_order)
        }))
    }

    async fn is_compaction_thread(&self, thread_id: &str) -> Result<bool, DsqlError> {
        let row: Option<(bool,)> =
            sqlx::query_as(r#"SELECT is_compaction FROM thread_hierarchy WHERE thread_id = $1"#)
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DsqlError(format!("Failed to check compaction thread: {}", e)))?;
        Ok(row.map(|(v,)| v).unwrap_or(false))
    }

    async fn mark_thread_as_compaction(&self, thread_id: &str) -> Result<(), DsqlError> {
        sqlx::query(r#"UPDATE thread_hierarchy SET is_compaction = TRUE WHERE thread_id = $1"#)
            .bind(thread_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DsqlError(format!("Failed to mark compaction thread: {}", e)))?;
        Ok(())
    }

    async fn get_thread_spawn_order(&self, thread_id: &str) -> Result<Option<i64>, DsqlError> {
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            r#"SELECT spawn_message_order FROM thread_hierarchy WHERE thread_id = $1"#,
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DsqlError(format!("Failed to get spawn order: {}", e)))?;
        Ok(row.and_then(|(v,)| v))
    }
}
