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

/// Load the merged RAP config for `cwd`: the cwd-local `.infinity/rap.json`
/// merged with the optional user-level config. Returns a human-readable line
/// describing which config source(s) were used (for display to clients) and
/// the merged config — `None` when neither source exists.
///
/// This is the single place the local/user merge semantics live; both the
/// lazily managed per-session servers and the migration flows go through it.
pub fn load_merged_rap_config(
    cwd: &std::path::Path,
    user_config_path: Option<&std::path::Path>,
) -> Result<(String, Option<ToolsConfig>), BoxError> {
    let cwd_rap = cwd.join(".infinity").join("rap.json");
    let local_config = cwd_rap
        .exists()
        .then(|| load_config(&cwd_rap))
        .transpose()?;
    let user_config = user_config_path
        .and_then(|p| p.exists().then(|| load_config(p)))
        .transpose()?;

    Ok(match (local_config, user_config) {
        (None, None) => (
            "Neither local nor user RAP configs exist, using empty config".into(),
            None,
        ),
        (None, Some(c)) => ("Using user config".into(), Some(c)),
        (Some(c), None) => ("Using local config".into(), Some(c)),
        (Some(mut l), Some(u)) => {
            l.merge(u);
            (
                "Both local and user RAP configs exist, merging".into(),
                Some(l),
            )
        }
    })
}
