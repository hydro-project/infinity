/// A model available for selection.
#[derive(Clone)]
pub struct ModelEntry {
    pub display_name: String,
    pub model_id: String,
    pub additional_request_params: Option<serde_json::Value>,
    pub context_window: usize,
}

/// Trait for model providers to expose their available models.
pub trait ModelProvider {
    fn available_models(&self) -> Vec<ModelEntry>;
    fn default_model_index(&self) -> usize;
    fn is_available(&self) -> impl Future<Output = Result<(), String>> + Send;
}

pub struct BedrockProvider;

impl ModelProvider for BedrockProvider {
    fn available_models(&self) -> Vec<ModelEntry> {
        vec![
            ModelEntry {
                display_name: "claude-opus-4-6 1m".to_owned(),
                model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
                additional_request_params: Some(serde_json::json!({
                    "anthropic_beta": ["context-1m-2025-08-07"]
                })),
                context_window: 1_000_000,
            },
            ModelEntry {
                display_name: "claude-opus-4-6".to_owned(),
                model_id: "global.anthropic.claude-opus-4-6-v1".to_owned(),
                additional_request_params: None,
                context_window: 200_000,
            },
        ]
    }

    fn default_model_index(&self) -> usize {
        0
    }

    async fn is_available(&self) -> Result<(), String> {
        Ok(())
    }
}
