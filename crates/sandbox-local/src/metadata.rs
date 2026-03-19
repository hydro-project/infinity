use std::path::PathBuf;

use async_trait::async_trait;

use sandbox_core::error::SandboxError;
use sandbox_core::metadata::MetadataStore;
use sandbox_core::types::RepoState;

/// File-based metadata store for local mode.
///
/// Persists each group's [`RepoState`] as `{base_dir}/{group_id}.json`.
#[derive(Clone)]
pub struct FileMetadataStore {
    base_dir: PathBuf,
}

impl FileMetadataStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn path_for(&self, group_id: &str) -> PathBuf {
        self.base_dir.join(format!("{group_id}.json"))
    }
}

#[async_trait]
impl MetadataStore for FileMetadataStore {
    async fn get(&self, group_id: &str) -> Result<Option<RepoState>, SandboxError> {
        let path = self.path_for(group_id);
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let state: RepoState = serde_json::from_str(&contents).map_err(|e| {
                    SandboxError::Other(format!("failed to parse {}: {e}", path.display()))
                })?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SandboxError::Io(e)),
        }
    }

    async fn put(&self, state: &RepoState) -> Result<(), SandboxError> {
        let path = self.path_for(&state.group_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(SandboxError::Io)?;
        }
        let json = serde_json::to_string_pretty(state)
            .map_err(|e| SandboxError::Other(format!("failed to serialize state: {e}")))?;
        std::fs::write(&path, json).map_err(SandboxError::Io)?;
        Ok(())
    }
}
