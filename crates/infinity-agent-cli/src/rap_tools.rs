//! RAP tool support for the CLI: plain HTTP client, in-memory toolset cache,
//! and a loader that creates core `RapTool`s for local tool servers.

use async_trait::async_trait;
use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::rap_tool::RapTool;
use infinity_agent_core::tools::toolset_loader::ToolsetLoader;
use infinity_agent_core::traits::{HttpClient, InputSender, ToolsetCache};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ── Plain HTTP client (no SigV4) ──

#[derive(Debug)]
pub struct SimpleHttpError(String);
impl std::fmt::Display for SimpleHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SimpleHttpError {}

#[derive(Clone)]
pub struct SimpleHttpClient {
    client: reqwest::Client,
}

impl SimpleHttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl HttpClient for SimpleHttpClient {
    type Error = SimpleHttpError;

    async fn post(&self, url: &str, body: &str) -> Result<u16, SimpleHttpError> {
        let resp = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| SimpleHttpError(e.to_string()))?;
        Ok(resp.status().as_u16())
    }

    async fn get(&self, url: &str) -> Result<(u16, Vec<u8>), SimpleHttpError> {
        let resp = self
            .client
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| SimpleHttpError(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SimpleHttpError(e.to_string()))?;
        Ok((status, bytes.to_vec()))
    }
}

// ── In-memory toolset cache ──

#[derive(Debug)]
pub struct CacheError(String);
impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for CacheError {}

pub struct InMemoryToolsetCache {
    store: Arc<Mutex<HashMap<String, String>>>,
}

impl InMemoryToolsetCache {
    pub fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl ToolsetCache for InMemoryToolsetCache {
    type Error = CacheError;

    async fn get_cached(&self, key: &str) -> Result<Option<String>, CacheError> {
        Ok(self.store.lock().unwrap().get(key).cloned())
    }

    async fn put_cache(&self, key: &str, json: &str) -> Result<(), CacheError> {
        self.store
            .lock()
            .unwrap()
            .insert(key.to_string(), json.to_string());
        Ok(())
    }
}

// ── Load RAP tools from configured servers ──

pub async fn load_rap_tools<M: InputSender + 'static>(
    server_urls: &[String],
) -> Result<Vec<Box<dyn Tool<M>>>, BoxError> {
    let http_client = SimpleHttpClient::new();
    let cache = InMemoryToolsetCache::new();
    let loader = ToolsetLoader::new(http_client.clone(), cache);

    let loaded = loader.load_toolsets(server_urls, "cli-session").await?;

    let mut tools: Vec<Box<dyn Tool<M>>> = Vec::new();
    for ts in loaded {
        let endpoint = ts.manifest.endpoint.clone();
        for def in ts.manifest.tools {
            tracing::info!("Loaded RAP tool: {} from {}", def.name, endpoint);
            tools.push(Box::new(RapTool {
                name: def.name,
                description: def.description,
                parameters: def.input_schema,
                endpoint: endpoint.clone(),
                http_client: http_client.clone(),
                display_script: def.display_script,
            }));
        }
    }
    Ok(tools)
}
