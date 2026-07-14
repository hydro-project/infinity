//! RAP tool support for the CLI: loads tools from RAP servers using rap-client.

use std::collections::HashMap;

use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::rap_tool::RapTool;
use infinity_agent_core::traits::InputSender;
use rap_client::http::InMemoryToolsetCache;
pub use rap_client::http::SimpleHttpClient;
use rap_client::toolset_loader::ToolsetLoader;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ── Load RAP tools from configured servers ──

pub struct LoadedRapTools<M: InputSender + 'static> {
    pub tools: Vec<Box<dyn Tool<M>>>,
    /// Servers that declared needsMigration: true, as (config_id, url) pairs.
    pub migration_servers: Vec<(String, String)>,
    /// Maps each loaded RAP tool name → the base URL of the server that
    /// provides it. Used to route protocol messages (e.g. `/tool_call_status`
    /// queries) to the server that originally received an invocation.
    pub tool_servers: HashMap<String, String>,
}

pub async fn load_rap_tools<M: InputSender + 'static>(
    servers: &[(String, Option<String>)],
) -> Result<LoadedRapTools<M>, BoxError> {
    let http_client = SimpleHttpClient::new();
    let cache = InMemoryToolsetCache::new();
    let loader = ToolsetLoader::new(http_client.clone(), cache);

    let server_urls: Vec<String> = servers.iter().map(|(url, _)| url.clone()).collect();
    let loaded = loader.load_toolsets(&server_urls, "cli-session").await?;

    let mut tools: Vec<Box<dyn Tool<M>>> = Vec::new();
    let mut migration_servers = Vec::new();
    let mut tool_servers = HashMap::new();
    for ts in loaded {
        let endpoint = ts.manifest.endpoint.clone();
        // Resolve the configured base URL for this toolset's server. Falls
        // back to the endpoint with any trailing `/invoke` path stripped.
        let base_url = servers
            .iter()
            .find(|(u, _)| endpoint.starts_with(u.as_str()))
            .map(|(u, _)| u.clone())
            .unwrap_or_else(|| {
                endpoint
                    .trim_end_matches('/')
                    .trim_end_matches("/invoke")
                    .to_owned()
            });
        if ts.manifest.needs_migration {
            // Find the (url, id) entry for this toolset
            if let Some((url, Some(id))) = servers
                .iter()
                .find(|(u, _)| endpoint.starts_with(u.as_str()))
            {
                migration_servers.push((id.clone(), url.clone()));
            }
        }
        for def in ts.manifest.tools {
            tracing::info!("Loaded RAP tool: {} from {}", def.name, endpoint);
            tool_servers.insert(def.name.clone(), base_url.clone());
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
    Ok(LoadedRapTools {
        tools,
        migration_servers,
        tool_servers,
    })
}
