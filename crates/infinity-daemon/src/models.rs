//! Registry of model providers and the models they offer.
//!
//! Providers are stored in a map keyed by their stable unique id (e.g.
//! `"bedrock"`). Models are identified globally by a [`ModelRef`]
//! (provider id + provider-scoped model id).

use std::collections::HashMap;
use std::sync::Arc;

use infinity_agent_core::model_provider::{ModelEntry, ModelProvider};
use infinity_protocol::ModelRef;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
            for entry in provider.list_models().await? {
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
