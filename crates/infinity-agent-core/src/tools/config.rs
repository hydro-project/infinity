use serde::{Deserialize, Serialize};

/// Configuration for a single toolset server entry.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSetConfig {
    /// RAP toolset server — tools are loaded via `.well-known/rap-toolset`.
    ToolsetServer { server_url: String },
}

impl ToolSetConfig {
    pub fn server_url(&self) -> &str {
        match self {
            ToolSetConfig::ToolsetServer { server_url } => server_url,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ToolsConfig {
    pub tool_sets: Vec<ToolSetConfig>,
}

impl ToolsConfig {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let config_str = std::env::var("TOOLS_CONFIG")?;
        let config = serde_json::from_str(&config_str)?;
        Ok(config)
    }

    pub fn from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Extract all toolset server URLs from the config.
    pub fn toolset_server_urls(&self) -> Vec<String> {
        self.tool_sets
            .iter()
            .map(|ts| ts.server_url().to_string())
            .collect()
    }
}
