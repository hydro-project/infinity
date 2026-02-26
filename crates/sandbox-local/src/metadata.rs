use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use sandbox_core::error::SandboxError;
use sandbox_core::metadata::MetadataStore;
use sandbox_core::types::RepoState;

/// In-memory metadata store for local mode.
#[derive(Default, Clone)]
pub struct InMemoryMetadataStore {
    data: Arc<RwLock<HashMap<String, RepoState>>>,
}

impl InMemoryMetadataStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MetadataStore for InMemoryMetadataStore {
    async fn get(&self, group_id: &str) -> Result<Option<RepoState>, SandboxError> {
        let data = self.data.read().await;
        Ok(data.get(group_id).cloned())
    }

    async fn put(&self, state: &RepoState) -> Result<(), SandboxError> {
        let mut data = self.data.write().await;
        data.insert(state.group_id.clone(), state.clone());
        Ok(())
    }
}
