//! Shared helpers for unit tests within this crate.

use rig_mock::{MockCompletionModel, MockModelController, mock_model};

use infinity_provider_protocol::{ModelEntry, SingleModelProvider};

/// Wrap a mock model as a single-model provider. The model is registered as
/// `"mock"` with a zero context window and no output token limit.
pub(crate) fn mock_provider() -> (
    SingleModelProvider<MockCompletionModel>,
    MockModelController,
) {
    mock_provider_with_image_support(false)
}

/// Like [`mock_provider`] but with configurable image input support.
pub(crate) fn mock_provider_with_image_support(
    supports_image_input: bool,
) -> (
    SingleModelProvider<MockCompletionModel>,
    MockModelController,
) {
    let (model, ctrl) = mock_model();
    let entry = ModelEntry {
        model_id: "mock".to_owned(),
        display_name: "mock".to_owned(),
        context_window: 0,
        max_output_tokens: None,
        supports_image_input,
    };
    (SingleModelProvider::new(entry, model), ctrl)
}
