use infinity_agent_core::tools::config::ToolsConfig;
use std::path::PathBuf;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub fn user_config_path() -> Result<PathBuf, BoxError> {
    let home = dirs::home_dir().ok_or("could not determine home directory")?;
    Ok(home.join(".infinity").join("rap.json"))
}

/// Path to the model providers config: `~/.infinity/providers.json`.
pub fn providers_config_path() -> Result<PathBuf, BoxError> {
    let home = dirs::home_dir().ok_or("could not determine home directory")?;
    Ok(home.join(".infinity").join("providers.json"))
}

pub fn load_config(path: &std::path::Path) -> Result<ToolsConfig, BoxError> {
    ToolsConfig::from_file(path)
}
