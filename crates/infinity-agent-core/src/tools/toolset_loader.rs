use serde::{Deserialize, Serialize};
use tracing;

use crate::traits::{HttpClient, ToolsetCache};

/// A single tool definition within a RAP toolset manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RapToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub annotations: Option<serde_json::Value>,
}

/// A RAP toolset manifest as returned by `/.well-known/rap-toolset`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RapToolset {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub endpoint: String,
    pub tools: Vec<RapToolDef>,
}

/// A loaded toolset ready to be converted into Tool trait objects.
pub struct LoadedToolset {
    pub manifest: RapToolset,
}

impl LoadedToolset {
    pub fn from_manifest(manifest: RapToolset) -> Self {
        Self { manifest }
    }
}

/// Loads RAP toolsets from `.well-known/rap-toolset` endpoints,
/// caches them for the duration of the agent session,
/// and converts them into Tool trait objects.
pub struct ToolsetLoader<H: HttpClient, C: ToolsetCache> {
    http_client: H,
    cache: C,
}

impl<H: HttpClient, C: ToolsetCache> ToolsetLoader<H, C> {
    pub fn new(http_client: H, cache: C) -> Self {
        Self { http_client, cache }
    }

    /// Load toolsets for the given server base URLs, scoped to a session.
    pub async fn load_toolsets(
        &self,
        server_urls: &[String],
        session_id: &str,
    ) -> Result<Vec<LoadedToolset>, Box<dyn std::error::Error + Send + Sync>> {
        let mut results = Vec::new();
        for url in server_urls {
            let toolset = self.load_single(url, session_id).await?;
            results.push(toolset);
        }
        Ok(results)
    }

    async fn load_single(
        &self,
        server_url: &str,
        session_id: &str,
    ) -> Result<LoadedToolset, Box<dyn std::error::Error + Send + Sync>> {
        let cache_key = format!("__toolset__{}_{}", session_id, server_url);

        if let Some(cached_json) = self
            .cache
            .get_cached(&cache_key)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
        {
            let cached: RapToolset = serde_json::from_str(&cached_json)?;
            tracing::info!(
                "Using cached toolset '{}' for session {}",
                cached.name,
                session_id
            );
            return Ok(LoadedToolset::from_manifest(cached));
        }

        let toolset = self.fetch_from_well_known(server_url).await?;

        let json = serde_json::to_string(&toolset)?;
        let _ = self.cache.put_cache(&cache_key, &json).await;

        tracing::info!(
            "Fetched toolset '{}' with {} tools",
            toolset.name,
            toolset.tools.len()
        );
        Ok(LoadedToolset::from_manifest(toolset))
    }

    async fn fetch_from_well_known(
        &self,
        server_url: &str,
    ) -> Result<RapToolset, Box<dyn std::error::Error + Send + Sync>> {
        let well_known_url = format!(
            "{}/.well-known/rap-toolset",
            server_url.trim_end_matches('/')
        );
        tracing::info!("Fetching toolset from {}", well_known_url);

        let (status, body) = self
            .http_client
            .get(&well_known_url)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if status < 200 || status >= 300 {
            let body_str = String::from_utf8_lossy(&body);
            return Err(format!(
                "Toolset fetch from {} returned status {}: {}",
                well_known_url, status, body_str
            )
            .into());
        }

        let toolset: RapToolset = serde_json::from_slice(&body)?;
        Ok(toolset)
    }
}
