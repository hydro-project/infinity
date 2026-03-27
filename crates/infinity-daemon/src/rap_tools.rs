//! RAP tool support for the CLI: loads tools from RAP servers using rap-client.

use infinity_agent_core::tools::Tool;
use infinity_agent_core::tools::rap_tool::RapTool;
use infinity_agent_core::traits::InputSender;
use rap_client::http::InMemoryToolsetCache;
pub use rap_client::http::SimpleHttpClient;
use rap_client::toolset_loader::ToolsetLoader;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
