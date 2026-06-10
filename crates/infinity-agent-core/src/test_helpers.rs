//! Shared helpers for unit tests within this crate.

use rig_mock::{MockCompletionModel, MockModelController, mock_model};

use crate::model_provider::{ModelEntry, SingleModelProvider};

/// Wrap a mock model as a single-model provider. The model is registered as
/// `"mock"` with a zero context window and no output token limit.
pub(crate) fn mock_provider() -> (
    SingleModelProvider<MockCompletionModel>,
    MockModelController,
) {
    let (model, ctrl) = mock_model();
    let entry = ModelEntry {
        model_id: "mock".to_owned(),
        display_name: "mock".to_owned(),
        context_window: 0,
        max_output_tokens: None,
    };
    (SingleModelProvider::new(entry, model), ctrl)
}
