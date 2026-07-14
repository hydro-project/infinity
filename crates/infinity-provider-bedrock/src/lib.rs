//! AWS Bedrock implementation of the [`ModelProvider`] trait.
//!
//! Wraps a rig-bedrock client and handles all Bedrock-specific request
//! parameters internally: per-model `additional_model_request_fields` (e.g.
//! anthropic thinking configuration and beta flags) and per-model max output
//! token limits. Callers only deal in plain rig [`CompletionRequest`]s.

use async_trait::async_trait;
use infinity_provider_protocol::{
    ModelEntry, ModelProvider, ProviderCompletionResponse, erase_streaming_response,
};
use rig::client::CompletionClient;
use rig::completion::{CompletionError, CompletionModel, CompletionRequest};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A model offered by the Bedrock provider, along with the Bedrock-specific
/// invocation configuration that stays internal to this crate.
struct BedrockModel {
    entry: ModelEntry,
    /// The actual Bedrock model id to invoke. Multiple catalog entries may
    /// map to the same Bedrock model with different request parameters, so
    /// `entry.model_id` is a provider-scoped id that may differ from this.
    bedrock_model_id: String,
    /// Extra `additional_model_request_fields` merged into every request.
    additional_request_params: Option<serde_json::Value>,
}

/// [`ModelProvider`] backed by AWS Bedrock via rig-bedrock.
pub struct BedrockProvider {
    client: rig_bedrock::client::Client,
    models: Vec<BedrockModel>,
}

impl BedrockProvider {
    /// Create a provider using AWS configuration from the environment.
    pub fn from_env() -> Self {
        use rig::client::ProviderClient;
        Self::new(rig_bedrock::client::Client::from_env())
    }

    pub fn new(client: rig_bedrock::client::Client) -> Self {
        Self {
            client,
            models: default_models(),
        }
    }
}

fn default_models() -> Vec<BedrockModel> {
    let summarized_adaptive_thinking = serde_json::json!({
        "thinking": {
            "type": "adaptive",
            "display": "summarized"
        }
    });
    let adaptive_thinking = serde_json::json!({
        "thinking": {
            "type": "adaptive"
        }
    });
    vec![
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-fable-5".to_owned(),
                display_name: "claude-fable-5".to_owned(),
                context_window: 1_000_000,
                max_output_tokens: Some(128_000),
            },
            bedrock_model_id: "global.anthropic.claude-fable-5".to_owned(),
            additional_request_params: Some(summarized_adaptive_thinking.clone()),
        },
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-opus-4-8".to_owned(),
                display_name: "claude-opus-4.8".to_owned(),
                context_window: 1_000_000,
                max_output_tokens: Some(128_000),
            },
            bedrock_model_id: "global.anthropic.claude-opus-4-8".to_owned(),
            additional_request_params: Some(summarized_adaptive_thinking),
        },
        BedrockModel {
            entry: ModelEntry {
                // Provider-scoped id: same Bedrock model as below, but with
                // the 1M-context beta enabled.
                model_id: "global.anthropic.claude-opus-4-6-v1:1m".to_owned(),
                display_name: "claude-opus-4-6 1m".to_owned(),
                context_window: 1_000_000,
                max_output_tokens: Some(128_000),
            },
            bedrock_model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
            additional_request_params: Some(serde_json::json!({
                "thinking": {
                    "type": "adaptive"
                },
                "anthropic_beta": ["context-1m-2025-08-07"]
            })),
        },
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
                display_name: "claude-opus-4-6".to_owned(),
                context_window: 200_000,
                max_output_tokens: Some(128_000),
            },
            bedrock_model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
            additional_request_params: Some(adaptive_thinking.clone()),
        },
        BedrockModel {
            entry: ModelEntry {
                model_id: "global.anthropic.claude-sonnet-4-6".to_owned(),
                display_name: "claude-sonnet-4-6".to_owned(),
                context_window: 200_000,
                max_output_tokens: Some(64_000),
            },
            bedrock_model_id: "global.anthropic.claude-sonnet-4-6".to_owned(),
            additional_request_params: Some(adaptive_thinking),
        },
    ]
}

#[async_trait]
impl ModelProvider for BedrockProvider {
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        Ok(self.models.iter().map(|m| m.entry.clone()).collect())
    }

    async fn invoke_model(
        &self,
        model_id: &str,
        mut request: CompletionRequest,
    ) -> Result<ProviderCompletionResponse, CompletionError> {
        let known = self.models.iter().find(|m| m.entry.model_id == model_id);

        // Resolve the actual Bedrock model id (provider-scoped ids may alias
        // the same Bedrock model with different parameters). Unknown ids are
        // passed through unchanged so callers can invoke arbitrary models.
        let bedrock_model_id = known
            .map(|m| m.bedrock_model_id.as_str())
            .unwrap_or(model_id);

        // Merge the per-model request parameters (e.g. anthropic thinking
        // config) with any caller-supplied additional params (caller wins).
        // Unknown models get only what the caller provided, so non-anthropic
        // models are not sent anthropic-specific parameters.
        let params = match (
            known.and_then(|m| m.additional_request_params.clone()),
            request.additional_params.take(),
        ) {
            (Some(mut base), Some(caller)) => {
                merge_params(&mut base, &caller);
                Some(base)
            }
            (base, caller) => base.or(caller),
        };
        request.additional_params = params;

        if request.max_tokens.is_none() {
            request.max_tokens = known.and_then(|m| m.entry.max_output_tokens);
        }

        // The model is selected via the completion model below; a per-request
        // override would be silently ignored, so reject it as a caller bug.
        assert!(
            request.model.is_none(),
            "bug: request.model must not be set; pass the model id to invoke_model instead"
        );

        let model = self.client.completion_model(bedrock_model_id);
        Ok(erase_streaming_response(model.stream(request).await?))
    }
}

/// Shallow-merge `extra`'s top-level keys into `base` (extra wins).
fn merge_params(base: &mut serde_json::Value, extra: &serde_json::Value) {
    if let (Some(base_obj), Some(extra_obj)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
}
