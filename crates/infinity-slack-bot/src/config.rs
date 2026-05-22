use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Bot token (xoxb-...)
    pub bot_token: String,
    /// App-level token for Socket Mode (xapp-...)
    pub app_token: String,
    /// Default working directory for new sessions
    pub default_cwd: PathBuf,
    /// Allowed Slack user IDs
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path = infinity_protocol::state_dir().join("slack.json");
        let contents = std::fs::read_to_string(&path).map_err(|e| {
            format!("Failed to read {}: {e}\nCreate ~/.infinity/slack.json with bot_token, app_token, default_cwd", path.display())
        })?;
        let config: Config = serde_json::from_str(&contents)?;
        Ok(config)
    }

    pub fn is_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.is_empty() || self.allowed_users.contains(&user_id.to_owned())
    }
}
