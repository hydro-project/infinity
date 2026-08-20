//! The [`ModelSource`] trait: choosing the model for each completion round.

use std::sync::Arc;

use async_trait::async_trait;

use infinity_provider_protocol::{ModelEntry, ModelProvider};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The model a thread will use for one completion round.
#[derive(Clone)]
pub struct ResolvedModel {
    pub provider: Arc<dyn ModelProvider>,
    pub model_id: String,
    /// Context window size in tokens. Used for the auto-compaction threshold;
    /// `0` disables it.
    pub context_window: usize,
    /// Whether the model accepts image inputs. When `false`, image tool
    /// results are replaced with a text placeholder before invocation.
    pub supports_image_input: bool,
}

/// Chooses the model for each completion round of each thread.
///
/// Resolution happens at the start of every round, which is what makes
/// mid-session model switching safe: persist the new selection wherever your
/// implementation reads it from, and the next round picks it up. An in-flight
/// completion always finishes on the model it started with.
#[async_trait(?Send)]
pub trait ModelSource {
    async fn resolve(&self, thread_id: &str) -> Result<ResolvedModel, BoxError>;
}

/// A fixed model used for every thread and every round.
pub struct StaticModel {
    resolved: ResolvedModel,
}

impl StaticModel {
    /// Look up `model_id` in the provider's catalog to capture its context
    /// window and image-input support.
    pub async fn new(provider: Arc<dyn ModelProvider>, model_id: &str) -> Result<Self, BoxError> {
        let models = provider.list_models().await?;
        let entry = models
            .into_iter()
            .find(|m| m.model_id == model_id)
            .ok_or_else(|| format!("model '{model_id}' not found in provider catalog"))?;
        Ok(Self::from_entry(provider, &entry))
    }

    /// Build from an already-resolved [`ModelEntry`].
    pub fn from_entry(provider: Arc<dyn ModelProvider>, entry: &ModelEntry) -> Self {
        Self {
            resolved: ResolvedModel {
                provider,
                model_id: entry.model_id.clone(),
                context_window: entry.context_window,
                supports_image_input: entry.supports_image_input,
            },
        }
    }
}

#[async_trait(?Send)]
impl ModelSource for StaticModel {
    async fn resolve(&self, _thread_id: &str) -> Result<ResolvedModel, BoxError> {
        Ok(self.resolved.clone())
    }
}
