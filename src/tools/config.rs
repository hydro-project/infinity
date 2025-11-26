use serde::{Deserialize, Serialize};

use super::{lambda_tool::LambdaTool, lambda_mcp::LambdaMCP, Tool, ToolSet, VecToolSet};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolConfig {
    Lambda {
        name: String,
        description: String,
        parameters: serde_json::Value,
        queue_url: String,
    },
}

impl ToolConfig {
    pub fn into_tool(self) -> Box<dyn Tool> {
        match self {
            ToolConfig::Lambda {
                name,
                description,
                parameters,
                queue_url,
            } => Box::new(LambdaTool {
                name,
                description,
                parameters,
                queue_url,
            }),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSetConfig {
    Vec { tools: Vec<ToolConfig> },
    Mcp { name: String, queue_url: String },
}

impl ToolSetConfig {
    pub fn into_tool_set(self) -> Box<dyn ToolSet> {
        match self {
            ToolSetConfig::Vec { tools } => {
                let tool_impls = tools.into_iter().map(|t| t.into_tool()).collect();
                Box::new(VecToolSet::new(tool_impls))
            }
            ToolSetConfig::Mcp { name, queue_url } => Box::new(LambdaMCP::new(name, queue_url)),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ToolsConfig {
    pub tool_sets: Vec<ToolSetConfig>,
}

impl ToolsConfig {
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

    pub fn into_tool_sets(self) -> Vec<Box<dyn ToolSet>> {
        self.tool_sets
            .into_iter()
            .map(|ts| ts.into_tool_set())
            .collect()
    }
}
