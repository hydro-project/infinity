use async_trait::async_trait;
use aws_sdk_dynamodb::{Client as DynamoDbClient, types::AttributeValue};
use infinity_agent_core::ThreadId;
use infinity_agent_core::system::UserChoice;
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
        thread_id: &ThreadId<str>,
    ) -> Result<HashSet<String>, DynamoError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
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
            Ok(processed_ids)
        } else {
            Ok(HashSet::new())
        }
    }

    async fn add_processed_message_ids(
        &self,
        thread_id: &ThreadId<str>,
        message_ids: Vec<String>,
    ) -> Result<(), DynamoError> {
        if message_ids.is_empty() {
            return Ok(());
        }
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
            .update_expression("ADD processed_message_ids :ids")
            .expression_attribute_values(":ids", AttributeValue::Ss(message_ids))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to add processed message IDs: {}", e)))?;
        Ok(())
    }

    async fn get_metadata(
        &self,
        root_thread_id: &ThreadId<str>,
    ) -> Result<Option<serde_json::Value>, DynamoError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key(
                "session",
                AttributeValue::S(root_thread_id.as_str().to_owned()),
            )
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
        root_thread_id: &ThreadId<str>,
        metadata: serde_json::Value,
    ) -> Result<(), DynamoError> {
        let json = serde_json::to_string(&metadata)
            .map_err(|e| DynamoError(format!("Failed to serialize metadata: {}", e)))?;
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key(
                "session",
                AttributeValue::S(root_thread_id.as_str().to_owned()),
            )
            .update_expression("SET metadata = :metadata")
            .expression_attribute_values(":metadata", AttributeValue::S(json))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to set metadata: {}", e)))?;
        Ok(())
    }

    async fn get_active_subscriptions(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<rap_protocol::ToolCallId>, DynamoError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to get active subscriptions: {}", e)))?;

        Ok(result
            .item
            .and_then(|item| {
                if let Some(AttributeValue::Ss(ids)) = item.get("active_subscriptions") {
                    Some(
                        ids.iter()
                            .cloned()
                            .map(rap_protocol::ToolCallId::from)
                            .collect(),
                    )
                } else {
                    None
                }
            })
            .unwrap_or_default())
    }

    async fn add_active_subscription(
        &self,
        thread_id: &ThreadId<str>,
        tool_call_id: &rap_protocol::ToolCallId<str>,
    ) -> Result<(), DynamoError> {
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
            .update_expression("ADD active_subscriptions :id")
            .expression_attribute_values(
                ":id",
                AttributeValue::Ss(vec![tool_call_id.as_str().to_owned()]),
            )
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to add active subscription: {}", e)))?;
        Ok(())
    }

    async fn add_pending_user_choice(
        &self,
        thread_id: &ThreadId<str>,
        choice: UserChoice,
    ) -> Result<(), DynamoError> {
        let json = serde_json::to_string(&choice)
            .map_err(|e| DynamoError(format!("Failed to serialize pending user choice: {e}")))?;
        let existing = self
            .get_pending_user_choices(thread_id)
            .await?
            .into_iter()
            .find(|existing| existing.id == choice.id)
            .map(|existing| serde_json::to_string(&existing))
            .transpose()
            .map_err(|e| DynamoError(format!("Failed to serialize pending user choice: {e}")))?;
        if let Some(existing) = existing {
            self.client
                .update_item()
                .table_name(&self.table_name)
                .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
                .update_expression("DELETE pending_user_choices :old")
                .expression_attribute_values(":old", AttributeValue::Ss(vec![existing]))
                .send()
                .await
                .map_err(|e| DynamoError(format!("Failed to replace pending user choice: {e}")))?;
        }
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
            .update_expression("ADD pending_user_choices :new")
            .expression_attribute_values(":new", AttributeValue::Ss(vec![json]))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to add pending user choice: {e}")))?;
        Ok(())
    }

    async fn remove_pending_user_choice(
        &self,
        thread_id: &ThreadId<str>,
        choice_id: &rap_protocol::ChoiceId<str>,
    ) -> Result<(), DynamoError> {
        let Some(choice) = self
            .get_pending_user_choices(thread_id)
            .await?
            .into_iter()
            .find(|choice| choice.id == *choice_id)
        else {
            return Ok(());
        };
        let json = serde_json::to_string(&choice)
            .map_err(|e| DynamoError(format!("Failed to serialize pending user choice: {e}")))?;
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
            .update_expression("DELETE pending_user_choices :choice")
            .expression_attribute_values(":choice", AttributeValue::Ss(vec![json]))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to remove pending user choice: {e}")))?;
        Ok(())
    }

    async fn get_pending_user_choices(
        &self,
        thread_id: &ThreadId<str>,
    ) -> Result<Vec<UserChoice>, DynamoError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to get pending user choices: {e}")))?;
        let choices = result
            .item
            .as_ref()
            .and_then(|item| item.get("pending_user_choices"))
            .and_then(|value| match value {
                AttributeValue::Ss(values) => Some(values),
                _ => None,
            })
            .into_iter()
            .flatten()
            .map(|json| {
                serde_json::from_str(json).map_err(|e| {
                    DynamoError(format!("Failed to deserialize pending user choice: {e}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(choices)
    }

    async fn remove_active_subscription(
        &self,
        thread_id: &ThreadId<str>,
        tool_call_id: &rap_protocol::ToolCallId<str>,
    ) -> Result<(), DynamoError> {
        self.client
            .update_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(thread_id.as_str().to_owned()))
            .update_expression("DELETE active_subscriptions :id")
            .expression_attribute_values(
                ":id",
                AttributeValue::Ss(vec![tool_call_id.as_str().to_owned()]),
            )
            .send()
            .await
            .map_err(|e| DynamoError(format!("Failed to remove active subscription: {}", e)))?;
        Ok(())
    }
}
