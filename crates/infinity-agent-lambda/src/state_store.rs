use async_trait::async_trait;
use aws_sdk_dynamodb::{Client as DynamoDbClient, types::AttributeValue};
use infinity_agent_core::traits::StateStore;
use std::collections::HashSet;

#[derive(Clone)]
pub struct DynamoDbStateStore {
    client: DynamoDbClient,
    table_name: String,
}

#[derive(Debug)]
pub struct DynamoError(String);
impl std::fmt::Display for DynamoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for DynamoError {}

impl DynamoDbStateStore {
    pub fn new(client: DynamoDbClient, table_name: String) -> Self {
        Self { client, table_name }
    }
}

#[async_trait]
impl StateStore for DynamoDbStateStore {
    type Error = DynamoError;

    async fn get_processed_ids(
        &self,
        thread_id: &str,
    ) -> Result<(HashSet<String>, HashSet<String>), DynamoError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.to_string()))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to get processed IDs: {}", e)))?;

        if let Some(item) = result.item {
            let processed_ids =
                if let Some(AttributeValue::Ss(ids)) = item.get("processed_message_ids") {
                    ids.iter().cloned().collect()
                } else {
                    HashSet::new()
                };
            let processed_tools =
                if let Some(AttributeValue::Ss(ids)) = item.get("processed_tool_calls") {
                    ids.iter().cloned().collect()
                } else {
                    HashSet::new()
                };
            Ok((processed_ids, processed_tools))
        } else {
            Ok((HashSet::new(), HashSet::new()))
        }
    }

    async fn add_processed_message_ids(
        &self,
        thread_id: &str,
        message_ids: Vec<String>,
    ) -> Result<(), DynamoError> {
        if message_ids.is_empty() {
            return Ok(());
        }
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.to_string()))
            .update_expression("ADD processed_message_ids :ids")
            .expression_attribute_values(":ids", AttributeValue::Ss(message_ids))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to add processed message IDs: {}", e)))?;
        Ok(())
    }

    async fn add_processed_tool_calls(
        &self,
        thread_id: &str,
        tool_call_ids: Vec<String>,
    ) -> Result<(), DynamoError> {
        if tool_call_ids.is_empty() {
            return Ok(());
        }
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.to_string()))
            .update_expression("ADD processed_tool_calls :ids")
            .expression_attribute_values(":ids", AttributeValue::Ss(tool_call_ids))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to add processed tool calls: {}", e)))?;
        Ok(())
    }

    async fn get_metadata(
        &self,
        root_thread_id: &str,
    ) -> Result<Option<serde_json::Value>, DynamoError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(root_thread_id.to_string()))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to get metadata: {}", e)))?;

        Ok(result.item.and_then(|item| {
            item.get("metadata").and_then(|v| {
                if let AttributeValue::S(s) = v {
                    serde_json::from_str(s).ok()
                } else {
                    None
                }
            })
        }))
    }

    async fn set_metadata(
        &self,
        root_thread_id: &str,
        metadata: serde_json::Value,
    ) -> Result<(), DynamoError> {
        let json = serde_json::to_string(&metadata)
            .map_err(|e| DynamoError(format!("Failed to serialize metadata: {}", e)))?;
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(root_thread_id.to_string()))
            .update_expression("SET metadata = :metadata")
            .expression_attribute_values(":metadata", AttributeValue::S(json))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to set metadata: {}", e)))?;
        Ok(())
    }
}
