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
    /// Optional default model to use for new sessions. When set, overrides
    /// the daemon's default. Format: `{ "provider_id": "...", "model_id": "..." }`.
    #[serde(default)]
    pub default_model: Option<infinity_protocol::ModelRef>,
    /// Path this config was loaded from, if any. `None` for in-memory
    /// configs (tests), in which case persistence is a no-op.
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path = infinity_protocol::state_dir().join("slack.json");
        let contents = std::fs::read_to_string(&path).map_err(|e| {
            format!("Failed to read {}: {e}\nCreate ~/.infinity/slack.json with bot_token, app_token, default_cwd", path.display())
        })?;
        let mut config: Config = serde_json::from_str(&contents)?;
        config.path = Some(path);
        Ok(config)
    }

    /// Persist a new default model back to the config file so it survives
    /// restarts. Patches only the `default_model` field, leaving all other
    /// fields (including any unknown to this struct) untouched. No-op if the
    /// config was not loaded from a file.
    pub fn save_default_model(
        &self,
        model: &infinity_protocol::ModelRef,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let contents = std::fs::read_to_string(path)?;
        let mut value: serde_json::Value = serde_json::from_str(&contents)?;
        let obj = value
            .as_object_mut()
            .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
        obj.insert("default_model".to_owned(), serde_json::to_value(model)?);
        std::fs::write(path, serde_json::to_string_pretty(&value)?)?;
        Ok(())
    }

    pub fn is_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.is_empty() || self.allowed_users.contains(&user_id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(path: Option<PathBuf>) -> Config {
        Config {
            bot_token: String::new(),
            app_token: String::new(),
            default_cwd: PathBuf::from("/tmp"),
            allowed_users: vec![],
            default_model: None,
            path,
        }
    }

    fn model() -> infinity_protocol::ModelRef {
        infinity_protocol::ModelRef {
            provider_id: "bedrock".to_owned(),
            model_id: "claude-sonnet-4".to_owned(),
        }
    }

    #[test]
    fn save_default_model_patches_file_preserving_other_fields() {
        let path = std::env::temp_dir().join(format!(
            "slack_config_test_save_{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"bot_token":"xoxb","app_token":"xapp","default_cwd":"/tmp","unknown_field":42}"#,
        )
        .expect("write should succeed");

        let config = test_config(Some(path.clone()));
        config
            .save_default_model(&model())
            .expect("save should succeed");

        let contents = std::fs::read_to_string(&path).expect("read should succeed");
        let value: serde_json::Value =
            serde_json::from_str(&contents).expect("should be valid JSON");
        assert_eq!(value["default_model"]["provider_id"], "bedrock");
        assert_eq!(value["default_model"]["model_id"], "claude-sonnet-4");
        // Other fields, including ones unknown to Config, are preserved.
        assert_eq!(value["bot_token"], "xoxb");
        assert_eq!(value["unknown_field"], 42);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_default_model_without_path_is_noop() {
        let config = test_config(None);
        config
            .save_default_model(&model())
            .expect("no-op save should succeed");
    }
}
