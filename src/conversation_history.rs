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
