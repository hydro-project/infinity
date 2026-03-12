use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for a single toolset server entry.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSetConfig {
    /// RAP toolset server — tools are loaded via `.well-known/rap-toolset`.
    ToolsetServer { server_url: String },
    /// RAP toolset server launched via a CLI command.
    /// The command is spawned with `RAP_EMBEDDED=1` and must emit a JSON
    /// object on stdout containing `{ "port": <u16> }` once it is ready.
    ToolsetCommand { command: String },
    /// MCP server proxied behind RAP. The CLI spawns the command as a stdio
    /// subprocess and runs an in-process RAP server that translates between
    /// the two protocols.
    McpServer {
        name: String,
        command: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// Remote MCP server over HTTP, proxied behind RAP.
    HttpMcpServer {
        name: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// JSON object emitted on stdout by a command-based RAP server at startup.
#[derive(Debug, Deserialize)]
pub struct CommandServerReady {
    pub port: u16,
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

    /// Extract server URLs from entries that already have a URL.
    pub fn toolset_server_urls(&self) -> Vec<String> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::ToolsetServer { server_url } => Some(server_url.clone()),
                _ => None,
            })
            .collect()
    }

    /// Extract commands from entries that specify a command to launch.
    pub fn toolset_commands(&self) -> Vec<String> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::ToolsetCommand { command } => Some(command.clone()),
                _ => None,
            })
            .collect()
    }

    /// Extract MCP server configs: (name, command, env).
    pub fn mcp_servers(&self) -> Vec<(String, Vec<String>, HashMap<String, String>)> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::McpServer { name, command, env } => {
                    Some((name.clone(), command.clone(), env.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Extract HTTP MCP server configs: (name, url, headers).
    pub fn http_mcp_servers(&self) -> Vec<(String, String, HashMap<String, String>)> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::HttpMcpServer { name, url, headers } => {
                    Some((name.clone(), url.clone(), headers.clone()))
                }
                _ => None,
            })
            .collect()
    }
}
