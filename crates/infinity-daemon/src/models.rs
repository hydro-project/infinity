//! Registry of model providers and the models they offer.
//!
//! Providers are stored in a map keyed by their stable unique id (e.g.
//! `"bedrock"`). Models are identified globally by a [`ModelRef`]
//! (provider id + provider-scoped model id).
//!
//! Providers run as separate processes configured in
//! `~/.infinity/providers.json` (see [`ProvidersConfig`]): each entry maps a
//! provider id to the command that serves that provider over a Unix socket.
//! The daemon spawns each command, reads the socket path the process prints
//! on stdout, and talks to it through a
//! [`RemoteModelProvider`](infinity_provider_protocol::remote::RemoteModelProvider).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use infinity_protocol::ModelRef;
use infinity_provider_protocol::remote::RemoteModelProvider;
use infinity_provider_protocol::{ModelEntry, ModelProvider};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How long to wait for a spawned provider process to print its socket path.
const PROVIDER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration for a single model provider process in `providers.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Command (argv) that serves this provider over a Unix socket and
    /// prints the socket path on stdout.
    pub command: Vec<String>,
    /// Installation source info (mirroring rap.json toolset commands) so the
    /// provider can be re-installed by `infinity provider update`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The providers configured in `~/.infinity/providers.json`: a JSON object
/// mapping provider id to a [`ProviderConfig`], e.g.:
///
/// ```json
/// {
///   "bedrock": { "command": ["infinity-provider-bedrock"] },
///   "custom": { "command": ["my-provider", "--flag"], "crate_name": "my-provider" }
/// }
/// ```
///
/// Stored as a `Vec` of `(id, config)` pairs because the JSON object's
/// insertion order is significant — providers are registered in config
/// order, and the first model of the first provider is the global default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProvidersConfig {
    pub providers: Vec<(String, ProviderConfig)>,
}

impl ProvidersConfig {
    pub fn get(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers
            .iter()
            .find(|(pid, _)| pid == id)
            .map(|(_, config)| config)
    }

    /// Insert or replace the config for `id`, preserving its position when
    /// it already exists.
    pub fn upsert(&mut self, id: String, config: ProviderConfig) {
        if let Some((_, existing)) = self.providers.iter_mut().find(|(pid, _)| *pid == id) {
            *existing = config;
        } else {
            self.providers.push((id, config));
        }
    }
}

// Serialize as a JSON object (entries in `providers` order).
impl Serialize for ProvidersConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.providers.len()))?;
        for (id, config) in &self.providers {
            map.serialize_entry(id, config)?;
        }
        map.end()
    }
}

// Deserialize from a JSON object, preserving the document's entry order
// (serde visits map entries in the order they appear in the input).
impl<'de> Deserialize<'de> for ProvidersConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedMapVisitor;

        impl<'de> serde::de::Visitor<'de> for OrderedMapVisitor {
            type Value = ProvidersConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map from provider id to provider config")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut providers = Vec::new();
                while let Some((id, config)) = access.next_entry::<String, ProviderConfig>()? {
                    providers.push((id, config));
                }
                Ok(ProvidersConfig { providers })
            }
        }

        deserializer.deserialize_map(OrderedMapVisitor)
    }
}

/// Load the providers config from `path`. There is no implicit default: at
/// least one provider must be configured (e.g. via
/// `infinity provider install`) for the daemon to start.
pub fn load_providers_config(path: &Path) -> Result<ProvidersConfig, BoxError> {
    if !path.exists() {
        return Err(format!(
            "no model providers configured ({} not found); \
             run `infinity provider install <id> --crate <name>` to install one \
             (e.g. `infinity provider install bedrock --crate infinity-provider-bedrock`)",
            path.display()
        )
        .into());
    }
    let json = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let config: ProvidersConfig = serde_json::from_str(&json)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    if config.providers.is_empty() {
        return Err(format!("no providers configured in {}", path.display()).into());
    }
    let mut seen = std::collections::HashSet::new();
    for (provider_id, provider) in &config.providers {
        if provider_id.is_empty() {
            return Err(format!("empty provider id in {}", path.display()).into());
        }
        if !seen.insert(provider_id) {
            return Err(
                format!("duplicate provider id {provider_id} in {}", path.display()).into(),
            );
        }
        if provider.command.is_empty() {
            return Err(format!(
                "provider {provider_id} in {} has an empty command",
                path.display()
            )
            .into());
        }
    }
    Ok(config)
}

/// Forward a child output stream to the daemon's tracing log, line by line.
/// Also keeps the pipe drained so the child never blocks on a full pipe.
fn forward_output_to_log(
    provider_id: &str,
    stream_name: &'static str,
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) {
    let id = provider_id.to_owned();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stream).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::info!("provider {id} {stream_name}: {line}"),
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("failed reading {stream_name} of provider {id}: {e}");
                    break;
                }
            }
        }
    });
}

