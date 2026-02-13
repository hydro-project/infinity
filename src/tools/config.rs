use aws_sdk_dynamodb::Client as DynamoDbClient;
use aws_sdk_ssm::Client as SsmClient;
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

    pub async fn from_ssm(
        ssm_client: &SsmClient,
        param_name: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let response = ssm_client.get_parameter().name(param_name).send().await?;

        let value = response
            .parameter()
            .and_then(|p| p.value())
            .ok_or("SSM parameter value not found")?;

        let config = serde_json::from_str(value)?;
        Ok(config)
    }

    pub async fn from_dynamodb(
        dynamodb_client: &DynamoDbClient,
        table_name: &str,
        key: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let result = dynamodb_client
            .get_item()
            .table_name(table_name)
            .key(
                "session",
                aws_sdk_dynamodb::types::AttributeValue::S(key.to_string()),
            )
            .send()
            .await?;

        let item = result
            .item()
            .ok_or("Tools config item not found in DynamoDB")?;
        let config_str = item
            .get("config")
            .and_then(|v| v.as_s().ok())
            .ok_or("Tools config 'config' attribute not found or not a string")?;

        let config = serde_json::from_str(config_str)?;
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
