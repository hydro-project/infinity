use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for a single toolset server entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSetConfig {
    /// RAP toolset server — tools are loaded via `.well-known/rap-toolset`.
    ToolsetServer {
        server_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// RAP toolset server launched via a CLI command.
    /// The command is spawned with `RAP_EMBEDDED=1` and must emit a JSON
    /// object on stdout containing `{ "port": <u16> }` once it is ready.
    ToolsetCommand {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        crate_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// MCP server proxied behind RAP. The CLI spawns the command as a stdio
    /// subprocess and runs an in-process RAP server that translates between
    /// the two protocols.
    McpServer {
        name: String,
        command: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Remote MCP server over HTTP, proxied behind RAP.
    HttpMcpServer {
        name: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}

impl ToolSetConfig {
    /// Optional identifier used for deduplication during config merging.
    pub fn id(&self) -> Option<&str> {
        match self {
            ToolSetConfig::ToolsetServer { id, .. }
            | ToolSetConfig::ToolsetCommand { id, .. }
            | ToolSetConfig::McpServer { id, .. }
            | ToolSetConfig::HttpMcpServer { id, .. } => id.as_deref(),
        }
    }
}

/// JSON object emitted on stdout by a command-based RAP server at startup.
#[derive(Debug, Deserialize)]
pub struct CommandServerReady {
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolsConfig {
    pub tool_sets: Vec<ToolSetConfig>,
}

impl ToolsConfig {
    pub fn empty() -> Self {
        Self {
            tool_sets: Vec::new(),
        }
    }

    pub fn merge(&mut self, other: ToolsConfig) {
        let existing_ids: std::collections::HashSet<String> = self
            .tool_sets
            .iter()
            .filter_map(|ts| ts.id().map(|s| s.to_owned()))
            .collect();
        for ts in other.tool_sets {
            if let Some(id) = ts.id()
                && existing_ids.contains(id)
            {
                continue;
            }
            self.tool_sets.push(ts);
        }
    }

    pub fn add_command(&mut self, command: String) {
        self.tool_sets.push(ToolSetConfig::ToolsetCommand {
            command,
            id: None,
            crate_name: None,
            git: None,
            path: None,
        });
    }

    pub fn add_installed_command(
        &mut self,
        command: String,
        crate_name: String,
        git: Option<String>,
        path: Option<String>,
    ) {
        self.tool_sets.push(ToolSetConfig::ToolsetCommand {
            id: Some(crate_name.clone()),
            command,
            crate_name: Some(crate_name),
            git,
            path,
        });
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let config_str = std::env::var("TOOLS_CONFIG")?;
        let config = serde_json::from_str(&config_str)?;
        Ok(config)
    }

    pub fn from_file(
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = std::fs::read_to_string(path)?;
        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Extract server URLs from entries that already have a URL: (url, id).
    pub fn toolset_server_urls(&self) -> Vec<(String, Option<String>)> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::ToolsetServer { server_url, id, .. } => {
                    Some((server_url.clone(), id.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Extract commands from entries that specify a command to launch: (command, id).
    pub fn toolset_commands(&self) -> Vec<(String, Option<String>)> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::ToolsetCommand { command, id, .. } => {
                    Some((command.clone(), id.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Extract MCP server configs: (name, command, env, id).
    #[expect(clippy::type_complexity, reason = "internal // TODO")]
    pub fn mcp_servers(
        &self,
    ) -> Vec<(String, Vec<String>, HashMap<String, String>, Option<String>)> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::McpServer {
                    name,
                    command,
                    env,
                    id,
                } => Some((name.clone(), command.clone(), env.clone(), id.clone())),
                _ => None,
            })
            .collect()
    }

    /// Extract HTTP MCP server configs: (name, url, headers, id).
    #[expect(clippy::type_complexity, reason = "internal // TODO")]
    pub fn http_mcp_servers(
        &self,
    ) -> Vec<(String, String, HashMap<String, String>, Option<String>)> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::HttpMcpServer {
                    name,
                    url,
                    headers,
                    id,
                } => Some((name.clone(), url.clone(), headers.clone(), id.clone())),
                _ => None,
            })
            .collect()
    }

    /// Return toolset commands that have installation source info (for `rap update`).
    pub fn installable_commands(&self) -> Vec<(String, String, Option<String>, Option<String>)> {
        self.tool_sets
            .iter()
            .filter_map(|ts| match ts {
                ToolSetConfig::ToolsetCommand {
                    command,
                    crate_name: Some(cn),
                    git,
                    path,
                    ..
                } => Some((command.clone(), cn.clone(), git.clone(), path.clone())),
                _ => None,
            })
            .collect()
    }
}