/// Spawn a provider process and wait for it to print its socket path on
/// stdout. The returned [`Child`](tokio::process::Child) kills the process
/// when dropped; callers must keep it alive for as long as the provider is
/// in use.
///
/// The child gets a null stdin and piped stdout/stderr — never the daemon's
/// own handles, which can be closed pipes when the daemon was auto-launched
/// in the background. Everything the provider prints (beyond the initial
/// socket path line) is forwarded to the daemon's log.
pub async fn spawn_provider(
    provider_id: &str,
    command: &[String],
) -> Result<(RemoteModelProvider, tokio::process::Child), BoxError> {
    let mut child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            format!(
                "failed to spawn provider {provider_id} ({}): {e}",
                command[0]
            )
        })?;

    // Forward stderr to the log right away so startup logs are captured and
    // a chatty provider can't fill the pipe while we wait for the socket
    // path below.
    let stderr = child
        .stderr
        .take()
        .expect("bug: piped child stderr missing");
    forward_output_to_log(provider_id, "stderr", stderr);

    let stdout = child
        .stdout
        .take()
        .expect("bug: piped child stdout missing");
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let socket_path = tokio::time::timeout(PROVIDER_STARTUP_TIMEOUT, lines.next_line())
        .await
        .map_err(|_| format!("provider {provider_id} did not print a socket path in time"))?
        .map_err(|e| format!("failed reading socket path from provider {provider_id}: {e}"))?
        .ok_or_else(|| format!("provider {provider_id} exited without printing a socket path"))?;

    // Forward any further stdout output to the log as well.
    forward_output_to_log(provider_id, "stdout", lines.into_inner());

    Ok((RemoteModelProvider::new(socket_path.trim()), child))
}

/// Spawn every provider in the config. Returns the `(provider_id, provider)`
/// pairs for [`ModelCatalog::new`] plus the child process handles, which
/// must be kept alive for as long as the providers are in use.
pub async fn spawn_configured_providers(
    config: &ProvidersConfig,
) -> Result<
    (
        Vec<(String, Arc<dyn ModelProvider>)>,
        Vec<tokio::process::Child>,
    ),
    BoxError,
> {
    let mut providers: Vec<(String, Arc<dyn ModelProvider>)> = Vec::new();
    let mut children = Vec::new();
    for (provider_id, provider_config) in &config.providers {
        let (provider, child) = spawn_provider(provider_id, &provider_config.command).await?;
        tracing::info!(
            "provider {provider_id} serving on {}",
            provider.socket_path().display()
        );
        providers.push((provider_id.clone(), Arc::new(provider) as _));
        children.push(child);
    }
    Ok((providers, children))
}

/// A model in the catalog, tagged with the provider that offers it.
#[derive(Clone)]
pub struct CatalogModel {
    pub provider_id: String,
    pub entry: ModelEntry,
}

/// All registered providers and their available models.
pub struct ModelCatalog {
    /// Providers keyed by their stable unique id.
    providers: HashMap<String, Arc<dyn ModelProvider>>,
    /// Flattened list of all available models, in registration order.
    models: Vec<CatalogModel>,
    /// The global default model, used for any thread without a selected model.
    default: ModelRef,
}

impl ModelCatalog {
    /// Build a catalog from `(provider_id, provider)` pairs (in registration
    /// order). Provider ids must be stable, unique, and non-empty. The first
    /// model of the first provider becomes the global default.
    pub async fn new(providers: Vec<(String, Arc<dyn ModelProvider>)>) -> Result<Self, BoxError> {
        let mut provider_map: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        let mut models = Vec::new();

        for (provider_id, provider) in providers {
            // The empty string is reserved: the conversation store uses an
            // empty provider id as the serde sentinel for thread metadata
            // that predates per-thread model tracking.
            if provider_id.is_empty() {
                return Err("model provider id must not be empty".into());
            }
            if provider_map.contains_key(&provider_id) {
                return Err(format!("duplicate model provider id: {provider_id}").into());
            }
            for entry in provider
                .list_models()
                .await
                .map_err(|e| format!("failed to list models from provider {provider_id}: {e}"))?
            {
                models.push(CatalogModel {
                    provider_id: provider_id.clone(),
                    entry,
                });
            }
            provider_map.insert(provider_id, provider);
        }

        let default = models
            .first()
            .map(|m| ModelRef {
                provider_id: m.provider_id.clone(),
                model_id: m.entry.model_id.clone(),
            })
            .ok_or("no models available from any provider")?;

        Ok(Self {
            providers: provider_map,
            models,
            default,
        })
    }

    /// All available models, in registration order.
    pub fn models(&self) -> &[CatalogModel] {
        &self.models
    }

    /// The global default model.
    pub fn default_ref(&self) -> &ModelRef {
        &self.default
    }

    /// The entry for the global default model.
    pub fn default_entry(&self) -> &ModelEntry {
        self.find(&self.default)
            .expect("bug: default model missing from catalog")
    }

