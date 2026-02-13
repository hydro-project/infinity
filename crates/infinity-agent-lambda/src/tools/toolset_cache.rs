use async_trait::async_trait;
use aws_sdk_dynamodb::{Client as DynamoDbClient, types::AttributeValue};
use infinity_agent_core::traits::ToolsetCache;

pub struct DynamoDbToolsetCache {
    client: DynamoDbClient,
    table_name: String,
}

#[derive(Debug)]
pub struct CacheError(String);
impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for CacheError {}

impl DynamoDbToolsetCache {
    pub fn new(client: DynamoDbClient, table_name: String) -> Self {
        Self { client, table_name }
    }
}

#[async_trait]
impl ToolsetCache for DynamoDbToolsetCache {
    type Error = CacheError;

    async fn get_cached(&self, cache_key: &str) -> Result<Option<String>, CacheError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(cache_key.to_string()))
            .send()
            .await
            .map_err(|e| CacheError(format!("Cache get failed: {}", e)))?;

        Ok(result.item().and_then(|item| {
            item.get("toolset_json")
                .and_then(|v| v.as_s().ok())
                .map(|s| s.to_string())
        }))
    }

    async fn put_cache(&self, cache_key: &str, json: &str) -> Result<(), CacheError> {
        self.client
            .put_item()
            .table_name(&self.table_name)
            .item("session", AttributeValue::S(cache_key.to_string()))
            .item("toolset_json", AttributeValue::S(json.to_string()))
            .send()
            .await
            .map_err(|e| CacheError(format!("Cache put failed: {}", e)))?;
        Ok(())
    }
}
