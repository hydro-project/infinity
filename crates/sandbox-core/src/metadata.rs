use async_trait::async_trait;

use crate::error::SandboxError;
use crate::types::RepoState;

/// Trait for storing and retrieving repo metadata, keyed by group_id.
/// In-memory for local, DynamoDB for remote.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn get(&self, group_id: &str) -> Result<Option<RepoState>, SandboxError>;
    async fn put(&self, state: &RepoState) -> Result<(), SandboxError>;
    async fn delete(&self, group_id: &str) -> Result<(), SandboxError>;
    async fn list_all(&self) -> Result<Vec<RepoState>, SandboxError> {
        Ok(Vec::new())
    }
}