    /// Look up a model's entry by reference.
    pub fn find(&self, model: &ModelRef) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.provider_id == model.provider_id && m.entry.model_id == model.model_id)
            .map(|m| &m.entry)
    }

    /// Look up a provider by its id.
    pub fn provider(&self, provider_id: &str) -> Option<&Arc<dyn ModelProvider>> {
        self.providers.get(provider_id)
    }

    /// Resolve a selected model to a concrete `(ModelRef, entry)`, falling
    /// back to the global default when the selection is no longer available.
    /// Returns `(model, entry, fell_back)` where `fell_back` is true when a
    /// stale selection was replaced by the default.
    pub fn resolve(&self, selected: &ModelRef) -> (ModelRef, &ModelEntry, bool) {
        if let Some(entry) = self.find(selected) {
            return (selected.clone(), entry, false);
        }
        let entry = self
            .find(&self.default)
            .expect("bug: default model missing from catalog");
        (self.default.clone(), entry, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_providers_config_is_an_error() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let Err(err) = load_providers_config(&dir.path().join("providers.json")) else {
            panic!("missing config should be an error");
        };
        assert!(
            err.to_string().contains("infinity provider install"),
            "error should point at `infinity provider install`: {err}"
        );
    }

    #[test]
    fn providers_config_parses_entries_and_preserves_order() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("providers.json");
        // Note: ids deliberately not in alphabetical order — the document
        // order must be preserved (it determines the default provider).
        std::fs::write(
            &path,
            r#"{
                "zeta": { "command": ["zeta-provider"] },
                "bedrock": {
                    "command": ["infinity-provider-bedrock"],
                    "crate_name": "infinity-provider-bedrock"
                },
                "custom": {
                    "command": ["/usr/local/bin/my-provider", "--flag", "value"],
                    "crate_name": "my-provider",
                    "git": "https://example.com/my-provider.git"
                }
            }"#,
        )
        .expect("write config");
        let config = load_providers_config(&path).expect("load config");
        let ids: Vec<&str> = config.providers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["zeta", "bedrock", "custom"]);
        let custom = config.get("custom").expect("custom provider");
        assert_eq!(
            custom.command,
            vec![
                "/usr/local/bin/my-provider".to_owned(),
                "--flag".to_owned(),
                "value".to_owned()
            ]
        );
        assert_eq!(custom.crate_name.as_deref(), Some("my-provider"));
        assert_eq!(
            custom.git.as_deref(),
            Some("https://example.com/my-provider.git")
        );
        assert_eq!(custom.path, None);
    }

    #[test]
    fn providers_config_round_trips_in_order() {
        let mut config = ProvidersConfig::default();
        config.upsert(
            "zeta".to_owned(),
            ProviderConfig {
                command: vec!["zeta-provider".to_owned()],
                crate_name: None,
                git: None,
                path: None,
            },
        );
        config.upsert(
            "alpha".to_owned(),
            ProviderConfig {
                command: vec!["alpha-provider".to_owned()],
                crate_name: Some("alpha-provider".to_owned()),
                git: None,
                path: Some("/src/alpha".to_owned()),
            },
        );
        let json = serde_json::to_string_pretty(&config).expect("serialize");
        // "zeta" must come before "alpha" in the serialized object.
        assert!(json.find("zeta").expect("zeta") < json.find("alpha").expect("alpha"));
        let parsed: ProvidersConfig = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, config);
    }

    #[test]
    fn providers_config_upsert_replaces_in_place() {
        let mut config = ProvidersConfig::default();
        let entry = |cmd: &str| ProviderConfig {
            command: vec![cmd.to_owned()],
            crate_name: None,
            git: None,
            path: None,
        };
        config.upsert("first".to_owned(), entry("first-v1"));
        config.upsert("second".to_owned(), entry("second-v1"));
        config.upsert("first".to_owned(), entry("first-v2"));
        let ids: Vec<&str> = config.providers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["first", "second"]);
        assert_eq!(
            config.get("first").map(|p| p.command.clone()),
            Some(vec!["first-v2".to_owned()])
        );
    }

    #[test]
    fn providers_config_rejects_invalid_entries() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("providers.json");

        std::fs::write(&path, "{}").expect("write config");
        assert!(load_providers_config(&path).is_err(), "empty config");

        std::fs::write(&path, r#"{"bedrock": {"command": []}}"#).expect("write config");
        assert!(load_providers_config(&path).is_err(), "empty command");

        std::fs::write(
            &path,
            r#"{"a": {"command": ["x"]}, "a": {"command": ["y"]}}"#,
        )
        .expect("write config");
        assert!(load_providers_config(&path).is_err(), "duplicate ids");

        std::fs::write(&path, "not json").expect("write config");
        assert!(load_providers_config(&path).is_err(), "invalid json");
    }
}
