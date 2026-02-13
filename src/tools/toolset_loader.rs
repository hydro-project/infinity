use aws_sdk_dynamodb::Client as DynamoDbClient;
use aws_sdk_dynamodb::types::AttributeValue;
use lambda_runtime::{Error, tracing};
use serde::{Deserialize, Serialize};

use super::lambda_tool::LambdaTool;
use super::rap_http::RapHttpClient;
use super::{Tool, ToolSet, VecToolSet};

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

/// Loads RAP toolsets from `.well-known/rap-toolset` endpoints,
/// caches them in DynamoDB for the duration of the agent session,
/// and converts them into Tool trait objects.
pub struct ToolsetLoader {
    dynamodb_client: DynamoDbClient,
    table_name: String,
    http_client: RapHttpClient,
}

impl ToolsetLoader {
    pub fn new(
        dynamodb_client: DynamoDbClient,
        table_name: String,
        http_client: RapHttpClient,
    ) -> Self {
        Self {
            dynamodb_client,
            table_name,
            http_client,
        }
    }

    /// Load toolsets for the given server base URLs, scoped to a session.
    /// Uses the DynamoDB cache for the lifetime of the session.
    pub async fn load_toolsets(
        &self,
        server_urls: &[String],
        session_id: &str,
    ) -> Result<Vec<LoadedToolset>, Error> {
        let mut results = Vec::new();
        for url in server_urls {
            let toolset = self.load_single(url, session_id).await?;
            results.push(toolset);
        }
        Ok(results)
    }

    /// Load a single toolset. Uses session-scoped DynamoDB cache if available.
    async fn load_single(
        &self,
        server_url: &str,
        session_id: &str,
    ) -> Result<LoadedToolset, Error> {
        let cache_key = format!("__toolset__{}_{}", session_id, server_url);
        if let Some(cached) = self.get_cached(&cache_key).await? {
            tracing::info!(
                "Using cached toolset '{}' for session {}",
                cached.name,
                session_id
            );
            return Ok(LoadedToolset::from_manifest(cached));
        }

        // Fetch from well-known endpoint
        let toolset = self.fetch_from_well_known(server_url).await?;

        // Cache it for this session
        self.put_cache(&cache_key, &toolset).await?;

        tracing::info!(
            "Fetched toolset '{}' with {} tools",
            toolset.name,
            toolset.tools.len()
        );
        Ok(LoadedToolset::from_manifest(toolset))
    }

    /// Fetch toolset from `{server_url}/.well-known/rap-toolset` using SigV4-signed GET.
    async fn fetch_from_well_known(&self, server_url: &str) -> Result<RapToolset, Error> {
        let well_known_url = format!(
            "{}/.well-known/rap-toolset",
            server_url.trim_end_matches('/')
        );
        tracing::info!("Fetching toolset from {}", well_known_url);

        let (status, body) = self.http_client.get_signed(&well_known_url).await?;

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body);
            return Err(Error::from(format!(
                "Toolset fetch from {} returned status {}: {}",
                well_known_url, status, body_str
            )));
        }

        let toolset: RapToolset = serde_json::from_slice(&body).map_err(|e| {
            Error::from(format!(
                "Failed to parse toolset from {}: {}",
                well_known_url, e
            ))
        })?;

        Ok(toolset)
    }

    /// Read cached toolset from DynamoDB.
    async fn get_cached(&self, cache_key: &str) -> Result<Option<RapToolset>, Error> {
        let result = self
            .dynamodb_client
            .get_item()
            .table_name(&self.table_name)
            .key("session", AttributeValue::S(cache_key.to_string()))
            .send()
            .await?;

        let item = match result.item() {
            Some(item) => item,
            None => return Ok(None),
        };

        let json_str = match item.get("toolset_json").and_then(|v| v.as_s().ok()) {
            Some(s) => s,
            None => return Ok(None),
        };

        match serde_json::from_str(json_str) {
            Ok(toolset) => Ok(Some(toolset)),
            Err(e) => {
                tracing::warn!("Failed to deserialize cached toolset: {}", e);
                Ok(None)
            }
        }
    }

    /// Write toolset to DynamoDB cache.
    async fn put_cache(&self, cache_key: &str, toolset: &RapToolset) -> Result<(), Error> {
        let json_str = serde_json::to_string(toolset)?;

        self.dynamodb_client
            .put_item()
            .table_name(&self.table_name)
            .item("session", AttributeValue::S(cache_key.to_string()))
            .item("toolset_json", AttributeValue::S(json_str))
            .send()
            .await?;

        Ok(())
    }
}

/// A loaded toolset ready to be converted into Tool trait objects.
pub struct LoadedToolset {
    pub manifest: RapToolset,
}

impl LoadedToolset {
    fn from_manifest(manifest: RapToolset) -> Self {
        Self { manifest }
    }

    /// Convert into a ToolSet of LambdaTools.
    pub fn into_tool_set(self) -> Box<dyn ToolSet> {
        let endpoint = self.manifest.endpoint.clone();
        let tools: Vec<Box<dyn Tool>> = self
            .manifest
            .tools
            .into_iter()
            .map(|def| -> Box<dyn Tool> {
                Box::new(LambdaTool {
                    name: def.name,
                    description: def.description,
                    parameters: def.input_schema,
                    function_url: endpoint.clone(),
                })
            })
            .collect();
        Box::new(VecToolSet::new(tools))
    }
}
