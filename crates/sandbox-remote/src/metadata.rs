use async_trait::async_trait;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;

use sandbox_core::error::SandboxError;
use sandbox_core::metadata::MetadataStore;
use sandbox_core::types::{RepoState, SandboxMode};

pub struct DynamoMetadataStore {
    client: Client,
    table_name: String,
}

impl DynamoMetadataStore {
    pub fn new(client: Client, table_name: String) -> Self {
        Self { client, table_name }
    }
}

#[async_trait]
impl MetadataStore for DynamoMetadataStore {
    async fn get(&self, group_id: &str) -> Result<Option<RepoState>, SandboxError> {
        let result = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("group_id", AttributeValue::S(group_id.to_string()))
            .send()
            .await
            .map_err(|e| SandboxError::MetadataError(format!("DynamoDB get failed: {e}")))?;

        let Some(item) = result.item else {
            return Ok(None);
        };

        let remote_uri = item
            .get("remote_uri")
            .and_then(|v| v.as_s().ok())
            .ok_or_else(|| SandboxError::MetadataError("missing remote_uri".into()))?
            .clone();

        let bookmark = item
            .get("bookmark")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .unwrap_or_else(|| format!("sandbox-{group_id}"));

        let base_revision = item
            .get("base_revision")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .unwrap_or_default();

        Ok(Some(RepoState {
            group_id: group_id.to_string(),
            remote_uri,
            bookmark,
            mode: SandboxMode::Jj { base_revision },
            sandbox_path: None,
            write_orig_granted: false,
            write_path_grants: Default::default(),
        }))
    }

    async fn put(&self, state: &RepoState) -> Result<(), SandboxError> {
        let mut req = self
            .client
            .put_item()
            .table_name(&self.table_name)
            .item("group_id", AttributeValue::S(state.group_id.clone()))
            .item("remote_uri", AttributeValue::S(state.remote_uri.clone()))
            .item("bookmark", AttributeValue::S(state.bookmark.clone()));

        if let SandboxMode::Jj { ref base_revision } = state.mode {
            req = req.item("base_revision", AttributeValue::S(base_revision.clone()));
        }

        req.send()
            .await
            .map_err(|e| SandboxError::MetadataError(format!("DynamoDB put failed: {e}")))?;

        Ok(())
    }
}
